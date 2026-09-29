//! A judged step's target and its preconditions (agent-channel contract §4
//! "Preconditions" and "Secret fields"). The checks run under the step's
//! command custody, after the custody and observation gates and before its
//! first input, in the contract's order: page identity, the node, its box,
//! then the effect ceiling on that node. A failed check refuses the step and
//! nothing is pressed or typed.
//!
//! What a node structurally is (`NodeFacts`) is read in the channel's
//! isolated world, where page script cannot redefine what is read, by the
//! same function the observation lists candidates with.

use std::collections::HashSet;
use std::sync::Mutex;

use serde::Deserialize;
use serde_json::{json, Value};

use super::ceiling::{self, TargetFacts};
use super::frame::{Bounds, Preconditions};
use super::step::{Judged, PreparedStep, Target};
use crate::native::actions::DaemonState;
use crate::native::cdp::client::CdpClient;

/// The isolated world the channel reads pages in, per frame.
pub(crate) const WORLD: &str = "agent-browser-channel";

/// Page sessions remembered as recording secret fields.
const RECORDED_SESSIONS: usize = 4096;

/// Records, from each document's start, the inputs that were password
/// fields at any moment: a field a page turns into plain text stays secret.
const RECORDER: &str = r#"(() => {
    if (globalThis.__ambitSecrets) return true;
    const seen = globalThis.__ambitSecrets = new WeakSet();
    const note = (el) => {
        if (el && el.localName === 'input' && String(el.type || '').toLowerCase() === 'password') seen.add(el);
    };
    const scan = (root) => { if (root.querySelectorAll) root.querySelectorAll('input').forEach(note); };
    const watch = () => {
        new MutationObserver((records) => {
            for (const record of records) {
                if (record.type === 'attributes') note(record.target);
                else for (const node of record.addedNodes) if (node.nodeType === 1) { note(node); scan(node); }
            }
        }).observe(document, { subtree: true, childList: true, attributes: true, attributeFilter: ['type'] });
        scan(document);
    };
    watch();
    return true;
})()"#;

/// The structural facts of one element (`NodeFacts`). A press on an element
/// inside a control activates that control, and a label its field, so those
/// are judged as the control they activate; the box stays the element's.
pub(crate) const FACTS: &str = r#"((el) => {
    const own = (node) => {
        const tag = node.localName || '';
        const attr = (name) => node.getAttribute ? node.getAttribute(name) : null;
        const role = ((attr('role') || '').trim().split(/\s+/)[0] || '').toLowerCase();
        const inputType = tag === 'input' ? String(node.type || 'text').toLowerCase() : null;
        const complete = (attr('autocomplete') || '').trim().toLowerCase().slice(0, 64);
        const tokens = complete ? complete.split(/\s+/) : [];
        const recorded = globalThis.__ambitSecrets;
        const secret = inputType === 'password'
            || tokens.some((t) => t === 'one-time-code' || t === 'current-password' || t === 'new-password' || t.startsWith('cc-'))
            || !!(recorded && recorded.has(node));
        const listed = tag === 'button' || tag === 'input' || tag === 'select' || tag === 'textarea';
        const form = listed ? node.form : (node.closest ? node.closest('form') : null);
        const link = (tag === 'a' || tag === 'area') && node.hasAttribute('href');
        const download = link && node.hasAttribute('download');
        const ids = (attr('aria-controls') || '').trim();
        const controls = !!ids && ids.split(/\s+/).some((id) => !!node.ownerDocument.getElementById(id));
        const submit = !!form && ((tag === 'button' && String(node.type || 'submit').toLowerCase() === 'submit')
            || (tag === 'input' && (inputType === 'submit' || inputType === 'image')));
        const editable = !!node.isContentEditable;
        const textRole = ['textbox', 'searchbox', 'combobox', 'spinbutton', 'slider'].includes(role);
        const field = (tag === 'input' && !['submit', 'image', 'button', 'reset', 'file', 'hidden'].includes(inputType))
            || tag === 'textarea' || tag === 'select' || editable || textRole;
        const kind = download ? 'download' : link ? 'link' : inputType === 'file' ? 'file'
            : submit ? 'submit' : field ? 'field'
            : (tag === 'summary' || attr('aria-expanded') !== null || controls) ? 'disclosure'
            : role === 'tab' ? 'tab' : 'other';
        let method = null;
        if (form && (submit || field)) {
            const given = submit && node.hasAttribute('formmethod') ? attr('formmethod') : form.getAttribute('method');
            const m = String(given || 'get').trim().toLowerCase();
            method = m === 'post' || m === 'dialog' ? m : 'get';
        }
        const action = link ? node.href : submit ? node.formAction : (form && field) ? form.action : null;
        return { kind, method, inputType, multiline: tag === 'textarea' || editable, secret,
            origin: action || null, autocomplete: complete || null, controls, download };
    };
    let facts = own(el);
    if (facts.kind === 'other' && el.closest) {
        const label = el.closest('label');
        const control = el.closest('a[href],area[href],button,summary,[role=button],[role=link],[role=tab],[role=menuitem]');
        const activated = control && control !== el ? control : (label && label.control) || null;
        if (activated) facts = own(activated);
    }
    const box = el.getBoundingClientRect();
    facts.box = { x: box.x, y: box.y, width: box.width, height: box.height };
    facts.text = (el.textContent || '').replace(/\s+/g, ' ').trim().slice(0, 200);
    const view = el.ownerDocument.defaultView;
    facts.frameOrigin = view && view !== view.top ? el.ownerDocument.location.href : null;
    return facts;
})"#;

