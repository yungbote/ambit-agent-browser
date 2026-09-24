//! The open tabs: what is read from each page, and the roster a refusal
//! lists when its recovery is selecting a tab explicitly.

use super::{format_tab_id, BrowserManager, PageInfo};
use crate::native::cdp::client::CdpClient;
use futures_util::{stream, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;

/// A refusal lists at most this many tabs; `tab_list` lists every tab.
const ROSTER_TABS: usize = 20;
/// Characters of each roster text; a longer text is cut and ends in `…`.
const ROSTER_TEXT_CHARS: usize = 100;
/// Pages observed at once.
const OBSERVED_AT_ONCE: usize = 16;
/// The bound on observing every page: a page whose renderer cannot answer
/// (busy, suspended, discarded) is left unobserved rather than holding the
/// command until the CDP timeout.
const OBSERVATION_BOUND: Duration = Duration::from_secs(2);

/// A page sets `document.title` to any string: read a bounded one.
pub(super) const TITLE_EXPRESSION: &str = "document.title.slice(0,4096)";

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

/// Evaluates `expression` in a page's observation realm: an isolated world
/// of its main frame, whose globals page script cannot redefine.
pub(super) async fn observe_page(
    client: &CdpClient,
    session: &str,
    expression: &str,
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
    let result = client
        .send_command(
            "Runtime.evaluate",
            Some(json!({
                "expression": expression, "contextId": context, "returnByValue": true,
            })),
            Some(session),
        )
        .await?;
    Ok(result["result"]["value"].clone())
}

impl BrowserManager {
    /// Evaluates `expression` in the observation realm of every open page but
    /// the one a JavaScript dialog pauses (`dialog_session`), whose renderer
    /// answers nothing until the dialog is resolved. Pages are observed
    /// `OBSERVED_AT_ONCE` at a time within `OBSERVATION_BOUND`; the result
    /// holds the pages that answered, by index.
    pub(super) async fn observe_pages(
        &self,
        expression: &str,
        dialog_session: Option<&str>,
    ) -> Vec<(usize, Result<Value, String>)> {
        // Each observation owns its session and client handle: a future that
        // borrowed the page list would not be `Send` for every lifetime.
        let sessions: Vec<_> = self
            .pages
            .iter()
            .enumerate()
            .filter(|(_, page)| Some(page.session_id.as_str()) != dialog_session)
            .map(|(index, page)| (index, page.session_id.clone()))
            .collect();
        let client = &self.client;
        stream::iter(sessions)
            .map(|(index, session)| {
                let client = client.clone();
                async move { (index, observe_page(&client, &session, expression).await) }
            })
            .buffer_unordered(OBSERVED_AT_ONCE)
            .take_until(tokio::time::sleep(OBSERVATION_BOUND))
            .collect()
            .await
    }

    /// Reads every open page's current title. Chrome reports a title when a
    /// tab opens or navigates, not when its page sets another one (a mailbox
    /// counting unread mail, a page titling itself once loaded), so the title
    /// known for a background tab is otherwise stale. A page that does not
    /// answer keeps its last known title; one a dialog pauses cannot change
    /// its title until the dialog is resolved.
    pub(crate) async fn observe_titles(&mut self, dialog_session: Option<&str>) {
        let observations = self
            .observe_pages(&format!("({{title:{TITLE_EXPRESSION}}})"), dialog_session)
            .await;
        self.record_titles(&observations);
    }

    /// Keeps the title each answering page reported.
    pub(super) fn record_titles(&mut self, observations: &[(usize, Result<Value, String>)]) {
        for (index, page) in observations {
            if let Some(title) = page.as_ref().ok().and_then(|page| page["title"].as_str()) {
                self.pages[*index].title = title.to_string();
            }
        }
    }

    /// The open tabs listed by a refusal or failure whose recovery is selecting
    /// a tab explicitly (`browser_active_page_ambiguous`, `tab_gone`,
    /// `tab_closed_during_command`, `tab_not_found`), bounded unlike
    /// `tab_list` (see `roster`). The active tab is the one commands act on;
    /// none is while a pinned tab is gone.
    pub(crate) fn tab_roster(&self) -> Value {
        roster(
            &self.pages,
            (!self.bound_target_is_gone()).then_some(self.active_page_index),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Each page's title is read in its observation realm and kept. A page
    /// whose renderer never answers keeps its last known title without
    /// holding the read past its bound, and the page a dialog pauses is not
    /// asked at all.
    #[tokio::test]
    async fn titles_are_read_from_each_page_within_the_bound() {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (asked_tx, mut asked) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let command: Value = serde_json::from_str(&text).unwrap();
                let session = command["sessionId"].as_str().unwrap().to_string();
                asked_tx.send(session.clone()).unwrap();
                if session == "session-2" {
                    continue; // A busy renderer answers nothing.
                }
                let result = match command["method"].as_str().unwrap() {
                    "Page.getFrameTree" => json!({"frameTree": {"frame": {"id": "F"}}}),
                    "Page.createIsolatedWorld" => json!({"executionContextId": 7}),
                    "Runtime.evaluate" => {
                        assert_eq!(command["params"]["contextId"], 7);
                        json!({"result": {"type": "object", "value": {"title": "Inbox (4) - Acme Mail"}}})
                    }
                    method => panic!("unexpected {method}"),
                };
                let reply = json!({"id": command["id"], "result": result});
                ws.send(Message::Text(reply.to_string())).await.unwrap();
            }
        });
        let mut manager = super::super::tests::test_manager(vec![
            tab(1, None, "Inbox", "https://mail.example/"),
            tab(2, None, "Report", "https://reports.example/"),
            tab(3, None, "Checkout", "https://shop.example/"),
        ])
        .await;
        manager.client = std::sync::Arc::new(
            CdpClient::connect(&format!("ws://{address}"))
                .await
                .unwrap(),
        );

        let started = std::time::Instant::now();
        manager.observe_titles(Some("session-3")).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < OBSERVATION_BOUND + Duration::from_secs(1),
            "{elapsed:?}"
        );
        let titles: Vec<_> = manager
            .tab_list()
            .iter()
            .map(|tab| tab["title"].clone())
            .collect();
        assert_eq!(titles, ["Inbox (4) - Acme Mail", "Report", "Checkout"]);
        let mut sessions = Vec::new();
        while let Ok(session) = asked.try_recv() {
            sessions.push(session);
        }
        assert!(sessions.contains(&"session-2".to_string()));
        assert!(!sessions.contains(&"session-3".to_string()), "{sessions:?}");
    }
}
