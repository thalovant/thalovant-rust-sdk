//! The Home Assistant link: a hub asks a home, and the home always answers.
//!
//! A home skill on the hub sends [`HOME_REQUEST`] to the account's Home
//! Assistant connection; the integration hands the utterance to Home
//! Assistant's conversation agent and answers with [`HOME_RESPONSE`]. The
//! rules every SDK keeps (`home-link-vectors.json`):
//!
//! - every request gets exactly one answer, within the hub's 10 seconds;
//! - the answer is a reply (OVOS-MSG-1 §5.2), so it goes back the way the
//!   request came;
//! - `speech` is plain text, never markup;
//! - `response_type` is one of [`RESPONSE_TYPES`], and an `error` names one of
//!   [`ERROR_CODES`]. When the SDK has to answer for a handler -- it failed,
//!   it was too slow, it answered outside the contract -- the speech is empty:
//!   the hub speaks its own sentence for the code, in the device's language,
//!   which the SDK does not know.
//!
//! [`answer_home_requests`] answers every request a [`HubSession`] receives;
//! [`answer_home_request`] answers one, for a caller running its own loop.

mod entities;

use std::{future::Future, panic::AssertUnwindSafe, sync::Arc, time::Duration};

use futures_util::FutureExt;
use serde_json::Value;

use crate::{
    errors::Result,
    events::{Data, Event},
    rich::strip_ssml,
    session::{HandlerId, HubSession, WeakHubSession},
    Client,
};

/// The message a hub sends to ask the home something.
pub const HOME_REQUEST: &str = "thalovant.home.request";
/// The message the home answers with.
pub const HOME_RESPONSE: &str = "thalovant.home.response";
/// The hub treats silence after this long as `timeout`.
pub const HOME_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a handler has by default: a second inside the hub's bound, so the
/// SDK's own `timeout` answer still lands before the hub gives up.
pub const DEFAULT_HOME_HANDLER_TIMEOUT: Duration = Duration::from_secs(9);
/// Every `response_type` a response may carry.
pub const RESPONSE_TYPES: [&str; 3] = ["action_done", "query_answer", "error"];
/// Every `error_code` an `error` response may carry.
pub const ERROR_CODES: [&str; 6] = [
    "no_intent_match",
    "no_valid_targets",
    "failed_to_handle",
    "unknown",
    "timeout",
    "agent_unavailable",
];

/// One [`HOME_REQUEST`]: what was said, in which language.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct HomeRequest {
    /// The id the answer must carry back; `""` when the hub sent none.
    pub request_id: String,
    /// What the person said.
    pub utterance: String,
    /// The language it was said in, such as `en-US`.
    pub lang: Option<String>,
    /// The Home Assistant conversation it continues, when there is one.
    pub conversation_id: Option<String>,
    /// The event it arrived as; the answer is a reply to it.
    pub event: Event,
}

impl HomeRequest {
    /// Read a request from the event it arrived as. A field that is missing,
    /// empty or not a string is absent.
    pub fn from_event(event: &Event) -> Self {
        let text = |key: &str| {
            event
                .data
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        Self {
            request_id: text("request_id").unwrap_or_default(),
            utterance: text("utterance").unwrap_or_default(),
            lang: text("lang"),
            conversation_id: text("conversation_id"),
            event: event.clone(),
        }
    }
}

/// What a handler says back.
///
/// `speech` may carry markup; it is sent as plain text ([`plain_speech`]).
/// Build one with [`HomeAnswer::action_done`], [`HomeAnswer::query_answer`]
/// or [`HomeAnswer::error`], and the `with_` methods. The default is an
/// `action_done` with nothing to say.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct HomeAnswer {
    /// What to say.
    pub speech: String,
    /// One of [`RESPONSE_TYPES`]; anything else is sent as `error`/`unknown`.
    pub response_type: String,
    /// One of [`ERROR_CODES`], with an `error`; ignored with any other type.
    pub error_code: Option<String>,
    /// Whether Home Assistant expects the person to say more.
    pub continue_conversation: bool,
    /// The Home Assistant conversation this answer belongs to; the request's
    /// is echoed when this is `None`.
    pub conversation_id: Option<String>,
}

impl Default for HomeAnswer {
    fn default() -> Self {
        Self::new("action_done", "")
    }
}

impl HomeAnswer {
    /// An answer of any `response_type`, including one outside the contract,
    /// which is sent as `error`/`unknown`.
    pub fn new(response_type: impl Into<String>, speech: impl Into<String>) -> Self {
        Self {
            speech: speech.into(),
            response_type: response_type.into(),
            error_code: None,
            continue_conversation: false,
            conversation_id: None,
        }
    }

