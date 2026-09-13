//! Managed hub connections with explicit host scheduling and no ambiguous replay.
use crate::transport::TransportConnectionPhase;
use crate::{AskOptions, Client, Context, Data, Event, Reply, Result, ThalovantError};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex as StateMutex,
    },
    time::{Duration, Instant},
};
use tokio::{
    sync::{broadcast, Mutex},
    task::JoinHandle,
};

#[derive(Clone, Copy, Debug)]
pub struct HubSessionPolicy {
    pub retry: Duration,
    pub retry_ceiling: Duration,
    pub probe: Duration,
    pub probe_down: Duration,
}
impl Default for HubSessionPolicy {
    fn default() -> Self {
        Self {
            retry: Duration::from_secs(10),
            retry_ceiling: Duration::from_secs(120),
            probe: Duration::from_secs(60),
            probe_down: Duration::from_secs(5),
        }
    }
}
impl HubSessionPolicy {
    pub fn validate(self) -> Result<Self> {
        if self.retry.is_zero()
            || self.retry_ceiling < self.retry
            || self.probe.is_zero()
            || self.probe_down.is_zero()
            || Instant::now().checked_add(self.retry_ceiling).is_none()
        {
            return Err(ThalovantError::Connection(
                "invalid hub session policy".into(),
            ));
        }
        Ok(self)
    }
    pub fn next_wait(&self, current: Duration) -> Duration {
        current.saturating_mul(2).min(self.retry_ceiling)
    }
}
pub async fn alive(client: &Client) -> bool {
    !matches!(
        client.connection_info().await.phase,
        TransportConnectionPhase::Closed | TransportConnectionPhase::Error
    )
}
#[derive(Clone, Debug)]
pub enum HubSessionEvent {
    Message(Event),
    Lagged(u64),
    Disconnected,
}
type ConnectFuture = Pin<Box<dyn Future<Output = Result<Client>> + Send>>;
struct Held {
    client: Option<Client>,
    retired: Option<Client>,
    relay: Option<JoinHandle<()>>,
}
struct Events {
    generation: u64,
    sender: Option<broadcast::Sender<HubSessionEvent>>,
}
struct Retry {
    at: Option<Instant>,
    wait: Duration,
}
struct Inner {
    connect: Box<dyn Fn() -> ConnectFuture + Send + Sync>,
    policy: HubSessionPolicy,
    held: Mutex<Held>,
    events: StateMutex<Events>,
    retry: StateMutex<Retry>,
    closed: AtomicBool,
    warming: AtomicBool,
    has_client: AtomicBool,
}
/// Clone handles share the same client, admission barrier and subscription bus.
/// The factory connects a fresh client and owns cleanup if it fails/cancels.
#[derive(Clone)]
pub struct HubSession {
    inner: Arc<Inner>,
}
impl HubSession {
    pub fn new<F, Fut>(connect: F, policy: HubSessionPolicy) -> Result<Self>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Client>> + Send + 'static,
    {
        let policy = policy.validate()?;
        let (sender, _) = broadcast::channel(256);
        Ok(Self {
            inner: Arc::new(Inner {
                connect: Box::new(move || Box::pin(connect())),
                policy,
                held: Mutex::new(Held {
                    client: None,
                    retired: None,
                    relay: None,
                }),
                events: StateMutex::new(Events {
                    generation: 0,
                    sender: Some(sender),
                }),
                retry: StateMutex::new(Retry {
                    at: None,
                    wait: policy.retry,
                }),
                closed: AtomicBool::new(false),
                warming: AtomicBool::new(false),
                has_client: AtomicBool::new(false),
            }),
        })
    }
    pub fn held(&self) -> bool {
        self.inner.has_client.load(Ordering::Acquire)
    }
    pub fn retry_at(&self) -> Option<Instant> {
        self.inner.retry.lock().unwrap().at
    }
    pub fn retry_wait(&self) -> Duration {
        self.inner.retry.lock().unwrap().wait
    }
    pub fn probe_delay(&self) -> Duration {
        if self.held() {
            self.inner.policy.probe
        } else {
            self.inner.policy.probe_down
        }
    }
    pub fn subscribe(&self) -> Result<broadcast::Receiver<HubSessionEvent>> {
        self.inner
            .events
            .lock()
            .unwrap()
            .sender
            .as_ref()
            .map(|sender| sender.subscribe())
            .ok_or_else(|| ThalovantError::Connection("hub session is closed".into()))
    }
    async fn cleanup(held: &mut Held) -> Result<()> {
        if let Some(client) = &held.retired {
            client.close().await?;
            held.retired = None
        }
        Ok(())
    }
    async fn drop_client(&self, held: &mut Held) -> Result<()> {
        {
            let mut events = self.inner.events.lock().unwrap();
            events.generation = events.generation.wrapping_add(1);
        }
        if let Some(relay) = held.relay.take() {
            relay.abort()
        }
        if let Some(client) = held.client.take() {
            self.inner.has_client.store(false, Ordering::Release);
            held.retired = Some(client)
        }
        Self::cleanup(held).await
    }
    async fn ensure(&self, held: &mut Held) -> Result<Client> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(ThalovantError::Connection("hub session is closed".into()));
        }
        Self::cleanup(held).await?;
        if let Some(client) = &held.client {
            return Ok(client.clone());
        }
        let client = match (self.inner.connect)().await {
            Ok(client) => client,
            Err(error) => {
                let mut retry = self.inner.retry.lock().unwrap();
                retry.at = Some(Instant::now() + retry.wait);
                retry.wait = self.inner.policy.next_wait(retry.wait);
                return Err(error);
            }
        };
        if self.inner.closed.load(Ordering::Acquire) {
            held.retired = Some(client);
            Self::cleanup(held).await?;
            return Err(ThalovantError::Connection("hub session is closed".into()));
        }
        let mut receiver = client.transport.subscribe();
        let generation = {
            let mut events = self.inner.events.lock().unwrap();
            events.generation = events.generation.wrapping_add(1);
            events.generation
        };
        let weak = Arc::downgrade(&self.inner);
        held.relay = Some(tokio::spawn(async move {
            loop {
                let received = receiver.recv().await;
                let Some(inner) = weak.upgrade() else { break };
                let events = inner.events.lock().unwrap();
                if events.generation != generation || inner.closed.load(Ordering::Acquire) {
                    break;
                }
                let Some(sender) = events.sender.as_ref() else {
                    break;
                };
                match received {
                    Ok(event) => {
                        let _ = sender.send(HubSessionEvent::Message(event));
                    }
                    Err(broadcast::error::RecvError::Lagged(count)) => {
                        let _ = sender.send(HubSessionEvent::Lagged(count));
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        let _ = sender.send(HubSessionEvent::Disconnected);
                        break;
                    }
                }
            }
        }));
        held.client = Some(client.clone());
        self.inner.has_client.store(true, Ordering::Release);
        let mut retry = self.inner.retry.lock().unwrap();
        retry.at = None;
        retry.wait = self.inner.policy.retry;
        Ok(client)
    }
    /// Start one coalesced off-path attempt; a host may await its result.
    pub fn warm(&self) -> Option<JoinHandle<Result<()>>> {
        if self.inner.closed.load(Ordering::Acquire)
            || self.retry_at().is_some_and(|at| Instant::now() < at)
            || self.inner.warming.swap(true, Ordering::AcqRel)
        {
            return None;
        }
        struct Reset(Arc<Inner>);
        impl Drop for Reset {
            fn drop(&mut self) {
                self.0.warming.store(false, Ordering::Release)
            }
        }
        let reset = Reset(self.inner.clone());
        let session = self.clone();
        Some(tokio::spawn(async move {
            let _reset = reset;
            let mut held = session.inner.held.lock().await;
            session.ensure(&mut held).await.map(|_| ())
        }))
    }
    pub async fn probe(&self) -> Result<()> {
        let Ok(mut held) = self.inner.held.try_lock() else {
            return Ok(());
        };
        if self.inner.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        if let Some(client) = &held.client {
            if !alive(client).await {
                self.drop_client(&mut held).await?
            }
        }
        let needs = held.client.is_none();
        drop(held);
        if needs {
            drop(self.warm());
        }
        Ok(())
    }
    async fn call<T, F, Fut>(&self, operation: F) -> Result<T>
    where
        F: FnOnce(Client) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let mut held = self.inner.held.lock().await;
        if let Some(client) = &held.client {
            if !alive(client).await {
                self.drop_client(&mut held).await?
            }
        }
        let client = self.ensure(&mut held).await?;
        let result = operation(client).await;
        if result.as_ref().is_err_and(|error| {
            !matches!(
                error,
                ThalovantError::Runtime(_) | ThalovantError::PolicyDenied { .. }
            )
        }) {
            self.drop_client(&mut held).await?
        }
        result
    }
    pub async fn ask(&self, text: &str, options: AskOptions) -> Result<Reply> {
        self.call(|client| async move { client.ask_with_options(text, options).await })
            .await
    }
    pub async fn emit(&self, event_type: &str, data: Data, context: Context) -> Result<()> {
        self.call(|client| async move { client.emit(event_type, data, context).await })
            .await
    }
    /// Terminal close retains responsibility for a failed cleanup; retry close.
    pub async fn close(&self) -> Result<()> {
        self.inner.closed.store(true, Ordering::Release);
        {
            self.inner.events.lock().unwrap().sender = None;
        }
        let mut held = self.inner.held.lock().await;
        self.drop_client(&mut held).await
    }
}
pub fn hub_hostname(master: &str) -> String {
    let text = master.trim();
    if text.is_empty() {
        return String::new();
    }
    url::Url::parse(&if text.contains("://") {
        text.to_owned()
    } else {
        format!("wss://{text}")
    })
    .ok()
    .and_then(|u| u.host_str().map(str::to_owned))
    .unwrap_or_default()
}
#[derive(Clone, Debug)]
pub struct OriginAttempt {
    pub address: Option<String>,
    pub host: String,
    pub handshake_timeout: Option<Duration>,
    pub connect_timeout: Duration,
}
/// The builder owns a per-transport origin override and failed-attempt cleanup.
/// URLs and TLS server names must retain the public host; never disable TLS checks.
pub struct OriginPreference {
    pub address: String,
    pub handshake_timeout: Duration,
    pub cooldown: Duration,
    quiet_until: StateMutex<Option<Instant>>,
}
impl OriginPreference {
    pub fn new(address: String) -> Self {
        Self {
            address,
            handshake_timeout: Duration::from_millis(1500),
            cooldown: Duration::from_secs(300),
            quiet_until: StateMutex::new(None),
        }
    }
    pub fn cooling_down(&self) -> bool {
        self.quiet_until
            .lock()
            .unwrap()
            .is_some_and(|until| Instant::now() < until)
    }
    pub async fn connect<T, F, Fut>(&self, options: OriginAttempt, build: F) -> Result<T>
    where
        F: Fn(OriginAttempt) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        if self.handshake_timeout.is_zero()
            || self.cooldown.is_zero()
            || Instant::now().checked_add(self.cooldown).is_none()
        {
            return Err(ThalovantError::Connection(
                "origin budgets must be positive".into(),
            ));
        }
        if !self.address.is_empty() && !options.host.is_empty() && !self.cooling_down() {
            let mut preferred = options.clone();
            preferred.address = Some(self.address.clone());
            preferred.handshake_timeout = Some(self.handshake_timeout);
            match build(preferred).await {
                Ok(client) => {
                    *self.quiet_until.lock().unwrap() = None;
                    return Ok(client);
                }
                Err(_) => {
                    *self.quiet_until.lock().unwrap() = Some(Instant::now() + self.cooldown);
                }
            }
        }
        let mut public = options;
        public.address = None;
        build(public).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Identity;
    use std::sync::atomic::AtomicUsize;
    use tokio::sync::Notify;

