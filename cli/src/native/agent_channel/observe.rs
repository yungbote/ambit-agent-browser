//! What a frame reads after its last executed step (agent-channel contract
//! §5 "observation" and "Candidate", §4 "resolve"): the interactive nodes the
//! page offers next, and the nodes the host's selectors resolve to.
//!
//! Candidates are the interactive nodes whose box meets the band, the layout
//! viewport extended by one viewport height above and below: those inside
//! the viewport in document order, then the rest by distance. Their facts are
//! read in the channel's isolated world by the function the effect ceiling
//! judges targets with; their role, name, value and states are the
//! accessibility tree's, as a snapshot projects them.
//!
//! Refs are stable: a node keeps the ref it has, and a new node gets the next
//! number. Nothing here renumbers or drops a ref.

use futures_util::future::join_all;
use serde::Deserialize;
use serde_json::{json, Value};

use super::target::{self, Accessibility, NodeFacts, Recorders, FACTS};
use super::CANDIDATES;
use crate::native::actions::DaemonState;
use crate::native::cdp::client::CdpClient;

/// Elements whose accessibility is read for an observation: the candidates
/// and a margin for those the tree ignores.
const READ: usize = CANDIDATES + 30;
/// Frames an observation lists.
const FRAMES: usize = 16;
/// The longest name or value a candidate carries, in characters.
const TEXT: usize = 200;

/// Lists the interactive elements in the band in their order, keeps the
/// first `READ` of them for the host to read, and measures the frames and
/// canvases the band shows.
fn scan() -> String {
    format!(
        r#"(() => {{
    const facts = {FACTS};
    const width = Math.max(1, innerWidth), height = Math.max(1, innerHeight);
    const roles = new Set(['button', 'link', 'textbox', 'checkbox', 'radio', 'combobox', 'listbox',
        'menuitem', 'menuitemcheckbox', 'menuitemradio', 'option', 'searchbox', 'slider',
        'spinbutton', 'switch', 'tab', 'treeitem']);
    const meets = (r) => r.width > 0 && r.height > 0 && r.bottom > -height && r.top < 2 * height
        && r.right > 0 && r.left < width;
    const structural = (el) => {{
        const tag = el.localName;
        if (tag === 'a' || tag === 'area') return el.hasAttribute('href');
        if (tag === 'button' || tag === 'select' || tag === 'textarea' || tag === 'summary') return true;
        if (tag === 'input') return String(el.type).toLowerCase() !== 'hidden';
        const role = ((el.getAttribute('role') || '').trim().split(/\s+/)[0] || '').toLowerCase();
        if (roles.has(role)) return true;
        if (el.isContentEditable && !(el.parentElement && el.parentElement.isContentEditable)) return true;
        const tabindex = el.getAttribute('tabindex');
        return (tabindex !== null && tabindex !== '-1') || el.hasAttribute('onclick');
    }};
    const pointer = (el) => {{
        if (getComputedStyle(el).cursor !== 'pointer') return false;
        const parent = el.parentElement;
        return !parent || getComputedStyle(parent).cursor !== 'pointer';
    }};
    const listed = [];
    const visit = (root) => {{
        const walker = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT);
        for (let el = walker.nextNode(); el; el = walker.nextNode()) {{
            if (el.shadowRoot) visit(el.shadowRoot);
            if (el.localName === 'iframe' || el.localName === 'frame') continue;
            const known = structural(el);
            if (!known && (el.localName === 'html' || el.localName === 'body')) continue;
            const box = el.getBoundingClientRect();
            if (!meets(box)) continue;
            if (!known && !pointer(el)) continue;
            const style = getComputedStyle(el);
            if (style.visibility === 'hidden' || style.visibility === 'collapse') continue;
            const inside = box.bottom > 0 && box.top < height && box.right > 0 && box.left < width;
            const distance = inside ? 0 : box.top >= height ? box.top - height : -box.bottom;
            listed.push({{ el, inside, distance, order: listed.length }});
        }}
    }};
    visit(document.documentElement);
    listed.sort((a, b) => a.inside !== b.inside ? (a.inside ? -1 : 1)
        : a.inside ? a.order - b.order : (a.distance - b.distance) || (a.order - b.order));
    const kept = listed.slice(0, {READ}).map((item) => item.el);
    globalThis.__ambitObserved = kept;
    const frames = [];
    for (const frame of document.querySelectorAll('iframe, frame')) {{
        const box = frame.getBoundingClientRect();
        if (!meets(box)) continue;
        let origin = '';
        try {{ origin = new URL(frame.getAttribute('src') || 'about:blank', document.baseURI).href; }} catch (_) {{}}
        frames.push({{ origin, box: {{ x: box.x, y: box.y, width: box.width, height: box.height }} }});
        if (frames.length === {FRAMES}) break;
    }}
    const canvases = Array.from(document.querySelectorAll('canvas'), (canvas) => canvas.getBoundingClientRect())
        .filter((box) => box.width > 0 && box.height > 0);
    let covered = 0;
    if (canvases.length) {{
        for (let i = 0; i < 32; i++) for (let j = 0; j < 32; j++) {{
            const x = (i + 0.5) * width / 32, y = (j + 0.5) * height / 32;
            if (canvases.some((box) => x >= box.left && x < box.right && y >= box.top && y < box.bottom)) covered++;
        }}
    }}
    return {{ viewport: {{ width: Math.round(width), height: Math.round(height) }},
        total: listed.length, facts: kept.map(facts), frames, canvasCoverage: covered / 1024 }};
}})()"#
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Scan {
    viewport: Value,
    total: usize,
    facts: Vec<NodeFacts>,
    frames: Vec<ScannedFrame>,
    canvas_coverage: f64,
}