/// Finds the deepest focused element, through open shadow roots and
/// same-origin frames.
pub(crate) const FOCUSED: &str = r#"(() => {
    let el = document.activeElement;
    for (;;) {
        if (el && el.shadowRoot && el.shadowRoot.activeElement) { el = el.shadowRoot.activeElement; continue; }
        if (el && (el.localName === 'iframe' || el.localName === 'frame')) {
            let inner = null;
            try { inner = el.contentDocument && el.contentDocument.activeElement; } catch (_) {}
            if (inner && inner !== el.contentDocument.body) { el = inner; continue; }
        }
        return el || document.body;
    }
})()"#;

/// Finds the deepest element under the pointer, as the page last saw it.
const POINTED: &str = r#"(() => {
    let doc = document, el = null;
    for (;;) {
        const hovered = doc.querySelectorAll(':hover');
        const deepest = hovered.length ? hovered[hovered.length - 1] : null;
        if (!deepest) return el;
        el = deepest;
        if (deepest.localName !== 'iframe' && deepest.localName !== 'frame') return el;
        try { if (!deepest.contentDocument) return el; doc = deepest.contentDocument; } catch (_) { return el; }
    }
})()"#;

/// What an element structurally is, and where it is.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NodeFacts {
    #[serde(flatten)]
    pub(crate) target: TargetFacts,
    /// For a link, its `href`; for a submit control or a form field, its
    /// form's action.
    pub(crate) origin: Option<String>,
    pub(crate) autocomplete: Option<String>,
    pub(crate) controls: bool,
    pub(crate) download: bool,
    /// Its border box in viewport CSS pixels.
    #[serde(rename = "box")]
    pub(crate) bounds: Bounds,
    /// Its text, whitespace collapsed, at most 200 characters.
    pub(crate) text: String,
    /// The URL of its document when that is a child frame's.
    #[serde(default)]
    pub(crate) frame_origin: Option<String>,
}

impl NodeFacts {
    /// The candidate's `effect`: the facts the host judges its own ceiling
    /// by, with the origin reduced as the tab roster reduces URLs.
    pub(crate) fn effect(&self) -> Option<Value> {
        let mut effect = serde_json::Map::new();
        if let Some(origin) = self.origin.as_deref() {
            let reduced = crate::native::browser::url_origin(origin);
            if !reduced.is_empty() {
                effect.insert("origin".into(), json!(reduced));
            }
        }
        if let Some(method) = self.target.method {
            effect.insert("method".into(), json!(method));
        }
        if let Some(input_type) = &self.target.input_type {
            effect.insert("inputType".into(), json!(input_type));
        }
        if let Some(autocomplete) = &self.autocomplete {
            effect.insert("autocomplete".into(), json!(autocomplete));
        }
        if self.controls {
            effect.insert("controls".into(), json!(true));
        }
        if self.download {
            effect.insert("download".into(), json!(true));
        }
        (!effect.is_empty()).then_some(Value::Object(effect))
    }
}