    /// Something was done: `action_done`.
    pub fn action_done(speech: impl Into<String>) -> Self {
        Self::new("action_done", speech)
    }

    /// A question was answered: `query_answer`.
    pub fn query_answer(speech: impl Into<String>) -> Self {
        Self::new("query_answer", speech)
    }

    /// An `error` with one of [`ERROR_CODES`] and no speech of its own: the
    /// hub speaks its sentence for the code. Add speech with
    /// [`HomeAnswer::with_speech`] when you have some.
    pub fn error(error_code: impl Into<String>) -> Self {
        Self::new("error", "").with_error_code(error_code)
    }

    /// This answer, saying `speech`.
    pub fn with_speech(mut self, speech: impl Into<String>) -> Self {
        self.speech = speech.into();
        self
    }

    /// This answer, with `error_code`.
    pub fn with_error_code(mut self, error_code: impl Into<String>) -> Self {
        self.error_code = Some(error_code.into());
        self
    }

    /// This answer, expecting the person to say more, or not.
    pub fn with_continue_conversation(mut self, continue_conversation: bool) -> Self {
        self.continue_conversation = continue_conversation;
        self
    }

    /// This answer, in the Home Assistant conversation `conversation_id`.
    pub fn with_conversation_id(mut self, conversation_id: impl Into<String>) -> Self {
        self.conversation_id = Some(conversation_id.into());
        self
    }
}

/// Speech a device can say as it is: markup removed ([`strip_ssml`]),
/// character references decoded, whitespace collapsed to single spaces and
/// trimmed.
///
/// Decoded as Python's `html.unescape` decodes: every numeric reference, and
/// the named references of HTML 4, `&apos;`, and the upper-case spellings HTML
/// keeps for a few of them, with or without the semicolon where HTML allows
/// it. Any other HTML5 name is left as written. `&nbsp;` decodes to U+00A0,
/// which then counts as whitespace, as every Unicode space does.
pub fn plain_speech(text: &str) -> String {
    let stripped = strip_ssml(text);
    let decoded = entities::unescape(&stripped);
    decoded
        .split(is_space)
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whitespace as Python's `str.isspace` sees it: Unicode's, plus the four
/// ASCII separators U+001C to U+001F.
fn is_space(character: char) -> bool {
    character.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&character)
}

/// The [`HOME_RESPONSE`] payload for `answer`, held to the contract.
///
/// A `response_type` outside [`RESPONSE_TYPES`], or an `error` without one of
/// [`ERROR_CODES`], becomes `error`/`unknown`, keeping its speech. `error_code`
/// is sent only with an `error`, and `conversation_id` only when the answer or
/// the request has one.
pub fn home_response(request: &HomeRequest, answer: HomeAnswer) -> Data {
    let mut response_type = answer.response_type.as_str();
    let mut error_code = answer
        .error_code
        .as_deref()
        .filter(|_| response_type == "error");
    if !RESPONSE_TYPES.contains(&response_type)
        || (response_type == "error" && !error_code.is_some_and(|code| ERROR_CODES.contains(&code)))
    {
        response_type = "error";
        error_code = Some("unknown");
    }
    let mut payload = Data::new();
    payload.insert(
        "request_id".into(),
        Value::from(request.request_id.as_str()),
    );
    payload.insert("speech".into(), Value::from(plain_speech(&answer.speech)));
    payload.insert("response_type".into(), Value::from(response_type));
    payload.insert(
        "continue_conversation".into(),
        Value::from(answer.continue_conversation),
    );
    if let Some(error_code) = error_code.filter(|code| !code.is_empty()) {
        payload.insert("error_code".into(), Value::from(error_code));
    }
    let conversation_id = answer
        .conversation_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .or(request.conversation_id.as_deref())
        .filter(|id| !id.is_empty());
    if let Some(conversation_id) = conversation_id {
        payload.insert("conversation_id".into(), Value::from(conversation_id));
    }
    payload
}

/// Anything that can answer a message the hub sent, back along its route:
/// [`Client`] and [`HubSession`].
pub trait Replier {
    /// Emit `msg_type` with `data` as a reply to `event`; see
    /// [`Client::reply`].
    fn reply(
        &self,
        event: &Event,
        msg_type: &str,
        data: Data,
    ) -> impl Future<Output = Result<()>> + Send;
}

