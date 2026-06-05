pub const DEFAULT_USER_AGENT: &str = "thalovant-rust-sdk/0.1";

pub const EVENT_RECOGNIZER_LOOP_UTTERANCE: &str = "recognizer_loop:utterance";
pub const EVENT_UTTERANCE_HANDLED: &str = "recognizer_loop:utterance_handled";
pub const EVENT_SPEAK: &str = "speak";
pub const EVENT_INTENT_FAILURE: &str = "intent_failure";
pub const EVENT_POLICY_DENIED: &str = "thalovant:policy_denied";

pub fn is_failure_event(name: &str) -> bool {
    matches!(name, EVENT_INTENT_FAILURE | EVENT_POLICY_DENIED)
}