/// An element the channel holds in its isolated world.
#[derive(Debug, Clone)]
pub(crate) struct Node {
    pub(crate) backend_node_id: i64,
    /// The DevTools session the element's document belongs to.
    pub(crate) session: String,
    /// Its handle in the isolated world of that session's main frame.
    pub(crate) object: String,
}

/// The pages whose documents record secret fields from their start.
#[derive(Default)]
pub(crate) struct Recorders(Mutex<HashSet<String>>);

impl Recorders {
    /// The execution context of the channel's world in `session`'s main
    /// frame (`frame_world`).
    pub(crate) async fn world(&self, client: &CdpClient, session: &str) -> Result<i64, String> {
        let tree = client
            .send_command_no_params("Page.getFrameTree", Some(session))
            .await?;
        let frame = tree["frameTree"]["frame"]["id"]
            .as_str()
            .ok_or("The page has no main frame")?;
        self.frame_world(client, session, frame).await
    }

    /// The execution context of the channel's world in the frame `frame` of
    /// `session`. The first time for a session, the secret-field recorder is
    /// installed for every document to come in each of its frames, and the
    /// landing watcher's binding for every context of the world.
    pub(crate) async fn frame_world(
        &self,
        client: &CdpClient,
        session: &str,
        frame: &str,
    ) -> Result<i64, String> {
        let first = {
            let mut recorded = self.0.lock().unwrap_or_else(|error| error.into_inner());
            // Sessions end with their tabs and browsers; installing again is
            // harmless, since the recorder installs itself once per document.
            if recorded.len() >= RECORDED_SESSIONS {
                recorded.clear();
            }
            recorded.insert(session.to_string())
        };
        if first {
            let _ = client
                .send_command_no_params("Accessibility.enable", Some(session))
                .await;
            client
                .send_command(
                    "Page.addScriptToEvaluateOnNewDocument",
                    Some(json!({ "source": RECORDER, "worldName": WORLD })),
                    Some(session),
                )
                .await?;
            client
                .send_command(
                    "Runtime.addBinding",
                    Some(json!({ "name": super::landed::BINDING, "executionContextName": WORLD })),
                    Some(session),
                )
                .await?;
        }
        let world = client
            .send_command(
                "Page.createIsolatedWorld",
                Some(json!({ "frameId": frame, "worldName": WORLD })),
                Some(session),
            )
            .await?;
        let context = world["executionContextId"]
            .as_i64()
            .ok_or("The page's channel world is unavailable")?;
        // The document that was already open gets its recorder now.
        client
            .send_command(
                "Runtime.evaluate",
                Some(
                    json!({ "expression": RECORDER, "contextId": context, "returnByValue": true }),
                ),
                Some(session),
            )
            .await?;
        Ok(context)
    }
}

/// Resolves `backend_node_id` into the channel's world of `session`. A node
/// of a child frame that world cannot hold is read in its own frame's page
/// world instead.
pub(crate) async fn node(
    client: &CdpClient,
    session: &str,
    context: i64,
    backend_node_id: i64,
) -> Result<Node, String> {
    let resolve = |context: Option<i64>| {
        let mut params = json!({ "backendNodeId": backend_node_id, "objectGroup": WORLD });
        if let Some(context) = context {
            params["executionContextId"] = json!(context);
        }
        client.send_command("DOM.resolveNode", Some(params), Some(session))
    };
    let resolved = match resolve(Some(context)).await {
        Ok(resolved) => resolved,
        Err(_) => resolve(None).await?,
    };
    let object = resolved["object"]["objectId"]
        .as_str()
        .ok_or("The node is gone")?;
    Ok(Node {
        backend_node_id,
        session: session.to_string(),
        object: object.to_string(),
    })
}

