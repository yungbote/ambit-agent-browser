//! Whether a page is between documents: its main frame began a navigation
//! to another document that has neither committed nor stopped.
//!
//! In that window Chrome holds every command the page's renderer answers
//! (Runtime, DOM, `Page.getFrameTree`, `Page.setInterceptFileChooserDialog`)
//! until the navigation commits, or until it stops without committing;
//! input, captures and target queries answer at once. Measured on Chrome 149
//! for links, form posts, redirects, reloads, history, script and
//! `Page.navigate` navigations, 204 responses, downloads, stops and
//! superseded navigations, same-site and cross-site; a child frame's
//! navigation holds nothing its page answers. So whatever must answer
//! within a bound does not read a page between documents.

use std::collections::HashSet;

use super::cdp::types::CdpEvent;

/// What one of a page's own events says about its document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    /// A navigation to another document began.
    Leaving,
    /// The page holds a document again: one committed, or the navigation
    /// stopped without one (cancelled, a 204, a download, superseded).
    Settled,
}

/// What `event`, from the page's own session, says about the document of
/// the page whose main frame is `main_frame`.
pub(crate) fn change(event: &CdpEvent, main_frame: &str) -> Option<Change> {
    let params = &event.params;
    match event.method.as_str() {
        "Page.frameStartedNavigating" if params["frameId"] == main_frame => {
            let within = matches!(
                params["navigationType"].as_str(),
                Some("sameDocument" | "historySameDocument")
            );
            (!within).then_some(Change::Leaving)
        }
        "Page.frameNavigated" if params["frame"]["id"] == main_frame => Some(Change::Settled),
        "Page.frameStoppedLoading" if params["frameId"] == main_frame => Some(Change::Settled),
        _ => None,
    }
}

/// The pages between documents, by session.
#[derive(Debug, Default)]
pub(crate) struct Documents(HashSet<String>);

impl Documents {
    /// Notes an event from the page of `session`, whose main frame is
    /// `main_frame`.
    pub(crate) fn note(&mut self, session: &str, main_frame: &str, event: &CdpEvent) {
        match change(event, main_frame) {
            Some(Change::Leaving) => {
                self.0.insert(session.to_string());
            }
            Some(Change::Settled) => {
                self.0.remove(session);
            }
            None => {}
        }
    }

    /// The page of `session` went away.
    pub(crate) fn forget(&mut self, session: &str) {
        self.0.remove(session);
    }

    pub(crate) fn between(&self, session: &str) -> bool {
        self.0.contains(session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    const MAIN: &str = "MAIN";

    fn event(method: &str, params: Value) -> CdpEvent {
        CdpEvent {
            method: method.into(),
            params,
            session_id: Some("S".into()),
        }
    }

    fn started(frame: &str, kind: &str) -> CdpEvent {
        event(
            "Page.frameStartedNavigating",
            json!({ "frameId": frame, "navigationType": kind }),
        )
    }

    fn noted(events: &[CdpEvent]) -> bool {
        let mut documents = Documents::default();
        for event in events {
            documents.note("S", MAIN, event);
        }
        documents.between("S")
    }

    #[test]
    fn a_navigation_to_another_document_leaves_it_until_the_commit() {
        for kind in [
            "differentDocument",
            "reload",
            "historyDifferentDocument",
            "restore",
        ] {
            assert!(noted(&[started(MAIN, kind)]), "{kind}");
        }
        let committed = event(
            "Page.frameNavigated",
            json!({ "frame": { "id": MAIN, "loaderId": "L2", "url": "https://shop.example/" } }),
        );
        assert!(!noted(&[started(MAIN, "differentDocument"), committed]));
    }

    #[test]
    fn a_navigation_that_stops_without_a_commit_settles_the_page() {
        // A 204, a download, a stop and a superseded navigation all stop the
        // main frame's loading; the superseding one then leaves again.
        let stopped = event("Page.frameStoppedLoading", json!({ "frameId": MAIN }));
        assert!(!noted(&[
            started(MAIN, "differentDocument"),
            stopped.clone()
        ]));
        assert!(noted(&[
            started(MAIN, "differentDocument"),
            stopped,
            started(MAIN, "differentDocument"),
        ]));
    }

    #[test]
    fn same_document_navigations_and_child_frames_leave_nothing() {
        assert!(!noted(&[started(MAIN, "sameDocument")]));
        assert!(!noted(&[started(MAIN, "historySameDocument")]));
        assert!(!noted(&[started("CHILD", "differentDocument")]));
        // A child frame's commit does not settle a page that is leaving.
        let child = event(
            "Page.frameNavigated",
            json!({ "frame": { "id": "CHILD", "parentId": MAIN, "loaderId": "L9" } }),
        );
        assert!(noted(&[started(MAIN, "differentDocument"), child]));
        // Loading alone, as a same-document history change reports it, is
        // not leaving.
        assert!(!noted(&[event(
            "Page.frameStartedLoading",
            json!({ "frameId": MAIN })
        )]));
    }

    #[test]
    fn a_page_that_went_away_is_forgotten() {
        let mut documents = Documents::default();
        documents.note("S", MAIN, &started(MAIN, "differentDocument"));
        assert!(documents.between("S"));
        documents.forget("S");
        assert!(!documents.between("S"));
    }
}
