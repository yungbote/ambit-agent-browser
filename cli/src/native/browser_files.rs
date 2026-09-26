//! Product-only human file destinations. Bytes stay in staged files; a short
//! lived opaque destination retains the actual renderer node and document.
//! CDP events invalidate custody before their ordinary broadcast can lag.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{json, Value};

use super::activity::InputSource;
use super::browser_control::ControlError;
use super::cdp::client::CdpClient;
use super::display::DisplayClient;

const WORLD: &str = "ambit-human-files";
const GROUP: &str = "ambit-human-files";
pub(crate) const MAX_FILES: usize = 64;

fn unavailable(message: &str) -> ControlError {
    ControlError::new("browser_control_files_unavailable", message)
}

fn stale() -> ControlError {
    ControlError::new(
        "browser_control_file_stale",
        "The upload destination changed. Choose the files again from the current page.",
    )
}

#[derive(Clone, Debug)]
struct Destination {
    id: String,
    controller: String,
    session: String,
    frame: String,
    backend: Option<i64>,
    binding: Option<Binding>,
}

#[derive(Clone, Debug)]
struct Binding {
    root_session: String,
    object: String,
    kind: &'static str,
    accept: String,
    multiple: bool,
}

impl Destination {
    fn descriptor(&self) -> Value {
        let binding = self.binding.as_ref().unwrap();
        json!({"destinationId":self.id,"kind":binding.kind,
            "accept":binding.accept,"multiple":binding.multiple})
    }
}

#[derive(Default)]
struct State {
    controller: Option<String>,
    sessions: HashSet<String>,
    destination: Option<Destination>,
}

/// Human file destinations, and a revision that moves whenever a picker
/// opens or a destination ends, so viewers learn of it without polling.
pub(crate) struct FileDestinations(Mutex<State>, tokio::sync::watch::Sender<u64>);

impl Default for FileDestinations {
    fn default() -> Self {
        Self(Mutex::default(), tokio::sync::watch::channel(0).0)
    }
}

