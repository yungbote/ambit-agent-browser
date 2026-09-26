//! Window presentation joins the existing daemon command boundary.

use super::DaemonState;
use crate::native::browser::ACTIVE_PAGE_AMBIGUOUS;
use crate::native::display::{Surface, DEVICE_SCALE_FACTOR};
use crate::native::stream::layout;

use serde_json::{json, Value};
use std::time::Instant;

/// Explicit browser and tab management, which addresses no implicit active
/// page: the window observation does not run for it.
pub(super) fn explicit_browser_action(command: &Value) -> bool {
    matches!(
        command["action"].as_str(),
        Some(
            "launch"
                | "close"
                | "cdp_url"
                | "tab_list"
                | "tab_new"
                | "tab_switch"
                | "window_new"
                | "dialog"
        )
    ) || command["action"] == crate::connection::INTERNAL_DAEMON_SHUTDOWN_ACTION
}

/// A snapshot or screenshot is itself the observation every gate asks for.
pub(super) fn observes_page(command: &Value) -> bool {
    matches!(command["action"].as_str(), Some("snapshot" | "screenshot"))
}

/// What a command's effect lands on, as far as browser events outside the
/// agent's commands can change it. A command that names its target (a URL,
/// tab, window, selector, ref or script) resolves it against the live
/// browser when it runs, so no such event makes it stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Target {
    /// Named by the command itself.
    Named,
    /// Whatever has focus: key and text input with no element, and the
    /// answer to a JavaScript dialog, which goes to whichever one is open.
    Focus,
    /// Wherever the pointer is: a button pressed or released in place.
    Pointer,
    /// Viewport coordinates, as current as the view they were read from.
    Point,
    /// Anything: a supervised program can send input to focus, the pointer
    /// and any point, and no image fences its points.
    Any,
}

/// The target of a daemon command. Handlers that dispatch input without
/// resolving an element are the only ones that do not name theirs.
pub(super) fn target(command: &Value) -> Target {
    match command["action"].as_str().unwrap_or_default() {
        "press" | "keydown" | "keyup" | "keyboard" | "inserttext" | "input_keyboard" => {
            Target::Focus
        }
        // A pending dialog may be one a person's actions opened; reading its
        // status names nothing to act on.
        "dialog" => match command["response"].as_str() {
            Some("status") => Target::Named,
            _ => Target::Focus,
        },
        "run_playwright" => Target::Any,
        // Copy and paste press the platform shortcut on the focused element.
        "clipboard" => match command
            .get("subAction")
            .or_else(|| command.get("operation"))
            .and_then(Value::as_str)
        {
            Some("copy" | "paste") => Target::Focus,
            _ => Target::Named,
        },
        "mousedown" | "mouseup" => Target::Pointer,
        "mousemove" | "mouse" | "wheel" | "input_mouse" | "input_touch" | "swipe" => Target::Point,
        _ => Target::Named,
    }
}

/// Whether the host checked this command's point against the image it was
/// read from: the daemon then refuses it as `browser_observation_stale` when
/// the page, its generation or its geometry no longer match that image.
pub(super) fn point_fenced(command: &Value) -> bool {
    command
        .get(crate::native::feedback::REQUEST_FIELD)
        .and_then(|request| request.get("expectedObservation"))
        .is_some_and(|expected| !expected.is_null())
}

/// Whether a command must wait for a fresh observation because the browser
/// changed outside the agent's commands (a person held it, or an input's
/// outcome is unknown). Only focus, the pointer and unfenced points can have
/// moved under the agent; a command that names its target runs, and the
/// host's feedback shows it the page it acted on.
pub(super) fn observation_required(command: &Value, needs_observation: bool) -> bool {
    needs_observation
        && match target(command) {
            Target::Named => false,
            Target::Focus | Target::Pointer | Target::Any => true,
            Target::Point => !point_fenced(command),
        }
}

/// Whether a window layout applied after this command was received made its
/// target stale. Layout moves content under the pointer and under viewport
/// coordinates the host did not fence; it changes no focus and no element's
/// identity.
pub(super) fn layout_made_stale(command: &Value) -> bool {
    match target(command) {
        Target::Named | Target::Focus => false,
        Target::Pointer | Target::Any => true,
        Target::Point => !point_fenced(command),
    }
}

/// The refusal of a command `observation_required` holds back. Host-bound
/// callers receive a fresh observation with it, which is the observation it
/// asks for; others take a snapshot or screenshot.
pub(super) const OBSERVATION_REQUIRED: &str = "The browser changed outside your commands since your last observation (a person used it, or an earlier input's outcome is unknown), so focus, the pointer and any open dialog may have changed. This command can act on them without naming an element, so nothing was sent. Observe the page, then send it again, or address an element by selector or ref.";

