//! Exact attached/captured values at the model output boundary. The registry
//! survives detach: a value already copied by a page must remain redacted.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub(crate) const MARKER: &str = "[redacted credential]";

#[derive(Clone, Default)]
pub(crate) struct Redaction(Arc<RwLock<BTreeSet<String>>>);

impl Redaction {
    pub(crate) fn register(&self, value: &str) {
        if value.is_empty() {
            return;
        }
        let mut values = self.0.write().unwrap_or_else(|error| error.into_inner());
        values.insert(value.to_owned());
    }

    /// Keys may carry stored values too. Never print an input in an error.
    pub(crate) fn register_tree(&self, value: &Value) {
        match value {
            Value::String(text) => self.register(text),
            Value::Number(number) => self.register(&number.to_string()),
            Value::Bool(boolean) => self.register(&boolean.to_string()),
            Value::Array(values) => values.iter().for_each(|value| self.register_tree(value)),
            Value::Object(values) => {
                // Structured-clone tags describe encoding, not stored values.
                // Registering `object` or `$` would damage unrelated replies.
                match values.get("$").and_then(Value::as_str) {
                    Some("object" | "map") => {
                        if let Some(entries) = values.get("entries").and_then(Value::as_array) {
                            for entry in entries {
                                self.register_tree(entry);
                            }
                        }
                    }
                    Some("set") => {
                        if let Some(value) = values.get("values") {
                            self.register_tree(value);
                        }
                    }
                    Some("undefined") => {}
                    Some(_) => {
                        for (key, value) in values {
                            if !["$", "kind", "type"].contains(&key.as_str()) {
                                self.register_tree(value);
                            }
                        }
                    }
                    None => {
                        for (key, value) in values {
                            self.register(key);
                            self.register_tree(value);
                        }
                    }
                }
            }
            Value::Null => {}
        }
    }

    pub(crate) fn scrub(&self, value: &mut Value) {
        let values = self.0.read().unwrap_or_else(|error| error.into_inner());
        let longest = longest_value(value);
        let mut unique = BTreeSet::new();
        let mut variants = Vec::new();
        for raw in values.iter().filter(|raw| raw.len() <= longest) {
            for encoding in Encoding::ALL {
                let encoded = encoding.encode(raw);
                if encoded.len() <= longest {
                    let digest: [u8; 32] = Sha256::digest(encoded.as_bytes()).into();
                    if unique.insert((encoded.len(), digest)) {
                        variants.push((encoded.len(), raw.as_str(), encoding));
                    }
                }
            }
        }
        // Keep the existing longest-first rule without retaining every
        // expanded encoding beside the full browser state. A value larger
        // than every output leaf cannot occur in that output in any encoding.
        variants.sort_unstable_by_key(|(bytes, _, _)| std::cmp::Reverse(*bytes));
        for (_, raw, encoding) in variants {
            scrub(value, &[encoding.encode(raw).as_ref()]);
        }
    }

    /// Native response metadata belongs to the protocol. Only model data
    /// and error text are values copied from the page or a program.
    pub(crate) fn scrub_response(&self, response: &mut Value) {
        if let Some(data) = response.get_mut("data") {
            self.scrub(data);
        }
        if let Some(error) = response.get_mut("error") {
            self.scrub(error);
        }
    }

    pub(crate) fn scrub_browser_response(&self, response: &mut Value) {
        if let Some(data) = response.get_mut("data") {
            self.scrub_protocol(data);
        }
        if let Some(error) = response.get_mut("error") {
            self.scrub(error);
        }
    }