/// What `node` structurally is.
pub(crate) async fn facts(client: &CdpClient, node: &Node) -> Result<NodeFacts, String> {
    let read = client
        .send_command(
            "Runtime.callFunctionOn",
            Some(json!({ "objectId": node.object,
                "functionDeclaration": format!("function() {{ return {FACTS}(this); }}"),
                "returnByValue": true })),
            Some(&node.session),
        )
        .await?;
    if read.get("exceptionDetails").is_some() {
        return Err("The node could not be read".into());
    }
    serde_json::from_value(read["result"]["value"].clone()).map_err(|error| error.to_string())
}

/// The element a judged step acts on, found as the step finds it.
async fn find(
    target: &Target,
    state: &DaemonState,
    recorders: &Recorders,
) -> Result<Option<Node>, String> {
    let browser = state.browser.as_ref().ok_or("Browser not launched")?;
    let client = &browser.client;
    let page = browser.active_session_id()?.to_string();
    let (object, session) = match target {
        Target::Selector(selector) => {
            crate::native::element::set_active_frame(state.active_frame.as_ref());
            match crate::native::element::resolve_element_object_id(
                client,
                &page,
                &state.ref_map,
                selector,
                &state.iframe_sessions,
            )
            .await
            {
                Ok(found) => found,
                // No match, or a ref the daemon dropped: no node.
                Err(_) => return Ok(None),
            }
        }
        Target::Focus | Target::Pointer => {
            let context = recorders.world(client, &page).await?;
            let expression = if *target == Target::Focus {
                FOCUSED
            } else {
                POINTED
            };
            let read = client
                .send_command(
                    "Runtime.evaluate",
                    Some(json!({ "expression": expression, "contextId": context })),
                    Some(&page),
                )
                .await?;
            match read["result"]["objectId"].as_str() {
                Some(object) => (object.to_string(), page.clone()),
                None => return Ok(None),
            }
        }
    };
    let described = client
        .send_command(
            "DOM.describeNode",
            Some(json!({ "objectId": object })),
            Some(&session),
        )
        .await?;
    let Some(backend_node_id) = described["node"]["backendNodeId"].as_i64() else {
        return Ok(None);
    };
    let context = recorders.world(client, &session).await?;
    node(client, &session, context, backend_node_id)
        .await
        .map(Some)
}

/// A refused step, as the native response the host reads.
pub(crate) fn refusal(command: &Value, code: &str, error: &str, data: Value) -> Value {
    json!({ "id": command["id"], "success": false, "code": code, "error": error, "data": data })
}

fn stale(command: &Value, precondition: &str, observed: Value, error: &str) -> Value {
    refusal(
        command,
        "browser_observation_stale",
        error,
        json!({ "precondition": precondition, "observed": observed }),
    )
}

/// Checks a step's page identity, then, for a judged step, its node, box and
/// effect ceiling, and answers the node the step addresses. A step that is
/// not judged may still carry page identity.
pub(crate) async fn check(
    step: &PreparedStep,
    command: &Value,
    state: &DaemonState,
    recorders: &Recorders,
) -> Result<Option<Node>, Value> {
    check_page(&step.preconditions, command, state).await?;
    let Some(judged) = &step.judged else {
        return Ok(None);
    };
    let found = find(&judged.target, state, recorders)
        .await
        .map_err(|error| {
            refusal(
                command,
                "browser_observation_stale",
                &format!("The step's target could not be read ({error}). Nothing was done; observe the page again."),
                json!({ "precondition": "backendNodeId", "observed": null }),
            )
        })?;
    if let Some(expected) = step.preconditions.backend_node_id {
        if found.as_ref().map(|node| node.backend_node_id) != Some(expected) {
            return Err(stale(
                command,
                "backendNodeId",
                json!(found.as_ref().map(|node| node.backend_node_id)),
                "The step's target is not the node it was chosen as (it moved, was replaced or is gone). Nothing was pressed or typed; observe the page again.",
            ));
        }
    }
    let client = &state
        .browser
        .as_ref()
        .ok_or("Browser not launched")
        .map_err(|error| refusal(command, "browser_runtime_unavailable", error, json!({})))?
        .client;
    let facts = match &found {
        Some(node) => Some(facts(client, node).await.map_err(|error| {
            stale(
                command,
                "backendNodeId",
                json!(node.backend_node_id),
                &format!("The step's target could not be read ({error}). Nothing was pressed or typed; observe the page again."),
            )
        })?),
        None => None,
    };
    if let Some(recorded) = step.preconditions.bounds {
        let current = facts.as_ref().map(|facts| facts.bounds);
        if !current.is_some_and(|current| current.contains(recorded.center())) {
            return Err(stale(
                command,
                "box",
                json!(current),
                "The step's target is no longer where it was observed. Nothing was pressed or typed; observe the page again.",
            ));
        }
    }
    let target = facts.as_ref().map_or(
        TargetFacts {
            kind: ceiling::Kind::Other,
            method: None,
            input_type: None,
            multiline: false,
            secret: false,
        },
        |facts| facts.target.clone(),
    );
    if !ceiling::admits(judged.ceiling, judged.interaction, &target) {
        return Err(effect_refusal(command, judged, found.as_ref(), &target, client).await);
    }
    Ok(found)
}

