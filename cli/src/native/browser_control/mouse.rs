//! Agent mouse transport for the browser's existing owned display.
//!
//! CDP performs the ordinary pre-hover used to resolve a page point. Its
//! trusted renderer event measures the native position, including Chrome's
//! chrome and page zoom. Only the display helper sends buttons. A mapping
//! lives for one command or an explicitly held gesture, never across takeover.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{json, Value};

use crate::native::activity::{InputSource, NativePointer};
use crate::native::cdp::client::CdpClient;
use crate::native::display::{DisplayClient, Surface};

const GEOMETRY: &str = "({scale:devicePixelRatio*(visualViewport?.scale??1),width:innerWidth,height:innerHeight,offsetX:visualViewport?.offsetLeft??0,offsetY:visualViewport?.offsetTop??0})";

fn outcome_unknown(message: impl AsRef<str>) -> String {
    let message = message.as_ref();
    if message.starts_with("browser_control_outcome_unknown: ") {
        message.into()
    } else {
        format!("browser_control_outcome_unknown: {message}")
    }
}

const UNMEASURED: &str =
    "The browser did not report the native mouse position. No native button was sent.";

/// A CDP move whose trusted renderer event, when this page's own document
/// receives it, measures where page coordinates are on the native display.
async fn pre_hover(
    client: &CdpClient,
    session: &str,
    x: f64,
    y: f64,
) -> Result<Option<NativePointer>, String> {
    let mut command = client
        .enqueue_command(
            "Input.dispatchMouseEvent",
            Some(json!({
                "type": "mouseMoved", "x": x, "y": y, "buttons": 0,
            })),
            Some(session),
        )
        .await?;
    let response = tokio::time::timeout(Duration::from_secs(5), command.acknowledgment())
        .await
        .map_err(|_| {
            "The browser did not acknowledge the pre-hover; inspect it before retrying."
        })??;
    if let Some(error) = response.error {
        return Err(format!("Browser pre-hover failed: {error}"));
    }
    Ok(command.native_pointer())
}

/// Chooses the visible point of this page's own document nearest `(x, y)`.
/// A point over an iframe, object or embed delivers its events to that
/// document instead, so it cannot measure this page.
fn calibration_probe(x: f64, y: f64) -> String {
    format!(
        r#"((x, y) => {{
            const width = innerWidth, height = innerHeight;
            if (!(x >= 0 && y >= 0 && x < width && y < height)) return {{ width, height, inside: false }};
            const own = (px, py) => {{
                const element = document.elementFromPoint(px, py);
                return !!element && !/^(IFRAME|FRAME|OBJECT|EMBED)$/.test(element.tagName);
            }};
            let best = null;
            for (let i = 0; i < 9; i++) for (let j = 0; j < 9; j++) {{
                const px = Math.min(width - 1, Math.round((i + 0.5) * width / 9));
                const py = Math.min(height - 1, Math.round((j + 0.5) * height / 9));
                const distance = (px - x) ** 2 + (py - y) ** 2;
                if ((!best || distance < best.distance) && own(px, py)) best = {{ x: px, y: py, distance }};
            }}
            return {{ width, height, inside: true, calibration: best && {{ x: best.x, y: best.y }} }};
        }})({x}, {y})"#
    )
}

/// The page-to-native mapping is one affine relation for the whole page, so a
/// point whose events reach a child frame is measured at another visible
/// point of the page. A point outside the page is refused before any input.
async fn calibrate(
    client: &CdpClient,
    session: &str,
    x: f64,
    y: f64,
) -> Result<NativePointer, String> {
    let read = client
        .send_command(
            "Runtime.evaluate",
            Some(json!({ "expression": calibration_probe(x, y), "returnByValue": true })),
            Some(session),
        )
        .await?;
    let view = &read["result"]["value"];
    if read.get("exceptionDetails").is_some() || !view.is_object() {
        return Err(UNMEASURED.into());
    }
    if view["inside"] != true {
        return Err(format!(
            "The point ({x}, {y}) is outside the visible page ({}x{} CSS pixels). No native input was sent; scroll it into view or choose a visible point.",
            view["width"], view["height"]
        ));
    }
    let (Some(calibration_x), Some(calibration_y)) = (
        view["calibration"]["x"].as_f64(),
        view["calibration"]["y"].as_f64(),
    ) else {
        return Err(UNMEASURED.into());
    };
    pre_hover(client, session, calibration_x, calibration_y)
        .await?
        .ok_or_else(|| UNMEASURED.into())
}

