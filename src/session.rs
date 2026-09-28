//! Managed hub connections with explicit host scheduling and no ambiguous replay.
use crate::transport::TransportConnectionPhase;
use crate::{AskOptions, Client, Context, Data, Event, Identity, Reply, Result, ThalovantError};
use futures_util::future::BoxFuture;
use std::{
    collections::HashMap,
    future::Future,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex as StateMutex, Weak,
    },
    time::{Duration, Instant},
};
use tokio::{
    sync::{broadcast, Mutex, Notify},
    task::{AbortHandle, JoinHandle},
};

/// How long a new link must stay up before [`HubSession::connect`] counts it.
///
/// A hub that does not know a connection's key -- or does not know it yet --
/// says so only by closing the socket right after the handshake.
pub const DEFAULT_SETTLE_WINDOW: Duration = Duration::from_millis(750);
/// How long [`HubSession::run`] keeps trying through refusals before it gives
/// up. A connection just created is refused until its hub has admitted it --
/// about ninety seconds -- so a refusal is only final once it has lasted this
/// long.
pub const DEFAULT_REFUSAL_GRACE: Duration = Duration::from_secs(600);

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
/// What one attempt to keep a link up came to, for [`LinkSupervisor::after`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LinkOutcome {
    /// The link is up.
    Up,
    /// An established link went down.
    Dropped,
    /// The hub or the network could not be reached.
    Failed,
    /// The hub turned the credentials away ([`ThalovantError::is_hub_refused`]).
    Refused,
    /// The hub's Noise key is not the pinned one
    /// ([`ThalovantError::is_hub_key_changed`]).
    KeyChanged,
}

/// What to do after an outcome; see [`LinkSupervisor::after`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LinkDecision {
    /// The link is up: hold it.
    Hold,
    /// Try again after `wait` (zero: at once).
    Retry {
        /// How long to wait first.
        wait: Duration,
    },
    /// Stop: retrying cannot help. `reason` is [`LinkOutcome::Refused`] or
    /// [`LinkOutcome::KeyChanged`].
    GiveUp {
        /// Why.
        reason: LinkOutcome,
    },
}

/// How a long-lived link is kept up, as a pure function of what happened and
/// when. [`HubSession::run`] asks it after every attempt, and every SDK
/// follows the same rules (`link-keeping-vectors.json`):
///
/// - [`LinkOutcome::Up`]: hold, and start the ladder and the refusal clock
///   afresh;
/// - [`LinkOutcome::Dropped`]: dial again at once;
/// - [`LinkOutcome::Failed`]: wait the ladder's step -- `policy.retry`,
///   doubling to `policy.retry_ceiling` -- and stop counting refusals;
/// - [`LinkOutcome::Refused`]: a new connection is refused until its hub
///   admits it, so wait the ladder's step as for a failure, until refusals
///   have lasted `refusal_grace` since the first of them (inclusive); then
///   give up;
/// - [`LinkOutcome::KeyChanged`]: give up at once.
#[derive(Clone, Debug)]
pub struct LinkSupervisor {
    policy: HubSessionPolicy,
    refusal_grace: Duration,
    wait: Duration,
    refused_since: Option<Duration>,
}

impl LinkSupervisor {
    /// A supervisor with `policy`'s ladder and `refusal_grace`
    /// ([`DEFAULT_REFUSAL_GRACE`]).
    pub fn new(policy: HubSessionPolicy, refusal_grace: Duration) -> Self {
        Self {
            policy,
            refusal_grace,
            wait: policy.retry,
            refused_since: None,
        }
    }