impl DaemonState {
    /// The agent's own window layout (launch, sign-in, `set viewport`, or a
    /// controller's legacy `viewport` input while no view presents): the
    /// window is laid out exactly and, with DevTools, proven before this
    /// returns. A presenting view's layouts run outside command custody
    /// (`stream::layout`) and are proven by `prove_window_layout`.
    pub(crate) async fn apply_window_layout(
        &mut self,
        width: u32,
        height: u32,
        events: Option<tokio::sync::broadcast::Receiver<crate::native::cdp::types::CdpEvent>>,
    ) -> Result<Surface, String> {
        if self.browser.is_none() {
            return self.apply_display_layout(width, height).await;
        }
        let events = if let Some(events) = events {
            events
        } else {
            let events = self
                .browser
                .as_ref()
                .ok_or("Browser not launched")?
                .client
                .subscribe();
            self.drain_cdp_events_background().await?;
            events
        };
        let page_blocked = self.pending_dialog.is_some();
        let browser = self.browser.as_mut().ok_or("Browser not launched")?;
        let info = browser.window_info().await?;
        let window = info
            .active_window()
            .ok_or("The active browser window is ambiguous")?;
        let id = window.id;
        let unchanged = browser
            .display_client()
            .is_some_and(|display| display.ready())
            && info.width == width * DEVICE_SCALE_FACTOR
            && info.height == height * DEVICE_SCALE_FACTOR
            && window.x == 0
            && window.y == 0
            && window.width == info.width
            && window.height == info.height
            && !browser.layout_unproven(page_blocked)
            && self.viewport.is_none();
        if unchanged {
            return Ok(browser.display_client().unwrap().surface());
        }
        // Refs and the selected frame stay: a layout changes no element's
        // identity. Each use of a ref measures its element again, and a ref
        // outlives only the document its snapshot read (`element::lookup_ref`).
        self.window_page_error = Some("browser_layout_pending");
        let (surface, page_blocked) = browser
            .resize_window(width, height, id, page_blocked, events)
            .await?;
        if !page_blocked {
            self.viewport = None;
        }
        self.window_page_error = page_blocked.then_some("browser_dialog_open");
        self.publish_window_layout(&surface).await;
        Ok(surface)
    }

    /// A window without DevTools, as while a person signs in, follows its
    /// display alone: the helper resizes it, with no page paint to confirm.
    async fn apply_display_layout(&mut self, width: u32, height: u32) -> Result<Surface, String> {
        let display = self.window_display().ok_or("Browser not launched")?;
        let applied = layout::apply(&display, width, height, false).await?;
        self.publish_window_layout(&applied.surface).await;
        Ok(applied.surface)
    }

    /// A controller's `viewport` input (an older viewer resizing the window
    /// it controls): the same layout a presenter gets, outside the agent's
    /// page proof, which the agent's next command runs. The framebuffer may
    /// stay at a size class only while every viewer crops to the window.
    pub(crate) async fn apply_controller_layout(
        &mut self,
        width: u32,
        height: u32,
    ) -> Result<Surface, String> {
        let display = self.window_display().ok_or("Browser not launched")?;
        let size_class = self
            .stream_server
            .as_ref()
            .is_some_and(|server| server.media.roster().crops_visible());
        let applied = layout::apply(&display, width, height, size_class).await?;
        self.publish_window_layout(&applied.surface).await;
        Ok(applied.surface)
    }

    /// Tells the stream the window's applied layout. The window, not the
    /// framebuffer, is the viewport its viewers see.
    pub(crate) async fn publish_window_layout(&self, surface: &Surface) {
        let (Some(server), Some(display)) = (self.stream_server.as_ref(), self.window_display())
        else {
            return;
        };
        let (width, height) = display.window();
        server
            .set_viewport(width / DEVICE_SCALE_FACTOR, height / DEVICE_SCALE_FACTOR)
            .await;
        server
            .presentation
            .update_surface(surface, display.window());
    }

    /// The agent's geometry gate: before the agent's next command acts on
    /// the page, the page must follow the newest window layout. A viewer's
    /// layout lands outside command custody; this proof runs once per layout
    /// (page metrics at the window's size and one compositor readback per
    /// visible page). Refs and the selected frame stay: a layout changes no
    /// element's identity, and each use measures its element again
    /// (`element::lookup_ref`, `scoped_frame`); points and the pointer are
    /// fenced by `layout_made_stale`. A dialog that blocked the proof is
    /// proven again once it is resolved.
    async fn prove_window_layout(&mut self) -> Result<(), (&'static str, &'static str)> {
        let page_blocked = self.pending_dialog.is_some();
        let Some(browser) = self.browser.as_ref() else {
            return Ok(());
        };
        if !browser.layout_unproven(page_blocked) {
            return Ok(());
        }
        let events = browser.client.subscribe();
        self.window_page_error = Some("browser_layout_pending");
        let proof = self
            .browser
            .as_mut()
            .unwrap()
            .prove_window_layout(page_blocked, events)
            .await;
        match proof {
            Ok(page_blocked) => {
                if !page_blocked {
                    self.viewport = None;
                }
                self.window_page_error = page_blocked.then_some("browser_dialog_open");
                Ok(())
            }
            Err(_) => Err((
                "browser_layout_pending",
                "The browser is applying its window layout.",
            )),
        }
    }