#[derive(Clone)]
struct Mapping {
    session: String,
    pointer: NativePointer,
    surface: String,
    window: (u32, i32, i32, u32, u32),
}

impl Mapping {
    fn point(&self, x: f64, y: f64, surface: &Surface) -> Result<(f64, f64), String> {
        let scale = self.pointer.geometry["scale"]
            .as_f64()
            .ok_or("Missing native pointer scale")?;
        let factor = f64::from(surface.device_scale_factor);
        let x = (self.pointer.screen_x * factor + (x - self.pointer.client_x) * scale).round();
        let y = (self.pointer.screen_y * factor + (y - self.pointer.client_y) * scale).round();
        if !x.is_finite()
            || !y.is_finite()
            || x < 0.0
            || y < 0.0
            || x >= f64::from(surface.width)
            || y >= f64::from(surface.height)
        {
            return Err("The mouse position is outside the current browser window.".into());
        }
        Ok((x, y))
    }
}

#[derive(Default)]
pub(super) struct NativeMouse {
    mappings: HashMap<String, Mapping>,
    buttons: i64,
    modifiers: i64,
    /// Set before the helper can perform input. Cancellation cannot clear it.
    unknown: bool,
}

impl NativeMouse {
    pub(super) fn begin_command(&mut self) {
        if self.buttons == 0 {
            self.mappings.clear();
        }
    }

    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(super) fn require_known(&self) -> Result<(), String> {
        if self.unknown {
            return Err("browser_control_outcome_unknown: Native input release is unconfirmed. Close the browser before sending more mouse input; do not replay the original action.".into());
        }
        Ok(())
    }

    pub(super) fn needs_release(&self) -> bool {
        self.buttons != 0 || self.modifiers != 0 || self.unknown
    }

    /// Cleanup has its own real acknowledgement. It never turns an earlier
    /// action with an unknown outcome into an acknowledged successful action.
    pub(super) async fn release(&mut self, display: &DisplayClient) -> Result<(), String> {
        self.unknown = true;
        display.reset().await.map_err(|error| {
            outcome_unknown(format!("Native input release is unconfirmed ({error}). Close the browser before sending more mouse input."))
        })?;
        self.reset();
        Ok(())
    }

    async fn finish(
        &mut self,
        result: Result<bool, String>,
        display: &DisplayClient,
    ) -> Result<bool, String> {
        if let Err(original) = result {
            if self.needs_release() {
                let original = outcome_unknown(original);
                if let Err(cleanup) = self.release(display).await {
                    return Err(format!("{original} {cleanup}"));
                }
                return Err(original);
            }
            return Err(original);
        }
        result
    }

    async fn current(
        &self,
        mapping: &Mapping,
        client: &CdpClient,
        display: &DisplayClient,
    ) -> Result<(), String> {
        let session = mapping.session.as_str();
        if !display.ready()
            || mapping.surface != display.surface().generation
            || mapping.pointer.page_generation != client.page_generation(session)
            || mapping
                .pointer
                .source_page
                .as_ref()
                .is_some_and(|source| client.page_generation(&source.session) != source.generation)
        {
            return Err(
                "The browser changed during this mouse gesture. Inspect it before continuing."
                    .into(),
            );
        }
        let info = display.info().await.map_err(|error| error.to_string())?;
        let window = info
            .active_window()
            .ok_or("The active browser window is unavailable")?;
        if mapping.window != (window.id, window.x, window.y, window.width, window.height) {
            return Err("The browser window moved during this mouse gesture.".into());
        }
        let read = client.send_command("Runtime.evaluate", Some(json!({
            "expression": GEOMETRY, "contextId": mapping.pointer.context, "returnByValue": true,
        })), Some(session)).await?;
        if read.get("exceptionDetails").is_some()
            || read["result"]["value"] != mapping.pointer.geometry
        {
            return Err("The page layout changed during this mouse gesture.".into());
        }
        if let Some(source) = mapping.pointer.source_page.as_ref() {
            let alive = client.send_command("Runtime.evaluate", Some(json!({"expression":"true","contextId":source.context,"returnByValue":true})), Some(&source.session)).await?;
            if alive["result"]["value"] != true {
                return Err("The pointer frame changed during this mouse gesture.".into());
            }
        }
        Ok(())
    }

