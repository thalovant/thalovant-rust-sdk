use crate::events::{Data, Event, Reply};
use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq)]
pub struct DisplayItem {
    pub kind: String,
    pub text: Option<String>,
    pub data: Option<Value>,
    pub title: Option<String>,
    pub payload: Option<String>,
    pub url: Option<String>,
    pub silent: bool,
}

pub fn strip_ssml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

pub fn rich_media_from_data(data: &Data) -> Map<String, Value> {
    for key in ["rich_media_data", "rich_media", "display"] {
        if let Some(media) = data.get(key).and_then(coerce_mapping) {
            if !media.is_empty() {
                return media;
            }
        }
    }
    let mut direct = Map::new();
    for key in [
        "table",
        "attachment",
        "attachments",
        "quick_replies",
        "buttons",
        "image",
        "images",
    ] {
        if let Some(value) = data.get(key) {
            direct.insert(key.to_string(), value.clone());
        }
    }
    direct
}

pub fn display_items_from_event_data(
    data: &Data,
    event_name: Option<&str>,
    max_text_chars: Option<usize>,
) -> Vec<DisplayItem> {
    let mut items = Vec::new();
    if let Some(text) = text_from_data(data) {
        for chunk in chunks(&strip_ssml(&text), max_text_chars) {
            items.push(DisplayItem {
                kind: "text".to_string(),
                text: Some(chunk),
                data: None,
                title: None,
                payload: None,
                url: None,
                silent: data.get("silent").and_then(Value::as_bool).unwrap_or(false)
                    || event_name == Some("write"),
            });
        }
    }
    let media = rich_media_from_data(data);
    if let Some(table) = media.get("table").map(coerce_json) {
        items.push(DisplayItem {
            kind: "table".to_string(),
            text: None,
            data: Some(table),
            title: None,
            payload: None,
            url: None,
            silent: false,
        });
    }
    for attachment in attachments(&media) {
        let payload = attachment
            .get("payload")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let url = string_value(
            payload
                .get("src")
                .or_else(|| payload.get("url"))
                .or_else(|| attachment.get("src"))
                .or_else(|| attachment.get("url")),
        );
        let kind = if attachment.get("type").and_then(Value::as_str) == Some("image") {
            "image"
        } else {
            "attachment"
        };
        items.push(DisplayItem {
            kind: kind.to_string(),
            text: None,
            data: Some(Value::Object(attachment.clone())),
            title: string_value(attachment.get("title")),
            payload: None,
            url,
            silent: false,
        });
    }
    let choices = choices(media.get("quick_replies").or_else(|| media.get("buttons")));
    if !choices.is_empty() {
        items.push(DisplayItem {
            kind: "choices".to_string(),
            text: None,
            data: Some(Value::Array(choices)),
            title: None,
            payload: None,
            url: None,
            silent: false,
        });
    }
    items
}

impl Event {
    pub fn display_text(&self) -> String {
        strip_ssml(&self.text())
    }

    pub fn rich_media(&self) -> Map<String, Value> {
        rich_media_from_data(&self.data)
    }

    pub fn display_items(&self, max_text_chars: Option<usize>) -> Vec<DisplayItem> {
        display_items_from_event_data(&self.data, Some(&self.name), max_text_chars)
    }
}

impl Reply {
    pub fn display_text(&self) -> String {
        strip_ssml(&self.text)
    }

    pub fn display_items(&self, max_text_chars: Option<usize>) -> Vec<DisplayItem> {
        let mut items = Vec::new();
        for event in &self.events {
            items.extend(event.display_items(max_text_chars));
        }
        if items.is_empty() && !self.text.is_empty() {
            items.push(DisplayItem {
                kind: "text".to_string(),
                text: Some(self.display_text()),
                data: None,
                title: None,
                payload: None,
                url: None,
                silent: false,
            });
        }
        items
    }
}

fn text_from_data(data: &Data) -> Option<String> {
    data.get("utterance")
        .and_then(Value::as_str)
        .or_else(|| data.get("text").and_then(Value::as_str))
        .map(str::to_string)
        .or_else(|| match data.get("utterances") {
            Some(Value::String(value)) => Some(value.clone()),
            Some(Value::Array(values)) => Some(
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            _ => None,
        })
}

fn coerce_json(value: &Value) -> Value {
    match value {
        Value::String(raw) => serde_json::from_str(raw).unwrap_or_else(|_| value.clone()),
        _ => value.clone(),
    }
}

fn coerce_mapping(value: &Value) -> Option<Map<String, Value>> {
    coerce_json(value).as_object().cloned()
}

fn attachments(media: &Map<String, Value>) -> Vec<Map<String, Value>> {
    let raw = media.get("attachments").or_else(|| media.get("attachment"));
    match raw.map(coerce_json) {
        Some(Value::Object(map)) => vec![map],
        Some(Value::Array(values)) => values
            .into_iter()
            .filter_map(|value| value.as_object().cloned())
            .collect(),
        _ => Vec::new(),
    }
}

fn choices(raw: Option<&Value>) -> Vec<Value> {
    let raw = raw.map(coerce_json);
    let values = match raw {
        Some(Value::Array(values)) => values,
        Some(value) => vec![value],
        None => Vec::new(),
    };
    values
        .into_iter()
        .filter_map(|value| match value {
            Value::String(text) => {
                Some(serde_json::json!({"title": text, "payload": text, "data": text}))
            }
            Value::Object(map) => {
                let title = string_value(
                    map.get("title")
                        .or_else(|| map.get("label"))
                        .or_else(|| map.get("text")),
                )
                .unwrap_or_default();
                let payload = string_value(map.get("payload").or_else(|| map.get("value")))
                    .unwrap_or_else(|| title.clone());
                Some(serde_json::json!({"title": title, "payload": payload, "data": map}))
            }
            _ => None,
        })
        .collect()
}

fn string_value(value: Option<&Value>) -> Option<String> {
    value.and_then(|value| {
        value
            .as_str()
            .map(str::to_string)
            .or_else(|| Some(value.to_string()))
    })
}

fn chunks(text: &str, max_chars: Option<usize>) -> Vec<String> {
    let Some(max_chars) = max_chars else {
        return vec![text.to_string()];
    };
    if text.len() <= max_chars {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut remaining = text.trim();
    while remaining.len() > max_chars {
        let index = remaining[..max_chars].rfind(' ').unwrap_or(max_chars);
        out.push(remaining[..index].trim().to_string());
        remaining = remaining[index..].trim();
    }
    if !remaining.is_empty() {
        out.push(remaining.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_display_items() {
        let mut data = Data::new();
        data.insert(
            "utterance".to_string(),
            Value::String("<speak>Hello</speak>".to_string()),
        );
        data.insert(
            "rich_media_data".to_string(),
            json!({
                "table": "[{\"name\":\"part\",\"status\":\"ok\"}]",
                "attachment": {"type": "image", "payload": {"src": "https://example.com/image.png"}},
                "quick_replies": [{"title": "Continue", "payload": "/continue"}]
            })
            .to_string()
            .into(),
        );
        let items = display_items_from_event_data(&data, Some("speak"), None);
        assert_eq!(
            items
                .iter()
                .map(|item| item.kind.as_str())
                .collect::<Vec<_>>(),
            vec!["text", "table", "image", "choices"]
        );
        assert_eq!(items[0].text.as_deref(), Some("Hello"));
        assert_eq!(
            items[2].url.as_deref(),
            Some("https://example.com/image.png")
        );
    }
}
