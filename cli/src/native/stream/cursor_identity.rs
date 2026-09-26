//! The pointer's identity, for viewers that draw the pointer themselves.
//!
//! The display helper reports the cursor Chrome shows on the owned window
//! (XFixes), mapped to a CSS keyword. The workspace image has no cursor
//! theme, so Chrome draws its cursors from the X core font and several
//! keywords share one image: for those the helper can name only the class
//! keyword, and with `members` the keywords that image stands for (the
//! arrow stands for `default` alone; the X that shows when Chrome sets no
//! cursor stands for `help`, `not-allowed`, `copy` and eight more, and is
//! reported as `default` too). The hovered element's computed `cursor` in
//! the visible page picks the member of that image's class, so a person
//! under control sees the cursor the page asked for, and never one the
//! window does not show. Without DevTools (sign-in), past a cross-origin
//! frame or past the deadline, the class keyword stands. Identities reach
//! viewers in the order the helper reported them, and a refinement never
//! outlives a newer identity.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{broadcast, RwLock};

use super::StreamMedia;
use crate::native::cdp::client::CdpClient;

/// A cursor record larger than this is not forwarded (the helper bounds a
/// page's own cursor image at 4 KiB).
const MAX_CURSOR_RECORD: usize = 96 * 1024;

/// How long the hovered element's computed cursor may take. A busy renderer
/// then leaves the class keyword, which is what the window shows.
const REFINE_DEADLINE: Duration = Duration::from_millis(50);

/// For a helper that does not send `members`: the keywords it reports for a
/// class of cursors that share one image on the workspace image (CfT 152
/// under Xvfb, no Xcursor theme, as the helper's qualification measured
/// them), with every keyword each stands for. A keyword outside this table
/// names its cursor exactly. Such a helper reports the arrow and the X both
/// as `default`, so `default` here is their union; it is deleted with the
/// last helper that does not classify its images.
const CURSOR_CLASSES: &[(&str, &[&str])] = &[
    (
        "default",
        &[
            "default",
            "help",
            "not-allowed",
            "no-drop",
            "copy",
            "alias",
            "context-menu",
            "vertical-text",
            "zoom-in",
            "zoom-out",
            "nesw-resize",
            "nwse-resize",
        ],
    ),
    ("pointer", &["pointer", "grabbing"]),
    ("progress", &["progress", "wait"]),
    ("move", &["move", "all-scroll"]),
    ("ew-resize", &["ew-resize", "col-resize"]),
    ("ns-resize", &["ns-resize", "row-resize"]),
];

/// The computed `cursor` of the deepest hovered element in the visible page,
/// through open shadow roots and same-origin frames; null for a hidden page
/// or when nothing is hovered.
const HOVERED_CURSOR: &str = r#"(() => {
  if (document.visibilityState !== "visible") return null;
  let root = document, node = null;
  for (let depth = 0; depth < 16; depth++) {
    const hovered = root.querySelectorAll(":hover");
    const deepest = hovered[hovered.length - 1];
    if (!deepest) break;
    node = deepest;
    const inner = deepest.shadowRoot || deepest.contentDocument;
    if (!inner) break;
    root = inner;
  }
  const view = node && node.ownerDocument.defaultView;
  return view ? view.getComputedStyle(node).cursor : null;
})()"#;

/// Publishes the pointer's identity to the stream's viewers and keeps the
/// newest for viewers that connect later.
#[derive(Clone)]
pub(super) struct CursorIdentities(Arc<Inner>);

struct Inner {
    frame_tx: broadcast::Sender<String>,
    media: Arc<StreamMedia>,
    client: Arc<RwLock<Option<Arc<CdpClient>>>>,
    session: Arc<RwLock<Option<String>>>,
    /// The order of the newest identity observed; only it may be published.
    newest: std::sync::Mutex<u64>,
}

impl CursorIdentities {
    /// `client` and `session` are the stream's DevTools connection and the
    /// active page's session, read when a class keyword needs refining.
    pub(super) fn new(
        frame_tx: broadcast::Sender<String>,
        media: Arc<StreamMedia>,
        client: Arc<RwLock<Option<Arc<CdpClient>>>>,
        session: Arc<RwLock<Option<String>>>,
    ) -> Self {
        Self(Arc::new(Inner {
            frame_tx,
            media,
            client,
            session,
            newest: std::sync::Mutex::new(0),
        }))
    }

    /// One identity the helper reported at media time `ts`. A class keyword
    /// is refined off the capture path; any other identity is published at
    /// once.
    pub(super) fn observe(&self, identity: Value, ts: u64) {
        let Some((record, members)) = cursor_record(identity, ts) else {
            return;
        };
        let order = self.next_order();
        if members
            .iter()
            .all(|member| record["css"] == member.as_str())
        {
            self.publish(order, record);
            return;
        }
        let identities = self.clone();
        tokio::spawn(async move {
            let mut record = record;
            if let Some(keyword) = identities
                .hovered_cursor()
                .await
                .and_then(|computed| member(&members, &computed))
            {
                record["css"] = json!(keyword);
            }
            identities.publish(order, record);
        });
    }