/// `pageGeneration`, then `geometrySha256`: the page the step was chosen on.
async fn check_page(
    preconditions: &Preconditions,
    command: &Value,
    state: &DaemonState,
) -> Result<(), Value> {
    if preconditions.page_generation.is_none() && preconditions.geometry_sha256.is_none() {
        return Ok(());
    }
    let current = state.browser.as_ref().and_then(|browser| {
        browser
            .active_session_id()
            .ok()
            .map(|session| browser.client.page_generation(session))
    });
    if let Some(expected) = &preconditions.page_generation {
        if current.as_ref() != Some(expected) {
            return Err(stale(
                command,
                "pageGeneration",
                json!(current),
                "The page changed since it was observed (it navigated, was reloaded or taken over). Nothing was done; observe the page again.",
            ));
        }
    }
    if let Some(expected) = &preconditions.geometry_sha256 {
        let observed = crate::native::feedback::page_identity(state)
            .await
            .ok()
            .map(|(id, _)| id.geometry_sha256);
        if observed.as_ref() != Some(expected) {
            return Err(stale(
                command,
                "geometrySha256",
                json!(observed),
                "The page's viewport or scale changed since it was observed. Nothing was done; observe the page again.",
            ));
        }
    }
    Ok(())
}

/// The ceiling's refusal: what the target is, so the plan can stop with it
/// or name it in a commit. A secret field's value is never reported.
async fn effect_refusal(
    command: &Value,
    judged: &Judged,
    node: Option<&Node>,
    target: &TargetFacts,
    client: &CdpClient,
) -> Value {
    let mut observed = json!({ "backendNodeId": node.map(|node| node.backend_node_id) });
    if let Some(node) = node {
        if let Ok(ax) = accessibility(client, &node.session, node.backend_node_id).await {
            observed["role"] = json!(ax.role);
            observed["name"] = json!(ax.name);
        }
    }
    if target.secret {
        observed["secret"] = json!(true);
    }
    let error = if target.secret && ceiling::required(judged.interaction, target).is_none() {
        "The step would type into a secret field (a password, one-time code or payment field). No plan types into one; nothing was typed.".to_string()
    } else {
        format!(
            "The step's target is outside the plan's `{}` effect ceiling. Nothing was pressed or typed; name it in a commit or change the plan.",
            judged.ceiling.as_str()
        )
    };
    refusal(
        command,
        "browser_effect_refused",
        &error,
        json!({ "precondition": "effects", "effects": judged.ceiling, "observed": observed }),
    )
}

/// Cuts `text` to at most `limit` characters, at a character boundary.
pub(crate) fn bounded(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((cut, _)) => text[..cut].to_string(),
        None => text.to_string(),
    }
}

/// A node's accessibility role, name, value and states: the snapshot
/// projection's fields.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Accessibility {
    pub(crate) role: String,
    pub(crate) name: String,
    pub(crate) value: Option<String>,
    pub(crate) states: Value,
    pub(crate) ignored: bool,
}