    /// The decision after `outcome`, observed at `now` (time since any fixed
    /// point on a monotonic clock).
    pub fn after(&mut self, outcome: LinkOutcome, now: Duration) -> LinkDecision {
        match outcome {
            LinkOutcome::Up => {
                self.wait = self.policy.retry;
                self.refused_since = None;
                return LinkDecision::Hold;
            }
            LinkOutcome::Dropped => {
                return LinkDecision::Retry {
                    wait: Duration::ZERO,
                }
            }
            LinkOutcome::KeyChanged => {
                return LinkDecision::GiveUp {
                    reason: LinkOutcome::KeyChanged,
                }
            }
            LinkOutcome::Refused => {
                let since = *self.refused_since.get_or_insert(now);
                if now.saturating_sub(since) >= self.refusal_grace {
                    return LinkDecision::GiveUp {
                        reason: LinkOutcome::Refused,
                    };
                }
            }
            LinkOutcome::Failed => self.refused_since = None,
        }
        let wait = self.wait;
        self.wait = self.policy.next_wait(self.wait);
        LinkDecision::Retry { wait }
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
/// A handler registered with [`HubSession::on`] or
/// [`HubSession::on_state_change`]; [`HubSession::off`] removes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HandlerId(u64);
type ConnectFuture = Pin<Box<dyn Future<Output = Result<Client>> + Send>>;
type EventHandler = Arc<dyn Fn(Event) -> BoxFuture<'static, ()> + Send + Sync>;
type StateCallback = Arc<dyn Fn(bool) + Send + Sync>;
/// The tasks one handler still has running, by task number, and whether the
/// handler has been removed.
#[derive(Default)]
struct Tasks {
    /// Set by `off` and `close` under this lock: no task starts after it,
    /// not even one a dispatch copied the handler for just before.
    retired: bool,
    running: HashMap<u64, AbortHandle>,
}
type Running = Arc<StateMutex<Tasks>>;

/// A handler task's entry in its registration's [`Running`] map, removed when
/// the task ends however it ends: finished, panicked, or aborted, even before
/// it first ran. Without it a handler that panics on some events leaves one
/// entry per event behind for as long as it stays registered.
struct RunningEntry {
    running: Running,
    task: u64,
}

impl Drop for RunningEntry {
    fn drop(&mut self) {
        self.running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .running
            .remove(&self.task);
    }
}

/// Start one handler task and record it, unless the handler was removed.
///
/// The check and the record happen under the lock `retire` takes, so a
/// dispatch that copied a handler just before `off` or `close` removed it
/// cannot start a task that outlives them. The lock is also held until the
/// task is recorded, so its entry is only ever removed after it was inserted.
fn start_task(running: &Running, task: u64, work: BoxFuture<'static, ()>) -> bool {
    let mut tasks = running
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if tasks.retired {
        return false;
    }
    let entry = RunningEntry {
        running: running.clone(),
        task,
    };
    let handle = tokio::spawn(async move {
        let _entry = entry;
        work.await;
    });
    tasks.running.insert(task, handle.abort_handle());
    true
}

/// Retire a removed handler: no task of it starts again, and every task it
/// still has running is aborted. The map is drained and its lock released
/// before any abort, so a task's own [`RunningEntry`] can never wait on it.
fn retire(running: &Running) {
    let tasks: Vec<AbortHandle> = {
        let mut tasks = running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tasks.retired = true;
        tasks.running.drain().map(|(_, task)| task).collect()
    };
    for task in tasks {
        task.abort();
    }
}
struct Registration {
    id: u64,
    event_type: String,
    handler: EventHandler,
    running: Running,
}
#[derive(Default)]
struct Handlers {
    next_id: u64,
    next_task: u64,
    events: Vec<Registration>,
    states: Vec<(u64, StateCallback)>,
}
#[derive(Clone, Copy)]
struct Link {
    settle: Duration,
    refusal_grace: Duration,
}
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
    handlers: StateMutex<Handlers>,
    link: StateMutex<Link>,
    /// Wakes [`HubSession::run`] for [`HubSession::close`].
    wake: Notify,
}
impl Inner {
    /// Hand `event` to every handler registered for its type, each on a task
    /// of its own.
    fn dispatch(&self, event: &Event) {
        let matching: Vec<(EventHandler, Running, u64)> = {
            let mut handlers = self.handlers.lock().unwrap();
            let matching: Vec<(EventHandler, Running)> = handlers
                .events
                .iter()
                .filter(|registration| registration.event_type == event.name)
                .map(|registration| (registration.handler.clone(), registration.running.clone()))
                .collect();
            matching
                .into_iter()
                .map(|(handler, running)| {
                    handlers.next_task = handlers.next_task.wrapping_add(1);
                    (handler, running, handlers.next_task)
                })
                .collect()
        };
        for (handler, running, task) in matching {
            // Built before any lock is taken: a handler may remove itself.
            let Ok(work) = std::panic::catch_unwind(AssertUnwindSafe(|| handler(event.clone())))
            else {
                continue;
            };
            start_task(&running, task, work);
        }
    }

