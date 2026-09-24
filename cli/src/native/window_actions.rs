//! Window presentation joins the existing daemon command boundary.

use super::DaemonState;
use crate::native::display::{window_pixels, Surface, DEVICE_SCALE_FACTOR};
use serde_json::Value;
use std::time::Instant;

fn explicit_browser_action(command: &Value) -> bool {
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
            && self.viewport.is_none();
        if unchanged {
            return Ok(browser.display_client().unwrap().surface());
        }
        if let Some(server) = self.stream_server.as_ref() {
            server.clear_frame();
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
        if let Some(server) = self.stream_server.as_ref() {
            server.set_viewport(width, height).await;
            server.presentation.update_surface(&surface);
        }
        Ok(surface)
    }

    /// A window without DevTools, as while a person signs in, follows its
    /// display alone: the helper resizes it, with no page paint to confirm.
    async fn apply_display_layout(&mut self, width: u32, height: u32) -> Result<Surface, String> {
        let display = self.window_display().ok_or("Browser not launched")?;
        let (display_width, display_height) = window_pixels(width, height)?;
        let info = display.info().await.map_err(|error| error.to_string())?;
        let window = info
            .active_window()
            .ok_or("The active browser window is ambiguous")?
            .id;
        display
            .resize(display_width, display_height, Some(window))
            .await
            .map_err(|error| error.to_string())?;
        display.finish_layout().await;
        let surface = display.surface();
        if let Some(server) = self.stream_server.as_ref() {
            server.set_viewport(width, height).await;
            server.presentation.update_surface(&surface);
        }
        Ok(surface)
    }

    pub(crate) async fn apply_pending_window_layout(&mut self) {
        let Some(server) = self.stream_server.clone() else {
            return;
        };
        server.presentation.expire();
        if !server.presentation.configured() {
            return;
        }
        let Some(display) = self.window_display() else {
            if let Some(request) = server.presentation.pending("unavailable") {
                server
                    .presentation
                    .complete(&request, "unavailable".into(), None);
            }
            return;
        };
        let control = self.browser_control.clone();
        let control = control.lock().await;
        if control.agent_error().is_some() {
            return;
        }
        let Ok(info) = display.info().await else {
            return;
        };
        let Some(window) = info.active_window() else {
            return;
        };
        let target = format!("{}:{}", display.identity(), window.id);
        let Some(request) = server.presentation.pending(&target) else {
            return;
        };
        // A viewer's layout is not human input: it needs no fresh observation
        // from the agent. Stale coordinates are already fenced by the cleared
        // refs, the rotated page generations and admission-time staleness.
        // Daemon command custody already excludes controller mutations and
        // native-window raw stream input is disabled. Release this gate so
        // event reconciliation and normal dialog handling can proceed.
        drop(control);
        let surface = self
            .apply_window_layout(request.config.width, request.config.height, None)
            .await
            .ok();
        server.presentation.complete(&request, target, surface);
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
        self.apply_pending_window_layout().await;
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
        self.drain_cdp_events_background().await.map_err(|_| ("browser_active_page_ambiguous", "The active browser page is not observable. Inspect the browser or select an existing tab explicitly."))?;
        if self.window_page_error == Some("browser_dialog_open")
            && self.pending_dialog.is_none()
            && self.viewport.is_some()
        {
            // A known modal prevented clearing an earlier page emulation.
            // Complete that same layout boundary once its renderer resumes.
            let surface = display.surface();
            self.apply_window_layout(
                surface.width / DEVICE_SCALE_FACTOR,
                surface.height / DEVICE_SCALE_FACTOR,
                None,
            )
            .await
            .map_err(|_| {
                (
                    "browser_layout_pending",
                    "The browser is applying its window layout.",
                )
            })?;
        }
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
