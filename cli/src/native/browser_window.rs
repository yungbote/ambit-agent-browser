//! Reconcile native window visibility with the browser's CDP page owner.

use super::{format_tab_id, BrowserManager, PageInfo};
use crate::native::display::{window_pixels, DisplayInfo, Surface};
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::{stream, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;

pub(crate) const ACTIVE_PAGE_AMBIGUOUS: &str = "browser_active_page_ambiguous";
/// A refusal lists at most this many tabs; `tab_list` lists every tab.
const ROSTER_TABS: usize = 20;
/// Characters of each roster text; a longer text is cut and ends in `…`.
const ROSTER_TEXT_CHARS: usize = 100;

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

/// The tabs a refusal lists: `tabs` in tab order, at most `ROSTER_TABS` of
/// them (an active tab beyond them takes the last place), and `tabCount`, the
/// number of open tabs, so a caller can tell whether the list is complete.
/// Each tab carries its stable id, any label, its title, its URL reduced to
/// the origin (never a path, query, fragment or opaque payload) and whether
/// it is the tab commands act on.
fn roster(pages: &[PageInfo], active: Option<usize>) -> Value {
    let mut listed: Vec<usize> = (0..pages.len().min(ROSTER_TABS)).collect();
    if let Some(index) = active.filter(|index| (ROSTER_TABS..pages.len()).contains(index)) {
        listed[ROSTER_TABS - 1] = index;
    }
    let tabs: Vec<Value> = listed
        .into_iter()
        .map(|index| {
            let page = &pages[index];
            let mut tab = json!({
                "tabId": format_tab_id(page.tab_id),
                "title": roster_text(&page.title),
                "origin": roster_text(&url_origin(&page.url)),
                "active": active == Some(index),
            });
            if let Some(label) = &page.label {
                tab["label"] = json!(roster_text(label));
            }
            tab
        })
        .collect();
    json!({ "tabs": tabs, "tabCount": pages.len() })
}

/// Whitespace runs collapsed to one space, cut to `ROSTER_TEXT_CHARS`
/// characters; a cut text ends in `…`.
fn roster_text(value: &str) -> String {
    let text = value.split_whitespace().collect::<Vec<_>>().join(" ");
    match text.char_indices().nth(ROSTER_TEXT_CHARS) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

/// `https://example.com:8443` for a URL with a tuple origin; the scheme alone
/// (`about:`, `data:`, `file:`, `chrome:`) for one without; nothing for an
/// unparsable URL.
fn url_origin(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) => match parsed.origin() {
            origin @ url::Origin::Tuple(..) => origin.ascii_serialization(),
            url::Origin::Opaque(_) => format!("{}:", parsed.scheme()),
        },
        Err(_) => String::new(),
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
        "expression": "({visible:document.visibilityState==='visible',focused:document.hasFocus(),title:document.title,innerWidth,innerHeight,dpr:devicePixelRatio,screenWidth:screen.width,screenHeight:screen.height})",
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

    /// The open tabs for a refusal whose recovery is selecting one
    /// explicitly, bounded unlike `tab_list` (see `roster`).
    pub(crate) fn tab_roster(&self) -> Value {
        roster(
            &self.pages,
            (!self.bound_target_is_gone()).then_some(self.active_page_index),
        )
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
        // The observation reads each page's title too: one set after its tab
        // opened (a tab a person opened, a page that retitled itself) is
        // otherwise unknown here, and a tab roster lists the current one.
        for (index, page) in &observations {
            if let Some(title) = page.as_ref().ok().and_then(|page| page["title"].as_str()) {
                self.pages[*index].title = title.to_string();
            }
        }
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

    fn tab(tab_id: u32, label: Option<&str>, title: &str, url: &str) -> PageInfo {
        PageInfo {
            tab_id,
            label: label.map(str::to_string),
            target_id: format!("TARGET{tab_id}"),
            session_id: format!("session-{tab_id}"),
            url: url.to_string(),
            title: title.to_string(),
            target_type: "page".to_string(),
        }
    }

    #[test]
    fn roster_lists_each_tab_by_id_title_and_origin_only() {
        let pages = [
            tab(
                1,
                None,
                "Inbox (3) - Gmail",
                "https://mail.google.com/mail/u/0/#inbox",
            ),
            tab(
                2,
                Some("docs"),
                "  React\n\t Docs ",
                "https://user:secret@react.dev:8443/learn?token=abc#intro",
            ),
            tab(3, None, "", "about:blank"),
            tab(4, None, "Report", "data:text/html,<p>secret</p>"),
            tab(5, None, "notes.txt", "file:///home/user/notes.txt"),
            tab(6, None, "New Tab", "chrome://newtab/"),
            tab(7, None, "", ""),
            tab(
                8,
                None,
                "Login",
                "blob:https://accounts.example.com/5f0c-secret",
            ),
        ];
        assert_eq!(
            roster(&pages, Some(0)),
            json!({
                "tabCount": 8,
                "tabs": [
                    {"tabId": "t1", "title": "Inbox (3) - Gmail", "origin": "https://mail.google.com", "active": true},
                    {"tabId": "t2", "label": "docs", "title": "React Docs", "origin": "https://react.dev:8443", "active": false},
                    {"tabId": "t3", "title": "", "origin": "about:", "active": false},
                    {"tabId": "t4", "title": "Report", "origin": "data:", "active": false},
                    {"tabId": "t5", "title": "notes.txt", "origin": "file:", "active": false},
                    {"tabId": "t6", "title": "New Tab", "origin": "chrome:", "active": false},
                    {"tabId": "t7", "title": "", "origin": "", "active": false},
                    {"tabId": "t8", "title": "Login", "origin": "https://accounts.example.com", "active": false},
                ],
            })
        );
        // Without an active page (a pinned tab that is gone) no tab is active.
        let unbound = roster(&pages, None);
        assert!(unbound["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tab| tab["active"] == false));
        assert_eq!(roster(&[], Some(0)), json!({"tabCount": 0, "tabs": []}));
    }

    #[test]
    fn roster_is_bounded_and_keeps_an_active_tab_beyond_the_bound() {
        let long = "é".repeat(ROSTER_TEXT_CHARS + 50);
        let pages: Vec<_> = (1..=25)
            .map(|id| tab(id, None, &long, &format!("https://site{id}.example/path")))
            .collect();
        let listed = roster(&pages, Some(22));
        let tabs = listed["tabs"].as_array().unwrap();
        assert_eq!(listed["tabCount"], 25);
        assert_eq!(tabs.len(), ROSTER_TABS);
        let ids: Vec<_> = tabs.iter().map(|tab| tab["tabId"].clone()).collect();
        let mut expected: Vec<_> = (1..ROSTER_TABS).map(|id| json!(format!("t{id}"))).collect();
        expected.push(json!("t23"));
        assert_eq!(ids, expected);
        assert_eq!(tabs[ROSTER_TABS - 1]["active"], true);
        assert_eq!(tabs.iter().filter(|tab| tab["active"] == true).count(), 1);
        // A title is cut at a character boundary and marked as cut.
        let title = tabs[0]["title"].as_str().unwrap();
        assert_eq!(title.chars().count(), ROSTER_TEXT_CHARS + 1);
        assert!(title.ends_with("é…"));
        // An active tab inside the bound keeps its own place.
        let inside = roster(&pages, Some(3));
        assert_eq!(inside["tabs"][3]["active"], true);
        assert_eq!(inside["tabs"][ROSTER_TABS - 1]["tabId"], "t20");
    }
}
