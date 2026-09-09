//! Correlated event subscriptions. Dropping a stream removes its subscription.
use crate::{Client, Context, Event, Result, RuntimeTransport, ThalovantError};
use std::{sync::Arc, time::Duration};
use tokio::{sync::broadcast, time::Instant};

/// Optional application filter; it must be fast and must not block the runtime.
pub type EventPredicate = Arc<dyn Fn(&Event) -> bool + Send + Sync>;

#[derive(Clone, Default)]
pub struct ListenOptions {
    /// Total duration including connection. None keeps listening until closed.
    pub timeout: Option<Duration>,
    pub max_events: Option<usize>,
    pub session_id: Option<String>,
    pub request_id: Option<String>,
    pub predicate: Option<EventPredicate>,
}

/// A bounded transport subscription. Lag is an explicit error, never silent loss.
/// Cancel a pending receive by dropping its future; drop/close this stream to
/// release the subscription. Neither operation closes the shared connection.
pub struct EventStream {
    receiver: Option<broadcast::Receiver<Event>>,
    transport: Option<RuntimeTransport>,
    event_name: String,
    expected: Context,
    options: ListenOptions,
    deadline: Option<Instant>,
    received: usize,
}

impl EventStream {
    fn new(
        receiver: broadcast::Receiver<Event>,
        transport: Option<RuntimeTransport>,
        event_name: &str,
        options: ListenOptions,
        deadline: Option<Instant>,
    ) -> Self {
        let mut expected = Context::new();
        if let Some(id) = &options.session_id {
            expected.insert("session_id".into(), id.clone().into());
        }
        if let Some(id) = &options.request_id {
            expected.insert("request_id".into(), id.clone().into());
        }
        Self {
            receiver: Some(receiver),
            transport,
            event_name: event_name.into(),
            expected,
            options,
            deadline,
            received: 0,
        }
    }

    pub fn close(&mut self) {
        self.receiver = None;
    }

    pub async fn recv(&mut self) -> Result<Option<Event>> {
        loop {
            if self.receiver.is_none() {
                return Ok(None);
            }
            if self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                self.close();
                return Err(ThalovantError::Timeout(
                    "event stream deadline expired".into(),
                ));
            }
            let wake = self
                .deadline
                .map_or(Instant::now() + Duration::from_millis(100), |deadline| {
                    deadline.min(Instant::now() + Duration::from_millis(100))
                });
            let result = tokio::select! {
                result = self.receiver.as_mut().expect("open receiver").recv() => Some(result),
                _ = tokio::time::sleep_until(wake) => None,
            };
            if self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                self.close();
                return Err(ThalovantError::Timeout(
                    "event stream deadline expired".into(),
                ));
            }
            match result {
                Some(Ok(event)) => {
                    if event.name != self.event_name
                        || !crate::event_matches_context(&event, Some(&self.expected))
                        || self
                            .options
                            .predicate
                            .as_ref()
                            .is_some_and(|filter| !filter(&event))
                    {
                        continue;
                    }
                    self.received += 1;
                    if self
                        .options
                        .max_events
                        .is_some_and(|max| self.received >= max)
                    {
                        self.close();
                    }
                    return Ok(Some(event));
                }
                Some(Err(broadcast::error::RecvError::Lagged(count))) => {
                    self.close();
                    return Err(ThalovantError::Runtime(format!(
                        "event subscription overflow: missed {count} events"
                    )));
                }
                Some(Err(broadcast::error::RecvError::Closed)) => {
                    self.close();
                    return Err(ThalovantError::Connection("event transport closed".into()));
                }
                None => {
                    if let Some(transport) = &self.transport {
                        let probe_deadline = self
                            .deadline
                            .map_or(Instant::now() + Duration::from_millis(100), |deadline| {
                                deadline.min(Instant::now() + Duration::from_millis(100))
                            });
                        let Ok(health) =
                            tokio::time::timeout_at(probe_deadline, transport.healthcheck()).await
                        else {
                            continue;
                        };
                        if !health.handshake_complete {
                            self.close();
                            return Err(ThalovantError::Connection(
                                "event transport is no longer authenticated".into(),
                            ));
                        }
                    }
                }
            }
        }
    }
}

