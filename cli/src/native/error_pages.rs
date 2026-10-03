//! Chrome's error pages. A main frame whose load fails commits Chrome's own
//! document in its place, under the `chrome-error:` scheme: for a refused
//! connection, a certificate Chrome does not trust, a Safe Browsing verdict
//! or an HTTPS-only upgrade alike. Nothing on such a page is the agent's to
//! press or type, and some of its controls decide what is the person's alone
//! (proceeding past a warning), so agent input is refused while one shows
//! (`ErrorPage::refused`). The scheme alone decides: a page's title and text
//! are localized and change. Navigation stays the agent's.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use serde_json::{json, Value};

use super::actions::CommandError;

/// The code of agent input refused while an error page shows.
pub(crate) const ERROR_PAGE: &str = "browser_error_page";

/// The error page a page's main frame shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ErrorPage {
    /// The address whose load failed: the error document's `unreachableUrl`.
    pub(crate) url: Option<String>,
    /// Chrome's net error for that load (`net::ERR_CERT_AUTHORITY_INVALID`),
    /// when its failure was seen.
    pub(crate) error: Option<String>,
    /// The failed navigation's loader. A navigation's document request has
    /// its loader's id, which binds the request's failure to this page.
    loader: String,
}

impl ErrorPage {
    /// The page as a clause: "Chrome's error page for <url> (<error>)".
    pub(crate) fn describe(&self) -> String {
        let mut text = "Chrome's error page".to_string();
        if let Some(url) = &self.url {
            text += &format!(" for {url}");
        }
        if let Some(error) = &self.error {
            text += &format!(" ({error})");
        }
        text
    }

    /// Agent input refused while this page shows: nothing of it was sent.
    pub(crate) fn refused(&self) -> CommandError {
        CommandError::with_data(
            format!(
                "{ERROR_PAGE}: {} is showing, and agent input is refused on it. Open another address, reload, or ask the person to decide.",
                self.describe()
            ),
            json!({ "failedUrl": self.url, "netError": self.error }),
        )
    }
}

/// What one page session has shown and failed to load.
#[derive(Default)]
struct Page {
    showing: Option<ErrorPage>,
    /// The latest of the session's document requests that failed: its id
    /// and net error, until the main frame commits.
    failed: Option<(String, String)>,
}

/// Each page session's error page, kept from its events as they arrive.
#[derive(Default)]
pub(crate) struct ErrorPages(Mutex<HashMap<String, Page>>);

impl ErrorPages {
    /// Notes one event from `session` (`None` for the browser's own).
    pub(crate) fn observe(&self, method: &str, params: &Value, session: Option<&str>) {
        let mut pages = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        match (method, session) {
            ("Page.frameNavigated", Some(session))
                if params["frame"]["parentId"]
                    .as_str()
                    .is_none_or(str::is_empty) =>
            {
                let frame = &params["frame"];
                let page = pages.entry(session.to_owned()).or_default();
                let failed = page.failed.take();
                page.showing = frame["url"]
                    .as_str()
                    .is_some_and(is_error_document)
                    .then(|| {
                        let loader = frame["loaderId"].as_str().unwrap_or_default().to_owned();
                        ErrorPage {
                            url: frame["unreachableUrl"].as_str().map(str::to_owned),
                            error: failed
                                .filter(|(request, _)| *request == loader)
                                .map(|(_, error)| error),
                            loader,
                        }
                    });
            }
            ("Network.loadingFailed", Some(session)) if params["type"] == "Document" => {
                let (Some(request), Some(error)) =
                    (params["requestId"].as_str(), params["errorText"].as_str())
                else {
                    return;
                };
                let page = pages.entry(session.to_owned()).or_default();
                match page.showing.as_mut() {
                    // The failure reached the reader after the error page.
                    Some(shown) if shown.loader == request && shown.error.is_none() => {
                        shown.error = Some(error.to_owned());
                    }
                    _ => page.failed = Some((request.to_owned(), error.to_owned())),
                }
            }
            ("Target.detachedFromTarget", None) => {
                if let Some(session) = params["sessionId"].as_str() {
                    pages.remove(session);
                }
            }
            _ => {}
        }
    }

    /// The error page the main frame of the page attached as `page` shows.
    pub(crate) fn showing(&self, page: &str) -> Option<ErrorPage> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(page)
            .and_then(|page| page.showing.clone())
    }
}