    fn client() -> Client {
        Client::new(Identity::from_json(r#"{"access_key":"fixture","password":"password","site_id":"fixture","default_master":"https://hub.example"}"#).unwrap())
    }

    #[tokio::test]
    async fn ambiguous_operation_is_not_replayed_and_next_call_rebuilds() {
        let builds = Arc::new(AtomicUsize::new(0));
        let count = builds.clone();
        let session = HubSession::new(
            move || {
                count.fetch_add(1, Ordering::SeqCst);
                async { Ok(client()) }
            },
            HubSessionPolicy::default(),
        )
        .unwrap();
        let dispatches = AtomicUsize::new(0);
        let result: Result<()> = session
            .call(|_| async {
                dispatches.fetch_add(1, Ordering::SeqCst);
                Err(ThalovantError::Connection("closed after dispatch".into()))
            })
            .await;
        assert!(result.is_err());
        assert_eq!(dispatches.load(Ordering::SeqCst), 1);
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert!(!session.held());
        session.call(|_| async { Ok(()) }).await.unwrap();
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        assert!(session.held());
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn foreground_bypasses_backoff_and_close_is_terminal() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let count = attempts.clone();
        let session = HubSession::new(
            move || {
                let attempt = count.fetch_add(1, Ordering::SeqCst);
                async move {
                    if attempt == 0 {
                        Err(ThalovantError::Connection("offline".into()))
                    } else {
                        Ok(client())
                    }
                }
            },
            HubSessionPolicy::default(),
        )
        .unwrap();
        assert_eq!(session.probe_delay(), Duration::from_secs(5));
        assert!(session.warm().unwrap().await.unwrap().is_err());
        assert!(session.warm().is_none());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(session.retry_wait(), Duration::from_secs(20));
        session.call(|_| async { Ok(()) }).await.unwrap();
        assert_eq!(session.retry_wait(), Duration::from_secs(10));
        assert_eq!(session.probe_delay(), Duration::from_secs(60));
        let mut events = session.subscribe().unwrap();
        session.close().await.unwrap();
        assert!(matches!(
            events.recv().await,
            Err(broadcast::error::RecvError::Closed)
        ));
        assert!(session.subscribe().is_err());
        assert!(session.warm().is_none());
        assert!(session.call(|_| async { Ok(()) }).await.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn close_waits_for_admitted_operation_and_policy_error_keeps_connection() {
        let session =
            HubSession::new(|| async { Ok(client()) }, HubSessionPolicy::default()).unwrap();
        let denied: Result<()> = session
            .call(|_| async { Err(ThalovantError::Runtime("denied".into())) })
            .await;
        assert!(denied.is_err());
        assert!(session.held());
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let running = session.clone();
        let admitted = entered.clone();
        let retiring = release.clone();
        let owner = tokio::spawn(async move {
            running
                .call(|_| async move {
                    admitted.notify_one();
                    retiring.notified().await;
                    Ok(())
                })
                .await
        });
        entered.notified().await;
        let closing_session = session.clone();
        let closing = tokio::spawn(async move { closing_session.close().await });
        // This is the admission barrier, not a timing assertion: close cannot
        // acquire it until the operation releases its ownership.
        assert!(session.inner.held.try_lock().is_err());
        assert!(!closing.is_finished());
        release.notify_one();
        owner.await.unwrap().unwrap();
        closing.await.unwrap().unwrap();
        assert!(!session.held());
    }

    #[tokio::test]
    async fn origin_fallback_preserves_host_and_respects_cooldown() {
        let preference = OriginPreference::new("10.0.0.2".into());
        let attempts = StateMutex::new(Vec::new());
        let options = OriginAttempt {
            host: "hub.example".into(),
            address: None,
            connect_timeout: Duration::from_secs(12),
            handshake_timeout: None,
        };
        for _ in 0..2 {
            let result = preference
                .connect(options.clone(), |attempt| {
                    let preferred = attempt.address.is_some();
                    attempts.lock().unwrap().push(attempt);
                    async move {
                        if preferred {
                            Err(ThalovantError::Connection("offline".into()))
                        } else {
                            Ok("public")
                        }
                    }
                })
                .await
                .unwrap();
            assert_eq!(result, "public");
        }
        let attempts = attempts.lock().unwrap();
        assert_eq!(attempts.len(), 3);
        assert!(attempts.iter().all(|a| a.host == "hub.example"));
        assert_eq!(
            attempts[0].handshake_timeout,
            Some(Duration::from_millis(1500))
        );
        assert!(attempts[1].address.is_none());
        assert!(attempts[2].address.is_none());
        assert!(preference.cooling_down());
    }

    #[test]
    fn policy_rejects_invalid_and_overflowing_budgets() {
        for policy in [
            HubSessionPolicy {
                retry: Duration::ZERO,
                ..Default::default()
            },
            HubSessionPolicy {
                retry_ceiling: Duration::MAX,
                ..Default::default()
            },
            HubSessionPolicy {
                retry_ceiling: Duration::from_secs(1),
                ..Default::default()
            },
        ] {
            assert!(policy.validate().is_err());
        }
        assert_eq!(
            HubSessionPolicy::default().next_wait(Duration::from_secs(80)),
            Duration::from_secs(120)
        );
    }
}