impl Replier for Client {
    fn reply(
        &self,
        event: &Event,
        msg_type: &str,
        data: Data,
    ) -> impl Future<Output = Result<()>> + Send {
        Client::reply(self, event, msg_type, data)
    }
}

impl Replier for HubSession {
    fn reply(
        &self,
        event: &Event,
        msg_type: &str,
        data: Data,
    ) -> impl Future<Output = Result<()>> + Send {
        HubSession::reply(self, event, msg_type, data)
    }
}

/// Answer one request: run `handler`, then reply whatever happened.
///
/// The handler is bounded by `timeout` ([`DEFAULT_HOME_HANDLER_TIMEOUT`]).
/// Whatever it does, exactly one [`HOME_RESPONSE`] goes out, as a reply to
/// `event`:
///
/// - its answer, held to the contract by [`home_response`];
/// - `failed_to_handle` when it returns an error or panics;
/// - `timeout` when it has not answered within `timeout`.
///
/// The reply gets what is left of the hub's [`HOME_REQUEST_TIMEOUT`] after
/// the handler (or of `timeout`, when that is longer): an answer delivered
/// after the hub has given up is only noise. A reply still unsent then is
/// [`ThalovantError::Timeout`](crate::ThalovantError::Timeout).
///
/// Returns the payload sent, or the error sending it.
pub async fn answer_home_request<R, H, Fut, E>(
    replier: &R,
    event: &Event,
    handler: H,
    timeout: Duration,
) -> Result<Data>
where
    R: Replier,
    H: FnOnce(HomeRequest) -> Fut,
    Fut: Future<Output = std::result::Result<HomeAnswer, E>>,
{
    let reply_by = tokio::time::Instant::now() + HOME_REQUEST_TIMEOUT.max(timeout);
    let request = HomeRequest::from_event(event);
    let answer = handle(handler, request.clone(), timeout).await;
    let payload = home_response(&request, answer);
    tokio::time::timeout_at(
        reply_by,
        replier.reply(event, HOME_RESPONSE, payload.clone()),
    )
    .await
    .map_err(|_| {
        crate::ThalovantError::Timeout(
            "the home response could not be sent before the hub's deadline".into(),
        )
    })??;
    Ok(payload)
}

/// Run a handler to an answer, whatever it does.
async fn handle<H, Fut, E>(handler: H, request: HomeRequest, timeout: Duration) -> HomeAnswer
where
    H: FnOnce(HomeRequest) -> Fut,
    Fut: Future<Output = std::result::Result<HomeAnswer, E>>,
{
    let failed = || HomeAnswer::error("failed_to_handle");
    let Ok(running) = std::panic::catch_unwind(AssertUnwindSafe(|| handler(request))) else {
        return failed();
    };
    match tokio::time::timeout(timeout, AssertUnwindSafe(running).catch_unwind()).await {
        Ok(Ok(Ok(answer))) => answer,
        Ok(Ok(Err(_))) | Ok(Err(_)) => failed(),
        Err(_) => HomeAnswer::error("timeout"),
    }
}

/// Answer every [`HOME_REQUEST`] `session` receives, on every client it
/// builds, until the returned subscription is stopped or dropped.
///
/// Each request is answered on a task of its own by
/// [`answer_home_request`], so a slow one does not hold up the next.
/// Stopping cancels the answers still running. Fails only when the session is
/// closed.
#[must_use = "dropping the subscription stops answering"]
pub fn answer_home_requests<H, Fut, E>(
    session: &HubSession,
    handler: H,
    timeout: Duration,
) -> Result<HomeRequestSubscription>
where
    H: Fn(HomeRequest) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = std::result::Result<HomeAnswer, E>> + Send + 'static,
    E: Send + 'static,
{
    let handler = Arc::new(handler);
    // Weak, so that the handler the session holds does not keep the session
    // alive.
    let weak = session.downgrade();
    let id = session.on(HOME_REQUEST, move |event| {
        let handler = handler.clone();
        let weak = weak.clone();
        async move {
            let Some(session) = weak.upgrade() else {
                return;
            };
            // A reply that cannot be sent has nowhere to be reported: the
            // hub answers the person with its own `timeout`.
            let _ =
                answer_home_request(&session, &event, |request| handler(request), timeout).await;
        }
    })?;
    Ok(HomeRequestSubscription {
        session: session.downgrade(),
        id: Some(id),
    })
}

/// Answering a session's home requests; see [`answer_home_requests`].
///
/// [`stop`](Self::stop) it, or drop it, to stop answering and cancel the
/// answers still running.
#[must_use = "dropping the subscription stops answering"]
pub struct HomeRequestSubscription {
    session: WeakHubSession,
    id: Option<HandlerId>,
}

