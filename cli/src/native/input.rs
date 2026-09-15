//! Translation of the existing stream input protocol into CDP input commands.
//! Callers own authorization, serialization, and acknowledgment policy.

use serde_json::{json, Value};
use std::collections::BTreeMap;

pub(crate) fn mouse_button_mask(button: &str) -> i32 {
    match button {
        "left" => 1,
        "right" => 2,
        "middle" => 4,
        "back" => 8,
        "forward" => 16,
        _ => 0,
    }
}

pub(crate) fn primary_button_from_mask(buttons: i32) -> &'static str {
    if buttons & 1 != 0 {
        "left"
    } else if buttons & 2 != 0 {
        "right"
    } else if buttons & 4 != 0 {
        "middle"
    } else if buttons & 8 != 0 {
        "back"
    } else if buttons & 16 != 0 {
        "forward"
    } else {
        "none"
    }
}

/// Releases for acknowledged held input, scoped to its actual page session.
/// Remove a release before sending it: a lost acknowledgment cannot cause an
/// automatic retry of that same key-up, mouse-up, or touch cancellation.
#[derive(Clone, Default)]
pub(crate) struct HeldInputs(BTreeMap<(String, String), Value>);

impl HeldInputs {
    pub(crate) fn command(
        &self,
        session_id: &str,
        kind: &str,
        event: &Value,
        pending_buttons: Option<i32>,
    ) -> Option<(&'static str, Value)> {
        let (method, mut params) = input_command(kind, event)?;
        if kind == "input_mouse" && event.get("buttons").is_none() {
            let mut buttons = pending_buttons.unwrap_or_else(|| {
                self.0
                    .iter()
                    .filter(|((session, identity), _)| {
                        session == session_id && identity.starts_with("mouse:")
                    })
                    .fold(0, |buttons, (_, release)| {
                        buttons | mouse_button_mask(release["button"].as_str().unwrap_or("none"))
                    })
            });
            let button = mouse_button_mask(params["button"].as_str().unwrap_or("none"));
            match params["type"].as_str() {
                Some("mousePressed") => buttons |= button,
                Some("mouseReleased") => buttons &= !button,
                _ => {}
            }
            params["buttons"] = json!(buttons);
        }
        if kind == "input_mouse" && params["type"] == "mouseMoved" {
            params["button"] = json!(primary_button_from_mask(
                params["buttons"].as_i64().unwrap_or(0) as i32
            ));
        }
        Some((method, params))
    }

    fn identity(event: &Value) -> Option<String> {
        match event["type"].as_str()? {
            "input_mouse" => Some(format!(
                "mouse:{}",
                event["button"].as_str().unwrap_or("none")
            )),
            "input_keyboard" => Some(format!(
                "key:{}",
                event["code"]
                    .as_str()
                    .filter(|code| !code.is_empty())
                    .or_else(|| event["key"].as_str())?
            )),
            "input_touch" => Some("touch".to_string()),
            _ => None,
        }
    }

    pub(crate) fn before_send(&mut self, session_id: &str, event: &Value) {
        if matches!(
            event["eventType"].as_str(),
            Some("mouseReleased" | "keyUp" | "touchEnd" | "touchCancel")
        ) {
            if let Some(identity) = Self::identity(event) {
                self.0.remove(&(session_id.to_string(), identity));
            }
        }
    }

    pub(crate) fn acknowledged(&mut self, session_id: &str, event: &Value) {
        self.before_send(session_id, event);
        let Some(identity) = Self::identity(event) else {
            return;
        };
        let release = match event["eventType"].as_str() {
            Some("mousePressed")
                if event["button"]
                    .as_str()
                    .is_some_and(|button| button != "none") =>
            {
                Some(
                    json!({ "type": "input_mouse", "eventType": "mouseReleased", "x": event["x"], "y": event["y"], "button": event["button"], "buttons": 0, "clickCount": 0, "modifiers": 0 }),
                )
            }
            Some("keyDown" | "rawKeyDown") => {
                let mut release =
                    json!({ "type": "input_keyboard", "eventType": "keyUp", "modifiers": 0 });
                for key in ["key", "code", "windowsVirtualKeyCode"] {
                    if let Some(value) = event.get(key) {
                        release[key] = value.clone();
                    }
                }
                Some(release)
            }
            Some("touchStart" | "touchMove") => Some(
                json!({ "type": "input_touch", "eventType": "touchCancel", "touchPoints": [], "modifiers": 0 }),
            ),
            _ => None,
        };
        if event["type"] == "input_mouse" {
            for ((session, key), release) in self.0.iter_mut() {
                if session == session_id && key.starts_with("mouse:") {
                    release["x"] = event["x"].clone();
                    release["y"] = event["y"].clone();
                }
            }
        }
        if let Some(release) = release {
            self.0.insert((session_id.to_string(), identity), release);
        }
    }