impl FileDestinations {
    /// Moves whenever the pending destination appears or goes away.
    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.1.subscribe()
    }

    fn changed(&self) {
        self.1.send_modify(|revision| *revision += 1);
    }

    pub(crate) fn active(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .controller
            .is_some()
    }

    pub(crate) fn begin(&self, controller: &str) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.controller.as_deref() != Some(controller) {
            state.controller = Some(controller.into());
            if state.destination.take().is_some() {
                self.changed();
            }
        }
    }

    pub(crate) fn end(&self) -> Vec<String> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.controller = None;
        if state.destination.take().is_some() {
            self.changed();
        }
        state.sessions.drain().collect()
    }

    pub(crate) fn intercepted(&self, session: &str) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sessions
            .insert(session.into());
    }

    /// Invalidate the selected document and its renderer, without making an
    /// unrelated child-frame navigation cancel a staged upload. Retained node
    /// connectivity is also checked before delivery, including ancestor removal.
    pub(crate) fn observe(&self, method: &str, params: &Value, session: Option<&str>) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if method == "Target.detachedFromTarget" {
            if let Some(detached) = params["sessionId"].as_str() {
                state.sessions.remove(detached);
                if state.destination.as_ref().is_some_and(|destination| {
                    destination.session == detached
                        || destination
                            .binding
                            .as_ref()
                            .is_some_and(|binding| binding.root_session == detached)
                }) {
                    state.destination = None;
                    self.changed();
                }
            }
            return;
        }
        let Some(session) = session else { return };
        if state
            .destination
            .as_ref()
            .is_some_and(|destination| match method {
                "Page.frameNavigated" | "Page.documentOpened" => {
                    params["frame"]["id"].as_str() == Some(destination.frame.as_str())
                        || (params["frame"]["parentId"]
                            .as_str()
                            .is_none_or(str::is_empty)
                            && (destination.session == session
                                || destination
                                    .binding
                                    .as_ref()
                                    .is_some_and(|binding| binding.root_session == session)))
                }
                "Page.frameDetached" => {
                    params["frameId"].as_str() == Some(destination.frame.as_str())
                }
                "Runtime.executionContextsCleared" => destination.session == session,
                _ => false,
            })
        {
            state.destination = None;
            self.changed();
        }
        if method != "Page.fileChooserOpened" || !state.sessions.contains(session) {
            return;
        }
        let Some(controller) = state.controller.clone() else {
            return;
        };
        let Some(frame) = params["frameId"].as_str().filter(|value| !value.is_empty()) else {
            return;
        };
        state.destination = Some(Destination {
            id: uuid::Uuid::new_v4().to_string(),
            controller,
            session: session.into(),
            frame: frame.into(),
            backend: params["backendNodeId"].as_i64(),
            binding: None,
        });
        self.changed();
    }

    fn pending(&self, controller: &str) -> Option<Destination> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .destination
            .as_ref()
            .filter(|destination| destination.controller == controller)
            .cloned()
    }

    fn current(&self, controller: &str, id: &str) -> Result<Destination, ControlError> {
        self.pending(controller)
            .filter(|destination| destination.id == id)
            .ok_or_else(stale)
    }

    fn replace_binding(&self, destination: Destination) -> Result<(), ControlError> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .destination
            .as_ref()
            .is_none_or(|current| current.id != destination.id)
        {
            return Err(stale());
        }
        state.destination = Some(destination);
        Ok(())
    }

    fn insert(&self, destination: Destination) -> Result<(), ControlError> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.controller.as_deref() != Some(destination.controller.as_str()) {
            return Err(stale());
        }
        state.destination = Some(destination);
        Ok(())
    }

    pub(crate) fn dismiss(&self, controller: &str, id: &str) -> Result<(), ControlError> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .destination
            .as_ref()
            .is_none_or(|current| current.controller != controller || current.id != id)
        {
            return Err(stale());
        }
        state.destination = None;
        self.changed();
        Ok(())
    }
}

pub(crate) async fn intercept(
    client: &CdpClient,
    session: &str,
    enabled: bool,
) -> Result<(), String> {
    client
        .send_command(
            "Page.setInterceptFileChooserDialog",
            Some(json!({"enabled":enabled})),
            Some(session),
        )
        .await?;
    if enabled {
        client.files.intercepted(session);
    }
    Ok(())
}

pub(crate) async fn stop(client: &CdpClient) {
    for session in client.files.end() {
        let _ = tokio::time::timeout(std::time::Duration::from_millis(200), async {
            let _ = intercept(client, &session, false).await;
            let _ = client
                .send_command(
                    "Runtime.releaseObjectGroup",
                    Some(json!({"objectGroup":GROUP})),
                    Some(&session),
                )
                .await;
        })
        .await;
    }
}

