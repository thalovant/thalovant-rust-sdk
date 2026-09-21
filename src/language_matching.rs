//! OVOS-compatible locale distances from langcodes 3.5.1 CLDR tables.
//! Tuple algorithm adapted from langcodes (MIT; see LICENSE-langcodes).
use serde::Deserialize;
use std::{collections::BTreeMap, sync::LazyLock};
#[derive(Deserialize)]
struct MatchingData {
    likely: BTreeMap<String, String>,
    languages: BTreeMap<String, String>,
    scripts: BTreeMap<String, String>,
    territories: BTreeMap<String, String>,
    default_scripts: BTreeMap<String, String>,
    macrolanguages: BTreeMap<String, String>,
    distances: BTreeMap<String, BTreeMap<String, u32>>,
    regions: BTreeMap<String, Vec<String>>,
}
static DATA: LazyLock<MatchingData> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../data/language-matching.json"))
        .expect("valid embedded CLDR data")
});
#[derive(Default)]
struct Tag {
    language: String,
    script: String,
    region: String,
}
fn script(value: &str) -> bool {
    value.len() == 4 && value.bytes().all(|c| c.is_ascii_lowercase())
}
fn region(value: &str) -> bool {
    (value.len() == 2 && value.bytes().all(|c| c.is_ascii_lowercase()))
        || (value.len() == 3 && value.bytes().all(|c| c.is_ascii_digit()))
}
fn parse(value: &str, aliases: bool) -> Tag {
    let mut value = value.trim().replace('_', "-").to_lowercase();
    if aliases {
        if let Some(alias) = DATA.languages.get(&value) {
            value = alias.to_lowercase();
        }
    }
    let mut tokens = value.split('-');
    let primary = tokens.next().filter(|s| !s.is_empty()).unwrap_or("und");
    let mut base = if aliases {
        DATA.languages.get(primary).map(|alias| parse(alias, false))
    } else {
        None
    }
    .unwrap_or_else(|| Tag {
        language: primary.into(),
        ..Default::default()
    });
    let mut only_script = true;
    for token in tokens {
        if !script(token) {
            only_script = false;
        }
        if token.len() == 1 {
            break;
        }
        if script(token) {
            base.script = DATA
                .scripts
                .get(token)
                .cloned()
                .unwrap_or_else(|| token[..1].to_uppercase() + &token[1..]);
        } else if region(token) {
            base.region = DATA
                .territories
                .get(token)
                .cloned()
                .unwrap_or_else(|| token.to_uppercase());
        }
    }
    if DATA.default_scripts.get(&base.language) == Some(&base.script) {
        base.script.clear();
    }
    if base.language == "pt" && base.script.is_empty() && base.region.is_empty() && only_script {
        base.region = "PT".into();
    }
    base
}
fn maximize(mut value: Tag) -> Tag {
    if value.language == "und" && value.script.is_empty() && value.region.is_empty() {
        return Tag {
            language: "und".into(),
            script: "Zzzz".into(),
            region: "ZZ".into(),
        };
    }
    if let Some(language) = DATA.macrolanguages.get(&value.language) {
        value.language = language.clone();
    }
    let join = |parts: &[&str]| {
        parts
            .iter()
            .filter(|v| !v.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join("-")
    };
    let mut probes = vec![
        join(&[&value.language, &value.script, &value.region]),
        join(&[&value.language, &value.region]),
        join(&[&value.language, &value.script]),
        value.language.clone(),
    ];
    if !value.script.is_empty() {
        probes.push(format!("und-{}", value.script));
    }
    probes.push("und".into());
    let found = probes
        .iter()
        .find_map(|p| DATA.likely.get(p))
        .expect("embedded default likely subtag");
    let parts: Vec<_> = found.split('-').collect();
    if value.language == "und" {
        value.language = parts[0].into();
    }
    if value.script.is_empty() {
        value.script = parts[1].into();
    }
    if value.region.is_empty() {
        value.region = parts[2].into();
    }
    value
}
fn distance(wanted: &str, candidate: &str) -> u32 {
    let a = maximize(parse(wanted, true));
    let b = maximize(parse(candidate, true));
    let lookup = |from: &str, to: &str, fallback| {
        DATA.distances
            .get(from)
            .and_then(|map| map.get(to))
            .copied()
            .unwrap_or(fallback)
    };
    let mut result = if a.language == b.language {
        0
    } else {
        lookup(&a.language, &b.language, 80)
    };
    let pa = format!("{}_{}", a.language, a.script);
    let pb = format!("{}_{}", b.language, b.script);
    if a.script != b.script {
        result += lookup(&pa, &pb, 50);
    }
    if a.region == b.region {
        return result;
    }
    let inside = |group: &str, region: &str| DATA.regions[group].iter().any(|r| r == region);
    let mut td = 4;
    if pa == pb {
        if a.language == "ar" {
            if inside("MAGHREB", &a.region) != inside("MAGHREB", &b.region) {
                td = 5;
            }
        } else if a.language == "en" {
            if (a.region == "GB" && !inside("US", &b.region))
                || (!inside("US", &a.region) && b.region == "GB")
            {
                td = 3;
            } else if inside("US", &a.region) != inside("US", &b.region) {
                td = 5;
            }
        } else if inside("LATIN_AMERICA", &a.region) && b.region == "419" {
            td = 1;
        } else if a.language == "es" || a.language == "pt" {
            if inside("AMERICAS", &a.region) != inside("AMERICAS", &b.region) {
                td = 5;
            }
        } else if pa == "zh_Hant" && inside("CNSAR", &a.region) != inside("CNSAR", &b.region) {
            td = 5;
        }
    }
    result + td
}
/// Nearest OVOS-compatible registration at distance ten or less. Ties retain
/// iteration order, including equivalent zero-distance tags.
/// The form a language is usually written in, when that differs from `tag`.
///
/// `en-CA` and `en-AT` both to `en-us`, `fr-BE` to `fr-fr`, `pt-AO` to
/// `pt-br`, from CLDR's likely subtags. `None` when there is nothing
/// different to try, so a caller can tell "already the usual form" from
/// "no idea".
///
/// Listing and asking do not agree about languages, and this closes the gap.
/// A hub matches an utterance to the closest language it knows, so a phone
/// set to `en-CA` is understood by skills registered under `en-US`; its
/// manifest is keyed by exact tag, so the same hub lists nothing for `en-CA`.
///
/// Lower case, because that is how skills register and how the manifest is
/// keyed: an exact lookup with BCP47's `en-US` finds nothing.
pub fn usual_form(tag: &str) -> Option<String> {
    if tag.trim().is_empty() {
        return None;
    }
    let base = parse(tag, true).language;
    // "und" is the tag for "no idea", and `parse` produces it for anything it
    // cannot read. CLDR's guess for an unknown language is English, so
    // without this a blank tag lists a hub in a language nobody asked for.
    if base.is_empty() || base == "und" {
        return None;
    }
    // `maximize` does not fail on a language it has never heard of: it walks
    // its probes down to "und" and takes the root locale's region, so "zzz"
    // comes back "zzz-us". Round-tripping the tag does not catch that, because
    // the unknown language is carried through unchanged. A direct entry in the
    // likely table is what says CLDR has heard of this language.
    if !DATA.likely.contains_key(&base) {
        return None;
    }
    let likely = maximize(Tag {
        language: base,
        ..Default::default()
    });
    let usual = if likely.region.is_empty() {
        likely.language.clone()
    } else {
        format!("{}-{}", likely.language, likely.region)
    }
    .to_lowercase();
    // Byte comparison, NOT sameLanguage. They are not the same test, and the difference is the whole point: the canonical spelling is en-US, the manifest is keyed en-us, and sameLanguage calls those equal -- so the retry that exists for exactly this case suppressed itself.
    if usual == tag.trim() {
        None
    } else {
        Some(usual)
    }
}

pub fn closest_language<'a>(
    target: &str,
    available: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    let mut best = None;
    let mut minimum = u32::MAX;
    for candidate in available {
        let value = distance(target, candidate);
        if value < minimum {
            best = Some(candidate);
            minimum = value;
        }
    }
    if minimum <= 10 {
        best
    } else {
        None
    }
}

