use std::{cmp::Ordering, fs};
use thalovant::inventory::*;

fn sample() -> Inventory {
    Inventory::from_json(r#"{"cache_version":1,"hub_id":"hub","hub_name":"Kitchen","source":"hub","generated_at":"2026-09-13T00:00:00Z","notes":[],"skills":[{"id":"weather","title":"Weather","locales":["en-us"],"intents":[{"id":"weather.now","name":"weather.now","skill_id":"weather","engine":"padatious","phrases":{"fr-fr":["météo"],"en-us":["weather","what is the weather"]}}]},{"id":"unknown","title":"Unknown","locales":[],"intents":[]}]}"#).unwrap()
}
#[test]
fn inventory_round_trip_preserves_locale_knowledge_and_phrase_order() {
    let inventory = Inventory::from_json(&sample().as_json().unwrap()).unwrap();
    assert!(inventory.live());
    assert!(inventory.has_phrases());
    assert_eq!(inventory.skills[0].speaks("en-gb"), Some(true));
    assert_eq!(inventory.skills[0].speaks("de"), Some(false));
    assert_eq!(inventory.skills[1].speaks("en"), None);
    assert_eq!(
        inventory.skills[0].intents[0].examples(None, 0),
        vec!["météo"]
    );
    assert_eq!(
        inventory.skills[0].intents[0].examples(Some("en-gb"), 0),
        vec!["weather", "what is the weather"]
    );
    assert_eq!(languages_present(&inventory), vec!["en-us", "fr-fr"]);
    assert_eq!(inventory, sample());
}
#[test]
fn inventory_rejects_incomplete_and_wrongly_typed_cache_shapes() {
    let raw = sample().as_json().unwrap();
    for broken in [
        raw.replace(",\"notes\":[]", ""),
        raw.replace("\"cache_version\":1", "\"cache_version\":true"),
        raw.replace("\"locales\":[\"en-us\"]", "\"locales\":42"),
        format!("{raw} garbage"),
    ] {
        assert!(Inventory::from_json(&broken).is_err(), "{broken}");
    }
}
#[test]
fn cache_is_private_optional_and_rejects_traversal_corruption_and_large_files() {
    let directory =
        std::env::temp_dir().join(format!("thalovant-inventory-{}", uuid::Uuid::new_v4()));
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(directory.clone());
    let cache = InventoryCache::new(&directory);
    cache.store("valid", &sample());
    assert!(cache.load("valid").is_some());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(cache.path("valid").unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    cache.store("../../escape", &sample());
    assert!(cache.load("../../escape").is_none());
    fs::write(cache.path("valid").unwrap(), "{broken").unwrap();
    assert!(cache.load("valid").is_none());
    fs::OpenOptions::new()
        .write(true)
        .open(cache.path("valid").unwrap())
        .unwrap()
        .set_len(8 * 1024 * 1024 + 1)
        .unwrap();
    assert!(cache.load("valid").is_none());
    cache.store("valid", &sample());
    assert!(cache.load("valid").is_some());
    assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
}
#[test]
fn presentation_helpers_cover_empty_affixes_and_natural_order() {
    assert_eq!(friendly_title("ovos-skill-weather.openvoiceos"), "Weather");
    assert_eq!(
        common_affix(&["weather.intent", "time.intent"]),
        (Some("suffix"), "intent".into())
    );
    assert_eq!(strip_affix("", Some("suffix"), ""), "");
    assert_eq!(compare_names("Skill2", "Skill10"), Ordering::Less);
}

#[test]
fn shared_python_reference_survives_sorted_json() {
    let data: serde_json::Value =
        serde_json::from_str(include_str!("data/inventory-vectors.json")).unwrap();
    let inventory = Inventory::from_json(&data["inventory"].to_string()).unwrap();
    for row in data["examples"].as_array().unwrap() {
        assert_eq!(
            inventory.skills[0].intents[0].examples(
                row["language"].as_str(),
                row["limit"].as_u64().unwrap() as usize
            ),
            serde_json::from_value::<Vec<String>>(row["expected"].clone()).unwrap(),
            "{row}"
        );
    }
    for row in data["speaks"].as_array().unwrap() {
        assert_eq!(
            inventory.skills[0].speaks(row["language"].as_str().unwrap()),
            row["expected"].as_bool()
        );
    }
    assert_eq!(inventory.skills[1].speaks("en"), None);
}
