//! Privacy-preserving activity observed at native CDP acknowledgement.
//! Coordinates describe dispatched input; DOM scrolling has no invented cursor.

use serde_json::{json, Value};
use tokio::sync::broadcast;

use super::cdp::types::CdpEvent;
use super::input::mouse_button_mask;

pub(crate) const EVENT: &str = "Ambit.inputAcknowledged";
pub(crate) const FRAME_GENERATION: &str = "ambitPageGeneration";

#[derive(Clone, Copy)]
pub(crate) enum InputSource {
    Agent,
    Human,
}

impl InputSource {
    fn name(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Human => "human",
        }
    }
}

/// Created before dispatch and committed only on an acknowledged response.
/// Keeping the observed page identity avoids relabelling a prior input after
/// navigation. No key, text, selector, command or controller ID is retained.
pub(crate) struct ActivityObservation {
    event: CdpEvent,
    sender: broadcast::Sender<CdpEvent>,
}

impl ActivityObservation {
    pub(crate) fn new(
        mut value: Value,
        session: &str,
        generation: String,
        source: InputSource,
        sender: broadcast::Sender<CdpEvent>,
    ) -> Self {
        value["pageGeneration"] = json!(generation);
        value["source"] = json!(source.name());
        Self {
            event: CdpEvent {
                method: EVENT.into(),
                params: value,
                session_id: Some(session.into()),
            },
            sender,
        }
    }

    pub(crate) fn acknowledged(mut self) {
        self.event.params["timestamp"] = json!(super::stream::timestamp_ms());
        let _ = self.sender.send(self.event);
    }
}

pub(crate) fn reset(session: &str, generation: &str) -> CdpEvent {
    CdpEvent {
        method: EVENT.into(),
        session_id: Some(session.into()),
        params: json!({
            "type": "pointer", "eventType": "reset", "pageGeneration": generation,
            "timestamp": super::stream::timestamp_ms(),
        }),
    }
}

pub(crate) fn from_command(method: &str, params: &Value) -> Option<Value> {
    match method {
        "Input.dispatchMouseEvent" => {
            let event_type = match params["type"].as_str()? {
                "mouseMoved" => "move",
                "mousePressed" => "press",
                "mouseReleased" => "release",
                "mouseWheel" => "scroll",
                _ => return None,
            };
            let x = params["x"].as_f64()?;
            let y = params["y"].as_f64()?;
            if ![x, y]
                .iter()
                .all(|value| value.is_finite() && (0.0..=32768.0).contains(value))
            {
                return None;
            }
            let buttons = params["buttons"].as_i64().unwrap_or_else(|| {
                if event_type == "press" {
                    i64::from(mouse_button_mask(
                        params["button"].as_str().unwrap_or("none"),
                    ))
                } else {
                    0
                }
            });
            let modifiers = params["modifiers"].as_i64().unwrap_or(0);
            if !(0..=31).contains(&buttons) || !(0..=15).contains(&modifiers) {
                return None;
            }
            Some(json!({ "type": "pointer", "eventType": event_type,
                "x": x, "y": y, "buttons": buttons, "modifiers": modifiers }))
        }
        "Input.insertText" if params["text"].as_str().is_some_and(|text| !text.is_empty()) => {
            Some(json!({ "type": "activity", "kind": "typing" }))
        }
        "Input.dispatchKeyEvent"
            if matches!(
                params["type"].as_str(),
                Some("char" | "keyDown" | "rawKeyDown")
            ) && (params["text"].as_str().is_some_and(|text| !text.is_empty())
                || matches!(params["key"].as_str(), Some("Backspace" | "Delete"))) =>
        {
            Some(json!({ "type": "activity", "kind": "typing" }))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projections_contain_visual_fields_only() {
        for method in ["Input.insertText", "Input.dispatchKeyEvent"] {
            let value = from_command(method, &json!({ "type": "char", "text": "private password", "key": "private", "controllerId": "secret" })).unwrap();
            assert_eq!(value, json!({ "type": "activity", "kind": "typing" }));
        }
        assert_eq!(from_command("Input.dispatchMouseEvent", &json!({ "type": "mousePressed", "x": 12, "y": 34, "button": "left", "private": "secret" })).unwrap(),
            json!({ "type": "pointer", "eventType": "press", "x": 12.0, "y": 34.0, "buttons": 1, "modifiers": 0 }));
        assert!(from_command(
            "Input.dispatchMouseEvent",
            &json!({ "type": "mouseMoved", "x": -1, "y": 0 })
        )
        .is_none());
        assert!(from_command(
            "Input.dispatchKeyEvent",
            &json!({ "type": "keyDown", "key": "Shift" })
        )
        .is_none());
    }

    #[test]
    fn unacknowledged_observation_emits_nothing() {
        let (sender, mut receiver) = broadcast::channel(4);
        let make = || {
            ActivityObservation::new(
                json!({ "type": "activity", "kind": "scrolling" }),
                "page",
                "generation".into(),
                InputSource::Agent,
                sender.clone(),
            )
        };
        drop(make());
        assert!(receiver.try_recv().is_err());
        make().acknowledged();
        let event = receiver.try_recv().unwrap();
        assert_eq!(event.params["source"], "agent");
        assert_eq!(event.params["pageGeneration"], "generation");
        assert!(event.params["timestamp"].as_u64().unwrap() > 0);
    }
}
