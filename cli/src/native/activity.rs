//! Privacy-preserving activity observed at native CDP acknowledgement.
//! Coordinates describe dispatched input; DOM scrolling has no invented cursor.

use serde_json::{json, Value};
use tokio::sync::broadcast;

use super::cdp::types::CdpEvent;
use super::input::mouse_button_mask;

pub(crate) const EVENT: &str = "Ambit.inputAcknowledged";
pub(crate) const FRAME_GENERATION: &str = "ambitPageGeneration";
pub(crate) const POINTER_BINDING: &str = "__ambitWindowPointer";
pub(crate) const POINTER_WORLD: &str = "ambit-window-pointer";

/// Geometry measured by one trusted renderer event in an isolated realm.
/// It is input to the owned display, never a replacement input acknowledgement.
#[derive(Clone, Debug)]
pub(crate) struct NativePointerFrame {
    pub session: String,
    pub context: i64,
    pub generation: String,
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Debug)]
pub(crate) struct NativePointer {
    pub context: i64,
    pub page_generation: String,
    pub client_x: f64,
    pub client_y: f64,
    pub screen_x: f64,
    pub screen_y: f64,
    pub geometry: Value,
    pub source_page: Option<NativePointerFrame>,
}

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
///
/// A renderer pointer event joins the observation only when its private
/// token, session, event type and client coordinates all match the command;
/// an event from an earlier command therefore cannot describe a later one.
pub(crate) struct ActivityObservation {
    event: CdpEvent,
    sender: broadcast::Sender<CdpEvent>,
    /// The owner's session and page generation for the same target, captured
    /// at dispatch. They replace this connection's identities only when the
    /// acknowledged observation is published.
    published_as: Option<(String, String)>,
    /// The command expects a trusted renderer pointer event to join it.
    native_tracked: bool,
    native_context: Option<i64>,
    native_geometry: Option<Value>,
    native_source_page: Option<NativePointerFrame>,
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
            published_as: None,
            native_tracked: false,
            native_context: None,
            native_geometry: None,
            native_source_page: None,
        }
    }

    /// Input dispatched through an operation-local connection is observed by
    /// viewers of the owning connection's page. Native measurement still
    /// joins renderer events of the dispatching session.
    pub(crate) fn published_as(
        mut self,
        sender: broadcast::Sender<CdpEvent>,
        session: String,
        generation: String,
    ) -> Self {
        self.sender = sender;
        self.published_as = Some((session, generation));
        self
    }

    pub(crate) fn acknowledged(mut self) {
        self.event.params["timestamp"] = json!(super::stream::timestamp_ms());
        if let Some((session, generation)) = self.published_as.take() {
            self.event.session_id = Some(session);
            self.event.params["pageGeneration"] = json!(generation);
        }
        let _ = self.sender.send(self.event.clone());
    }

    pub(crate) fn track_native(&mut self) {
        self.native_tracked = true;
    }

    pub(crate) fn set_native_context(&mut self, context: i64) {
        self.native_context = Some(context);
    }

    pub(crate) fn set_native_frame(&mut self, source: NativePointerFrame, geometry: Value) {
        self.native_source_page = Some(source);
        self.native_geometry = Some(geometry);
    }

    pub(crate) fn native_pointer(&self) -> Option<NativePointer> {
        Some(NativePointer {
            context: self.native_context?,
            page_generation: self.event.params["pageGeneration"].as_str()?.into(),
            client_x: self.event.params["x"].as_f64()?,
            client_y: self.event.params["y"].as_f64()?,
            screen_x: self.event.params["screenX"].as_f64()?,
            screen_y: self.event.params["screenY"].as_f64()?,
            geometry: self.native_geometry.clone()?,
            source_page: self.native_source_page.clone(),
        })
    }

    pub(crate) fn awaits_native_event(&self) -> bool {
        self.native_tracked && self.event.params.get("screenX").is_none()
    }

    /// The browser refused the command; nothing was observed.
    pub(crate) fn refused(self) {}

    pub(crate) fn native_event(&mut self, session: &str, payload: &Value) {
        if !self.native_tracked
            || self
                .native_source_page
                .as_ref()
                .map(|source| source.session.as_str())
                .or(self.event.session_id.as_deref())
                != Some(session)
            || self.event.params["eventType"] != payload["eventType"]
        {
            return;
        }
        for (command, actual) in [("x", "clientX"), ("y", "clientY")] {
            let (Some(command), Some(actual)) = (
                self.native_source_page
                    .as_ref()
                    .map(|source| if command == "x" { source.x } else { source.y })
                    .or_else(|| self.event.params[command].as_f64()),
                payload[actual].as_f64(),
            ) else {
                return;
            };
            if !actual.is_finite() || (command - actual).abs() > 1.0 {
                return;
            }
        }
        for field in ["screenX", "screenY"] {
            if !payload[field]
                .as_f64()
                .is_some_and(|value| value.is_finite() && (-32768.0..=32768.0).contains(&value))
            {
                return;
            }
        }
        self.event.params["screenX"] = payload["screenX"].clone();
        self.event.params["screenY"] = payload["screenY"].clone();
        if self.native_source_page.is_none()
            && payload["geometry"]["scale"]
                .as_f64()
                .is_some_and(|scale| scale.is_finite() && scale > 0.0)
        {
            self.native_geometry = Some(payload["geometry"].clone());
        }
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
    fn delegated_observation_is_published_as_the_owner_after_acknowledgement() {
        let (local, mut local_events) = broadcast::channel(4);
        let (owner, mut owner_events) = broadcast::channel(4);
        let observation = ActivityObservation::new(
            json!({ "type": "pointer", "eventType": "move", "x": 5.0, "y": 8.0 }),
            "operation-session",
            "operation-generation".into(),
            InputSource::Agent,
            local,
        )
        .published_as(owner, "owner-session".into(), "owner-generation".into());
        // Until acknowledgement the observation keeps the dispatching
        // connection's identities, which native measurement joins.
        assert_eq!(
            observation.event.session_id.as_deref(),
            Some("operation-session")
        );
        observation.acknowledged();
        assert!(local_events.try_recv().is_err());
        let event = owner_events.try_recv().unwrap();
        assert_eq!(event.session_id.as_deref(), Some("owner-session"));
        assert_eq!(event.params["pageGeneration"], "owner-generation");
        assert_eq!(event.params["source"], "agent");
        assert_eq!(event.params["x"], 5.0);
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