    /// The response to a command `prepare_window_command` refused. When no
    /// single tab is the active page, the refusal also lists the open tabs
    /// (`data`, see `BrowserManager::tab_roster`): the caller can select one
    /// explicitly without a `tab_list` round. Nothing is selected for it.
    pub(super) fn window_refusal(&self, id: &Value, code: &str, message: &str) -> Value {
        let mut refusal = json!({ "id": id, "success": false, "code": code, "error": message });
        if code == ACTIVE_PAGE_AMBIGUOUS {
            if let Some(browser) = self.browser.as_ref() {
                refusal["data"] = browser.tab_roster();
            }
        }
        refusal
    }

    pub(crate) async fn prepare_window_command(
        &mut self,
        command: &Value,
        received_at: Instant,
    ) -> Result<(), (&'static str, &'static str)> {
        // Controller acquisition owns its existing bounded preparation wait.
        // Passive presentation must not introduce a prior unbounded wait or
        // delay browser management and supervisor shutdown.
        if command["action"] == crate::native::browser_control::ACTION
            || explicit_browser_action(command)
        {
            return Ok(());
        }
        let Some(display) = self.window_display() else {
            self.window_page_error = None;
            return Ok(());
        };
        // While a person holds the browser, every command is refused for
        // that reason alone: the shared custody gate preserves its typed error.
        if self.browser_control.lock().await.agent_error().is_some() {
            return Ok(());
        }
        // Admission time is taken before waiting for command custody. A
        // queued point or pointer input cannot become safe merely because
        // another queued screenshot cleared the one-shot observation
        // requirement; a command that names its target resolves it now.
        if layout_made_stale(command) && display.changed_since(received_at) {
            return Err(("browser_observation_stale", "The browser window changed after this command was sent, so a point or the pointer it uses may no longer be over what you chose. Nothing was sent. Observe its current page, then send it again."));
        }
        self.drain_cdp_events_background().await.map_err(|_| (ACTIVE_PAGE_AMBIGUOUS, "The active browser page is not observable. Inspect the browser or select an existing tab explicitly."))?;
        self.prove_window_layout().await?;
        let observation = self
            .browser
            .as_mut()
            .unwrap()
            .synchronize_visible_page()
            .await;
        match observation {
            Ok(changed) => {
                self.window_page_error = None;
                if changed {
                    // Refs and the selected frame stay: each is bound to the
                    // document it was taken from, which the page shown now
                    // does not have (`element::lookup_ref`, `scoped_frame`).
                    self.refresh_active_iframe_sessions().await;
                    self.update_stream_client().await;
                }
            }
            Err(code) => {
                self.window_page_error = Some(code);
                return Err((code, "The active browser page is ambiguous. Inspect the browser or use tab list and select an existing tab explicitly."));
            }
        }
        if command
            .get(crate::native::feedback::REQUEST_FIELD)
            .is_none()
            && observation_required(
                command,
                self.browser_control.lock().await.needs_observation(),
            )
        {
            return Err(("browser_observation_required", OBSERVATION_REQUIRED));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::feedback::REQUEST_FIELD;
    use serde_json::json;

    fn fenced(mut command: Value) -> Value {
        command[REQUEST_FIELD] = json!({ "expectedObservation": {
            "targetId": "T", "loaderId": "L", "pageGeneration": "G", "geometrySha256": "sha256:0",
        } });
        command
    }

    #[test]
    fn observing_commands_are_never_refused_for_lack_of_an_observation() {
        for action in ["snapshot", "screenshot"] {
            let command = json!({ "action": action });
            assert!(observes_page(&command));
            assert!(!observation_required(&command, true));
        }
    }

    /// Production refused all of these after a person released the browser
    /// (open, tab_list, reload, eval, errors, set_viewport, auth_list), though
    /// each names what it acts on and resolves it against the live browser.
    #[test]
    fn commands_that_name_their_target_run_after_a_handback() {
        for command in [
            json!({ "action": "navigate", "url": "https://example.test/" }),
            json!({ "action": "url" }),
            json!({ "action": "title" }),
            json!({ "action": "tab_list" }),
            json!({ "action": "tab_new", "url": "https://example.test/" }),
            json!({ "action": "tab_switch", "tabId": "t2" }),
            json!({ "action": "window_new" }),
            json!({ "action": "auth_list" }),
            json!({ "action": "errors" }),
            json!({ "action": "evaluate", "script": "1" }),
            json!({ "action": "reload" }),
            json!({ "action": "viewport", "width": 800, "height": 600 }),
            json!({ "action": "click", "selector": "#go" }),
            json!({ "action": "click", "selector": "@e3" }),
            json!({ "action": "fill", "selector": "#q", "value": "x" }),
            json!({ "action": "type", "selector": "#q", "text": "x" }),
            json!({ "action": "scroll", "direction": "down", "amount": 300 }),
            json!({ "action": "drag", "source": "@e1", "target": "#drop" }),
            json!({ "action": "frame", "selector": "#embed" }),
            json!({ "action": "clipboard", "operation": "read" }),
            json!({ "action": "clipboard", "operation": "write", "text": "x" }),
            json!({ "action": "dialog", "response": "status" }),
        ] {
            assert_eq!(target(&command), Target::Named, "{command}");
            assert!(!observation_required(&command, true), "{command}");
            assert!(!layout_made_stale(&command), "{command}");
        }
    }

    /// What a person's control can move under the agent: focus (and the
    /// dialog it may have opened), the pointer and any point the host did not
    /// check against its image. A program can send any of these.
    #[test]
    fn input_to_focus_pointer_or_an_unfenced_point_waits_for_an_observation() {
        for command in [
            json!({ "action": "press", "key": "Enter" }),
            json!({ "action": "keydown", "key": "Shift" }),
            json!({ "action": "keyup", "key": "Shift" }),
            json!({ "action": "keyboard", "subaction": "type", "text": "x" }),
            json!({ "action": "keyboard", "subaction": "insertText", "text": "x" }),
            json!({ "action": "inserttext", "text": "x" }),
            json!({ "action": "clipboard", "operation": "copy" }),
            json!({ "action": "clipboard", "operation": "paste" }),
            json!({ "action": "mousedown", "button": "left" }),
            json!({ "action": "mouseup", "button": "left" }),
            json!({ "action": "mousemove", "x": 10, "y": 20 }),
            json!({ "action": "wheel", "deltaX": 0, "deltaY": 100 }),
            json!({ "action": "swipe", "direction": "up" }),
            // The pending dialog may be one the person's actions opened.
            json!({ "action": "dialog", "response": "accept" }),
            json!({ "action": "dialog", "response": "dismiss" }),
            // A program can press keys or click points the image it was
            // written from no longer shows.
            json!({ "action": "run_playwright", "code": "await page.keyboard.press('Enter')" }),
        ] {
            assert_ne!(target(&command), Target::Named, "{command}");
            assert!(observation_required(&command, true), "{command}");
            assert!(!observation_required(&command, false), "{command}");
        }
        // A point read from an image is fenced by that image instead: the
        // daemon refuses it as stale when the page no longer matches it.
        let point = json!({ "action": "mousemove", "x": 10, "y": 20 });
        assert!(!observation_required(&fenced(point.clone()), true));
        let mut unfenced = point;
        unfenced[REQUEST_FIELD] = json!({ "expectedObservation": null });
        assert!(observation_required(&unfenced, true));
        // A button has no point to fence: it goes wherever the pointer is.
        assert!(observation_required(
            &fenced(json!({ "action": "mousedown" })),
            true
        ));
        // Nor has a program: whatever image it came with, it can press keys.
        assert!(observation_required(
            &fenced(json!({ "action": "run_playwright", "code": "return 1" })),
            true
        ));
    }

    /// A layout changes no focus and no element's identity; it moves content
    /// under the pointer and under viewport coordinates, which a program may
    /// use.
    #[test]
    fn a_layout_makes_only_pointer_input_and_unfenced_points_stale() {
        assert!(!layout_made_stale(
            &json!({ "action": "press", "key": "Enter" })
        ));
        assert!(!layout_made_stale(
            &json!({ "action": "click", "selector": "@e1" })
        ));
        assert!(layout_made_stale(&json!({ "action": "mouseup" })));
        assert!(!layout_made_stale(
            &json!({ "action": "dialog", "response": "accept" })
        ));
        assert!(layout_made_stale(
            &json!({ "action": "run_playwright", "code": "return 1" })
        ));
        let point = json!({ "action": "mousemove", "x": 10, "y": 20 });
        assert!(layout_made_stale(&point));
        assert!(!layout_made_stale(&fenced(point)));
    }
}
