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

/// Whether a command must be refused until the agent observes the browser
/// again. Observing commands are never refused: they are how the requirement
/// is satisfied.
pub(super) fn observation_required(command: &Value, needs_observation: bool) -> bool {
    needs_observation && !observes_page(command)
}

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
        self.ref_map.clear();
        self.active_frame_id = None;
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
    /// visible page) and clears what was resolved against the old geometry.
    /// A dialog that blocked the proof is proven again once it is resolved.
    async fn prove_window_layout(&mut self) -> Result<(), (&'static str, &'static str)> {
        let page_blocked = self.pending_dialog.is_some();
        let Some(browser) = self.browser.as_ref() else {
            return Ok(());
        };
        if !browser.layout_unproven(page_blocked) {
            return Ok(());
        }
        let events = browser.client.subscribe();
        self.ref_map.clear();
        self.active_frame_id = None;
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
        // queued legacy operation cannot become safe merely because another
        // queued screenshot cleared the one-shot observation requirement.
        if !observes_page(command) && display.changed_since(received_at) {
            return Err(("browser_observation_stale", "The browser window changed while this command was queued. Observe its current page before choosing another action."));
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
                    self.ref_map.clear();
                    self.active_frame_id = None;
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
            return Err(("browser_observation_required", "The browser changed during window control. Run snapshot or screenshot and choose the next action from that fresh observation."));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn observing_commands_are_never_refused_for_lack_of_an_observation() {
        for action in ["snapshot", "screenshot"] {
            let command = json!({ "action": action });
            assert!(observes_page(&command));
            assert!(!observation_required(&command, true));
        }
        for action in ["click", "frame", "type", "mouse", "scroll"] {
            let command = json!({ "action": action });
            assert!(!observes_page(&command));
            assert!(observation_required(&command, true));
            assert!(!observation_required(&command, false));
        }
    }
}