pub(crate) async fn accessibility(
    client: &CdpClient,
    session: &str,
    backend_node_id: i64,
) -> Result<Accessibility, String> {
    let tree = client
        .send_command(
            "Accessibility.getPartialAXTree",
            Some(json!({ "backendNodeId": backend_node_id, "fetchRelatives": false })),
            Some(session),
        )
        .await?;
    let node = tree["nodes"]
        .as_array()
        .and_then(|nodes| {
            nodes
                .iter()
                .find(|node| node["backendDOMNodeId"] == backend_node_id)
        })
        .ok_or("The node has no accessibility node")?;
    let text = |value: &Value| match &value["value"] {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    };
    let mut states = serde_json::Map::new();
    for property in node["properties"].as_array().into_iter().flatten() {
        let value = &property["value"]["value"];
        match property["name"].as_str() {
            Some("checked") => {
                let checked = match value {
                    Value::String(text) => text.clone(),
                    Value::Bool(flag) => flag.to_string(),
                    _ => continue,
                };
                states.insert("checked".into(), json!(checked));
            }
            Some(name @ ("disabled" | "required" | "selected" | "expanded")) => {
                if let Some(flag) = value.as_bool() {
                    states.insert(name.into(), json!(flag));
                }
            }
            _ => {}
        }
    }
    Ok(Accessibility {
        role: text(&node["role"]).unwrap_or_default(),
        name: text(&node["name"]).unwrap_or_default(),
        value: text(&node["value"]),
        states: Value::Object(states),
        ignored: node["ignored"] == true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nodes_facts_read_as_the_classifier_takes_them() {
        let submit = facts(
            json!({ "kind": "submit", "method": "post", "inputType": null,
            "multiline": false, "secret": false,
            "origin": "https://user:secret@shop.example:8443/cart?token=1#pay",
            "autocomplete": null, "controls": false, "download": false,
            "box": { "x": 1, "y": 2, "width": 30, "height": 40 }, "text": "Buy" }),
        );
        assert_eq!(submit.target.kind, ceiling::Kind::Submit);
        assert_eq!(submit.target.method, Some(ceiling::Method::Post));
        assert!(submit.bounds.contains((16.0, 22.0)));
        assert_eq!(submit.frame_origin, None);
        // The effect states the facts the host judges its own ceiling by,
        // its origin reduced as the tab roster reduces URLs.
        assert_eq!(
            submit.effect(),
            Some(json!({ "origin": "https://shop.example:8443", "method": "post" }))
        );
        let password = facts(
            json!({ "kind": "field", "method": "post", "inputType": "password",
            "multiline": false, "secret": true, "origin": null,
            "autocomplete": "current-password", "controls": false, "download": false,
            "box": { "x": 0, "y": 0, "width": 1, "height": 1 }, "text": "" }),
        );
        assert!(password.target.secret);
        assert_eq!(
            password.effect(),
            Some(json!({ "method": "post", "inputType": "password",
                "autocomplete": "current-password" }))
        );
        let disclosure = facts(
            json!({ "kind": "disclosure", "method": null, "inputType": null,
            "multiline": false, "secret": false, "origin": null, "autocomplete": null,
            "controls": true, "download": false, "frameOrigin": "https://frame.example/x",
            "box": { "x": 0, "y": 0, "width": 1, "height": 1 }, "text": "More" }),
        );
        assert_eq!(disclosure.effect(), Some(json!({ "controls": true })));
        assert_eq!(
            disclosure.frame_origin.as_deref(),
            Some("https://frame.example/x")
        );
        let plain = facts(json!({ "kind": "other", "method": null, "inputType": null,
            "multiline": false, "secret": false, "origin": null, "autocomplete": null,
            "controls": false, "download": false,
            "box": { "x": 0, "y": 0, "width": 1, "height": 1 }, "text": "" }));
        assert_eq!(plain.effect(), None);
    }

    fn facts(value: Value) -> NodeFacts {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn names_and_values_are_bounded_at_a_character_boundary() {
        assert_eq!(bounded("abc", 200), "abc");
        let long = "é".repeat(250);
        let cut = bounded(&long, 200);
        assert_eq!(cut.chars().count(), 200);
        assert!(long.starts_with(&cut));
    }
}