#[cfg(test)]
mod usual_form_tests {
    use super::usual_form;

    /// The tag to retry a listing with, when the hub had nothing under the
    /// one asked for. These answers are CLDR's, not this SDK's, and must
    /// match the Python reference: a managed port that disagreed about which
    /// language to retry would list a different hub.
    #[test]
    fn a_regional_tag_becomes_the_form_skills_register() {
        assert_eq!(usual_form("en-CA").as_deref(), Some("en-us"));
        assert_eq!(usual_form("en-AT").as_deref(), Some("en-us"));
        assert_eq!(usual_form("fr-BE").as_deref(), Some("fr-fr"));
        assert_eq!(usual_form("pt-AO").as_deref(), Some("pt-br"));
        assert_eq!(usual_form("pt-PT").as_deref(), Some("pt-br"));
        assert_eq!(usual_form("de-AT").as_deref(), Some("de-de"));
    }

    #[test]
    fn a_tag_already_usual_has_nothing_to_retry_with() {
        // None rather than the same tag, so a hub that answered is never
        // asked twice.
        // Only byte-for-byte. The capital spelling is a different string to
        // a manifest keyed `en-us`, and suppressing its retry was the bug.
        assert_eq!(usual_form("en-US").as_deref(), Some("en-us"));
        assert_eq!(usual_form("fr-FR").as_deref(), Some("fr-fr"));
        assert_eq!(usual_form("en-us"), None);
        assert_eq!(usual_form("fr-fr"), None);
    }

    #[test]
    fn a_language_nobody_has_heard_of_is_none_and_not_a_guess() {
        // `maximize` does not fail on an unknown language: it walks down to
        // "und" and takes the root locale's region, so "zzz" would come back
        // "zzz-us" -- a confident United States for a language that does not
        // exist.
        assert_eq!(usual_form("zzz"), None);
        assert_eq!(usual_form(""), None);
        assert_eq!(usual_form("xx-YY"), None);
    }
}
