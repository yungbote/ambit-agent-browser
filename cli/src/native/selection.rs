//! Explicit plain-text copy from the browser's retained focus and selection.
//! No system clipboard, permission grant, focus change or page event is used.

use serde_json::json;
use std::collections::{HashMap, HashSet};

use super::cdp::client::CdpClient;

pub(crate) const MAX_COPY_BYTES: usize = 1024 * 1024;
const OBJECT_GROUP: &str = "agent-browser-selection";
const DEEP_ACTIVE: &str = "function(){let e=this.activeElement;while(e?.shadowRoot?.activeElement)e=e.shadowRoot.activeElement;return e;}";

#[derive(Debug)]
pub(crate) enum SelectionError {
    Unavailable,
    TooLarge,
}

impl From<String> for SelectionError {
    fn from(_: String) -> Self {
        Self::Unavailable
    }
}

/// Follow focused frame owners through CDP, so cross-origin frames and closed
/// shadow roots are ordinary focus descendants. DOM.describeNode supplies the
/// frame owner identity; DOM.resolveNode selects its isolated-world wrapper.
/// See https://chromedevtools.github.io/devtools-protocol/1-3/DOM/.
pub(crate) async fn selected_text(
    client: &CdpClient,
    root_session: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<String, SelectionError> {
    let mut sessions = HashSet::new();
    // A fixed, private object group bounds retained wrappers even if a prior
    // copy was cancelled. Copy commands share existing daemon serialization.
    let result = read_selection(client, root_session, iframe_sessions, &mut sessions).await;
    for session in &sessions {
        release(client, session).await;
    }
    result
}

async fn release(client: &CdpClient, session: &str) {
    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        client.send_command(
            "Runtime.releaseObjectGroup",
            Some(json!({ "objectGroup": OBJECT_GROUP })),
            Some(session),
        ),
    )
    .await;
}

async fn read_selection(
    client: &CdpClient,
    root_session: &str,
    iframe_sessions: &HashMap<String, String>,
    sessions: &mut HashSet<String>,
) -> Result<String, SelectionError> {
    let tree = client
        .send_command_no_params("Page.getFrameTree", Some(root_session))
        .await?;
    let mut frame = tree["frameTree"]["frame"]["id"]
        .as_str()
        .ok_or(SelectionError::Unavailable)?
        .to_string();
    let mut session = root_session.to_string();
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(frame.clone()) {
            return Err(SelectionError::Unavailable);
        }
        if let Some(sid) = iframe_sessions.get(&frame) {
            session = sid.clone();
        }
        if sessions.insert(session.clone()) {
            release(client, &session).await;
        }
        let world = client
            .send_command(
                "Page.createIsolatedWorld",
                Some(json!({
                    "frameId": frame, "worldName": "agent-browser-observation",
                })),
                Some(&session),
            )
            .await?;
        let context = world["executionContextId"]
            .as_i64()
            .ok_or(SelectionError::Unavailable)?;
        let focused = client
            .send_command(
                "Runtime.evaluate",
                Some(json!({
                    "expression": format!("({DEEP_ACTIVE}).call(document)"), "contextId": context,
                    "objectGroup": OBJECT_GROUP, "returnByValue": false,
                })),
                Some(&session),
            )
            .await?;
        let mut object = focused["result"]["objectId"]
            .as_str()
            .ok_or(SelectionError::Unavailable)?
            .to_string();
        loop {
            let node = client
                .send_command(
                    "DOM.describeNode",
                    Some(json!({ "objectId": object, "depth": 1, "pierce": true })),
                    Some(&session),
                )
                .await?;
            let node = &node["node"];
            if let Some(child_frame) = node["frameId"].as_str() {
                frame = child_frame.to_string();
                break;
            }
            let shadow = node["shadowRoots"].as_array().and_then(|roots| {
                roots
                    .iter()
                    .find(|root| root["shadowRootType"] != "user-agent")
            });
            if let Some(backend) = shadow.and_then(|root| root["backendNodeId"].as_i64()) {
                let root = client
                    .send_command(
                        "DOM.resolveNode",
                        Some(json!({ "backendNodeId": backend,
                    "executionContextId": context, "objectGroup": OBJECT_GROUP })),
                        Some(&session),
                    )
                    .await?;
                if let Some(root_id) = root["object"]["objectId"].as_str() {
                    let active = client.send_command("Runtime.callFunctionOn", Some(json!({ "objectId": root_id,
                        "functionDeclaration": DEEP_ACTIVE, "objectGroup": OBJECT_GROUP, "returnByValue": false })), Some(&session)).await?;
                    if let Some(active) = active["result"]["objectId"].as_str() {
                        object = active.to_string();
                        continue;
                    }
                }
            }
            let selection = client
                .send_command(
                    "Runtime.callFunctionOn",
                    Some(json!({
                        "objectId": object, "returnByValue": true,
                        "functionDeclaration": format!(r#"function() {{
                    const doc=this.ownerDocument;
                    const isInput=this.tagName==='INPUT'||this.tagName==='TEXTAREA';
                    const text=isInput && this.type==='password' ? ''
                        : isInput && typeof this.selectionStart==='number'
                        ? this.value.slice(this.selectionStart,this.selectionEnd)
                        : (doc.defaultView.getSelection()?.toString()??'');
                    return new TextEncoder().encode(text).length>{MAX_COPY_BYTES}
                        ? {{tooLarge:true}} : {{text}};
                }}"#),
                    })),
                    Some(&session),
                )
                .await?;
            let value = &selection["result"]["value"];
            if value["tooLarge"] == true {
                return Err(SelectionError::TooLarge);
            }
            let text = value["text"].as_str().ok_or(SelectionError::Unavailable)?;
            if text.len() > MAX_COPY_BYTES {
                return Err(SelectionError::TooLarge);
            }
            return Ok(text.to_string());
        }
    }
}
