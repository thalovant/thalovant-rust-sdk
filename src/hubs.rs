//! What to call a hub on a screen somebody is reading.
//!
//! Every control-plane read in this SDK returns raw JSON, so each caller picks
//! its own fields -- and on 2026-09-15 a phone offered somebody a list of
//! rooms called "ops-copilot", "daily-desk", "news-stream". Those are slugs.
//! The app was not careless: it read `name` and preferred it over `slug`, and
//! on that deployment `name` *holds* the slug. The name a person was shown
//! when the hub was made lives in `spec.catalog.title`.
//!
//! One place to get that wrong is better than one per app.

use serde_json::Value;

fn text(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// The readable name of a hub, never a slug when anything better exists.
pub fn hub_display_name(hub: &Value) -> String {
    if let Some(title) = text(
        hub.get("spec")
            .and_then(|spec| spec.get("catalog"))
            .and_then(|catalog| catalog.get("title")),
    ) {
        return title.to_string();
    }

    let name = text(hub.get("name"));
    let slug = text(hub.get("slug"));
    // A name that is exactly the slug is the slug.
    if let Some(name) = name {
        if Some(name) != slug {
            return name.to_string();
        }
    }

    let Some(identifier) = name.or(slug) else {
        return "A Thalovant hub".to_string();
    };
    let readable = identifier
        .split(['-', '_'])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if readable.is_empty() {
        "A Thalovant hub".to_string()
    } else {
        readable
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // What a phone calls a hub. It called them slugs until 2026-09-15.

    #[test]
    fn a_hub_is_called_what_a_person_was_shown() {
        // Exactly what a phone was offered: name IS the slug, and the readable
        // title sits in the catalog entry.
        let hub = json!({
            "name": "ops-copilot",
            "slug": "ops-copilot",
            "spec": {"catalog": {"title": "Ops Copilot"}}
        });
        assert_eq!(hub_display_name(&hub), "Ops Copilot");
    }

    #[test]
    fn a_real_name_wins_when_there_is_no_catalog_entry() {
        assert_eq!(
            hub_display_name(&json!({"name": "The Kitchen", "slug": "kitchen"})),
            "The Kitchen"
        );
    }

    #[test]
    fn a_slug_is_made_readable_rather_than_shown_raw() {
        assert_eq!(
            hub_display_name(&json!({"slug": "daily-desk"})),
            "Daily Desk"
        );
        assert_eq!(
            hub_display_name(&json!({"slug": "local_pulse"})),
            "Local Pulse"
        );
        assert_eq!(
            hub_display_name(&json!({"name": "news-stream", "slug": "news-stream"})),
            "News Stream"
        );
    }

    #[test]
    fn a_hub_described_with_nothing_still_says_something() {
        for hub in [
            json!({"id": "1"}),
            json!({"name": "", "slug": "   "}),
            json!({"spec": "nonsense"}),
            json!({"spec": {"catalog": []}}),
        ] {
            assert_eq!(hub_display_name(&hub), "A Thalovant hub", "{hub}");
        }
    }
}