#[derive(Deserialize)]
struct ScannedFrame {
    origin: String,
    #[serde(rename = "box")]
    bounds: Value,
}

/// A node as a candidate: its ref, identity, box, accessibility and facts.
/// A secret field's value is never in it.
fn candidate(ref_id: &str, backend_node_id: i64, facts: &NodeFacts, ax: &Accessibility) -> Value {
    let name = if ax.name.trim().is_empty() {
        facts.text.as_str()
    } else {
        ax.name.as_str()
    };
    let mut candidate = json!({
        "ref": ref_id,
        "backendNodeId": backend_node_id,
        "role": ax.role,
        "name": target::bounded(name, TEXT),
        "states": ax.states,
        "box": rounded(&facts.bounds),
    });
    if let Some(value) = ax.value.as_deref().filter(|_| !facts.target.secret) {
        candidate["value"] = json!(target::bounded(value, TEXT));
    }
    if let Some(effect) = facts.effect() {
        candidate["effect"] = effect;
    }
    if facts.target.secret {
        candidate["secret"] = json!(true);
    }
    if let Some(origin) = &facts.frame_origin {
        candidate["frameOrigin"] = json!(crate::native::browser::url_origin(origin));
    }
    candidate
}

fn rounded(bounds: &super::frame::Bounds) -> Value {
    let round = |value: f64| (value * 100.0).round() / 100.0;
    json!({ "x": round(bounds.x), "y": round(bounds.y), "width": round(bounds.width),
        "height": round(bounds.height) })
}

/// The page the browser shows now: its client, session and document.
struct Page {
    client: std::sync::Arc<CdpClient>,
    session: String,
    document: Option<String>,
}

fn page(state: &DaemonState) -> Result<Page, &'static str> {
    let browser = state.browser.as_ref().ok_or("no_active_page")?;
    let session = browser
        .active_session_id()
        .map_err(|_| "no_active_page")?
        .to_string();
    Ok(Page {
        client: browser.client.clone(),
        session,
        document: None,
    })
}

/// The interactive nodes the page offers next. Observing counts as looking
/// at the page for the daemon's gates, as a snapshot does.
pub(crate) async fn observation(state: &mut DaemonState, recorders: &Recorders) -> Value {
    match observe(state, recorders).await {
        Ok(observation) => {
            state.browser_control.lock().await.observed();
            observation
        }
        Err(code) => json!({ "status": "unavailable", "code": code }),
    }
}