    pub(crate) fn scrub_tool(&self, result: &mut Value, arbitrary_data: bool) {
        if let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) {
            for item in content {
                if item["type"] == "text" {
                    if let Some(text) = item["text"].as_str() {
                        if let Ok(mut parsed) = serde_json::from_str::<Value>(text) {
                            if parsed.get("success").is_some() {
                                if arbitrary_data {
                                    self.scrub_response(&mut parsed);
                                } else {
                                    self.scrub_browser_response(&mut parsed);
                                }
                            } else if arbitrary_data {
                                self.scrub(&mut parsed);
                            } else {
                                self.scrub_protocol(&mut parsed);
                            }
                            item["text"] = Value::String(parsed.to_string());
                        } else {
                            self.scrub(&mut item["text"]);
                        }
                    }
                }
            }
        }
        if let Some(response) = result.pointer_mut("/structuredContent/response") {
            if arbitrary_data {
                self.scrub_response(response);
            } else {
                self.scrub_browser_response(response);
            }
        }
        if let Some(browser) = result.pointer_mut("/structuredContent/browser") {
            self.scrub_protocol(browser);
        }
    }

    /// CDP and observation identity is not site data. Runtime's by-value
    /// payload is site data, including arbitrary nested JSON and keys.
    pub(crate) fn scrub_protocol(&self, value: &mut Value) {
        match value {
            Value::Array(values) => {
                for value in values {
                    self.scrub_protocol(value);
                }
            }
            Value::Object(values) => {
                let remote = values
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| {
                        [
                            "string",
                            "object",
                            "number",
                            "boolean",
                            "bigint",
                            "undefined",
                            "symbol",
                            "function",
                        ]
                        .contains(&kind)
                    });
                for (key, value) in values {
                    if remote && key == "value" {
                        self.scrub(value);
                    } else if ![
                        "id",
                        "sessionId",
                        "requestId",
                        "executionContextId",
                        "contextId",
                        "objectId",
                        "backendNodeId",
                        "nodeId",
                        "targetId",
                        "frameId",
                        "loaderId",
                        "channel",
                        "namespace",
                        "session",
                        "actionId",
                        "ownerGeneration",
                        "driverArtifactDigest",
                        "snapshotDigest",
                        "snapshotRef",
                        "itemRef",
                    ]
                    .contains(&key.as_str())
                    {
                        self.scrub_protocol(value);
                    }
                }
            }
            Value::String(_) => self.scrub(value),
            _ => {}
        }
    }
}

#[derive(Clone, Copy)]
enum Encoding {
    Plain,
    Url,
    Base64,
    Hex,
    UpperHex,
    Json,
}

impl Encoding {
    const ALL: [Self; 6] = [
        Self::Plain,
        Self::Url,
        Self::Base64,
        Self::Hex,
        Self::UpperHex,
        Self::Json,
    ];

    fn encode(self, value: &str) -> Cow<'_, str> {
        match self {
            Self::Plain => Cow::Borrowed(value),
            Self::Url => urlencoding::encode(value),
            Self::Base64 => Cow::Owned(STANDARD.encode(value.as_bytes())),
            Self::Hex => Cow::Owned(hex::encode(value.as_bytes())),
            Self::UpperHex => Cow::Owned(hex::encode_upper(value.as_bytes())),
            Self::Json => {
                let mut escaped = serde_json::to_string(value).expect("a string is JSON encodable");
                escaped.pop();
                escaped.remove(0);
                Cow::Owned(escaped)
            }
        }
    }
}

fn longest_value(value: &Value) -> usize {
    match value {
        Value::String(value) => value.len(),
        Value::Number(_) | Value::Bool(_) => value.to_string().len(),
        Value::Array(values) => values.iter().map(longest_value).max().unwrap_or(0),
        Value::Object(values) => values
            .iter()
            .map(|(key, value)| key.len().max(longest_value(value)))
            .max()
            .unwrap_or(0),
        Value::Null => 0,
    }
}

fn text(value: &str, values: &[&str]) -> String {
    if values.contains(&value) {
        return MARKER.into();
    }
    let mut result = value.to_owned();
    for value in values {
        // Small settings such as `1`, `a` or `true` are protected as whole
        // scalar values. Substring replacement would erase ordinary prose.
        if value.len() >= 8 && result.contains(value) {
            result = result.replace(value, MARKER);
        }
    }
    result
}

