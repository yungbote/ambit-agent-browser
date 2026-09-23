//! Reconcile native window visibility with the browser's CDP page owner.

use super::BrowserManager;
use crate::native::display::{window_pixels, DisplayInfo, Surface};
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::{stream, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;

pub(crate) const AMBIGUOUS: &str = "browser_active_page_ambiguous";

fn visible_page(observations: &[(usize, Result<Value, String>)], total: usize) -> Option<usize> {
    let mut visible = Vec::new();
    let mut focused = Vec::new();
    let complete = observations.len() == total && observations.iter().all(|(_, page)| page.is_ok());
    for (index, page) in observations {
        if let Ok(page) = page {
            if page["visible"] == true {
                visible.push(*index);
                if page["focused"] == true {
                    focused.push(*index);
                }
            }
        }
    }
    // Native focus identifies one page even when an unrelated background
    // renderer is suspended. Visibility alone needs a complete observation.
    match focused.as_slice() {
        [one] => Some(*one),
        [] if complete && visible.len() == 1 => Some(visible[0]),
        _ => None,
    }
}

async fn page_geometry(
    client: &crate::native::cdp::client::CdpClient,
    session: &str,
) -> Result<Value, String> {
    let tree = client
        .send_command_no_params("Page.getFrameTree", Some(session))
        .await?;
    let frame = tree["frameTree"]["frame"]["id"]
        .as_str()
        .ok_or("Page frame is unavailable")?;
    let world = client
        .send_command(
            "Page.createIsolatedWorld",
            Some(json!({
                "frameId": frame, "worldName": "agent-browser-observation",
            })),
            Some(session),
        )
        .await?;
    let context = world["executionContextId"]
        .as_i64()
        .ok_or("Page observation realm is unavailable")?;
    let result = client.send_command("Runtime.evaluate", Some(json!({
        "expression": "({visible:document.visibilityState==='visible',focused:document.hasFocus(),innerWidth,innerHeight,dpr:devicePixelRatio,screenWidth:screen.width,screenHeight:screen.height})",
        "contextId": context, "returnByValue": true,
    })), Some(session)).await?;
    let value = result["result"]["value"].clone();
    if !value["visible"].is_boolean() || !value["focused"].is_boolean() {
        return Err("Page visibility is unavailable".into());
    }
    Ok(value)
}

impl BrowserManager {
    pub(crate) async fn window_info(&self) -> Result<DisplayInfo, String> {
        self.display_client()
            .ok_or("The browser has no owned window display")?
            .info()
            .await
            .map_err(|error| error.to_string())
    }

    /// A native tab click does not run `tab_switch`. Observe the existing
    /// targets without activating any of them, then commit only an unambiguous
    /// visible/focused page. Browser window IDs and X11 XIDs are never equated.
    pub(crate) async fn synchronize_visible_page(&mut self) -> Result<bool, &'static str> {
        if self.display_client().is_none() {
            return Ok(false);
        }
        let info = self.window_info().await.map_err(|_| AMBIGUOUS)?;
        if info.active_window().is_none() {
            return Err(AMBIGUOUS);
        }
        let client = self.client.clone();
        let sessions: Vec<_> = self
            .pages
            .iter()
            .enumerate()
            .map(|(index, page)| (index, page.session_id.clone()))
            .collect();
        let observations = stream::iter(sessions)
            .map(|(index, session)| {
                let client = client.clone();
                async move { (index, page_geometry(&client, &session).await) }
            })
            .buffer_unordered(16)
            .take_until(tokio::time::sleep(Duration::from_secs(2)))
            .collect::<Vec<_>>()
            .await;
        let selected = visible_page(&observations, self.pages.len()).ok_or(AMBIGUOUS)?;
        if self.pin_tab && self.bound_target_id.as_deref() != Some(&self.pages[selected].target_id)
        {
            return Err(AMBIGUOUS);
        }
        let changed = self.active_page_index != selected;
        if changed {
            self.enable_domains(&self.pages[selected].session_id)
                .await
                .map_err(|_| AMBIGUOUS)?;
        }
        self.active_page_index = selected;
        self.bind_active_target();
        Ok(changed)
    }

    pub(crate) async fn resize_window(
        &mut self,
        width: u32,
        height: u32,
        window_id: u32,
        page_blocked: bool,
        mut events: tokio::sync::broadcast::Receiver<crate::native::cdp::types::CdpEvent>,
    ) -> Result<(Surface, bool), String> {
        let (display_width, display_height) = window_pixels(width, height)?;
        let display = self
            .display_client()
            .ok_or("The browser has no owned window display")?;
        self.client.rotate_all_page_generations();
        display
            .resize(display_width, display_height, Some(window_id))
            .await
            .map_err(|error| error.to_string())?;
        if page_blocked {
            // Chromium's native modal is the visible surface. Its renderer
            // cannot answer metrics or screenshots until the user resolves
            // it. Publish only the acknowledged native surface; the daemon
            // keeps page feedback unavailable and retains any emulation.
            display.finish_layout().await;
            return Ok((display.surface(), true));
        }
        // XConfigureWindow acknowledges native geometry before Chromium has
        // necessarily reflowed. Require its visible page metrics to agree
        // before publishing the applied surface or a host observation.
        let paint = async {
            // An earlier page emulation must not keep this layout at an
            // unrelated fixed width. A newly opened modal can block this
            // command too, so it shares the same bounded observation phase.
            for page in &self.pages {
                self.client
                    .send_command_no_params(
                        "Emulation.clearDeviceMetricsOverride",
                        Some(&page.session_id),
                    )
                    .await?;
            }
            loop {
                let mut ready = Vec::new();
                for page in &self.pages {
                    let geometry = page_geometry(&self.client, &page.session_id).await?;
                    if geometry["visible"] == true
                        && geometry["screenWidth"].as_u64() == Some(u64::from(width))
                        && geometry["screenHeight"].as_u64() == Some(u64::from(height))
                    {
                        let positive = |key: &str| {
                            geometry[key]
                                .as_f64()
                                .filter(|value| value.is_finite() && *value > 0.0)
                        };
                        if let (Some(content_width), Some(content_height), Some(page_dpr)) = (
                            positive("innerWidth"),
                            positive("innerHeight"),
                            positive("dpr"),
                        ) {
                            // Page zoom changes CSS dimensions and page DPR,
                            // independently of the native UI's raster scale.
                            ready.push((
                                page.session_id.clone(),
                                content_width * page_dpr,
                                content_height * page_dpr,
                                page_dpr,
                            ));
                        }
                    }
                }
                if !ready.is_empty() {
                    // Native XSync acknowledges the top-level window. This
                    // existing CDP compositor readback additionally commits
                    // its visible page before the first full-window capture.
                    // It is performed once per layout, never per video frame.
                    for (session, expected_width, expected_height, page_dpr) in ready {
                        let painted = self
                            .client
                            .send_command(
                                "Page.captureScreenshot",
                                Some(json!({
                                    "format": "jpeg", "quality": 1, "fromSurface": true,
                                    "captureBeyondViewport": false,
                                })),
                                Some(&session),
                            )
                            .await?;
                        let data = painted["data"]
                            .as_str()
                            .filter(|data| data.len() <= 32 * 1024 * 1024)
                            .ok_or("The browser page paint could not be observed")?;
                        let bytes = STANDARD
                            .decode(data)
                            .map_err(|_| "The browser page paint was invalid")?;
                        let dimensions = image::ImageReader::new(std::io::Cursor::new(bytes))
                            .with_guessed_format()
                            .map_err(|_| "The browser page paint format was invalid")?
                            .into_dimensions()
                            .map_err(|_| "The browser page paint dimensions were invalid")?;
                        if (f64::from(dimensions.0) - expected_width).abs() > page_dpr
                            || (f64::from(dimensions.1) - expected_height).abs() > page_dpr
                        {
                            return Err(
                                "The browser compositor has not applied its new viewport".into()
                            );
                        }
                    }
                    return Ok::<(), String>(());
                }
                tokio::time::sleep(Duration::from_millis(16)).await;
            }
        };
        let modal = async {
            loop {
                match events.recv().await {
                    Ok(event) if event.method == "Page.javascriptDialogOpening" => {
                        return Ok::<(), String>(())
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return Err("Browser observation ended during window layout".into())
                    }
                    _ => {}
                }
            }
        };
        // The receiver was armed before draining the daemon's existing event
        // owner. A dialog that opens during resize is therefore observed too.
        let page_blocked = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                result = paint => result.map(|_| false),
                result = modal => result.map(|_| true),
            }
        })
        .await
        .map_err(|_| "The browser has not acknowledged the new page layout")??;
        display.finish_layout().await;
        Ok((display.surface(), page_blocked))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(visible: bool, focused: bool) -> Result<Value, String> {
        Ok(json!({"visible": visible, "focused": focused}))
    }

    #[test]
    fn active_page_requires_observed_visibility_or_native_focus() {
        assert_eq!(
            visible_page(&[(0, page(false, false)), (1, page(true, false))], 2),
            Some(1)
        );
        assert_eq!(
            visible_page(&[(0, Err("suspended".into())), (1, page(true, true))], 2),
            Some(1)
        );
        assert_eq!(visible_page(&[(1, page(true, true))], 2), Some(1));
        assert_eq!(visible_page(&[(1, page(true, false))], 2), None);
        assert_eq!(
            visible_page(&[(0, page(true, true)), (1, page(true, true))], 2),
            None
        );
        assert_eq!(
            visible_page(&[(0, page(true, false)), (1, page(true, false))], 2),
            None
        );
        assert_eq!(visible_page(&[], 0), None);
    }
}