async fn observe(state: &mut DaemonState, recorders: &Recorders) -> Result<Value, &'static str> {
    let mut page = page(state)?;
    let client = page.client.clone();
    page.document = crate::native::element::page_document(&client, &page.session).await;
    let context = recorders
        .world(&client, &page.session)
        .await
        .map_err(|_| "browser_observation_unavailable")?;
    let scanned = client
        .send_command(
            "Runtime.evaluate",
            Some(json!({ "expression": scan(), "contextId": context, "returnByValue": true })),
            Some(&page.session),
        )
        .await
        .map_err(|_| "browser_observation_unavailable")?;
    if scanned.get("exceptionDetails").is_some() {
        return Err("browser_observation_unavailable");
    }
    let scan: Scan = serde_json::from_value(scanned["result"]["value"].clone())
        .map_err(|_| "browser_observation_unavailable")?;
    let elements = observed_elements(&client, &page.session, context)
        .await
        .map_err(|_| "browser_observation_unavailable")?;
    let accessibility = join_all(
        elements
            .iter()
            .map(|object| accessibility_of(&client, &page.session, object.as_deref())),
    )
    .await;
    let mut candidates = Vec::new();
    let mut ignored = 0;
    for (facts, read) in scan.facts.iter().zip(accessibility) {
        let Some((backend_node_id, ax)) = read else {
            continue;
        };
        if ax.ignored {
            ignored += 1;
            continue;
        }
        if candidates.len() == CANDIDATES {
            continue;
        }
        let ref_id = state.ref_map.observed(
            backend_node_id,
            None,
            (&ax.role, &ax.name),
            page.document.as_deref(),
        );
        candidates.push(candidate(&ref_id, backend_node_id, facts, &ax));
    }
    let _ = client
        .send_command(
            "Runtime.releaseObjectGroup",
            Some(json!({ "objectGroup": target::WORLD })),
            Some(&page.session),
        )
        .await;
    let frames: Vec<Value> = scan
        .frames
        .iter()
        .map(|frame| {
            json!({ "origin": crate::native::browser::url_origin(&frame.origin),
                "box": frame.bounds, "observed": false })
        })
        .collect();
    Ok(json!({
        "viewport": scan.viewport,
        "candidates": candidates,
        "omitted": scan.total.saturating_sub(ignored + candidates.len()),
        "frames": frames,
        "canvasCoverage": scan.canvas_coverage.clamp(0.0, 1.0),
    }))
}

/// The handles of the elements the scan kept, in its order.
async fn observed_elements(
    client: &CdpClient,
    session: &str,
    context: i64,
) -> Result<Vec<Option<String>>, String> {
    let array = client
        .send_command(
            "Runtime.evaluate",
            Some(
                json!({ "expression": "globalThis.__ambitObserved", "contextId": context,
                "objectGroup": target::WORLD }),
            ),
            Some(session),
        )
        .await?;
    let Some(array) = array["result"]["objectId"].as_str() else {
        return Ok(Vec::new());
    };
    let properties = client
        .send_command(
            "Runtime.getProperties",
            Some(json!({ "objectId": array, "ownProperties": true })),
            Some(session),
        )
        .await?;
    let mut elements = Vec::new();
    for property in properties["result"].as_array().into_iter().flatten() {
        let Some(index) = property["name"]
            .as_str()
            .and_then(|name| name.parse::<usize>().ok())
        else {
            continue;
        };
        if elements.len() <= index {
            elements.resize(index + 1, None);
        }
        elements[index] = property["value"]["objectId"].as_str().map(str::to_string);
    }
    Ok(elements)
}

/// An element's node id and accessibility, read from its handle.
async fn accessibility_of(
    client: &CdpClient,
    session: &str,
    object: Option<&str>,
) -> Option<(i64, Accessibility)> {
    let object = object?;
    let described = client
        .send_command(
            "DOM.describeNode",
            Some(json!({ "objectId": object })),
            Some(session),
        )
        .await
        .ok()?;
    let backend_node_id = described["node"]["backendNodeId"].as_i64()?;
    let ax = target::accessibility(client, session, backend_node_id)
        .await
        .unwrap_or(Accessibility {
            ignored: false,
            ..Accessibility::default()
        });
    Some((backend_node_id, ax))
}