    async fn prepare(
        &mut self,
        client: &CdpClient,
        session: &str,
        display: &DisplayClient,
        x: f64,
        y: f64,
    ) -> Result<Mapping, String> {
        self.require_known()?;
        if let Some(mapping) = self.mappings.get(session) {
            self.current(mapping, client, display).await?;
            return Ok(mapping.clone());
        }
        if self.buttons != 0 {
            return Err("The held mouse gesture has no observed position in this page. Release the mouse before starting a new gesture.".into());
        }
        let surface = display.surface();
        if !display.ready() {
            return Err("The browser window is applying its layout.".into());
        }
        let info = display.info().await.map_err(|error| error.to_string())?;
        let window = info
            .active_window()
            .ok_or("The active browser window is unavailable")?;
        let window = (window.id, window.x, window.y, window.width, window.height);
        let pointer = match pre_hover(client, session, x, y).await? {
            Some(pointer) => pointer,
            None => calibrate(client, session, x, y).await?,
        };
        let mapping = Mapping {
            session: session.into(),
            pointer,
            surface: surface.generation,
            window,
        };
        self.current(&mapping, client, display).await?;
        self.mappings.insert(session.into(), mapping.clone());
        Ok(mapping)
    }

    pub(super) async fn dispatch(
        &mut self,
        params: Value,
        client: &CdpClient,
        session: &str,
        display: &DisplayClient,
        dialog_sessions: &[&str],
        allow_release_cleanup: bool,
    ) -> Result<bool, String> {
        // Do not turn another command into a retry of an unconfirmed reset.
        self.require_known()?;
        let coordinates = (|| {
            let x = params["x"]
                .as_f64()
                .filter(|x| x.is_finite())
                .ok_or("Invalid mouse x")?;
            let y = params["y"]
                .as_f64()
                .filter(|y| y.is_finite())
                .ok_or("Invalid mouse y")?;
            Ok::<_, String>((x, y))
        })();
        let (x, y) = match coordinates {
            Ok(point) => point,
            Err(error) => return self.finish(Err(error), display).await,
        };
        let mapping = match self.prepare(client, session, display, x, y).await {
            Ok(mapping) => mapping,
            Err(error) => {
                // An explicit release needs no stale page coordinates. This
                // branch precedes any new native dispatch; a failed attempted
                // input below must retain its original failure instead.
                if allow_release_cleanup && params["type"] == "mouseReleased" && self.buttons != 0 {
                    self.release(display).await?;
                    return Ok(false);
                }
                return self.finish(Err(error), display).await;
            }
        };
        let result = async {
            let point = mapping.point(x, y, &display.surface())?;
            self.dispatch_at(params, point, &mapping, client, display, dialog_sessions)
                .await
        }
        .await;
        self.finish(result, display).await
    }