/// Whether `url` is one of Chrome's error documents, by its scheme alone.
fn is_error_document(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|url| url.scheme() == "chrome-error")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAILED: &str = "https://self-signed.test/";
    const ERROR: &str = "net::ERR_CERT_AUTHORITY_INVALID";

    fn navigated(pages: &ErrorPages, frame: Value) {
        pages.observe(
            "Page.frameNavigated",
            &json!({ "frame": frame, "type": "Navigation" }),
            Some("page"),
        );
    }

    fn error_document(loader: &str) -> Value {
        json!({ "id": "main", "loaderId": loader, "url": "chrome-error://chromewebdata/",
            "unreachableUrl": FAILED })
    }

    fn failed(pages: &ErrorPages, request: &str, kind: &str) {
        pages.observe(
            "Network.loadingFailed",
            &json!({ "requestId": request, "type": kind, "errorText": ERROR }),
            Some("page"),
        );
    }

    fn shown(error: Option<&str>) -> Option<ErrorPage> {
        Some(ErrorPage {
            url: Some(FAILED.into()),
            error: error.map(str::to_owned),
            loader: "L1".into(),
        })
    }

    #[test]
    fn an_error_document_shows_with_the_failure_of_its_own_navigation_in_either_order() {
        let pages = ErrorPages::default();
        failed(&pages, "L1", "Document");
        navigated(&pages, error_document("L1"));
        assert_eq!(pages.showing("page"), shown(Some(ERROR)));

        let pages = ErrorPages::default();
        navigated(&pages, error_document("L1"));
        assert_eq!(pages.showing("page"), shown(None));
        failed(&pages, "L1", "Document");
        assert_eq!(pages.showing("page"), shown(Some(ERROR)));
    }

    #[test]
    fn another_load_failure_never_names_the_page_error() {
        let pages = ErrorPages::default();
        failed(&pages, "L0", "Document");
        failed(&pages, "R7", "Image");
        navigated(&pages, error_document("L1"));
        failed(&pages, "R8", "Script");
        assert_eq!(pages.showing("page"), shown(None));
    }

    #[test]
    fn the_scheme_alone_decides_and_a_document_commit_ends_the_page() {
        let pages = ErrorPages::default();
        navigated(
            &pages,
            json!({ "id": "main", "loaderId": "L0", "url": "https://warning.test/privacy-error" }),
        );
        assert_eq!(pages.showing("page"), None);
        navigated(
            &pages,
            json!({ "id": "main", "loaderId": "L1", "url": "CHROME-ERROR://chromewebdata/" }),
        );
        assert!(pages.showing("page").is_some());
        navigated(
            &pages,
            json!({ "id": "main", "loaderId": "L2", "url": FAILED }),
        );
        assert_eq!(pages.showing("page"), None);
    }

    #[test]
    fn a_child_frame_error_or_another_session_leaves_the_page_alone() {
        let pages = ErrorPages::default();
        navigated(
            &pages,
            json!({ "id": "child", "parentId": "main", "loaderId": "L1",
                "url": "chrome-error://chromewebdata/" }),
        );
        assert_eq!(pages.showing("page"), None);
        navigated(&pages, error_document("L1"));
        assert_eq!(pages.showing("other"), None);
    }

    #[test]
    fn a_detached_session_is_forgotten() {
        let pages = ErrorPages::default();
        navigated(&pages, error_document("L1"));
        pages.observe(
            "Target.detachedFromTarget",
            &json!({ "sessionId": "page" }),
            None,
        );
        assert_eq!(pages.showing("page"), None);
    }

    #[test]
    fn the_refusal_names_what_failed_and_what_the_agent_can_do() {
        let refusal = shown(Some(ERROR)).unwrap().refused();
        assert_eq!(
            refusal.error,
            format!("browser_error_page: Chrome's error page for {FAILED} ({ERROR}) is showing, and agent input is refused on it. Open another address, reload, or ask the person to decide.")
        );
        assert_eq!(
            refusal.data,
            Some(json!({ "failedUrl": FAILED, "netError": ERROR }))
        );
        let unknown = ErrorPage {
            url: None,
            error: None,
            loader: String::new(),
        }
        .refused();
        assert!(unknown.error.starts_with(
            "browser_error_page: Chrome's error page is showing, and agent input is refused on it."
        ));
        assert_eq!(
            unknown.data,
            Some(json!({ "failedUrl": null, "netError": null }))
        );
    }
}