pub(crate) fn validate_paths(files: &[String]) -> Result<(), ControlError> {
    if !(1..=MAX_FILES).contains(&files.len()) {
        return Err(ControlError::invalid("Choose between 1 and 64 files."));
    }
    for file in files {
        let path = Path::new(file);
        if file.len() > 4096
            || file.contains('\0')
            || !path.is_absolute()
            || path.components().any(|part| {
                matches!(
                    part,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
        {
            return Err(ControlError::invalid(
                "Files must be absolute canonical staged paths.",
            ));
        }
        let canonical = path
            .canonicalize()
            .map_err(|_| unavailable("A staged upload file is no longer available."))?;
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|_| unavailable("A staged upload file is no longer available."))?;
        if canonical != path || !metadata.is_file() {
            return Err(ControlError::invalid("Upload files must be canonical regular files; directories and links are not accepted."));
        }
    }
    Ok(())
}

pub(crate) struct FilePage {
    pub client: Arc<CdpClient>,
    pub root_session: String,
    pub frames: Vec<(String, String)>,
    pub display: Option<Arc<DisplayClient>>,
}

impl FilePage {
    async fn command(
        &self,
        method: &str,
        params: Value,
        session: &str,
    ) -> Result<Value, ControlError> {
        self.client
            .send_command(method, Some(params), Some(session))
            .await
            .map_err(|_| stale())
    }

    async fn world(&self, frame: &str, session: &str) -> Result<i64, ControlError> {
        let result = self
            .command(
                "Page.createIsolatedWorld",
                json!({"frameId":frame,"worldName":WORLD}),
                session,
            )
            .await?;
        result["executionContextId"].as_i64().ok_or_else(stale)
    }

    async fn release(&self) {
        let mut sessions = HashSet::new();
        for (_, session) in &self.frames {
            if sessions.insert(session) {
                let _ = self
                    .client
                    .send_command(
                        "Runtime.releaseObjectGroup",
                        Some(json!({"objectGroup":GROUP})),
                        Some(session),
                    )
                    .await;
            }
        }
    }

    async fn bind(
        &self,
        mut destination: Destination,
        object: &str,
        kind: &'static str,
    ) -> Result<Destination, ControlError> {
        let result = self.command("Runtime.callFunctionOn", json!({
            "objectId":object,"functionDeclaration":"function(){const n=this.node;return {valid:n?.isConnected&&n.ownerDocument===this.document,input:n instanceof HTMLInputElement&&n.type==='file',accept:n.accept??'',multiple:!!n.multiple,directory:!!n.webkitdirectory,disabled:!!n.disabled};}",
            "returnByValue":true}), &destination.session).await?;
        let value = &result["result"]["value"];
        if value["valid"] != true || value["disabled"] == true {
            return Err(stale());
        }
        if value["directory"] == true {
            return Err(ControlError::new(
                "browser_control_file_unsupported",
                "Choose individual files; folder uploads are not supported.",
            ));
        }
        if kind == "chooser" && value["input"] != true {
            return Err(stale());
        }
        destination.binding = Some(Binding {
            root_session: self.root_session.clone(),
            object: object.into(),
            kind: if kind == "chooser" {
                "chooser"
            } else if value["input"] == true {
                "input"
            } else {
                "drop"
            },
            accept: value["accept"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(4096)
                .collect(),
            multiple: value["input"] != true || value["multiple"] == true,
        });
        Ok(destination)
    }

    pub(crate) async fn chooser(&self, controller: &str) -> Result<Value, ControlError> {
        let Some(mut destination) = self.client.files.pending(controller) else {
            return Ok(Value::Null);
        };
        if !self
            .frames
            .iter()
            .any(|(frame, session)| frame == &destination.frame && session == &destination.session)
        {
            return Ok(Value::Null);
        }
        if let Some(binding) = &destination.binding {
            return Ok(if binding.kind == "chooser" {
                destination.descriptor()
            } else {
                Value::Null
            });
        }
        let backend = destination.backend.ok_or_else(|| {
            unavailable(
                "This picker is not attached to a file input. Use the website's file-upload input.",
            )
        })?;
        self.release().await;
        let context = self.world(&destination.frame, &destination.session).await?;
        let resolved = self
            .command(
                "DOM.resolveNode",
                json!({"backendNodeId":backend,"executionContextId":context,"objectGroup":GROUP}),
                &destination.session,
            )
            .await?;
        let node = resolved["object"]["objectId"].as_str().ok_or_else(stale)?;
        let wrapper = self
            .command(
                "Runtime.callFunctionOn",
                json!({"objectId":node,"objectGroup":GROUP,
            "functionDeclaration":"function(){return {node:this,document:this.ownerDocument};}"}),
                &destination.session,
            )
            .await?;
        let object = wrapper["result"]["objectId"].as_str().ok_or_else(stale)?;
        destination = self.bind(destination, object, "chooser").await?;
        let descriptor = destination.descriptor();
        self.client.files.replace_binding(destination)?;
        Ok(descriptor)
    }

    /// Observe the actual requested hover in every admitted renderer realm.
    /// Screen coordinates use native UI scale, while each event supplies its
    /// own frame-local CSS coordinates. No browser-chrome offset is guessed.
    pub(crate) async fn drop_destination(
        &self,
        controller: &str,
        x: f64,
        y: f64,
        deadline: Instant,
    ) -> Result<Value, ControlError> {
        self.release().await;
        let token = uuid::Uuid::new_v4().to_string();
        let mut contexts = Vec::new();
        let surface = self.display.as_ref().map(|display| display.surface());
        for (frame, session) in &self.frames {
            let context = self.world(frame, session).await?;
            let expected = if let Some(surface) = &surface {
                json!({"screen":true,"x":x+f64::from(surface.origin_x),"y":y+f64::from(surface.origin_y),"scale":surface.device_scale_factor})
            } else {
                // CDP mouse coordinates belong to the main frame. Child
                // renderers still capture their exact trusted event target.
                json!({"screen":false})
            };
            let expression = format!(
                r#"(() => {{
                const expected={expected}, token={token};
                if(globalThis.__ambitFileHoverListener) removeEventListener('pointermove',globalThis.__ambitFileHoverListener,true);
                globalThis.__ambitFileHover=null;
                let resolve;
                globalThis.__ambitFileHoverWait=new Promise(done=>resolve=done);
                globalThis.__ambitFileHoverListener=event=>{{
                    if(!event.isTrusted || event.buttons!==0) return;
                    if(expected.screen&&(Math.abs(event.screenX*expected.scale-expected.x)>expected.scale||Math.abs(event.screenY*expected.scale-expected.y)>expected.scale)) return;
                    const node=event.composedPath()[0];
                    globalThis.__ambitFileHover={{token,node,document:node.ownerDocument,x:event.clientX,y:event.clientY}};
                    resolve(globalThis.__ambitFileHover);
                }};
                addEventListener('pointermove',globalThis.__ambitFileHoverListener,{{capture:true,passive:true}});
            }})()"#,
                token = serde_json::to_string(&token).unwrap()
            );
            self.command(
                "Runtime.evaluate",
                json!({"expression":expression,"contextId":context}),
                session,
            )
            .await?;
            contexts.push((frame, session, context));
        }
        check_deadline(deadline)?;
        if let Some(display) = &self.display {
            display
                .input(&[
                    json!({"type":"input_mouse","eventType":"mouseMoved","x":x,"y":y,"buttons":0}),
                ])
                .await
                .map_err(|_| ControlError::unknown())?;
        } else {
            self.client
                .send_command_from(
                    "Input.dispatchMouseEvent",
                    Some(json!({"type":"mouseMoved","x":x,"y":y,"buttons":0})),
                    Some(&self.root_session),
                    InputSource::Human,
                )
                .await
                .map_err(|_| ControlError::unknown())?;
        }
        let observations = futures_util::future::join_all(contexts.into_iter().map(|(frame, session, context)| {
            let token = token.clone();
            async move {
                let expression = format!(
                    "(async()=>{{const hover=await Promise.race([globalThis.__ambitFileHoverWait,new Promise(done=>setTimeout(()=>done(null),500))]);removeEventListener('pointermove',globalThis.__ambitFileHoverListener,true);delete globalThis.__ambitFileHoverListener;delete globalThis.__ambitFileHover;delete globalThis.__ambitFileHoverWait;return hover?.token==={} ? hover : null;}})()",
                    serde_json::to_string(&token).unwrap()
                );
                let result = self.command("Runtime.evaluate",json!({"expression":expression,"contextId":context,"objectGroup":GROUP,"awaitPromise":true}),session).await;
                (frame,session,result)
            }
        })).await;
        let mut selected = None;
        for (frame, session, result) in observations {
            let result = result?;
            if let Some(object) = result["result"]["objectId"].as_str() {
                if selected.is_some() {
                    return Err(unavailable("The browser could not identify one drop target. Move the pointer and try again."));
                }
                selected = Some(
                    self.bind(
                        Destination {
                            id: uuid::Uuid::new_v4().to_string(),
                            controller: controller.into(),
                            session: session.clone(),
                            frame: frame.clone(),
                            backend: None,
                            binding: None,
                        },
                        object,
                        "drop",
                    )
                    .await?,
                );
            }
        }
        check_deadline(deadline)?;
        let destination = selected.ok_or_else(|| {
            unavailable(
                "Drop files on the web page. Move the pointer over the upload area and try again.",
            )
        })?;
        let descriptor = destination.descriptor();
        self.client.files.insert(destination)?;
        Ok(descriptor)
    }

    /// Native drag dispatch uses the retained element's current geometry. The
    /// exact backend hit is checked before every stage and the renderer's
    /// trusted event target is checked afterward. A moving page can refuse a
    /// drop; it cannot silently replace its selected destination in our state.
    async fn drop_point(
        &self,
        node: &str,
        backend: i64,
        session: &str,
    ) -> Result<(f64, f64), ControlError> {
        // Flush renderer layout before CDP reads its content quads. A style
        // mutation may otherwise leave the previous compositor geometry.
        self.command("Runtime.callFunctionOn", json!({"objectId":node,"functionDeclaration":"function(){return this.getBoundingClientRect().width;}","returnByValue":true}), session).await?;
        let quads = self
            .command("DOM.getContentQuads", json!({"objectId":node}), session)
            .await?;
        for quad in quads["quads"].as_array().into_iter().flatten() {
            let Some(points) = quad.as_array().filter(|points| points.len() == 8) else {
                continue;
            };
            let coordinates: Option<Vec<f64>> = points.iter().map(Value::as_f64).collect();
            let Some(coordinates) = coordinates else {
                continue;
            };
            let x = (coordinates[0] + coordinates[2] + coordinates[4] + coordinates[6]) / 4.0;
            let y = (coordinates[1] + coordinates[3] + coordinates[5] + coordinates[7]) / 4.0;
            if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 {
                continue;
            }
            let hit = self.command("DOM.getNodeForLocation", json!({"x":x.round() as i64,"y":y.round() as i64,"includeUserAgentShadowDOM":false}), session).await?;
            if hit["backendNodeId"].as_i64() == Some(backend) {
                return Ok((x, y));
            }
        }
        Err(stale())
    }

    pub(crate) async fn set_files(
        &self,
        controller: &str,
        id: &str,
        files: &[String],
        deadline: Instant,
    ) -> Result<(), ControlError> {
        validate_paths(files)?;
        let destination = self.client.files.current(controller, id)?;
        let binding = destination.binding.as_ref().ok_or_else(stale)?;
        if binding.root_session != self.root_session
            || !self.frames.iter().any(|(frame, session)| {
                frame == &destination.frame && session == &destination.session
            })
        {
            return Err(stale());
        }
        if !binding.multiple && files.len() != 1 {
            return Err(ControlError::invalid("This upload input accepts one file."));
        }
        let valid = self.command("Runtime.callFunctionOn", json!({"objectId":binding.object,"returnByValue":true,
            "arguments":[{"value":binding.kind!="drop"},{"value":files.len()}],
            "functionDeclaration":"function(input,count){const n=this.node;return n?.isConnected&&n.ownerDocument===this.document&&!n.disabled&&!n.webkitdirectory&&(!input||(n instanceof HTMLInputElement&&n.type==='file'&&(n.multiple||count===1)));}"}), &destination.session).await?;
        if valid["result"]["value"] != true {
            return Err(stale());
        }
        let node = self
            .command(
                "Runtime.callFunctionOn",
                json!({"objectId":binding.object,"objectGroup":GROUP,
            "functionDeclaration":"function(){return this.node;}"}),
                &destination.session,
            )
            .await?;
        let node = node["result"]["objectId"].as_str().ok_or_else(stale)?;
        check_deadline(deadline)?;
        self.client.files.current(controller, id)?;
        if binding.kind == "chooser" {
            self.client.files.dismiss(controller, id)?;
            self.client
                .send_command(
                    "DOM.setFileInputFiles",
                    Some(json!({"objectId":node,"files":files})),
                    Some(&destination.session),
                )
                .await
                .map_err(|_| ControlError::unknown())?;
        } else {
            let described = self
                .command(
                    "DOM.describeNode",
                    json!({"objectId":node}),
                    &destination.session,
                )
                .await?;
            let backend = described["node"]["backendNodeId"]
                .as_i64()
                .ok_or_else(stale)?;
            self.command("Runtime.callFunctionOn", json!({"objectId":binding.object,"returnByValue":true,
                "functionDeclaration":r#"function(){
                    this.receipts={}; const binding=this;
                    if(globalThis.__ambitFileDragListener){for(const name of ['dragenter','dragover','drop'])removeEventListener(name,globalThis.__ambitFileDragListener,true);}
                    globalThis.__ambitFileDragListener=event=>{binding.receipts[event.type]=event.isTrusted&&event.composedPath()[0]===binding.node&&binding.node.isConnected&&binding.node.ownerDocument===binding.document;};
                    for(const name of ['dragenter','dragover','drop'])addEventListener(name,globalThis.__ambitFileDragListener,{capture:true,passive:true});
                }"#}), &destination.session).await?;
            let result = async {
                for (kind, event) in [("dragEnter","dragenter"),("dragOver","dragover"),("drop","drop")] {
                    let (x, y) = self.drop_point(node, backend, &destination.session).await?;
                    check_deadline(deadline)?;
                    self.client.files.current(controller, id)?;
                    if kind == "drop" { self.client.files.dismiss(controller, id)?; }
                    self.client.send_command_from("Input.dispatchDragEvent", Some(json!({"type":kind,"x":x,"y":y,
                        "data":{"items":[],"files":files,"dragOperationsMask":1}})), Some(&destination.session), InputSource::Human).await.map_err(|_| ControlError::unknown())?;
                    let receipt = self.command("Runtime.callFunctionOn", json!({"objectId":binding.object,"returnByValue":true,
                        "arguments":[{"value":event}],"functionDeclaration":"function(event){return this.receipts[event]===true;}"}), &destination.session).await.map_err(|error| if kind=="drop" {ControlError::unknown()} else {error})?;
                    if receipt["result"]["value"] != true {
                        return Err(if kind=="drop" {ControlError::unknown()} else {
                            ControlError::new("browser_control_drop_rejected", "The page did not accept the drag at the selected element. Try its upload button.")
                        });
                    }
                }
                Ok(())
            }.await;
            let _ = self.command("Runtime.callFunctionOn",json!({"objectId":binding.object,
                "functionDeclaration":"function(){for(const name of ['dragenter','dragover','drop'])removeEventListener(name,globalThis.__ambitFileDragListener,true);delete globalThis.__ambitFileDragListener;}"}),&destination.session).await;
            if result.is_err() {
                let _ = self.client.files.dismiss(controller, id);
                self.client
                    .send_command_no_params("Input.cancelDragging", Some(&destination.session))
                    .await
                    .map_err(|_| ControlError::unknown())?;
            }
            result?;
        }
        self.release().await;
        Ok(())
    }
}

fn check_deadline(deadline: Instant) -> Result<(), ControlError> {
    if Instant::now() >= deadline {
        return Err(ControlError::new(
            "browser_control_expired",
            "Control expired before file delivery. Choose the files again after taking control.",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "browser_files_tests.rs"]
mod tests;
