//! Reconcile native window visibility with the browser's CDP page owner.

use super::tabs::{observe_page, TITLE_EXPRESSION};
use super::BrowserManager;
use crate::native::cdp::client::CdpClient;
use crate::native::display::{window_pixels, DisplayInfo, Surface};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::time::Duration;

pub(crate) const ACTIVE_PAGE_AMBIGUOUS: &str = "browser_active_page_ambiguous";

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

/// What the visibility observation reads of a page: whether it is visible and
/// focused, its title, and the geometry a window layout waits for.
fn visibility_expression() -> String {
    format!("({{visible:document.visibilityState==='visible',focused:document.hasFocus(),title:{TITLE_EXPRESSION},innerWidth,innerHeight,dpr:devicePixelRatio,screenWidth:screen.width,screenHeight:screen.height}})")
}

/// A visibility observation, admitted only when it tells whether its page is
/// visible and focused.
fn visibility(page: Value) -> Result<Value, String> {
    if page["visible"].is_boolean() && page["focused"].is_boolean() {
        Ok(page)
    } else {
        Err("Page visibility is unavailable".into())
    }
}

async fn page_geometry(client: &CdpClient, session: &str) -> Result<Value, String> {
    observe_page(client, session, &visibility_expression())
        .await
        .and_then(visibility)
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
        let info = self
            .window_info()
            .await
            .map_err(|_| ACTIVE_PAGE_AMBIGUOUS)?;
        if info.active_window().is_none() {
            return Err(ACTIVE_PAGE_AMBIGUOUS);
        }
        let observations: Vec<_> = self
            .observe_pages(&visibility_expression(), None)
            .await
            .into_iter()
            .map(|(index, page)| (index, page.and_then(visibility)))
            .collect();
        // The observation reads each page's title too, so the roster of an
        // ambiguous-page refusal lists the current ones.
        self.record_titles(&observations);
        let selected =
            visible_page(&observations, self.pages.len()).ok_or(ACTIVE_PAGE_AMBIGUOUS)?;
        if self.pin_tab && self.bound_target_id.as_deref() != Some(&self.pages[selected].target_id)
        {
            return Err(ACTIVE_PAGE_AMBIGUOUS);
        }
        let changed = self.active_page_index != selected;
        if changed {
            self.enable_domains(&self.pages[selected].session_id)
                .await
                .map_err(|_| ACTIVE_PAGE_AMBIGUOUS)?;
        }
        self.active_page_index = selected;
        self.bind_active_target();
        Ok(changed)
    }

    /// Lays the window out at `width` × `height` CSS pixels, exactly, and
    /// proves the page follows it before returning: the agent's own layouts
    /// (launch, sign-in, `set viewport`). A viewer's layouts run outside
    /// command custody (`stream::layout`) and are proven at the agent's next
    /// command instead.
    pub(crate) async fn resize_window(
        &mut self,
        width: u32,
        height: u32,
        window_id: u32,
        page_blocked: bool,
        events: tokio::sync::broadcast::Receiver<crate::native::cdp::types::CdpEvent>,
    ) -> Result<(Surface, bool), String> {
        let (display_width, display_height) = window_pixels(width, height)?;
        let display = self
            .display_client()
            .ok_or("The browser has no owned window display")?;
        {
            let layout = display.layout().await;
            self.client.rotate_all_page_generations();
            display
                .resize(
                    &layout,
                    display_width,
                    display_height,
                    Some(window_id),
                    false,
                )
                .await
                .map_err(|error| error.to_string())?;
        }
        let page_blocked = self.prove_window_layout(page_blocked, events).await?;
        Ok((display.surface(), page_blocked))
    }

    /// Proves the pages follow the window's newest layout: page metrics at
    /// the window's size, then one compositor readback per visible page.
    /// Returns whether a JavaScript dialog kept the proof from the page.
    /// The proof is recorded for that layout epoch, so it runs once.
    pub(crate) async fn prove_window_layout(
        &mut self,
        page_blocked: bool,
        mut events: tokio::sync::broadcast::Receiver<crate::native::cdp::types::CdpEvent>,
    ) -> Result<bool, String> {
        let display = self
            .display_client()
            .ok_or("The browser has no owned window display")?;
        // No layout lands while its proof runs, which it would never pass:
        // a person's drag waits for this one proof, as for one agent input.
        let _atomic = display.atomic_input().await;
        let epoch = display.layout_epoch();
        let (window_width, window_height) = display.window();
        let (width, height) = (
            window_width / crate::native::display::DEVICE_SCALE_FACTOR,
            window_height / crate::native::display::DEVICE_SCALE_FACTOR,
        );
        if page_blocked {
            // Chromium's native modal is the visible surface. Its renderer
            // cannot answer metrics or screenshots until the user resolves
            // it. The daemon keeps page feedback unavailable and retains any
            // emulation; the proof runs again once the dialog is gone.
            self.layout_proof = Some((epoch, true));
            return Ok(true);
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
        self.layout_proof = Some((epoch, page_blocked));
        Ok(page_blocked)
    }

    /// Whether the newest layout still needs its page proof: it was never
    /// proven, or a dialog that blocked its proof has since been resolved.
    pub(crate) fn layout_unproven(&self, page_blocked: bool) -> bool {
        let Some(display) = self.display_client() else {
            return false;
        };
        match self.layout_proof {
            Some((epoch, false)) => epoch != display.layout_epoch(),
            Some((epoch, true)) => epoch != display.layout_epoch() || !page_blocked,
            None => true,
        }
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