    async fn dispatch_at(
        &mut self,
        params: Value,
        point: (f64, f64),
        mapping: &Mapping,
        client: &CdpClient,
        display: &DisplayClient,
        dialog_sessions: &[&str],
    ) -> Result<bool, String> {
        let session = mapping.session.as_str();
        let event_type = params["type"].as_str().ok_or("Missing mouse event type")?;
        if !matches!(
            event_type,
            "mouseMoved" | "mousePressed" | "mouseReleased" | "mouseWheel"
        ) {
            return Err("Unsupported mouse event type".into());
        }
        // The helper changes one physical button per press/release. A caller's
        // claimed bitmask cannot clear the owner's actual acknowledged hold.
        let button = i64::from(crate::native::input::mouse_button_mask(
            params["button"].as_str().unwrap_or("left"),
        ));
        let buttons = match event_type {
            "mousePressed" => self.buttons | button,
            "mouseReleased" => self.buttons & !button,
            _ => self.buttons,
        };
        let modifiers = params["modifiers"].as_i64().unwrap_or(0);
        let surface = display.surface();
        let screen_x = point.0 / f64::from(surface.device_scale_factor);
        let screen_y = point.1 / f64::from(surface.device_scale_factor);
        // Held moves may cross an iframe. Their real helper acknowledgement
        // proves movement; only button boundaries need a renderer/dialog join.
        let observe = !matches!(
            (event_type, self.buttons),
            ("mouseMoved", 1..) | ("mouseReleased", 0)
        );
        let mut events = client.subscribe();
        let mut event = crate::native::input::stream_event("input_mouse", &params);
        event["x"] = json!(point.0);
        event["y"] = json!(point.1);
        event["buttons"] = json!(buttons);
        let mut activity =
            crate::native::activity::from_command("Input.dispatchMouseEvent", &params)
                .ok_or("Invalid mouse activity")?;
        activity["buttons"] = json!(buttons);
        activity["screenX"] = json!(screen_x);
        activity["screenY"] = json!(screen_y);
        let observation = client.observe_activity(
            activity,
            session,
            mapping.pointer.page_generation.clone(),
            InputSource::Agent,
        );
        self.unknown = true;
        if let Err(error) = display.input(&[event]).await {
            if error.operation_performed == Some(json!(false)) {
                self.unknown = false;
                return Err(error.to_string());
            }
            return Err(format!("browser_control_outcome_unknown: Native input may have executed ({error}). Do not replay the original action."));
        }
        self.buttons = buttons;
        self.modifiers = modifiers;
        self.unknown = false;
        observation.acknowledged();
        if !observe {
            return Ok(false);
        }
        let observed = client.send_command(
            "Runtime.evaluate",
            Some(json!({
                "expression": "true", "contextId": mapping.pointer.context,
                "awaitPromise": true, "returnByValue": true,
            })),
            Some(session),
        );
        tokio::pin!(observed);
        // The helper receipt proves native dispatch. This page readback joins
        // dialog observation; it does not claim DOM handling or a click effect.
        // Never cancel an in-flight helper RPC when a renderer dialog opens.
        loop {
            tokio::select! {
                result = &mut observed => {
                    let result = result.map_err(outcome_unknown)?;
                    if result["result"]["value"] != true { return Err(outcome_unknown("The current page could not be observed after native input. Inspect the page before retrying the action.")); }
                    return Ok(false);
                }
                event = events.recv() => match event {
                    Ok(event) if event.method == "Page.javascriptDialogOpening" && event.session_id.as_deref().is_none_or(|id| dialog_sessions.contains(&id)) => return Ok(true),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return Err(outcome_unknown("The browser disconnected after native input. Do not replay the action.")),
                    _ => {},
                },
            }
        }
    }

    pub(super) async fn drag(
        &mut self,
        client: &CdpClient,
        display: &DisplayClient,
        page_session: &str,
        source: (&str, f64, f64),
        target: (&str, f64, f64),
    ) -> Result<bool, String> {
        self.require_known()?;
        let result = self
            .drag_inner(client, display, page_session, source, target)
            .await;
        self.finish(result, display).await
    }

