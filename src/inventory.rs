//! Presentable skill inventory and an optional, private, best-effort disk cache.
use crate::{closest_language, Result, ThalovantError, DEFAULT_LISTING};
use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::{
    cmp::Ordering,
    collections::BTreeSet,
    fmt,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

pub const INVENTORY_CACHE_VERSION: u32 = 1;
pub const INVENTORY_CACHE_TTL: Duration = Duration::from_secs(3600);
pub const HUB_SOURCE: &str = "hub";
/// Phrase pairs preserve JSON language order, including when no language is requested.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    pub id: String,
    pub name: String,
    pub skill_id: String,
    pub engine: String,
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(
        serialize_with = "serialize_phrases",
        deserialize_with = "deserialize_phrases"
    )]
    pub phrases: Vec<(String, Vec<String>)>,
}
fn serialize_phrases<S: Serializer>(
    phrases: &[(String, Vec<String>)],
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(Some(phrases.len()))?;
    for (tag, texts) in phrases {
        map.serialize_entry(tag, texts)?;
    }
    map.end()
}
fn deserialize_phrases<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<(String, Vec<String>)>, D::Error> {
    struct Phrases;
    impl<'de> Visitor<'de> for Phrases {
        type Value = Vec<(String, Vec<String>)>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a language to phrase-list object")
        }
        fn visit_map<M: MapAccess<'de>>(
            self,
            mut map: M,
        ) -> std::result::Result<Self::Value, M::Error> {
            let mut out: Self::Value = Vec::new();
            while let Some((key, value)) = map.next_entry::<String, Vec<String>>()? {
                if let Some(entry) = out.iter_mut().find(|e| e.0 == key) {
                    entry.1 = value
                } else {
                    out.push((key, value));
                }
            }
            Ok(out)
        }
    }
    deserializer.deserialize_map(Phrases)
}
impl Intent {
    pub fn examples(&self, language: Option<&str>, limit: usize) -> Vec<String> {
        let mut tags = self
            .languages
            .iter()
            .filter(|tag| self.phrases.iter().any(|p| &p.0 == *tag))
            .map(String::as_str)
            .collect::<Vec<_>>();
        for (tag, _) in &self.phrases {
            if !tags.contains(&tag.as_str()) {
                tags.push(tag);
            }
        }
        let tag = language
            .filter(|s| !s.is_empty())
            .and_then(|lang| closest_language(lang, tags.iter().copied()));
        let pool = if language.is_some_and(|s| !s.is_empty()) {
            tag.and_then(|tag| self.phrases.iter().find(|v| v.0 == tag))
                .map(|v| &v.1)
        } else {
            tags.first()
                .and_then(|tag| self.phrases.iter().find(|v| v.0 == *tag))
                .map(|v| &v.1)
        };
        let Some(pool) = pool else { return Vec::new() };
        if limit == 0 {
            return pool.clone();
        };
        DEFAULT_LISTING
            .rank(pool, language)
            .into_iter()
            .take(limit)
            .collect()
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub id: String,
    pub title: String,
    pub locales: Vec<String>,
    pub intents: Vec<Intent>,
}
impl Skill {
    pub fn declares_locales(&self) -> bool {
        !self.locales.is_empty()
    }
    pub fn speaks(&self, language: &str) -> Option<bool> {
        (!self.locales.is_empty())
            .then(|| closest_language(language, self.locales.iter().map(String::as_str)).is_some())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inventory {
    pub cache_version: u32,
    pub hub_id: String,
    pub hub_name: String,
    pub source: String,
    pub generated_at: String,
    pub skills: Vec<Skill>,
    pub notes: Vec<String>,
}
impl Inventory {
    pub fn live(&self) -> bool {
        matches!(self.source.as_str(), "hub" | "ovos-runtime")
    }
    pub fn intents(&self) -> impl Iterator<Item = &Intent> {
        self.skills.iter().flat_map(|s| &s.intents)
    }
    pub fn has_phrases(&self) -> bool {
        self.intents().any(|i| !i.phrases.is_empty())
    }
    pub fn as_json(&self) -> Result<String> {
        if self.cache_version != INVENTORY_CACHE_VERSION {
            return Err(ThalovantError::Listing(
                "not a current inventory cache".into(),
            ));
        }
        let mut value = self.clone();
        for intent in value.skills.iter_mut().flat_map(|skill| &mut skill.intents) {
            let mut tags = intent.languages.clone();
            tags.retain(|tag| intent.phrases.iter().any(|p| &p.0 == tag));
            for (tag, _) in &intent.phrases {
                if !tags.contains(tag) {
                    tags.push(tag.clone());
                }
            }
            intent.languages = tags;
        }
        Ok(serde_json::to_string(&value)?)
    }
    pub fn from_json(raw: &str) -> Result<Self> {
        let mut value: Self = serde_json::from_str(raw)?;
        for intent in value.skills.iter_mut().flat_map(|skill| &mut skill.intents) {
            let mut tags = Vec::new();
            for tag in intent
                .languages
                .iter()
                .chain(intent.phrases.iter().map(|p| &p.0))
            {
                if intent.phrases.iter().any(|p| &p.0 == tag) && !tags.contains(tag) {
                    tags.push(tag.clone());
                }
            }
            intent.languages = tags;
        }
        if value.cache_version != INVENTORY_CACHE_VERSION {
            return Err(ThalovantError::Listing(
                "not a current inventory cache".into(),
            ));
        }
        Ok(value)
    }
}
pub fn languages_present(inventory: &Inventory) -> Vec<String> {
    inventory
        .skills
        .iter()
        .flat_map(|s| {
            s.locales.iter().chain(
                s.intents
                    .iter()
                    .flat_map(|i| i.phrases.iter().map(|v| &v.0)),
            )
        })
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
pub fn friendly_title(id: &str) -> String {
    let mut name = id;
    for prefix in ["thalovant-skill-", "ovos-skill-", "skill-"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            name = rest;
            break;
        }
    }
    if let Some((head, _)) = name.rsplit_once('.') {
        name = head
    }
    let name = name.replace(['-', '_'], " ");
    let name = name.trim();
    if name.is_empty() {
        return id.to_owned();
    }
    let mut start = true;
    let mut out = String::new();
    for ch in name.chars() {
        if !ch.is_alphabetic() {
            start = true;
            out.push(ch)
        } else if start {
            let upper = ch.to_uppercase().collect::<String>();
            let mut chars = upper.chars();
            if let Some(first) = chars.next() {
                out.push(first)
            }
            for rest in chars {
                out.extend(rest.to_lowercase())
            }
            start = false
        } else {
            out.extend(ch.to_lowercase())
        }
    }
    out
}
fn tokens(name: &str) -> Vec<&str> {
    name.split(['.', '_']).filter(|p| !p.is_empty()).collect()
}
pub fn humanize(name: &str) -> String {
    tokens(name).join(" ")
}
pub fn common_affix(names: &[&str]) -> (Option<&'static str>, String) {
    let parts = names.iter().map(|n| tokens(n)).collect::<Vec<_>>();
    if parts.len() < 2 || parts.iter().any(|p| p.len() < 2) {
        return (None, String::new());
    }
    if parts.iter().all(|p| p.last() == parts[0].last()) {
        return (Some("suffix"), parts[0].last().unwrap().to_string());
    }
    if parts.iter().all(|p| p[0] == parts[0][0]) {
        return (Some("prefix"), parts[0][0].to_owned());
    }
    (None, String::new())
}
pub fn strip_affix(name: &str, kind: Option<&str>, token: &str) -> String {
    if kind.is_none() {
        return name.to_owned();
    }
    let mut parts = tokens(name);
    if kind == Some("suffix") && parts.last() == Some(&token) {
        parts.pop();
    } else if kind == Some("prefix") && parts.first() == Some(&token) {
        parts.remove(0);
    }
    let joined = parts.join(" ");
    if joined.is_empty() {
        name.to_owned()
    } else {
        joined
    }
}
pub fn compare_names(left: &str, right: &str) -> Ordering {
    fn chunks(s: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut numeric = None;
        for c in s.to_lowercase().chars() {
            let now = c.is_ascii_digit();
            if numeric != Some(now) {
                out.push(String::new());
                numeric = Some(now)
            }
            out.last_mut().unwrap().push(c)
        }
        out
    }
    let a = chunks(left);
    let b = chunks(right);
    for (x, y) in a.iter().zip(&b) {
        let nx = x.as_bytes()[0].is_ascii_digit();
        let ny = y.as_bytes()[0].is_ascii_digit();
        let compared = if nx && ny {
            let x = x.trim_start_matches('0');
            let y = y.trim_start_matches('0');
            x.len().cmp(&y.len()).then_with(|| x.cmp(y))
        } else if nx != ny {
            ny.cmp(&nx)
        } else {
            x.cmp(y)
        };
        if compared != Ordering::Equal {
            return compared;
        }
    }
    a.len().cmp(&b.len())
}
pub fn identity_host(path: &Path) -> Option<String> {
    let raw: serde_json::Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    url::Url::parse(raw.get("default_master")?.as_str()?)
        .ok()?
        .host_str()
        .map(str::to_owned)
}
pub struct InventoryCache {
    pub directory: PathBuf,
    pub ttl: Duration,
}
impl Default for InventoryCache {
    fn default() -> Self {
        let base = std::env::var_os("XDG_CACHE_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                if cfg!(windows) {
                    std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
                } else {
                    None
                }
            })
            .or_else(|| {
                std::env::var_os("HOME")
                    .or_else(|| std::env::var_os("USERPROFILE"))
                    .map(|home| PathBuf::from(home).join(".cache"))
            });
        Self {
            directory: base.map(|base| base.join("thalovant")).unwrap_or_default(),
            ttl: INVENTORY_CACHE_TTL,
        }
    }
}

impl InventoryCache {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            ttl: INVENTORY_CACHE_TTL,
        }
    }
    pub fn key(mode: &str, identity: Option<&Path>) -> String {
        let text = identity
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let host = identity
            .and_then(identity_host)
            .unwrap_or_else(|| "local".into());
        let readable = host
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "._-".contains(c) {
                    c
                } else {
                    '-'
                }
            })
            .take(40)
            .collect::<String>();
        let digest = hex::encode(Sha256::digest(format!("{mode}|{text}")));
        format!("{mode}-{readable}-{}", &digest[..8])
    }
    pub fn path(&self, key: &str) -> Result<PathBuf> {
        if self.directory.as_os_str().is_empty()
            || key.is_empty()
            || key.len() > 160
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        {
            return Err(ThalovantError::Listing(
                "invalid inventory cache key".into(),
            ));
        }
        Ok(self.directory.join(format!("intents-{key}.json")))
    }
    pub fn load(&self, key: &str) -> Option<Inventory> {
        let path = self.path(key).ok()?;
        let file = fs::File::open(&path).ok()?;
        let info = file.metadata().ok()?;
        const MAX_SIZE: u64 = 8 * 1024 * 1024;
        if info.len() > MAX_SIZE {
            return None;
        }
        if SystemTime::now()
            .duration_since(info.modified().ok()?)
            .unwrap_or_default()
            > self.ttl
        {
            return None;
        }
        let mut raw = String::new();
        file.take(MAX_SIZE + 1).read_to_string(&mut raw).ok()?;
        if raw.len() as u64 > MAX_SIZE {
            return None;
        }
        Inventory::from_json(&raw).ok()
    }
    pub fn store(&self, key: &str, inventory: &Inventory) {
        let Ok(target) = self.path(key) else { return };
        if fs::create_dir_all(&self.directory).is_err() {
            return;
        }
        let scratch = self
            .directory
            .join(format!(".intents-{}.partial", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&scratch)?;
            file.write_all(inventory.as_json()?.as_bytes())?;
            file.sync_all()?;
            drop(file);
            fs::rename(&scratch, target)?;
            Ok(())
        })();
        let _ = result;
        let _ = fs::remove_file(scratch);
    }
}
