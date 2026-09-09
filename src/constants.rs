pub const DEFAULT_USER_AGENT: &str = concat!("thalovant-rust-sdk/", env!("CARGO_PKG_VERSION"));

pub const EVENT_RECOGNIZER_LOOP_UTTERANCE: &str = "recognizer_loop:utterance";
pub const EVENT_UTTERANCE_HANDLED: &str = "ovos.utterance.handled";
pub const EVENT_SPEAK: &str = "speak";
pub const EVENT_OVOS_UTTERANCE_SPEAK: &str = "ovos.utterance.speak";
/// Legacy Mycroft name for the "no intent matched" bus event.
pub const EVENT_INTENT_FAILURE: &str = "complete_intent_failure";
/// Current OVOS name for the "no intent matched" bus event (renamed from the
/// legacy Mycroft `complete_intent_failure`).
pub const EVENT_INTENT_UNMATCHED: &str = "ovos.intent.unmatched";
pub const EVENT_POLICY_DENIED: &str = "hive.policy.denied";
pub const EVENT_QUERY_TIMEOUT: &str = "hive.query.timeout";
// The hub runtime's intent manifest (OVOS-INTENT-4 section 10) and the engines'
// own manifests, the names-only fallback. See `intents`.
pub const EVENT_INTENT_LIST: &str = "ovos.intent.list";
pub const EVENT_INTENT_LIST_RESPONSE: &str = "ovos.intent.list.response";
pub const EVENT_INTENT_DESCRIBE: &str = "ovos.intent.describe";
pub const EVENT_INTENT_DESCRIBE_RESPONSE: &str = "ovos.intent.describe.response";
pub const EVENT_ADAPT_MANIFEST_GET: &str = "intent.service.adapt.manifest.get";
pub const EVENT_ADAPT_MANIFEST: &str = "intent.service.adapt.manifest";
pub const EVENT_PADATIOUS_MANIFEST_GET: &str = "intent.service.padatious.manifest.get";
pub const EVENT_PADATIOUS_MANIFEST: &str = "intent.service.padatious.manifest";

pub fn is_failure_event(name: &str) -> bool {
    matches!(
        name,
        EVENT_INTENT_FAILURE | EVENT_INTENT_UNMATCHED | EVENT_POLICY_DENIED | EVENT_QUERY_TIMEOUT
    )
}

/// Optional OVOS fallback skill discovery.
pub const EVENT_FALLBACK_LIST: &str = "ovos.skills.fallback.list";
pub const EVENT_FALLBACK_LIST_RESPONSE: &str = "ovos.skills.fallback.list.response";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ovos_intent_unmatched_is_failure() {
        // OVOS renamed the "no intent matched" event; the current name must be
        // classified as a terminal failure so ask() fails promptly.
        assert!(is_failure_event("ovos.intent.unmatched"));
        assert!(is_failure_event(EVENT_INTENT_UNMATCHED));
    }

    #[test]
    fn legacy_complete_intent_failure_is_still_failure() {
        // The legacy Mycroft name must keep working for older stacks.
        assert!(is_failure_event("complete_intent_failure"));
        assert!(is_failure_event(EVENT_INTENT_FAILURE));
    }

    #[test]
    fn policy_and_timeout_events_are_failures() {
        assert!(is_failure_event(EVENT_POLICY_DENIED));
        assert!(is_failure_event(EVENT_QUERY_TIMEOUT));
    }

    #[test]
    fn matched_and_handler_events_are_not_failures() {
        assert!(!is_failure_event(EVENT_UTTERANCE_HANDLED));
        assert!(!is_failure_event(EVENT_SPEAK));
        assert!(!is_failure_event("some.other.event"));
    }
}