    async fn drag_inner(
        &mut self,
        client: &CdpClient,
        display: &DisplayClient,
        page_session: &str,
        source: (&str, f64, f64),
        target: (&str, f64, f64),
    ) -> Result<bool, String> {
        let end_mapping = self
            .prepare(client, target.0, display, target.1, target.2)
            .await?;
        let start_mapping = self
            .prepare(client, source.0, display, source.1, source.2)
            .await?;
        let start = start_mapping.point(source.1, source.2, &display.surface())?;
        let end = end_mapping.point(target.1, target.2, &display.surface())?;
        let params = |kind: &str, x: f64, y: f64, buttons: i64| json!({"type":kind,"x":x,"y":y,"button":"left","buttons":buttons,"clickCount":1});
        if self
            .dispatch_at(
                params("mousePressed", source.1, source.2, 1),
                start,
                &start_mapping,
                client,
                display,
                &[source.0, page_session],
            )
            .await?
        {
            return Ok(true);
        }
        for step in 1..=10 {
            self.current(&start_mapping, client, display).await?;
            let fraction = f64::from(step) / 10.0;
            let point = (
                (start.0 + (end.0 - start.0) * fraction).round(),
                (start.1 + (end.1 - start.1) * fraction).round(),
            );
            self.dispatch_at(
                params("mouseMoved", source.1, source.2, 1),
                point,
                &start_mapping,
                client,
                display,
                &[source.0, page_session],
            )
            .await?;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.current(&end_mapping, client, display).await?;
        self.dispatch_at(
            params("mouseReleased", target.1, target.2, 0),
            end,
            &end_mapping,
            client,
            display,
            &[target.0, page_session],
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapping(scale: f64, screen_y: f64) -> Mapping {
        Mapping {
            session: "page".into(),
            pointer: NativePointer {
                source_page: None,
                context: 7,
                page_generation: "page".into(),
                client_x: 210.0,
                client_y: 175.0,
                screen_x: 231.0,
                screen_y,
                geometry: json!({"scale":scale}),
            },
            surface: "surface".into(),
            window: (1, 0, 0, 2560, 1440),
        }
    }

    #[test]
    fn measured_native_origin_covers_zoom_fullscreen_and_devtools_without_chrome_height() {
        let surface = Surface::new(2560, 1440);
        for (screen_y, expected_y) in [(279.5, 559.0), (192.5, 385.0)] {
            let mapping = mapping(2.2, screen_y);
            assert_eq!(
                mapping.point(210.0, 175.0, &surface).unwrap(),
                (462.0, expected_y)
            );
            assert_eq!(
                mapping.point(260.0, 175.0, &surface).unwrap(),
                (572.0, expected_y)
            );
        }
    }

    #[test]
    fn pointer_mapping_rejects_outside_and_nonfinite_coordinates() {
        let surface = Surface::new(2560, 1440);
        let mapping = mapping(2.2, 279.5);
        for (x, y) in [
            (f64::NAN, 1.0),
            (f64::INFINITY, 1.0),
            (-1000.0, 0.0),
            (10000.0, 0.0),
            (0.0, 10000.0),
        ] {
            assert!(mapping.point(x, y, &surface).is_err());
        }
    }

    #[test]
    fn mappings_survive_only_a_held_gesture_and_unknown_input_requires_explicit_reset() {
        let mut mouse = NativeMouse::default();
        mouse.mappings.insert("page".into(), mapping(2.0, 262.0));
        mouse.begin_command();
        assert!(mouse.mappings.is_empty());
        mouse.mappings.insert("page".into(), mapping(2.0, 262.0));
        mouse.buttons = 1;
        mouse.unknown = true;
        mouse.begin_command();
        assert_eq!(mouse.mappings.len(), 1);
        assert!(mouse.unknown);
        mouse.reset();
        assert!(mouse.mappings.is_empty());
        assert!(!mouse.unknown);
        assert_eq!(mouse.buttons, 0);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn failed_readback_releases_held_input_but_preserves_the_original_failure() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let (display, peer, _frames) = DisplayClient::test_channel();
        let server = tokio::spawn(async move {
            let mut peer = BufReader::new(peer);
            let mut line = String::new();
            peer.read_line(&mut line).await.unwrap();
            let command: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(command["op"], "reset");
            let reply = format!("{}\n", json!({"id":command["id"],"success":true,"data":{}}));
            peer.get_mut().write_all(reply.as_bytes()).await.unwrap();
        });
        let mut mouse = NativeMouse {
            buttons: 1,
            modifiers: 2,
            ..Default::default()
        };
        mouse.mappings.insert("page".into(), mapping(2.0, 262.0));
        let original = "browser_control_outcome_unknown: Renderer readback failed after the press";
        assert_eq!(
            mouse
                .finish(Err(original.into()), &display)
                .await
                .unwrap_err(),
            original
        );
        assert!(!mouse.needs_release());
        assert!(mouse.mappings.is_empty());
        server.await.unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn lost_reset_receipt_retains_unknown_hold_and_fences_new_input() {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let (display, peer, _frames) = DisplayClient::test_channel();
        let server = tokio::spawn(async move {
            let mut peer = BufReader::new(peer);
            let mut line = String::new();
            peer.read_line(&mut line).await.unwrap();
            let command: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(command["op"], "reset");
            // The helper closes without acknowledging this cleanup.
        });
        let mut mouse = NativeMouse {
            buttons: 1,
            ..Default::default()
        };
        mouse.mappings.insert("page".into(), mapping(2.0, 262.0));
        let error = mouse
            .finish(Err("Original mouse action failed".into()), &display)
            .await
            .unwrap_err();
        assert!(error.starts_with("browser_control_outcome_unknown:"));
        assert!(error.contains("Original mouse action failed"));
        assert!(error.contains("release is unconfirmed"));
        assert!(mouse.unknown);
        assert_eq!(mouse.buttons, 1);
        mouse.begin_command();
        assert_eq!(mouse.mappings.len(), 1);
        assert!(mouse
            .require_known()
            .unwrap_err()
            .starts_with("browser_control_outcome_unknown:"));
        assert!(!display.available());
        server.await.unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn acknowledged_press_and_failed_page_readback_stay_unknown_through_mcp() {
        use futures_util::{SinkExt, StreamExt};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio_tungstenite::tungstenite::Message;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let cdp_server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
            for readback in [false, true] {
                let Message::Text(text) = socket.next().await.unwrap().unwrap() else {
                    panic!("expected CDP command")
                };
                let command: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(command["method"], "Runtime.evaluate");
                let response = if readback {
                    assert_eq!(command["params"]["expression"], "true");
                    json!({"id":command["id"],"error":{"code":-32000,"message":"Renderer readback timeout after native press"}})
                } else {
                    assert_eq!(command["params"]["expression"], GEOMETRY);
                    json!({"id":command["id"],"result":{"result":{"value":{"scale":2.0}}}})
                };
                socket
                    .send(Message::Text(response.to_string()))
                    .await
                    .unwrap();
            }
        });
        let client = CdpClient::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        let (display, peer, _frames) = DisplayClient::test_channel();
        let helper = tokio::spawn(async move {
            let mut peer = BufReader::new(peer);
            let mut operations = Vec::new();
            for expected in ["info", "input", "reset"] {
                let mut line = String::new();
                peer.read_line(&mut line).await.unwrap();
                let command: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(command["op"], expected);
                operations.push(expected);
                if expected == "input" {
                    assert_eq!(command["events"].as_array().unwrap().len(), 1);
                    assert_eq!(command["events"][0]["eventType"], "mousePressed");
                }
                let data = if expected == "info" {
                    json!({"width":2560,"height":1440,"windows":[{"id":1,"pid":1,"x":0,"y":0,"width":2560,"height":1440,"mapped":true,"focused":true,"overrideRedirect":false,"windowType":"normal"}]})
                } else {
                    json!({})
                };
                let response = format!(
                    "{}\n",
                    json!({"id":command["id"],"success":true,"data":data})
                );
                peer.get_mut().write_all(response.as_bytes()).await.unwrap();
            }
            operations
        });
        let mut mouse = NativeMouse::default();
        let mut measured = mapping(2.0, 262.0);
        measured.surface = display.surface().generation;
        measured.pointer.page_generation = client.page_generation("page");
        mouse.mappings.insert("page".into(), measured);
        let mut control = super::super::BrowserControl::default();
        control.set_display(Some(display.clone()));
        control.native_mouse = mouse;
        let error = control
            .agent_native_mouse(
                json!({"type":"mousePressed","x":210,"y":175,"button":"left","buttons":1}),
                &client,
                "page",
                &["page"],
            )
            .await
            .unwrap_err();
        assert!(error.starts_with("browser_control_outcome_unknown:"));
        assert!(error.contains("Renderer readback timeout"));
        assert!(!control.native_mouse.needs_release());
        assert!(control.needs_observation());
        assert_eq!(helper.await.unwrap(), ["info", "input", "reset"]);
        cdp_server.await.unwrap();
        let result = crate::mcp::native_error_result_for_test(&error);
        assert_eq!(result["isError"], true);
        assert_eq!(result["structuredContent"]["response"]["success"], false);
        assert_eq!(
            result["structuredContent"]["response"]["code"],
            "browser_control_outcome_unknown"
        );
        assert!(result["structuredContent"]["response"]
            .get("operationPerformed")
            .is_none());
        if let Ok(path) = std::env::var("AMBIT_TEST_NATIVE_MOUSE_MCP_RESULT") {
            std::fs::write(path, serde_json::to_vec_pretty(&result).unwrap()).unwrap();
        }
    }
}