    pub(crate) fn next_release(&self) -> Option<(String, Value)> {
        self.0
            .first_key_value()
            .map(|((session, _), event)| (session.clone(), event.clone()))
    }

    pub(crate) fn accepts<'a>(
        &self,
        events: impl IntoIterator<Item = (&'a str, &'a Value)>,
    ) -> bool {
        let mut next = self.clone();
        for (session_id, event) in events {
            next.before_send(session_id, event);
            next.acknowledged(session_id, event);
            if next.0.len() > 256 {
                return false;
            }
        }
        true
    }
}

pub(crate) fn stream_event(kind: &str, params: &Value) -> Value {
    let mut event = params.clone();
    event["eventType"] = event["type"].clone();
    event["type"] = json!(kind);
    event
}

/// `Input.dispatchKeyEvent` params for a client `input_keyboard` message.
///
/// Invariant: an omitted optional string is left out, never sent as `null`.
/// CDP rejects the whole command on a null string and the caller discards the
/// error, so one null silently drops the keystroke. `""` is not a substitute:
/// `text: ""` on a printable key inserts no character.
pub(crate) fn keyboard_params(parsed: &Value) -> Value {
    let mut params = serde_json::Map::new();
    params.insert(
        "type".into(),
        json!(parsed
            .get("eventType")
            .and_then(|v| v.as_str())
            .unwrap_or("keyDown")),
    );
    for field in ["key", "code", "text"] {
        if let Some(value) = parsed.get(field).and_then(|v| v.as_str()) {
            params.insert(field.into(), json!(value));
        }
    }
    params.insert(
        "windowsVirtualKeyCode".into(),
        json!(parsed
            .get("windowsVirtualKeyCode")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)),
    );
    params.insert(
        "modifiers".into(),
        json!(parsed
            .get("modifiers")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)),
    );
    Value::Object(params)
}

pub(crate) fn input_command(kind: &str, parsed: &Value) -> Option<(&'static str, Value)> {
    match kind {
        "input_mouse" => {
            let mut params = json!({
                "type": parsed.get("eventType").and_then(Value::as_str).unwrap_or("mouseMoved"),
                "x": parsed.get("x").and_then(Value::as_f64).unwrap_or(0.0),
                "y": parsed.get("y").and_then(Value::as_f64).unwrap_or(0.0),
                "button": parsed.get("button").and_then(Value::as_str).unwrap_or("none"),
                "clickCount": parsed.get("clickCount").and_then(Value::as_i64).unwrap_or(0),
                "deltaX": parsed.get("deltaX").and_then(Value::as_f64).unwrap_or(0.0),
                "deltaY": parsed.get("deltaY").and_then(Value::as_f64).unwrap_or(0.0),
                "modifiers": parsed.get("modifiers").and_then(Value::as_i64).unwrap_or(0),
            });
            if let Some(buttons) = parsed.get("buttons").and_then(Value::as_i64) {
                params["buttons"] = json!(buttons);
            }
            Some(("Input.dispatchMouseEvent", params))
        }
        "input_keyboard" if parsed["eventType"] == "insertText" => {
            Some(("Input.insertText", json!({ "text": parsed["text"] })))
        }
        "input_keyboard" => Some(("Input.dispatchKeyEvent", keyboard_params(parsed))),
        "input_touch" => Some((
            "Input.dispatchTouchEvent",
            json!({
                "type": parsed.get("eventType").and_then(Value::as_str).unwrap_or("touchStart"),
                "touchPoints": parsed.get("touchPoints").unwrap_or(&json!([])),
                "modifiers": parsed.get("modifiers").and_then(Value::as_i64).unwrap_or(0),
            }),
        )),
        _ => None,
    }
}