impl HomeRequestSubscription {
    /// Stop answering and cancel the answers still running.
    pub fn stop(mut self) {
        self.unsubscribe();
    }

    fn unsubscribe(&mut self) {
        if let (Some(id), Some(session)) = (self.id.take(), self.session.upgrade()) {
            session.off(id);
        }
    }
}

impl Drop for HomeRequestSubscription {
    fn drop(&mut self) {
        self.unsubscribe();
    }
}

impl std::fmt::Debug for HomeRequestSubscription {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HomeRequestSubscription")
            .field("id", &self.id)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A replier whose reply never completes.
    struct Stalling;

    impl Replier for Stalling {
        fn reply(
            &self,
            _event: &Event,
            _msg_type: &str,
            _data: Data,
        ) -> impl Future<Output = Result<()>> + Send {
            std::future::pending()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_reply_gets_only_what_is_left_of_the_hubs_bound() {
        let started = tokio::time::Instant::now();
        let event = Event::new(HOME_REQUEST, Data::new(), crate::Context::new(), None);
        let outcome = answer_home_request(
            &Stalling,
            &event,
            |_| async { Ok::<_, ()>(HomeAnswer::action_done("done")) },
            Duration::from_millis(50),
        )
        .await;
        assert!(matches!(outcome, Err(crate::ThalovantError::Timeout(_))));
        assert_eq!(started.elapsed(), HOME_REQUEST_TIMEOUT);
    }

    #[test]
    fn speech_decodes_what_html_unescape_decodes() {
        for (text, expected) in [
            ("a &amp; b", "a & b"),
            ("&lt;b&gt;", "<b>"),
            (
                "it&#39;s &apos;quoted&apos; &quot;twice&quot;",
                "it's 'quoted' \"twice\"",
            ),
            ("&#x41;&#X42;&#67;", "ABC"),
            ("&#65&#x42 c", "AB c"),
            ("caf&eacute; na&iuml;ve &euro;5", "café naïve €5"),
            ("&amp &copy 2026", "& © 2026"),
            ("&notit; &ampere;", "¬it; &ere;"),
            ("&AMP; &COPY;", "& ©"),
            ("&unknown; & &; &#; &#x;", "&unknown; & &; &#; &#x;"),
            ("&#128; &#150; &#129;", "€ – \u{81}"),
            (
                "&#0; &#xd800; &#1114112; &#99999999999999999999;",
                "\u{fffd} \u{fffd} \u{fffd} \u{fffd}",
            ),
            ("a&#1;b&#xfffe;c&#x7f;d", "abcd"),
            ("&#13;", ""),
        ] {
            assert_eq!(plain_speech(text), expected, "{text}");
        }
        // The one place this differs from html.unescape, as documented: an
        // HTML5 name outside HTML 4 is left as written.
        assert_eq!(plain_speech("&rightarrow;"), "&rightarrow;");
    }

    #[test]
    fn every_unicode_space_collapses_as_python_sees_it() {
        assert_eq!(
            plain_speech("\u{3000} a\u{a0}\u{2003}b\u{1f}c\u{85}\n\td \u{200b}"),
            "a b c d \u{200b}"
        );
        assert_eq!(plain_speech("   "), "");
        assert_eq!(plain_speech("<speak>  </speak>"), "");
    }

    #[test]
    fn the_contract_holds_whatever_the_handler_says() {
        let request = HomeRequest::from_event(&Event::new(
            HOME_REQUEST,
            serde_json::json!({"request_id": "r", "utterance": "u", "conversation_id": "c"})
                .as_object()
                .unwrap()
                .clone(),
            Default::default(),
            None,
        ));
        // An error code with a type that is not an error is dropped, not unknown.
        let payload = home_response(
            &request,
            HomeAnswer::action_done("ok").with_error_code("timeout"),
        );
        assert_eq!(payload.get("error_code"), None);
        assert_eq!(payload["response_type"], "action_done");
        // An error without a code is unknown.
        let payload = home_response(&request, HomeAnswer::new("error", "hm"));
        assert_eq!(payload["error_code"], "unknown");
        assert_eq!(payload["speech"], "hm");
        // The handler's own conversation wins over the request's.
        let payload = home_response(&request, HomeAnswer::default().with_conversation_id("mine"));
        assert_eq!(payload["conversation_id"], "mine");
        assert_eq!(
            home_response(&request, HomeAnswer::default())["conversation_id"],
            "c"
        );
    }
}