impl Client {
    /// Subscribe before connecting so an authenticated early event is retained.
    pub async fn listen(&self, event_name: &str, options: ListenOptions) -> Result<EventStream> {
        if event_name.trim().is_empty() || options.max_events == Some(0) {
            return Err(ThalovantError::Runtime(
                "event name and a positive event limit are required".into(),
            ));
        }
        let started = Instant::now();
        let deadline = match options.timeout {
            Some(duration) => Some(
                started
                    .checked_add(duration)
                    .ok_or_else(|| ThalovantError::Runtime("event timeout is too large".into()))?,
            ),
            None => None,
        };
        let receiver = self.transport.subscribe();
        self.connect_with_timeout(
            options
                .timeout
                .unwrap_or(Duration::from_secs(6))
                .min(Duration::from_secs(6)),
        )
        .await?;
        Ok(EventStream::new(
            receiver,
            Some(self.transport.clone()),
            event_name,
            options,
            deadline,
        ))
    }

    /// Wait for one event, with a twelve-second default total deadline.
    pub async fn wait_for_event(
        &self,
        event_name: &str,
        mut options: ListenOptions,
    ) -> Result<Event> {
        options.timeout = Some(options.timeout.unwrap_or(Duration::from_secs(12)));
        options.max_events = Some(1);
        self.listen(event_name, options)
            .await?
            .recv()
            .await?
            .ok_or_else(|| ThalovantError::Connection("event subscription closed".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(name: &str, request: &str, session: &str) -> Event {
        Event {
            name: name.into(),
            context: json!({"request_id":request,"session_id":session})
                .as_object()
                .unwrap()
                .clone(),
            data: Context::new(),
            raw: None,
        }
    }

    #[tokio::test]
    async fn concurrent_streams_filter_without_stealing_and_close_at_limit() {
        let (tx, _) = broadcast::channel(8);
        let options = |id: &str| ListenOptions {
            request_id: Some(id.into()),
            session_id: Some("caller".into()),
            max_events: Some(1),
            predicate: Some(Arc::new(|event| {
                event.data.get("accept") == Some(&json!(true))
            })),
            ..Default::default()
        };
        let mut first = EventStream::new(tx.subscribe(), None, "fixture", options("first"), None);
        let mut second = EventStream::new(tx.subscribe(), None, "fixture", options("second"), None);
        tx.send(event("other", "first", "caller")).unwrap();
        tx.send(event("fixture", "first", "caller")).unwrap();
        for id in ["second", "first"] {
            let mut value = event("fixture", id, "hub-rewritten");
            value.data.insert("accept".into(), true.into());
            tx.send(value).unwrap();
        }
        assert_eq!(
            first.recv().await.unwrap().unwrap().request_id().as_deref(),
            Some("first")
        );
        assert_eq!(
            second
                .recv()
                .await
                .unwrap()
                .unwrap()
                .request_id()
                .as_deref(),
            Some("second")
        );
        assert!(first.recv().await.unwrap().is_none());
        assert_eq!(tx.receiver_count(), 0);
    }

    #[tokio::test]
    async fn overflow_is_explicit_and_does_not_retire_a_fast_subscriber() {
        let (tx, _) = broadcast::channel(2);
        let mut slow = EventStream::new(
            tx.subscribe(),
            None,
            "fixture",
            ListenOptions::default(),
            None,
        );
        let mut fast = EventStream::new(
            tx.subscribe(),
            None,
            "fixture",
            ListenOptions::default(),
            None,
        );
        for _ in 0..5 {
            tx.send(event("fixture", "r", "s")).unwrap();
            assert!(fast.recv().await.unwrap().is_some());
        }
        assert!(slow
            .recv()
            .await
            .unwrap_err()
            .to_string()
            .contains("overflow"));
        assert_eq!(tx.receiver_count(), 1);
        drop(fast);
        assert_eq!(tx.receiver_count(), 0);
    }

    #[tokio::test]
    async fn cancellation_deadline_and_sender_loss_release_subscriptions() {
        let (tx, _) = broadcast::channel(2);
        let mut stream = EventStream::new(
            tx.subscribe(),
            None,
            "fixture",
            ListenOptions::default(),
            None,
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(10), stream.recv())
                .await
                .is_err()
        );
        // Cancelling one receive is safe; the stream still owns its subscription.
        tx.send(event("fixture", "r", "s")).unwrap();
        assert!(stream.recv().await.unwrap().is_some());
        stream.close();
        assert_eq!(tx.receiver_count(), 0);
        let mut expired = EventStream::new(
            tx.subscribe(),
            None,
            "fixture",
            ListenOptions::default(),
            Some(Instant::now()),
        );
        assert!(matches!(
            expired.recv().await,
            Err(ThalovantError::Timeout(_))
        ));
        assert_eq!(tx.receiver_count(), 0);
        let mut lost = EventStream::new(
            tx.subscribe(),
            None,
            "fixture",
            ListenOptions::default(),
            None,
        );
        drop(tx);
        assert!(matches!(
            lost.recv().await,
            Err(ThalovantError::Connection(_))
        ));
    }
}
