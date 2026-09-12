//! Locale-aware illustrative listings, using canonical thalovant-languages data.
use crate::{closest_language, Result, ThalovantError};
use fancy_regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::LazyLock,
};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ListingLanguage {
    pub trailing_words: Vec<String>,
    pub question_openers: Vec<String>,
    pub question_words_anywhere: Vec<String>,
    pub question_patterns: Vec<String>,
    pub written_forms: BTreeMap<String, String>,
    pub slot_examples: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ListingData {
    pub sentence_ends: String,
    pub languages: BTreeMap<String, ListingLanguage>,
}
/// Owned immutable rule snapshot, safe to share across threads.
#[derive(Debug)]
pub struct ListingRules {
    available: bool,
    data: ListingData,
    patterns: BTreeMap<String, Vec<Regex>>,
    written: BTreeMap<String, Vec<(Regex, String)>>,
}
fn failure(error: impl std::fmt::Display) -> ThalovantError {
    ThalovantError::Listing(error.to_string())
}
fn compile(expression: &str, ignore_case: bool) -> Result<Regex> {
    let boundary = r"(?:(?<![\p{L}\p{N}_])(?=[\p{L}\p{N}_])|(?<=[\p{L}\p{N}_])(?![\p{L}\p{N}_]))";
    let mut converted = String::new();
    let mut chars = expression.chars();
    let mut in_class = false;
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                if next == 'b' && !in_class {
                    converted.push_str(boundary);
                } else {
                    converted.push(ch);
                    converted.push(next);
                }
            } else {
                converted.push(ch);
            }
        } else {
            if ch == '[' {
                in_class = true;
            }
            if ch == ']' {
                in_class = false;
            }
            converted.push(ch);
        }
    }
    RegexBuilder::new(&converted)
        .case_insensitive(ignore_case)
        .backtrack_limit(100_000)
        .build()
        .map_err(failure)
}
impl ListingRules {
    /// None selects bare rendering. Invalid custom patterns fail construction.
    pub fn new(data: Option<ListingData>) -> Result<Self> {
        let mut result = Self {
            available: data.is_some(),
            data: data.unwrap_or_default(),
            patterns: BTreeMap::new(),
            written: BTreeMap::new(),
        };
        for (tag, language) in &result.data.languages {
            result.patterns.insert(
                tag.clone(),
                language
                    .question_patterns
                    .iter()
                    .map(|s| compile(s, true))
                    .collect::<Result<_>>()?,
            );
            result.written.insert(
                tag.clone(),
                language
                    .written_forms
                    .iter()
                    .map(|(word, written)| {
                        Ok((
                            compile(&format!(r"\b{}\b", regex::escape(word)), false)?,
                            written.clone(),
                        ))
                    })
                    .collect::<Result<_>>()?,
            );
        }
        Ok(result)
    }
    pub fn available(&self) -> bool {
        self.available
    }
    fn tag(&self, lang: Option<&str>) -> Option<&str> {
        lang.filter(|s| !s.is_empty())
            .and_then(|s| closest_language(s, self.data.languages.keys().map(String::as_str)))
    }
    pub fn language_data(&self, lang: Option<&str>) -> ListingLanguage {
        self.tag(lang)
            .and_then(|tag| self.data.languages.get(tag))
            .cloned()
            .unwrap_or_default()
    }
    fn words(
        &self,
        lang: Option<&str>,
        select: fn(&ListingLanguage) -> &Vec<String>,
    ) -> BTreeSet<String> {
        let tags = if lang.is_some_and(|s| !s.is_empty()) {
            self.tag(lang).into_iter().collect::<Vec<_>>()
        } else {
            self.data.languages.keys().map(String::as_str).collect()
        };
        tags.into_iter()
            .flat_map(|tag| {
                select(&self.data.languages[tag])
                    .iter()
                    .map(|s| s.to_lowercase())
            })
            .collect()
    }
    pub fn dangling(&self, text: &str, lang: Option<&str>) -> bool {
        text.trim_end_matches(|c| c == ' ' || self.data.sentence_ends.contains(c))
            .split_whitespace()
            .last()
            .is_some_and(|word| {
                self.words(lang, |d| &d.trailing_words)
                    .contains(&word.to_lowercase())
            })
    }
    /// Matching failures (including bounded backtracking) are returned explicitly.
    pub fn asks(&self, text: &str, lang: Option<&str>) -> Result<bool> {
        if let Some(tag) = self.tag(lang) {
            for pattern in &self.patterns[tag] {
                if pattern.is_match(text).map_err(failure)? {
                    return Ok(true);
                }
            }
        }
        let words = text
            .split_whitespace()
            .map(|w| {
                w.trim_matches(|c| ",;:!?.’'\"()".contains(c))
                    .to_lowercase()
            })
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>();
        let openers = self.words(lang, |d| &d.question_openers);
        let anywhere = self.words(lang, |d| &d.question_words_anywhere);
        Ok(words.first().is_some_and(|word| openers.contains(word))
            || words.iter().any(|word| anywhere.contains(word)))
    }
    /// Known punctuation and dangling prefixes are preserved. Unknown rules or
    /// regex failures leave a bare line rather than guessing punctuation.
    pub fn as_sentence(&self, text: &str, lang: Option<&str>) -> String {
        let text = text.trim();
        let Some(first) = text.chars().next() else {
            return String::new();
        };
        let mut text = first.to_uppercase().collect::<String>() + &text[first.len_utf8()..];
        if text
            .chars()
            .last()
            .is_some_and(|c| self.data.sentence_ends.contains(c))
            || self.dangling(&text, lang)
        {
            return text;
        }
        let Some(tag) = self.tag(lang) else {
            return text;
        };
        let data = &self.data.languages[tag];
        if data.question_openers.is_empty()
            && data.question_words_anywhere.is_empty()
            && data.question_patterns.is_empty()
        {
            return text;
        }
        for (pattern, written) in &self.written[tag] {
            let mut next = String::new();
            let mut previous = 0;
            for matched in pattern.find_iter(&text) {
                let Ok(matched) = matched else { return text };
                next.push_str(&text[previous..matched.start()]);
                next.push_str(written);
                previous = matched.end();
            }
            next.push_str(&text[previous..]);
            text = next;
        }
        match self.asks(&text, lang) {
            Ok(true) => text.push('?'),
            Ok(false) => text.push('.'),
            Err(_) => {}
        }
        text
    }
    pub fn speakable(
        &self,
        pattern: &str,
        slots: &BTreeMap<String, String>,
        lang: Option<&str>,
    ) -> String {
        let mut merged = self.language_data(lang).slot_examples;
        merged.extend(slots.iter().map(|(k, v)| (k.clone(), v.clone())));
        crate::speakable(pattern, &merged)
    }
    pub fn rank(&self, phrases: &[String], lang: Option<&str>) -> Vec<String> {
        let mut rows = phrases.to_vec();
        rows.sort_by_cached_key(|text| {
            (
                self.dangling(text, lang),
                text.contains('{'),
                std::cmp::Reverse(text.split_whitespace().count().min(8)),
                text.chars().count(),
            )
        });
        rows
    }
}
pub static DEFAULT_LISTING: LazyLock<ListingRules> = LazyLock::new(|| {
    ListingRules::new(Some(
        serde_json::from_str(include_str!("../data/listing.json"))
            .expect("valid canonical language data"),
    ))
    .expect("valid canonical question patterns")
});
pub fn as_sentence(text: &str, lang: Option<&str>) -> String {
    DEFAULT_LISTING.as_sentence(text, lang)
}
pub fn speakable_with_language(
    pattern: &str,
    slots: &BTreeMap<String, String>,
    lang: Option<&str>,
) -> String {
    DEFAULT_LISTING.speakable(pattern, slots, lang)
}
/// Optional rendering controls. The default rules are bundled and require no I/O.
#[derive(Default)]
pub struct IntentExampleOptions<'a> {
    pub speakable: bool,
    pub sentence: bool,
    pub slots: BTreeMap<String, String>,
    pub listing: Option<&'a ListingRules>,
}