    /// The display that reported the identities is gone: none is current,
    /// and a refinement still in flight is never published.
    pub(super) fn reset(&self) {
        let mut newest = self.0.newest.lock().unwrap_or_else(|e| e.into_inner());
        *newest += 1;
        self.0.media.set_cursor(None);
    }

    fn next_order(&self) -> u64 {
        let mut newest = self.0.newest.lock().unwrap_or_else(|e| e.into_inner());
        *newest += 1;
        *newest
    }

    fn publish(&self, order: u64, record: Value) {
        let message = record.to_string();
        let newest = self.0.newest.lock().unwrap_or_else(|e| e.into_inner());
        if *newest != order {
            return;
        }
        self.0.media.set_cursor(Some(message.clone()));
        let _ = self.0.frame_tx.send(message);
    }

    async fn hovered_cursor(&self) -> Option<String> {
        let client = self.0.client.read().await.clone()?;
        let session = self.0.session.read().await.clone()?;
        let evaluated = tokio::time::timeout(
            REFINE_DEADLINE,
            client.send_command(
                "Runtime.evaluate",
                Some(
                    json!({ "expression": HOVERED_CURSOR, "returnByValue": true, "silent": true }),
                ),
                Some(&session),
            ),
        )
        .await
        .ok()?
        .ok()?;
        evaluated["result"]["value"].as_str().map(str::to_owned)
    }
}

/// The most keywords one cursor image can stand for (CSS names 36).
const MAX_MEMBERS: usize = 64;

/// The viewer record for one helper cursor identity (`serial`, a CSS keyword
/// or null, and for a page's own cursor its `image`), with the keywords its
/// image stands for: the helper's `members`, or for a helper that sends none
/// this driver's table. A record of another shape or size is not forwarded.
fn cursor_record(identity: Value, ts: u64) -> Option<(Value, Vec<String>)> {
    let serial = identity["serial"]
        .as_u64()
        .filter(|serial| *serial <= u64::from(u32::MAX))?;
    let css = &identity["css"];
    let image = identity.get("image");
    let members = match (css, image, identity.get("members")) {
        (Value::String(keyword), None, None) if !keyword.is_empty() => class_members(keyword)
            .unwrap_or_default()
            .iter()
            .map(|member| member.to_string())
            .collect(),
        (Value::String(keyword), None, Some(Value::Array(members)))
            if !keyword.is_empty() && (1..=MAX_MEMBERS).contains(&members.len()) =>
        {
            members
                .iter()
                .map(|member| {
                    member
                        .as_str()
                        .filter(|member| {
                            (1..=32).contains(&member.len())
                                && member
                                    .bytes()
                                    .all(|byte| byte.is_ascii_lowercase() || byte == b'-')
                        })
                        .map(str::to_owned)
                })
                .collect::<Option<Vec<_>>>()?
        }
        (Value::Null, Some(image), None) if image.is_object() => Vec::new(),
        _ => return None,
    };
    let mut record = json!({ "type": "cursor", "ts": ts, "serial": serial, "css": css });
    if let Some(image) = image {
        record["image"] = image.clone();
    }
    Some((record, members)).filter(|(record, _)| record.to_string().len() <= MAX_CURSOR_RECORD)
}

/// The keywords a class keyword stands for, when it names a class.
fn class_members(keyword: &str) -> Option<&'static [&'static str]> {
    CURSOR_CLASSES
        .iter()
        .find(|(class, _)| *class == keyword)
        .map(|(_, members)| *members)
}