/// Resolves each selector as a step resolves it, its first match: the node
/// as a candidate with how many nodes matched, or `{selector, matches: 0}`.
/// A resolved node keeps its ref or gets the next one.
pub(crate) async fn resolutions(
    state: &mut DaemonState,
    recorders: &Recorders,
    selectors: &[String],
) -> Value {
    let mut resolved = Vec::new();
    for selector in selectors {
        resolved.push(match resolve(state, recorders, selector).await {
            Some(resolution) => resolution,
            None => json!({ "selector": selector, "matches": 0 }),
        });
    }
    Value::Array(resolved)
}

async fn resolve(state: &mut DaemonState, recorders: &Recorders, selector: &str) -> Option<Value> {
    let page = page(state).ok()?;
    let client = page.client.clone();
    crate::native::element::set_active_frame(state.active_frame.as_ref());
    let (object, session) = crate::native::element::resolve_element_object_id(
        &client,
        &page.session,
        &state.ref_map,
        selector,
        &state.iframe_sessions,
    )
    .await
    .ok()?;
    let described = client
        .send_command(
            "DOM.describeNode",
            Some(json!({ "objectId": object })),
            Some(&session),
        )
        .await
        .ok()?;
    let backend_node_id = described["node"]["backendNodeId"].as_i64()?;
    let context = recorders.world(&client, &session).await.ok()?;
    let node = target::node(&client, &session, context, backend_node_id)
        .await
        .ok()?;
    let facts = target::facts(&client, &node).await.ok()?;
    let ax = target::accessibility(&client, &session, backend_node_id)
        .await
        .unwrap_or_default();
    let matches = matches(state, &client, &page.session, selector).await;
    // A node of a child frame with a session of its own is named by that
    // frame, so a step resolves its ref in the right session.
    let frame = (session != page.session)
        .then(|| {
            state
                .active_frame
                .as_ref()
                .map(|scope| scope.frame_id.clone())
        })
        .flatten();
    let document = crate::native::element::page_document(&client, &page.session).await;
    let ref_id = match crate::native::element::parse_ref(selector) {
        Some(existing) => existing,
        None => state.ref_map.observed(
            backend_node_id,
            frame.as_deref(),
            (&ax.role, &ax.name),
            document.as_deref(),
        ),
    };
    let mut resolution = candidate(&ref_id, backend_node_id, &facts, &ax);
    resolution["selector"] = json!(selector);
    resolution["matches"] = json!(matches.max(1));
    Some(resolution)
}

/// How many nodes a selector matches where a step looks for it: 1 for a ref,
/// otherwise its matches in the selected frame or the page.
async fn matches(state: &DaemonState, client: &CdpClient, page: &str, selector: &str) -> i64 {
    if crate::native::element::parse_ref(selector).is_some() {
        return 1;
    }
    let count = |root: &str| {
        match selector.strip_prefix("xpath=") {
        Some(xpath) => format!(
            "{root}.evaluate({}, {root}, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null).snapshotLength",
            serde_json::to_string(xpath).unwrap_or_default()
        ),
        None => format!(
            "{root}.querySelectorAll({}).length",
            serde_json::to_string(selector).unwrap_or_default()
        ),
    }
    };
    let read = match state.active_frame.as_ref() {
        Some(scope) => match state.iframe_sessions.get(&scope.frame_id) {
            Some(frame_session) => {
                client
                    .send_command(
                        "Runtime.evaluate",
                        Some(json!({ "expression": count("document"), "returnByValue": true })),
                        Some(frame_session),
                    )
                    .await
            }
            None => {
                let owner =
                    crate::native::element::frame_owner_object_id(client, page, &scope.frame_id)
                        .await;
                match owner {
                    Ok(owner) => {
                        client
                            .send_command(
                                "Runtime.callFunctionOn",
                                Some(json!({ "objectId": owner, "returnByValue": true,
                                    "functionDeclaration": format!("function() {{ const doc = this.contentDocument; return doc ? {} : 0; }}", count("doc")) })),
                                Some(page),
                            )
                            .await
                    }
                    Err(error) => Err(error),
                }
            }
        },
        None => {
            client
                .send_command(
                    "Runtime.evaluate",
                    Some(json!({ "expression": count("document"), "returnByValue": true })),
                    Some(page),
                )
                .await
        }
    };
    read.ok()
        .and_then(|read| read["result"]["value"].as_i64())
        .unwrap_or(1)
}