    /// Tell every state callback the link came up or went down.
    fn announce(&self, up: bool) {
        let callbacks: Vec<StateCallback> = self
            .handlers
            .lock()
            .unwrap()
            .states
            .iter()
            .map(|(_, callback)| callback.clone())
            .collect();
        for callback in callbacks {
            // A callback that panics must not take the session's state with it.
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| callback(up)));
        }
    }
}
/// Clone handles share the same client, admission barrier and subscription bus.
/// The factory connects a fresh client and owns cleanup if it fails/cancels.
#[derive(Clone)]
pub struct HubSession {
    inner: Arc<Inner>,
}
/// A session that does not keep it open: what a handler the session holds
/// keeps of it.
#[derive(Clone)]
pub(crate) struct WeakHubSession(Weak<Inner>);
impl WeakHubSession {
    pub(crate) fn upgrade(&self) -> Option<HubSession> {
        self.0.upgrade().map(|inner| HubSession { inner })
    }
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
                handlers: StateMutex::new(Handlers::default()),
                link: StateMutex::new(Link {
                    settle: DEFAULT_SETTLE_WINDOW,
                    refusal_grace: DEFAULT_REFUSAL_GRACE,
                }),
                wake: Notify::new(),
            }),
        })
    }
    /// A session whose clients connect with `identity`, over the protocol it
    /// prefers ([`Client::auto`]).
    ///
    /// A hub that closes the link while the connection is being set up, the
    /// way it refuses credentials (see
    /// [`WssTransport::closed_refused`](crate::WssTransport::closed_refused)),
    /// is reported as a refusal ([`ThalovantError::is_hub_refused`]).
    pub fn for_identity(identity: Identity, policy: HubSessionPolicy) -> Result<Self> {
        Self::new(
            move || {
                let identity = identity.clone();
                async move {
                    let client = Client::auto(identity)?;
                    match client.connect().await {
                        Ok(()) => Ok(client),
                        Err(error) => {
                            let refused = client.transport.closed_refused()
                                && matches!(error, ThalovantError::Connection(_))
                                && !error.is_hub_refused()
                                && !error.is_hub_key_changed();
                            let _ = client.close().await;
                            Err(if refused {
                                ThalovantError::hub_refused(
                                    "the hub closed the link during the handshake: it does not accept these credentials, or not yet",
                                )
                            } else {
                                error
                            })
                        }
                    }
                }
            },
            policy,
        )
    }
    /// This session, counting a new link only once it has stayed up for
    /// `window` ([`DEFAULT_SETTLE_WINDOW`]); zero counts it at once.
    pub fn with_settle_window(self, window: Duration) -> Self {
        self.inner.link.lock().unwrap().settle = window;
        self
    }
    /// This session, with [`HubSession::run`] giving up once refusals have
    /// lasted `grace` ([`DEFAULT_REFUSAL_GRACE`]). Zero is refused.
    pub fn with_refusal_grace(self, grace: Duration) -> Result<Self> {
        if grace.is_zero() {
            return Err(ThalovantError::Connection(
                "the refusal grace must be positive".into(),
            ));
        }
        self.inner.link.lock().unwrap().refusal_grace = grace;
        Ok(self)
    }
    /// How long a new link must stay up before [`HubSession::connect`]
    /// counts it.
    pub fn settle_window(&self) -> Duration {
        self.inner.link.lock().unwrap().settle
    }
    /// How long [`HubSession::run`] keeps trying through refusals.
    pub fn refusal_grace(&self) -> Duration {
        self.inner.link.lock().unwrap().refusal_grace
    }
    pub(crate) fn downgrade(&self) -> WeakHubSession {
        WeakHubSession(Arc::downgrade(&self.inner))
    }
    /// Call `handler` with every event of `event_type` that arrives, on the
    /// current client and on every one the session builds after it.
    ///
    /// Each event is handled on a task of its own, so a slow handler does not
    /// hold up the next event. [`HubSession::off`] removes the handler and
    /// cancels what it still has running. Fails only when the session is
    /// closed.
    pub fn on<F, Fut>(&self, event_type: &str, handler: F) -> Result<HandlerId>
    where
        F: Fn(Event) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(ThalovantError::Connection("hub session is closed".into()));
        }
        let handler: EventHandler = Arc::new(move |event| Box::pin(handler(event)));
        let mut handlers = self.inner.handlers.lock().unwrap();
        handlers.next_id += 1;
        let id = handlers.next_id;
        handlers.events.push(Registration {
            id,
            event_type: event_type.to_string(),
            handler,
            running: Running::default(),
        });
        Ok(HandlerId(id))
    }
    /// Call `callback` with `true` when the link comes up and `false` when it
    /// goes down. It runs while the session changes state, so keep it quick:
    /// spawn anything that waits.
    pub fn on_state_change<F>(&self, callback: F) -> HandlerId
    where
        F: Fn(bool) + Send + Sync + 'static,
    {
        let mut handlers = self.inner.handlers.lock().unwrap();
        handlers.next_id += 1;
        let id = handlers.next_id;
        handlers.states.push((id, Arc::new(callback)));
        HandlerId(id)
    }
    /// Remove a handler or a state callback. A handler's tasks still running
    /// are cancelled. `false` when it was not registered (any more).
    pub fn off(&self, id: HandlerId) -> bool {
        let mut handlers = self.inner.handlers.lock().unwrap();
        if let Some(index) = handlers
            .events
            .iter()
            .position(|registration| registration.id == id.0)
        {
            let registration = handlers.events.remove(index);
            drop(handlers);
            retire(&registration.running);
            return true;
        }
        let before = handlers.states.len();
        handlers
            .states
            .retain(|(registered, _)| *registered != id.0);
        handlers.states.len() != before
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
            if self.inner.has_client.swap(false, Ordering::AcqRel) {
                self.inner.announce(false);
            }
            held.retired = Some(client)
        }
        Self::cleanup(held).await
    }
    fn back_off(&self) {
        let mut retry = self.inner.retry.lock().unwrap();
        retry.at = Some(Instant::now() + retry.wait);
        retry.wait = self.inner.policy.next_wait(retry.wait);
    }
    async fn ensure(&self, held: &mut Held) -> Result<Client> {
        self.establish(held, false).await
    }
    /// The held client, or a new one; `settle` holds a new one to the settle
    /// window before counting it.
    async fn establish(&self, held: &mut Held, settle: bool) -> Result<Client> {
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
                self.back_off();
                return Err(error);
            }
        };
        if self.inner.closed.load(Ordering::Acquire) {
            held.retired = Some(client);
            Self::cleanup(held).await?;
            return Err(ThalovantError::Connection("hub session is closed".into()));
        }
        // Subscribed before the settle window, so what arrives during it is
        // handled once the link counts, not lost.
        let mut receiver = client.transport.subscribe();
        if settle {
            if let Err(error) = settled(&client, self.settle_window()).await {
                held.retired = Some(client);
                let _ = Self::cleanup(held).await;
                self.back_off();
                return Err(error);
            }
            if self.inner.closed.load(Ordering::Acquire) {
                held.retired = Some(client);
                Self::cleanup(held).await?;
                return Err(ThalovantError::Connection("hub session is closed".into()));
            }
        }
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
                        let _ = sender.send(HubSessionEvent::Message(event.clone()));
                        drop(events);
                        inner.dispatch(&event);
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
        {
            let mut retry = self.inner.retry.lock().unwrap();
            retry.at = None;
            retry.wait = self.inner.policy.retry;
        }
        if !self.inner.has_client.swap(true, Ordering::AcqRel) {
            self.inner.announce(true);
        }
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
    /// Answer a message the hub sent, back along the route it came; see
    /// [`Client::reply`].
    pub async fn reply(&self, event: &Event, msg_type: &str, data: Data) -> Result<()> {
        self.call(|client| async move { client.reply(event, msg_type, data).await })
            .await
    }
    /// Make one attempt: return with a live link, or say why there is none.
    ///
    /// A link already held and alive is kept. A new one counts only once it
    /// has stayed up for the settle window ([`DEFAULT_SETTLE_WINDOW`]): a hub
    /// that closes it before then without a status, or with 1000, 1005 or
    /// 1008, has refused the connection's credentials, which is
    /// a refusal ([`ThalovantError::is_hub_refused`]); any other early close is a
    /// [`ThalovantError::Connection`] drop. A failed attempt moves the retry
    /// ladder on.
    pub async fn connect(&self) -> Result<()> {
        let mut held = self.inner.held.lock().await;
        if let Some(client) = &held.client {
            if alive(client).await {
                return Ok(());
            }
            self.drop_client(&mut held).await?;
        }
        self.establish(&mut held, true).await.map(|_| ())
    }
    /// Stay connected until [`HubSession::close`], by policy.
    ///
    /// Connects with [`HubSession::connect`] and asks a [`LinkSupervisor`]
    /// what to do after every attempt. A held link is watched: a drop is
    /// noticed as it happens (and the link is also looked at every
    /// `policy.probe`), and is dialled again at once. After a failed attempt
    /// it waits out the retry ladder (10 s doubling to 120 s by default). A
    /// hub refusing the credentials is retried the same way until the
    /// refusals have lasted the refusal grace ([`DEFAULT_REFUSAL_GRACE`]): a
    /// new connection is refused until its hub admits it. Then this returns
    /// a refusal ([`ThalovantError::is_hub_refused`]). A hub whose key is not the pinned one
    /// ends it at once ([`ThalovantError::is_hub_key_changed`]): retrying
    /// cannot change that.
    ///
    /// Returns `Ok(())` once the session is closed. An identity the client
    /// cannot use at all ([`ThalovantError::MissingIdentityField`],
    /// [`ThalovantError::InvalidIdentity`],
    /// [`ThalovantError::UnsupportedProtocol`]) ends it at once with that
    /// error; everything else is retried.
    pub async fn run(&self) -> Result<()> {
        let origin = tokio::time::Instant::now();
        let mut supervisor = LinkSupervisor::new(self.inner.policy, self.refusal_grace());
        loop {
            let woken = self.inner.wake.notified();
            tokio::pin!(woken);
            woken.as_mut().enable();
            if self.inner.closed.load(Ordering::Acquire) {
                return Ok(());
            }
            let held = self.inner.held.lock().await.client.clone();
            if let Some(client) = held {
                if alive(&client).await {
                    tokio::select! {
                        _ = client.transport.stopped() => {}
                        _ = tokio::time::sleep(self.inner.policy.probe) => {}
                        _ = &mut woken => {}
                    }
                    if self.inner.closed.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    let mut held = self.inner.held.lock().await;
                    if let Some(current) = &held.client {
                        if !alive(current).await {
                            // A cleanup that fails is retried by the next
                            // attempt, which reports it.
                            let _ = self.drop_client(&mut held).await;
                            // Dialled again at once.
                            supervisor.after(LinkOutcome::Dropped, origin.elapsed());
                        }
                    }
                    continue;
                }
            }
            let decision = match self.connect().await {
                Ok(()) => {
                    supervisor.after(LinkOutcome::Up, origin.elapsed());
                    continue;
                }
                Err(error) if error.is_hub_key_changed() => {
                    supervisor.after(LinkOutcome::KeyChanged, origin.elapsed());
                    return Err(error);
                }
                Err(error) if error.is_hub_refused() => {
                    match supervisor.after(LinkOutcome::Refused, origin.elapsed()) {
                        LinkDecision::GiveUp { .. } => return Err(error),
                        decision => decision,
                    }
                }
                Err(
                    error @ (ThalovantError::MissingIdentityField(_)
                    | ThalovantError::InvalidIdentity(_)
                    | ThalovantError::UnsupportedProtocol(_)),
                ) => return Err(error),
                Err(_) => supervisor.after(LinkOutcome::Failed, origin.elapsed()),
            };
            if self.inner.closed.load(Ordering::Acquire) {
                return Ok(());
            }
            let wait = match decision {
                LinkDecision::Retry { wait } => wait,
                _ => Duration::ZERO,
            };
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = &mut woken => {}
            }
        }
    }
    /// Terminal close retains responsibility for a failed cleanup; retry close.
    ///
    /// Stops [`HubSession::run`] and forgets every handler and state
    /// callback, once they have heard the link go down.
    pub async fn close(&self) -> Result<()> {
        self.inner.closed.store(true, Ordering::Release);
        self.inner.wake.notify_waiters();
        {
            self.inner.events.lock().unwrap().sender = None;
        }
        let mut held = self.inner.held.lock().await;
        let dropped = self.drop_client(&mut held).await;
        drop(held);
        // Forget every handler and cancel what they still have running, as
        // `off` does, so a closed session owns no detached task.
        let registrations = {
            let mut handlers = self.inner.handlers.lock().unwrap();
            handlers.states.clear();
            std::mem::take(&mut handlers.events)
        };
        for registration in registrations {
            retire(&registration.running);
        }
        dropped
    }
}
/// Hold a new link to the settle window: `Ok` once it has stayed up for
/// `window`, or why it did not.
async fn settled(client: &Client, window: Duration) -> Result<()> {
    if window.is_zero() {
        return Ok(());
    }
    if tokio::time::timeout(window, client.transport.stopped())
        .await
        .is_err()
    {
        return Ok(());
    }
    Err(if client.transport.closed_refused() {
        ThalovantError::hub_refused(
            "the hub closed the link right after the handshake: it does not accept these credentials, or not yet",
        )
    } else {
        ThalovantError::Connection("the hub closed the link right after the handshake".into())
    })
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

    fn wss_client() -> Client {
        let identity = Identity::from_value(serde_json::json!({
            "access_key": "fixture", "password": "password", "site_id": "fixture",
            "default_master": "https://hub.example",
            "data_plane_endpoints": {"wss": "wss://hub.example/hivemind"},
        }))
        .unwrap();
        Client::with_protocol(identity, crate::HubProtocol::Wss).unwrap()
    }

    fn event(name: &str, n: u64) -> Event {
        Event::new(
            name,
            serde_json::json!({"n": n}).as_object().unwrap().clone(),
            Context::new(),
            None,
        )
    }

    async fn eventually(what: &str, mut done: impl FnMut() -> bool) {
        for _ in 0..2000 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("never happened: {what}");
    }

    /// Every client a session built.
    type Built = Arc<StateMutex<Vec<Client>>>;

    /// A session whose factory keeps every client it builds, closing each as
    /// a hub would -- `Some(refused)` -- 100 ms after the handshake, or not
    /// at all; the last entry of `closes` goes on repeating.
    fn building(closes: Vec<Option<bool>>) -> (HubSession, Built) {
        let built = Built::default();
        let kept = built.clone();
        let factory = move || {
            let mut built = kept.lock().unwrap();
            let close = closes.get(built.len()).or(closes.last()).copied().flatten();
            let client = wss_client();
            if let Some(refused) = close {
                let closing = client.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    closing.transport.close_for_test(refused).await;
                });
            }
            built.push(client.clone());
            std::future::ready(Ok(client))
        };
        let session = HubSession::new(factory, HubSessionPolicy::default()).unwrap();
        (session, built)
    }

    #[tokio::test]
    async fn no_task_starts_for_a_handler_already_removed() {
        // A dispatch that copied the handler just before off() or close()
        // removed it reaches start_task after retire(): nothing may start.
        let running = Running::default();
        let (ran, mut running_tasks) = tokio::sync::mpsc::unbounded_channel::<()>();
        let first = ran.clone();
        assert!(start_task(
            &running,
            1,
            Box::pin(async move {
                let _ = first.send(());
            })
        ));
        running_tasks
            .recv()
            .await
            .expect("a live handler's task runs");
        retire(&running);
        assert!(!start_task(
            &running,
            2,
            Box::pin(async move {
                let _ = ran.send(());
            })
        ));
        assert!(
            running_tasks.recv().await.is_none(),
            "the retired handler's task never ran, and its sender is gone"
        );
        assert!(running.lock().unwrap().running.is_empty());
    }

    #[tokio::test]
    async fn a_handler_that_panics_leaves_nothing_running_and_close_cancels_the_rest() {
        let (session, built) = building(vec![None]);
        let session = session.with_settle_window(Duration::ZERO);
        session
            .on("boom", |_| async move { panic!("a handler that panics") })
            .unwrap();
        let (started, mut starting) = tokio::sync::mpsc::unbounded_channel();
        let (ended, mut ending) = tokio::sync::mpsc::unbounded_channel::<()>();
        session
            .on("slow", move |_| {
                let started = started.clone();
                let ended = ended.clone();
                async move {
                    struct Ends(tokio::sync::mpsc::UnboundedSender<()>);
                    impl Drop for Ends {
                        fn drop(&mut self) {
                            let _ = self.0.send(());
                        }
                    }
                    let _ends = Ends(ended);
                    let _ = started.send(());
                    std::future::pending::<()>().await;
                }
            })
            .unwrap();
        session.connect().await.unwrap();
        let client = built.lock().unwrap()[0].clone();
        for n in 0..5 {
            client.transport.deliver_for_test(event("boom", n));
        }
        let running = |event_type: &str| {
            let handlers = session.inner.handlers.lock().unwrap();
            let registration = handlers
                .events
                .iter()
                .find(|registration| registration.event_type == event_type)
                .expect("registered");
            let count = registration.running.lock().unwrap().running.len();
            count
        };
        eventually("the panicked tasks are forgotten", || running("boom") == 0).await;

        client.transport.deliver_for_test(event("slow", 1));
        starting.recv().await.unwrap();
        assert_eq!(running("slow"), 1);
        session.close().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), ending.recv())
            .await
            .expect("close cancelled the running handler");
    }

    #[tokio::test]
    async fn a_handler_is_bound_onto_every_client_and_off_cancels_it() {
        let (session, built) = building(vec![None]);
        let session = session.with_settle_window(Duration::ZERO);
        let (heard, mut hearing) = tokio::sync::mpsc::unbounded_channel();
        let id = session
            .on("thalovant.home.request", move |event| {
                let heard = heard.clone();
                async move {
                    let _ = heard.send(event.data["n"].as_u64().unwrap());
                }
            })
            .unwrap();
        // Registered before the first client exists.
        session.connect().await.unwrap();
        let first = built.lock().unwrap()[0].clone();
        first.transport.deliver_for_test(event("other", 1));
        first
            .transport
            .deliver_for_test(event("thalovant.home.request", 2));
        assert_eq!(hearing.recv().await, Some(2));

        // The link drops; the next attempt builds another client, and the
        // handler is on it, while the old one is no longer heard.
        first.transport.close_for_test(false).await;
        session.connect().await.unwrap();
        let second = built.lock().unwrap()[1].clone();
        first
            .transport
            .deliver_for_test(event("thalovant.home.request", 3));
        second
            .transport
            .deliver_for_test(event("thalovant.home.request", 4));
        assert_eq!(hearing.recv().await, Some(4));

        // off() cancels what the handler still has running.
        let (started, mut starting) = tokio::sync::mpsc::unbounded_channel();
        let (ended, mut ending) = tokio::sync::mpsc::unbounded_channel::<()>();
        let slow = session
            .on("slow", move |_| {
                let started = started.clone();
                let ended = ended.clone();
                async move {
                    struct Ends(tokio::sync::mpsc::UnboundedSender<()>);
                    impl Drop for Ends {
                        fn drop(&mut self) {
                            let _ = self.0.send(());
                        }
                    }
                    let _ends = Ends(ended);
                    let _ = started.send(());
                    std::future::pending::<()>().await;
                }
            })
            .unwrap();
        second.transport.deliver_for_test(event("slow", 5));
        starting.recv().await.unwrap();
        assert!(session.off(slow));
        tokio::time::timeout(Duration::from_secs(2), ending.recv())
            .await
            .expect("the running handler was cancelled");
        assert!(session.off(id));
        assert!(!session.off(id));
        second
            .transport
            .deliver_for_test(event("thalovant.home.request", 6));
        // Nothing arrives: the channel ends instead, because off() dropped
        // the handler and the sender it held.
        assert!(
            !matches!(
                tokio::time::timeout(Duration::from_millis(100), hearing.recv()).await,
                Ok(Some(_))
            ),
            "a removed handler was still called"
        );
        session.close().await.unwrap();
        assert!(session.on("x", |_| async {}).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn the_settle_window_tells_a_refusal_from_a_drop() {
        for close in [Some(true), Some(false), None] {
            let (session, _) = building(vec![close]);
            let states = Arc::new(StateMutex::new(Vec::new()));
            let seen = states.clone();
            session.on_state_change(move |up| seen.lock().unwrap().push(up));
            let started = tokio::time::Instant::now();
            let result = session.connect().await;
            match close {
                Some(true) => assert!(
                    result.as_ref().is_err_and(ThalovantError::is_hub_refused),
                    "{result:?}"
                ),
                Some(false) => assert!(
                    matches!(result, Err(ThalovantError::Connection(_))),
                    "{result:?}"
                ),
                None => {
                    result.unwrap();
                    assert!(started.elapsed() >= DEFAULT_SETTLE_WINDOW);
                }
            }
            // A link that closed inside the window never counted, and moved
            // the ladder on.
            assert_eq!(session.held(), close.is_none());
            assert_eq!(
                *states.lock().unwrap(),
                vec![true; usize::from(close.is_none())]
            );
            let rung = if close.is_some() { 20 } else { 10 };
            assert_eq!(session.retry_wait(), Duration::from_secs(rung));
            session.close().await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn run_gives_up_once_refusals_outlast_the_grace() {
        // Refused at 0, 10 and 30 s: the third is 30 s after the first.
        let (session, built) = building(vec![Some(true)]);
        let session = session.with_refusal_grace(Duration::from_secs(30)).unwrap();
        let started = tokio::time::Instant::now();
        let error = session.run().await.unwrap_err();
        assert!(error.is_hub_refused(), "{error:?}");
        assert!(error.is_connection_error());
        assert_eq!(built.lock().unwrap().len(), 3);
        assert!(started.elapsed() >= Duration::from_secs(30));
        session.close().await.unwrap();

        // A drop in between is not a refusal, so the count starts again:
        // refused at 0, dropped at 10, refused at 30 and at 70.
        let (session, built) = building(vec![Some(true), Some(false), Some(true)]);
        let session = session.with_refusal_grace(Duration::from_secs(25)).unwrap();
        let started = tokio::time::Instant::now();
        assert!(session
            .run()
            .await
            .is_err_and(|error| error.is_hub_refused()));
        assert_eq!(built.lock().unwrap().len(), 4);
        assert!(started.elapsed() >= Duration::from_secs(70));
        session.close().await.unwrap();
        assert!(
            HubSession::new(|| async { Ok(client()) }, HubSessionPolicy::default())
                .unwrap()
                .with_refusal_grace(Duration::ZERO)
                .is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn run_keeps_the_link_and_close_ends_it() {
        let (session, built) = building(vec![None]);
        let states = Arc::new(StateMutex::new(Vec::new()));
        let seen = states.clone();
        session.on_state_change(move |up| seen.lock().unwrap().push(up));
        let running = tokio::spawn({
            let session = session.clone();
            async move { session.run().await }
        });
        eventually("the link came up", || *states.lock().unwrap() == [true]).await;

        // The link drops: run notices as it happens, not at the next probe a
        // minute later, and builds another.
        let dropped = tokio::time::Instant::now();
        let first = built.lock().unwrap()[0].clone();
        first.transport.close_for_test(false).await;
        eventually("the link came back", || {
            *states.lock().unwrap() == [true, false, true]
        })
        .await;
        assert!(dropped.elapsed() < Duration::from_secs(5));
        assert_eq!(built.lock().unwrap().len(), 2);

        // A held link is left alone at every probe.
        tokio::time::sleep(Duration::from_secs(200)).await;
        assert_eq!(built.lock().unwrap().len(), 2);

        session.close().await.unwrap();
        running.await.unwrap().unwrap();
        assert_eq!(*states.lock().unwrap(), [true, false, true, false]);
    }

    #[tokio::test]
    async fn an_identity_the_client_cannot_use_ends_run_at_once() {
        let session = HubSession::new(
            || async { Err(ThalovantError::MissingIdentityField("password")) },
            HubSessionPolicy::default(),
        )
        .unwrap();
        assert!(matches!(
            session.run().await,
            Err(ThalovantError::MissingIdentityField(_))
        ));
        session.close().await.unwrap();
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
