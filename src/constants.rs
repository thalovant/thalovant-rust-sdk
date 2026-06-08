pub const DEFAULT_USER_AGENT: &str = "thalovant-rust-sdk/0.2.5";

pub const EVENT_RECOGNIZER_LOOP_UTTERANCE: &str = "recognizer_loop:utterance";
pub const EVENT_UTTERANCE_HANDLED: &str = "ovos.utterance.handled";
pub const EVENT_SPEAK: &str = "speak";
pub const EVENT_INTENT_FAILURE: &str = "complete_intent_failure";
pub const EVENT_POLICY_DENIED: &str = "hive.policy.denied";

pub fn is_failure_event(name: &str) -> bool {
    matches!(name, EVENT_INTENT_FAILURE | EVENT_POLICY_DENIED)
}
