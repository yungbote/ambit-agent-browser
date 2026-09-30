//! Exact attached/captured values at the model output boundary. The registry
//! survives detach: a value already copied by a page must remain redacted.

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::Value;

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
        values.insert(urlencoding::encode(value).into_owned());
        values.insert(STANDARD.encode(value.as_bytes()));
        values.insert(hex::encode(value.as_bytes()));
        values.insert(hex::encode_upper(value.as_bytes()));
        if let Ok(escaped) = serde_json::to_string(value) {
            values.insert(escaped[1..escaped.len() - 1].to_owned());
        }
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
        // Longer secrets first: replacing a prefix must not expose a suffix.
        let mut values: Vec<&str> = values.iter().map(String::as_str).collect();
        values.sort_unstable_by_key(|text| std::cmp::Reverse(text.len()));
        scrub(value, &values);
    }

    /// Native response metadata belongs to the protocol. Only model data
    /// and error text are values copied from the page or a program.
    pub(crate) fn scrub_response(&self, op: &str, response: &mut Value) {
        if let Some(data) = response.get_mut("data") {
            self.scrub_tool_data(op, data);
        }
        if let Some(error) = response.get_mut("error") {
            self.scrub(error);
        }
    }

    pub(crate) fn scrub_tool(&self, result: &mut Value, op: &str) {
        if let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) {
            for item in content {
                if item["type"] == "text" {
                    if let Some(text) = item["text"].as_str() {
                        if let Ok(mut parsed) = serde_json::from_str::<Value>(text) {
                            if parsed.get("success").is_some() {
                                self.scrub_response(op, &mut parsed);
                            } else {
                                self.scrub_tool_data(op, &mut parsed);
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
            self.scrub_response(op, response);
        }
        if let Some(browser) = result.pointer_mut("/structuredContent/browser") {
            self.scrub_protocol(browser);
        }
    }

    fn scrub_tool_data(&self, op: &str, data: &mut Value) {
        let op = op.strip_prefix("agent_browser_").unwrap_or(op);
        if matches!(op, "eval" | "evaluate" | "run_playwright") {
            self.scrub(data);
        } else {
            // An invocation's native envelope is protocol; its output is arbitrary
            // website data and cannot borrow identity exemptions from that envelope.
            if matches!(op, "webmcp_invoke" | "webmcp_result" | "webmcp_cancel") {
                if let Some(output) = data.get_mut("output") {
                    self.scrub(output);
                }
            }
            if op == "webmcp_list" {
                if let Some(tools) = data.get_mut("tools").and_then(Value::as_array_mut) {
                    for tool in tools {
                        for field in ["inputSchema", "annotations"] {
                            if let Some(value) = tool.get_mut(field) {
                                self.scrub(value);
                            }
                        }
                    }
                }
            }
            self.scrub_protocol(data);
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
                        "pageGeneration",
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

fn text(value: &str, values: &[&str]) -> String {
    if values.contains(&value) {
        return MARKER.into();
    }
    let mut result = value.to_owned();
    for value in values {
        // Small settings such as `1`, `a` or `true` are protected as whole
        // scalar values. Substring replacement would erase ordinary prose.
        if value.len() >= 8 {
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
    fn semantic_page_generation_is_typed_identity_but_runtime_data_is_not() {
        let registry = Redaction::default();
        registry.register("generation-canary");
        let mut protocol = json!({"pageGeneration":"generation-canary", "result":{
            "type":"object","value":{"pageGeneration":"generation-canary","targetId":"generation-canary"}}});
        registry.scrub_protocol(&mut protocol);
        assert_eq!(protocol["pageGeneration"], "generation-canary");
        assert_eq!(protocol["result"]["value"]["pageGeneration"], MARKER);
        assert_eq!(protocol["result"]["value"]["targetId"], MARKER);
    }

    #[test]
    fn webmcp_output_does_not_inherit_native_identity_exemptions() {
        let registry = Redaction::default();
        let canary = "website-result-canary";
        registry.register(canary);
        let response = json!({"success":true,"data":{"invocationId":"invocation-1",
            "toolName":"report","frameId":canary,"status":"completed",
            "output":{"id":canary,"targetId":canary,"pageGeneration":canary,
                canary:{"nested":{"sessionId":canary}}}}});
        for op in [
            "agent_browser_webmcp_invoke",
            "agent_browser_webmcp_result",
            "agent_browser_webmcp_cancel",
            "webmcp_invoke",
            "webmcp_result",
            "webmcp_cancel",
        ] {
            let mut result = json!({"content":[{"type":"text","text":response.to_string()}],
            "structuredContent":{"response":response}});
            registry.scrub_tool(&mut result, op);
            let text: Value =
                serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
            for response in [&text, &result["structuredContent"]["response"]] {
                assert_eq!(response["data"]["frameId"], canary);
                assert!(!response["data"]["output"].to_string().contains(canary));
                assert_eq!(response["data"]["output"]["id"], MARKER);
                assert_eq!(
                    response["data"]["output"][MARKER]["nested"]["sessionId"],
                    MARKER
                );
            }
            let mut native = response.clone();
            registry.scrub_response(op, &mut native);
            assert_eq!(native, result["structuredContent"]["response"]);
        }
    }

    #[test]
    fn website_schemas_and_program_json_are_data_even_when_their_keys_look_like_identity() {
        let registry = Redaction::default();
        let canary = "schema-canary";
        registry.register(canary);
        let arbitrary = json!({"pageGeneration":canary,"targetId":canary,canary:canary});
        for op in [
            "evaluate",
            "eval",
            "run_playwright",
            "agent_browser_evaluate",
            "agent_browser_eval",
            "agent_browser_run_playwright",
        ] {
            let mut response = json!({"success":true,"data":arbitrary});
            registry.scrub_response(op, &mut response);
            assert!(!response.to_string().contains(canary), "{op}");
        }
        for op in ["webmcp_list", "agent_browser_webmcp_list"] {
            let mut response = json!({"success":true,"data":{"tools":[{"frameId":canary,
                "inputSchema":arbitrary,"annotations":arbitrary}]}});
            registry.scrub_response(op, &mut response);
            let tool = &response["data"]["tools"][0];
            assert_eq!(tool["frameId"], canary);
            assert!(!tool["inputSchema"].to_string().contains(canary));
            assert!(!tool["annotations"].to_string().contains(canary));
        }
    }

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