fn scrub(value: &mut Value, values: &[&str]) {
    match value {
        Value::String(value) => *value = text(value, values),
        Value::Number(_) | Value::Bool(_) => {
            if values.contains(&value.to_string().as_str()) {
                *value = Value::String(MARKER.into());
            }
        }
        Value::Array(values_in) => values_in.iter_mut().for_each(|value| scrub(value, values)),
        Value::Object(values_in) => {
            let original = std::mem::take(values_in);
            for (key, mut value) in original {
                scrub(&mut value, values);
                values_in.insert(text(&key, values), value);
            }
        }
        Value::Null => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exact_values_and_their_common_encodings_leave_no_model_value() {
        let registry = Redaction::default();
        let canary = "fixture-secret /+?";
        registry.register(canary);
        let mut output = json!({ canary: [canary, urlencoding::encode(canary),
            STANDARD.encode(canary), hex::encode(canary), hex::encode_upper(canary)],
            "safe": "a useful answer" });
        registry.scrub(&mut output);
        assert_eq!(output[MARKER], json!(vec![MARKER; 5]));
        assert_eq!(output["safe"], "a useful answer");
    }

    #[test]
    fn custody_registry_retains_one_copy_of_a_large_value() {
        let registry = Redaction::default();
        let canary = "nosecret-capacity-registry-".to_owned() + &"x".repeat(5 * 1024 * 1024);
        registry.register(&canary);
        registry.register(&canary);
        let retained = registry
            .0
            .read()
            .unwrap()
            .iter()
            .map(String::len)
            .sum::<usize>();
        assert_eq!(
            retained,
            canary.len(),
            "encoding variants are transient output work, not permanent state copies"
        );
        let started = std::time::Instant::now();
        let mut ordinary = json!({"text":"A useful ordinary browser result."});
        registry.scrub(&mut ordinary);
        eprintln!(
            "registry_receipt={{\"inputBytes\":{},\"retainedBytes\":{},\"ordinaryScrubUs\":{}}}",
            canary.len(),
            retained,
            started.elapsed().as_micros()
        );
        assert_eq!(ordinary["text"], "A useful ordinary browser result.");
    }

    #[test]
    fn lazy_encodings_preserve_substrings_overlap_and_json_escaping() {
        let registry = Redaction::default();
        registry.register("nosecret-prefix");
        registry.register("nosecret-prefix-longer");
        registry.register("nosecret-\"quoted\"\n雪");
        let mut output = json!([
            "before nosecret-prefix-longer after",
            "before nosecret-prefix after",
            "nosecret-\\\"quoted\\\"\\n雪",
            STANDARD.encode("nosecret-prefix-longer"),
            "safe answer",
        ]);
        registry.scrub(&mut output);
        assert_eq!(
            output,
            json!([
                format!("before {MARKER} after"),
                format!("before {MARKER} after"),
                MARKER,
                MARKER,
                "safe answer",
            ])
        );
    }

    #[test]
    fn captured_scalars_are_redacted_and_empty_values_do_not_erase_everything() {
        let registry = Redaction::default();
        registry.register("");
        registry.register_tree(&json!([42, true, null]));
        let mut output = json!([42, true, false, "ordinary text"]);
        registry.scrub(&mut output);
        assert_eq!(output, json!([MARKER, MARKER, false, "ordinary text"]));
    }

    #[test]
    fn short_storage_values_do_not_destroy_neighboring_text_or_protocol_ids() {
        let registry = Redaction::default();
        registry.register("a");
        registry.register("true");
        let mut value = json!({"prose":"a useful answer", "stored":"a", "boolean":true});
        registry.scrub(&mut value);
        assert_eq!(value["prose"], "a useful answer");
        assert_eq!(value["stored"], MARKER);
        assert_eq!(value["boolean"], MARKER);
        let mut protocol = json!({"id":42,"sessionId":"a","backendNodeId":42,"success":true,
            "result":{"type":"boolean","value":true},"title":"a useful answer"});
        registry.register("42");
        registry.scrub_protocol(&mut protocol);
        assert_eq!(protocol["id"], 42);
        assert_eq!(protocol["sessionId"], "a");
        assert_eq!(protocol["backendNodeId"], 42);
        assert_eq!(protocol["success"], true);
        assert_eq!(protocol["result"]["value"], MARKER);
        assert_eq!(protocol["title"], "a useful answer");
    }
}
