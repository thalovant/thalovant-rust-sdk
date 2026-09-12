use crate::events::Context;
use regex::Regex;
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::LazyLock};

#[derive(Clone, Debug, Default)]
pub struct RequestContextOptions {
    pub stt_lang: Option<String>,
    pub pipeline: Vec<String>,
    pub location: Option<Context>,
}

pub fn request_context(base: Option<&Context>, options: &RequestContextOptions) -> Option<Context> {
    let mut result = base.cloned().unwrap_or_default();
    let stages: Vec<&str> = options
        .pipeline
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if !stages.is_empty() {
        let mut session = result
            .get("session")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        session.insert("pipeline".into(), json!(stages));
        result.insert("session".into(), Value::Object(session));
    }
    if let Some(lang) = options
        .stt_lang
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        result.insert("stt_lang".into(), json!(lang));
    }
    if let Some(location) = options.location.as_ref().filter(|v| !v.is_empty()) {
        result.insert("location".into(), Value::Object(location.clone()));
    }
    (!result.is_empty()).then_some(result)
}

/// City is required. Coordinates may be JSON numbers or strings.
#[derive(Clone, Debug, Default)]
pub struct LocationOptions {
    pub city: String,
    pub region: String,
    pub country: String,
    pub timezone: String,
    pub latitude: Option<Value>,
    pub longitude: Option<Value>,
}
pub fn build_location(options: &LocationOptions) -> Option<Context> {
    let city = options.city.trim();
    if city.is_empty() {
        return None;
    }
    let mut result = Context::from_iter([("city".into(), json!(city))]);
    if !options.region.trim().is_empty() {
        result.insert("region".into(), json!(options.region.trim()));
    }
    if !options.country.trim().is_empty() {
        result.insert(
            "country_code".into(),
            json!(options.country.trim().to_uppercase()),
        );
    }
    if !options.timezone.trim().is_empty() {
        result.insert("timezone".into(), json!({"code": options.timezone.trim()}));
    }
    fn coordinate(value: &Option<Value>) -> Option<f64> {
        value.as_ref().and_then(|v| {
            v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        })
    }
    if let (Some(lat), Some(lon)) = (
        coordinate(&options.latitude),
        coordinate(&options.longitude),
    ) {
        if (lat != 0.0 || lon != 0.0)
            && (-90.0..=90.0).contains(&lat)
            && (-180.0..=180.0).contains(&lon)
        {
            result.insert(
                "coordinate".into(),
                json!({"latitude": lat, "longitude": lon}),
            );
        }
    }
    Some(result)
}

static OPTIONAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[[^\[\]]*\]").unwrap());
static GROUP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\(([^()]*)\)").unwrap());
static SLOT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{([a-z_][a-z0-9_]*)\}").unwrap());
static SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s{2,}").unwrap());
/// Render one illustrative sentence without inventing slot values.
pub fn speakable(pattern: &str, slots: &BTreeMap<String, String>) -> String {
    let mut text = pattern.to_owned();
    while OPTIONAL.is_match(&text) {
        text = OPTIONAL.replace_all(&text, "").into_owned();
    }
    while GROUP.is_match(&text) {
        text = GROUP
            .replace_all(&text, |m: &regex::Captures<'_>| {
                let options: Vec<_> = m[1].split('|').map(str::trim).collect();
                let real: Vec<_> = options.iter().copied().filter(|s| !s.is_empty()).collect();
                if real.len() < options.len() && real.len() <= 1 {
                    String::new()
                } else {
                    real.first().unwrap_or(&"").to_string()
                }
            })
            .into_owned();
    }
    text = SLOT
        .replace_all(&text, |m: &regex::Captures<'_>| {
            slots
                .get(&m[1])
                .cloned()
                .unwrap_or_else(|| m[1].replace('_', " "))
        })
        .into_owned();
    SPACES
        .replace_all(&text, " ")
        .trim_matches([' ', ','])
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Data, Event};
    #[test]
    fn hints_preserve_context_and_validate_location() {
        let base = json!({"session":{"pipeline":["old"],"session_id":"kept"}})
            .as_object()
            .unwrap()
            .clone();
        let result = request_context(
            Some(&base),
            &RequestContextOptions {
                stt_lang: Some(" fr ".into()),
                pipeline: vec![" ".into(), "intent".into()],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(result["session"]["pipeline"], json!(["intent"]));
        assert_eq!(base["session"]["pipeline"], json!(["old"]));
        assert_eq!(result["stt_lang"], "fr");
        assert!(request_context(None, &RequestContextOptions::default()).is_none());
        let opts = LocationOptions {
            city: " Montréal ".into(),
            country: " ca ".into(),
            latitude: Some(json!("45.5")),
            longitude: Some(json!("-73.5")),
            ..Default::default()
        };
        assert_eq!(build_location(&opts).unwrap()["country_code"], "CA");
        for (lat, lon) in [
            (json!(0), json!(0)),
            (json!(91), json!(0)),
            (json!(0), json!(181)),
            (json!("NaN"), json!(1)),
        ] {
            assert!(!build_location(&LocationOptions {
                latitude: Some(lat),
                longitude: Some(lon),
                ..opts.clone()
            })
            .unwrap()
            .contains_key("coordinate"));
        }
    }
    #[test]
    fn patterns_and_audio_follow_python_contract() {
        assert_eq!(
            speakable(
                "did i (already |)ask (about|for|to|) {thing}",
                &BTreeMap::new()
            ),
            "did i ask about thing"
        );
        let e = Event::new(
            crate::EVENT_AUDIO_QUEUE,
            json!({"binary_data":"00 ff\n10","lang":"fr"})
                .as_object()
                .unwrap()
                .clone(),
            Context::new(),
            None,
        );
        assert_eq!(e.audio_bytes().unwrap(), vec![0, 255, 16]);
        assert_eq!(e.lang().as_deref(), Some("fr"));
        for encoded in ["", "0", "0 0", "gg", "https://example.com", "00\u{a0}ff"] {
            assert!(Event::new(
                crate::EVENT_AUDIO_QUEUE,
                Data::from_iter([("binary_data".into(), json!(encoded))]),
                Context::new(),
                None
            )
            .audio_bytes()
            .is_err());
        }
        let mut budget = crate::events::ReplyMediaBudget::default();
        let clip = "00".repeat(crate::MAX_AUDIO_CLIP_BYTES);
        for _ in 0..5 {
            budget.accept(&Event::new(
                crate::EVENT_AUDIO_QUEUE,
                json!({"binary_data":clip}).as_object().unwrap().clone(),
                Context::new(),
                None,
            ));
        }
        assert_eq!(budget.dropped, 1);
    }
}