/// The member of a class a computed `cursor` names: its keyword, the last
/// entry of a list whose images did not load; `auto` shows the arrow. None
/// when the page asks for a cursor outside the class, so the window shows
/// something else and the class keyword stands.
fn member(members: &[String], computed: &str) -> Option<String> {
    let keyword = computed.rsplit(',').next()?.trim().to_ascii_lowercase();
    let keyword = if keyword == "auto" {
        "default"
    } else {
        keyword.as_str()
    };
    members.iter().find(|member| *member == keyword).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    #[test]
    fn only_a_keyword_or_an_image_is_a_cursor_identity() {
        assert!(cursor_record(json!({"serial": 1, "css": "pointer"}), 7).is_some());
        assert!(cursor_record(json!({"serial": 1, "css": null, "image": {}}), 7).is_some());
        for identity in [
            json!({"serial": 1, "css": "pointer", "image": {}}),
            json!({"serial": 1, "css": null}),
            json!({"serial": 1, "css": ""}),
            json!({"serial": 1}),
            json!({"serial": -1, "css": "text"}),
            json!({"serial": u64::from(u32::MAX) + 1, "css": "text"}),
            json!({"css": "text"}),
            json!("text"),
            json!({"serial": 1, "css": "default", "members": []}),
            json!({"serial": 1, "css": "default", "members": "default"}),
            json!({"serial": 1, "css": "default", "members": [1]}),
            json!({"serial": 1, "css": "default", "members": ["Not-Allowed"]}),
            json!({"serial": 1, "css": "default", "members": [""]}),
            json!({"serial": 1, "css": null, "image": {}, "members": ["default"]}),
        ] {
            assert!(cursor_record(identity.clone(), 7).is_none(), "{identity}");
        }
        let huge = "a".repeat(MAX_CURSOR_RECORD);
        assert!(
            cursor_record(json!({"serial": 1, "css": null, "image": {"png": huge}}), 7).is_none()
        );
    }

    /// A computed cursor refines a class keyword only to a member of that
    /// class: the page cannot make the viewer show a cursor the window does
    /// not.
    #[test]
    fn a_computed_cursor_picks_only_a_member_of_the_reported_class() {
        let refine = |class: &str, computed: &str| {
            let members = class_members(class)?;
            let owned: Vec<String> = members.iter().map(|member| member.to_string()).collect();
            let chosen = member(&owned, computed)?;
            members.iter().copied().find(|member| *member == chosen)
        };
        assert_eq!(refine("default", "not-allowed"), Some("not-allowed"));
        assert_eq!(refine("default", "help"), Some("help"));
        assert_eq!(refine("default", "auto"), Some("default"));
        assert_eq!(
            refine("default", "url(\"hand.png\") 4 4, zoom-in"),
            Some("zoom-in")
        );
        assert_eq!(refine("pointer", "grabbing"), Some("grabbing"));
        assert_eq!(refine("progress", "WAIT"), Some("wait"));
        assert_eq!(refine("move", "all-scroll"), Some("all-scroll"));
        assert_eq!(refine("ew-resize", "col-resize"), Some("col-resize"));
        assert_eq!(refine("ns-resize", "row-resize"), Some("row-resize"));
        // Outside the class the window shows something else.
        assert_eq!(refine("default", "pointer"), None);
        assert_eq!(refine("pointer", "auto"), None);
        assert_eq!(refine("progress", "text"), None);
        // Exact keywords are not classes.
        for exact in ["text", "crosshair", "grab", "cell", "none", "n-resize"] {
            assert!(class_members(exact).is_none(), "{exact}");
        }
        // Every member is a CSS keyword of exactly one class.
        let mut seen = std::collections::HashSet::new();
        for (class, members) in CURSOR_CLASSES {
            assert!(members.contains(class), "{class}");
            for member in *members {
                assert!(seen.insert(*member), "{member} in two classes");
            }
        }
    }

    /// A DevTools endpoint that answers each `Runtime.evaluate` with the next
    /// scripted computed cursor after its delay; `None` never answers.
    async fn devtools(
        answers: Vec<(u64, Option<&'static str>)>,
    ) -> (Arc<CdpClient>, tokio::sync::mpsc::UnboundedReceiver<Value>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (seen, requests) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let socket = tokio_tungstenite::accept_async(socket).await.unwrap();
            let (sink, mut source) = socket.split();
            let sink = Arc::new(tokio::sync::Mutex::new(sink));
            let mut answers = answers.into_iter();
            while let Some(Ok(Message::Text(text))) = source.next().await {
                let request: Value = serde_json::from_str(&text).unwrap();
                let _ = seen.send(request.clone());
                let Some((delay, Some(cursor))) = answers.next() else {
                    continue;
                };
                let sink = sink.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    let reply = json!({"id": request["id"], "sessionId": request["sessionId"],
                        "result": {"result": {"type": "string", "value": cursor}}});
                    let _ = sink
                        .lock()
                        .await
                        .send(Message::Text(reply.to_string()))
                        .await;
                });
            }
        });
        let client = CdpClient::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        (Arc::new(client), requests)
    }

    fn identities(
        client: Option<Arc<CdpClient>>,
    ) -> (
        CursorIdentities,
        broadcast::Receiver<String>,
        Arc<StreamMedia>,
    ) {
        let (frame_tx, messages) = broadcast::channel(16);
        let media = Arc::new(StreamMedia::new(Default::default()));
        let identities = CursorIdentities::new(
            frame_tx,
            media.clone(),
            Arc::new(RwLock::new(client)),
            Arc::new(RwLock::new(Some("page".into()))),
        );
        (identities, messages, media)
    }

    async fn next(messages: &mut broadcast::Receiver<String>) -> Value {
        let message = tokio::time::timeout(Duration::from_secs(2), messages.recv())
            .await
            .expect("a cursor message")
            .unwrap();
        serde_json::from_str(&message).unwrap()
    }

    /// The helper's class keyword becomes the member the hovered element
    /// asked for, read from the active page; an exact keyword is published
    /// without asking the page.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_class_keyword_is_refined_from_the_hovered_element() {
        let (client, mut requests) = devtools(vec![(0, Some("not-allowed"))]).await;
        let (identities, mut messages, media) = identities(Some(client));
        identities.observe(json!({"serial": 4, "css": "default"}), 11);
        let message = next(&mut messages).await;
        assert_eq!(message["css"], "not-allowed");
        assert_eq!(
            (message["serial"].as_u64(), message["ts"].as_u64()),
            (Some(4), Some(11))
        );
        assert_eq!(
            serde_json::from_str::<Value>(&media.cursor().unwrap()).unwrap(),
            message
        );
        let request = requests.recv().await.unwrap();
        assert_eq!(request["method"], "Runtime.evaluate");
        assert_eq!(request["sessionId"], "page");

        identities.observe(json!({"serial": 5, "css": "text"}), 12);
        assert_eq!(next(&mut messages).await["css"], "text");
        assert!(
            requests.try_recv().is_err(),
            "an exact keyword asks nothing"
        );
    }

    /// The helper names the keywords each image stands for. The arrow is
    /// `default` alone, so it is never refined into the X's keywords (a
    /// scrollbar of a `not-allowed` element shows the arrow); the X, also
    /// reported as `default`, is refined within its own keywords only.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_default_is_refined_only_within_the_image_the_helper_names() {
        let (client, mut requests) =
            devtools(vec![(0, Some("not-allowed")), (0, Some("pointer"))]).await;
        let (identities, mut messages, _media) = identities(Some(client));
        let x_cursor = json!([
            "context-menu",
            "help",
            "vertical-text",
            "alias",
            "copy",
            "no-drop",
            "not-allowed",
            "nesw-resize",
            "nwse-resize",
            "zoom-in",
            "zoom-out"
        ]);
        // The arrow over a `not-allowed` element's scrollbar.
        identities.observe(
            json!({"serial": 4, "css": "default", "members": ["default"]}),
            1,
        );
        assert_eq!(next(&mut messages).await["css"], "default");
        assert!(requests.try_recv().is_err(), "the arrow asks nothing");
        // The X over the element itself.
        identities.observe(
            json!({"serial": 5, "css": "default", "members": x_cursor}),
            2,
        );
        let refined = next(&mut messages).await;
        assert_eq!(
            (refined["serial"].as_u64(), refined["css"].as_str()),
            (Some(5), Some("not-allowed"))
        );
        assert!(refined.get("members").is_none(), "{refined}");
        // A page cursor outside the X's keywords leaves the class keyword.
        identities.observe(
            json!({"serial": 6, "css": "default", "members": x_cursor}),
            3,
        );
        assert_eq!(next(&mut messages).await["css"], "default");
    }

    /// A refinement never lands after a newer identity, and one the page
    /// does not answer in time leaves the class keyword.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinements_keep_the_helpers_order_and_their_deadline() {
        let (client, _requests) = devtools(vec![(150, Some("grabbing")), (0, None)]).await;
        let (identities, mut messages, media) = identities(Some(client));
        identities.observe(json!({"serial": 6, "css": "pointer"}), 1);
        identities.observe(json!({"serial": 7, "css": "text"}), 2);
        assert_eq!(next(&mut messages).await["serial"], 7);
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            messages.try_recv().is_err(),
            "the older refinement was dropped"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&media.cursor().unwrap()).unwrap()["serial"],
            7
        );

        let started = std::time::Instant::now();
        identities.observe(json!({"serial": 8, "css": "progress"}), 3);
        let message = next(&mut messages).await;
        assert_eq!(message["css"], "progress");
        assert!(
            started.elapsed() < REFINE_DEADLINE * 4,
            "{:?}",
            started.elapsed()
        );

        // A display that goes away takes its identity and any refinement
        // still in flight with it.
        identities.observe(json!({"serial": 9, "css": "default"}), 4);
        identities.reset();
        assert!(media.cursor().is_none());
        tokio::time::sleep(REFINE_DEADLINE * 2).await;
        assert!(messages.try_recv().is_err(), "nothing of the old display");
        assert!(media.cursor().is_none());
    }

    /// Without DevTools (a sign-in window) the class keyword stands.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn without_devtools_the_class_keyword_stands() {
        let (identities, mut messages, _media) = identities(None);
        identities.observe(json!({"serial": 10, "css": "default"}), 1);
        assert_eq!(next(&mut messages).await["css"], "default");
    }
}
