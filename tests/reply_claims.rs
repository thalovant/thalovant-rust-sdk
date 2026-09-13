use serde_json::{Map, Value};
use thalovant::{Event, Reply};
#[test]
fn shared_reply_claim_vectors() {
    let vectors: Value =
        serde_json::from_str(include_str!("data/reply-claim-vectors.json")).unwrap();
    for row in vectors["cases"].as_array().unwrap() {
        let handled = row["handled"].as_bool().unwrap();
        let failed = row["failed"].as_bool().unwrap();
        let reply = Reply {
            dropped_media: 0,
            text: "reply".into(),
            utterances: vec![],
            handled,
            ok: handled && !failed,
            session_id: None,
            request_id: None,
            events: row["contexts"]
                .as_array()
                .unwrap()
                .iter()
                .map(|context| {
                    Event::new(
                        "speak",
                        Map::new(),
                        context.as_object().unwrap().clone(),
                        None,
                    )
                })
                .collect(),
            failure_event: failed.then(|| Event::new("failure", Map::new(), Map::new(), None)),
        };
        assert_eq!(
            serde_json::json!(reply.pipeline_ids()),
            row["expected"]["pipeline_ids"],
            "{}",
            row["name"]
        );
        assert_eq!(
            serde_json::json!(reply.skill_ids()),
            row["expected"]["skill_ids"],
            "{}",
            row["name"]
        );
        assert_eq!(
            reply.claimed(),
            row["expected"]["claimed"].as_bool().unwrap(),
            "{}",
            row["name"]
        );
    }
}
