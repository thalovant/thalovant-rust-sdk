use serde_json::Value;
use std::collections::BTreeMap;
use thalovant::{
    as_sentence, closest_language, speakable_with_language, HubIntent, IntentExampleOptions,
    ListingData, ListingLanguage, ListingRules, DEFAULT_LISTING,
};

#[test]
fn published_python_listing_golden_cases() {
    let data: Value = serde_json::from_str(include_str!("data/listing-vectors.json")).unwrap();
    for row in data["cases"].as_array().unwrap() {
        let lang = row["lang"].as_str();
        let actual = match row["kind"].as_str().unwrap() {
            "sentence" => serde_json::json!(as_sentence(row["text"].as_str().unwrap(), lang)),
            "speakable" => serde_json::json!(speakable_with_language(
                row["text"].as_str().unwrap(),
                &BTreeMap::new(),
                lang
            )),
            "rank" => serde_json::json!(DEFAULT_LISTING.rank(
                &serde_json::from_value::<Vec<String>>(row["phrases"].clone()).unwrap(),
                lang
            )),
            _ => panic!("unknown golden case"),
        };
        assert_eq!(actual, row["expected"], "{row}");
    }
}
#[test]
fn ovos_language_matching_golden_cases() {
    let data: Value =
        serde_json::from_str(include_str!("data/language-matching-vectors.json")).unwrap();
    for row in data["cases"].as_array().unwrap() {
        let available = row["available"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tag| tag.as_str().unwrap());
        assert_eq!(
            closest_language(row["target"].as_str().unwrap(), available),
            row["expected"].as_str(),
            "{row}"
        );
    }
}
fn intent(phrases: BTreeMap<String, Vec<String>>) -> HubIntent {
    HubIntent {
        skill_id: "s".into(),
        name: "n".into(),
        engine: "padatious".into(),
        enabled: true,
        phrases,
    }
}
#[test]
fn selected_locale_and_rendered_result_limits() {
    let i = intent(BTreeMap::from([(
        "fr-FR".into(),
        vec!["volume {level} pour cent".into()],
    )]));
    assert_eq!(
        i.examples_with_listing(
            None,
            2,
            &IntentExampleOptions {
                sentence: true,
                ..Default::default()
            }
        ),
        ["Volume cinquante pour cent."]
    );
    assert_eq!(
        i.examples_with_listing(
            Some("fr-CA"),
            2,
            &IntentExampleOptions {
                sentence: true,
                slots: BTreeMap::from([("level".into(), "dix".into())]),
                ..Default::default()
            }
        ),
        ["Volume dix pour cent."]
    );
    let i = intent(BTreeMap::from([(
        "en-US".into(),
        [
            "[please]",
            "(repeat|say) that (again|)",
            "[please] repeat that",
            "volume [to] {level} percent",
        ]
        .map(String::from)
        .into(),
    )]));
    for limit in [0, 2] {
        assert_eq!(
            i.examples_with_listing(
                Some("en-US"),
                limit,
                &IntentExampleOptions {
                    sentence: true,
                    ..Default::default()
                }
            ),
            ["Repeat that.", "Volume fifty percent."]
        );
    }
    assert_eq!(i.examples(Some("en-US"), 0), i.phrases["en-US"]);
}
#[test]
fn independent_optional_rules_and_owned_data() {
    let mut data = ListingData {
        sentence_ends: ".!?".into(),
        languages: BTreeMap::from([(
            "xq".into(),
            ListingLanguage {
                question_patterns: vec!["(?i)^is it".into(), "(?m)^can it".into()],
                slot_examples: BTreeMap::from([("thing".into(), "the widget".into())]),
                ..Default::default()
            },
        )]),
    };
    let listing = ListingRules::new(Some(data.clone())).unwrap();
    data.languages.get_mut("xq").unwrap().question_patterns[0] = "never".into();
    assert_eq!(
        listing.as_sentence("is it ready", Some("xq")),
        "Is it ready?"
    );
    assert_eq!(
        listing.as_sentence("can it work", Some("xq")),
        "Can it work?"
    );
    assert_eq!(
        listing.speakable("open {thing}", &BTreeMap::new(), Some("xq-ZZ")),
        "open the widget"
    );
    assert_eq!(listing.as_sentence("go home", Some("xq")), "Go home.");
    assert_eq!(
        listing.as_sentence("what time is it", Some("en")),
        "What time is it"
    );
    let listing = ListingRules::new(Some(ListingData {
        sentence_ends: ".!?".into(),
        languages: BTreeMap::from([(
            "xq".into(),
            ListingLanguage {
                question_words_anywhere: vec!["plim".into()],
                ..Default::default()
            },
        )]),
    }))
    .unwrap();
    assert_eq!(
        listing.as_sentence("go plim now", Some("xq")),
        "Go plim now?"
    );
}
#[test]
fn no_data_invalid_patterns_and_bounded_backtracking() {
    let bare = ListingRules::new(None).unwrap();
    assert!(!bare.available());
    assert_eq!(
        bare.as_sentence("do i need a jacket", Some("en")),
        "Do i need a jacket"
    );
    assert_eq!(
        bare.speakable("volume [to] {level} percent", &BTreeMap::new(), Some("en")),
        "volume level percent"
    );
    let data = |pattern: &str| ListingData {
        sentence_ends: ".!?".into(),
        languages: BTreeMap::from([(
            "xq".into(),
            ListingLanguage {
                question_patterns: vec![pattern.into()],
                ..Default::default()
            },
        )]),
    };
    assert!(ListingRules::new(Some(data("("))).is_err());
    let costly = ListingRules::new(Some(data(r"(a|b|ab)*(?>c)$"))).unwrap();
    let text = "ab".repeat(40) + "cx";
    assert!(costly.asks(&text, Some("xq")).is_err());
    assert_eq!(
        costly.as_sentence(&text, Some("xq")),
        "A".to_owned() + &text[1..]
    );
}
