//! Retained tab custody. An origin belongs to every tab whose current or
//! recoverable frames have visited it, until that tab's history is retired.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use serde_json::{json, Value};

use super::{state, storage};
use crate::native::cdp::client::CdpClient;

#[derive(Default)]
struct Tab {
    origins: HashSet<String>,
    known: bool,
}

/// Current Chrome-owned execution and its actual namespace. This is scoped
/// to one retirement effect; it is not a persisted worker or authority ledger.
struct Worker {
    target: String,
    key: String,
    origin: String,
    dedicated: bool,
}

#[derive(Default)]
pub(crate) struct Documents {
    tabs: Mutex<HashMap<String, Tab>>,
    frames: Mutex<HashMap<String, String>>,
    keys: Mutex<HashSet<String>>,
    pending_keys: Mutex<HashSet<(String, String, String)>>,
    keys_changed: tokio::sync::Notify,
    inventory: std::sync::atomic::AtomicBool,
    non_documents: Mutex<HashSet<String>>,
}

fn storage_origin(key: &str) -> Option<String> {
    let prefix = key.split('^').next()?;
    let value = origin(prefix)?;
    (prefix == format!("{value}/")).then_some(value)
}

fn origin(value: &str) -> Option<String> {
    let url = url::Url::parse(value.strip_prefix("blob:").unwrap_or(value)).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.origin().ascii_serialization())
}

fn frame_origins(tree: &Value, origins: &mut HashSet<String>) {
    if let Some(value) = frame_origin(&tree["frame"]) {
        origins.insert(value);
    }
    if let Some(children) = tree["childFrames"].as_array() {
        for child in children {
            frame_origins(child, origins);
        }
    }
}

fn frame_origin(frame: &Value) -> Option<String> {
    frame["securityOrigin"]
        .as_str()
        .and_then(origin)
        .or_else(|| frame["url"].as_str().and_then(origin))
}

fn frame_ids(tree: &Value, ids: &mut Vec<String>) {
    if let Some(id) = tree["frame"]["id"].as_str() {
        ids.push(id.into());
    }
    if let Some(children) = tree["childFrames"].as_array() {
        for child in children {
            frame_ids(child, ids);
        }
    }
}

fn same_frame(tree: &Value, id: &str, loader: &str, expected: &str) -> bool {
    (tree["frame"]["id"] == id
        && tree["frame"]["loaderId"].as_str().unwrap_or("") == loader
        && frame_origin(&tree["frame"]).as_deref() == Some(expected))
        || tree["childFrames"].as_array().is_some_and(|children| {
            children
                .iter()
                .any(|child| same_frame(child, id, loader, expected))
        })
}

impl Documents {
    pub(crate) fn fresh(&self) {
        self.inventory
            .store(true, std::sync::atomic::Ordering::Release);
    }
    pub(crate) fn unobserved(&self) {
        self.inventory
            .store(false, std::sync::atomic::Ordering::Release);
    }
    pub(crate) fn is_document(&self, target: &str) -> bool {
        !self
            .non_documents
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains(target)
    }
    pub(crate) fn keys(&self, site: &str) -> Result<Vec<String>, &'static str> {
        let boundary = state::site_url(site)?;
        Ok(self
            .keys
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .filter(|key| {
                storage_origin(key).is_some_and(|origin| state::origin_in_site(&origin, &boundary))
            })
            .cloned()
            .collect())
    }
    pub(crate) async fn settle_keys(&self, site: &str) -> Result<(), &'static str> {
        if !self.inventory.load(std::sync::atomic::Ordering::Acquire) {
            return Err(storage::UNCLEARED);
        }
        let boundary = state::site_url(site)?;
        tokio::time::timeout(std::time::Duration::from_secs(8), async {
            loop {
                let changed = self.keys_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if !self
                    .pending_keys
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .iter()
                    .any(|(_, _, origin)| state::origin_in_site(origin, &boundary))
                {
                    return Ok(());
                }
                changed.await;
            }
        })
        .await
        .map_err(|_| storage::UNCLEARED)?
    }
    pub(crate) async fn admit_mutation(
        &self,
        client: &CdpClient,
        site: &str,
    ) -> Result<(), &'static str> {
        self.sessions(client, site, true).await?;
        self.settle_keys(site).await
    }

    pub(crate) async fn retain_key(
        &self,
        client: &CdpClient,
        session: &str,
        frame: &Value,
    ) -> Result<(), &'static str> {
        let Some(origin) = frame_origin(frame) else {
            return Ok(());
        };
        let id = frame["id"].as_str().ok_or(storage::UNCLEARED)?;
        let loader = frame["loaderId"].as_str().unwrap_or("").to_owned();
        let before = client
            .send_command_no_params("Page.getFrameTree", Some(session))
            .await
            .map_err(|_| storage::UNCLEARED)?;
        if !same_frame(&before["frameTree"], id, &loader, &origin) {
            return Err(storage::UNCLEARED);
        }
        let key = client
            .send_command(
                "Storage.getStorageKeyForFrame",
                Some(json!({"frameId":id})),
                Some(session),
            )
            .await
            .map_err(|_| storage::UNCLEARED)?;
        let key = key["storageKey"].as_str().ok_or(storage::UNCLEARED)?;
        if storage_origin(key).as_deref() != Some(&origin) {
            return Err(storage::UNCLEARED);
        }
        let after = client
            .send_command_no_params("Page.getFrameTree", Some(session))
            .await
            .map_err(|_| storage::UNCLEARED)?;
        if !same_frame(&after["frameTree"], id, &loader, &origin) {
            return Err(storage::UNCLEARED);
        }
        self.keys
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(key.into());
        self.pending_keys
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&(id.into(), loader, origin));
        self.keys_changed.notify_waiters();
        Ok(())
    }
    /// Runs on the CDP reader before broadcast, so event lag cannot silently
    /// forget the origin of a frame that has since entered the back cache.
    pub(crate) fn observe(&self, method: &str, params: &Value, target: Option<&str>) {
        let mut frames = self
            .frames
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut tabs = self.tabs.lock().unwrap_or_else(|error| error.into_inner());
        if method == "Target.attachedToTarget" {
            if let (Some(id), Some(kind)) = (
                params["targetInfo"]["targetId"].as_str(),
                params["targetInfo"]["type"].as_str(),
            ) {
                if !matches!(kind, "page" | "iframe") {
                    self.non_documents
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .insert(id.into());
                }
            }
        }
        if method == "Target.targetDestroyed" {
            if let Some(target) = params["targetId"].as_str() {
                self.non_documents
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .remove(target);
                // An OOP target disappears when its frame enters a retained
                // document. Its origin remains with that document's tab.
                if frames.get(target).is_none_or(|owner| owner == target) {
                    tabs.remove(target);
                    frames.retain(|_, owner| owner != target);
                }
            }
        } else if method == "Page.frameAttached" {
            if let (Some(frame), Some(target)) = (params["frameId"].as_str(), target) {
                let owner = frames.get(target).cloned().unwrap_or_else(|| target.into());
                frames.insert(frame.into(), owner.clone());
                if frame != owner {
                    if let Some(previous) = tabs.remove(frame) {
                        tabs.entry(owner)
                            .or_default()
                            .origins
                            .extend(previous.origins);
                    }
                }
            }
        } else if method == "Target.attachedToTarget"
            && params["waitingForDebugger"] == true
            && params["targetInfo"]["url"] == "about:blank"
        {
            if let Some(target) = params["targetInfo"]["targetId"].as_str() {
                tabs.entry(target.into()).or_default().known = true;
            }
        } else if method == "Page.frameNavigated" {
            if let Some(target) = target {
                let frame = params["frame"]["id"].as_str().unwrap_or(target);
                let owner = frames
                    .get(frame)
                    .or_else(|| frames.get(target))
                    .cloned()
                    .unwrap_or_else(|| target.into());
                frames.insert(frame.into(), owner.clone());
                if let Some(value) = frame_origin(&params["frame"]) {
                    self.pending_keys
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .insert((
                            frame.into(),
                            params["frame"]["loaderId"].as_str().unwrap_or("").into(),
                            value.clone(),
                        ));
                    tabs.entry(owner).or_default().origins.insert(value);
                }
            }
        } else if method.starts_with("DOMStorage.") {
            if let Some(key) = params["storageId"]["storageKey"].as_str() {
                if let Some(value) = storage_origin(key) {
                    if params["storageId"]["securityOrigin"]
                        .as_str()
                        .is_none_or(|claimed| claimed == value)
                    {
                        self.keys
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .insert(key.into());
                    }
                }
            }
        }
    }

    pub(crate) async fn seed(&self, client: &CdpClient, session: &str) -> Result<(), &'static str> {
        self.seed_current(client, session, false).await
    }
    pub(crate) async fn seed_fresh(
        &self,
        client: &CdpClient,
        session: &str,
    ) -> Result<(), &'static str> {
        self.seed_current(client, session, true).await?;
        client.seed_script_enabled(session);
        Ok(())
    }
    async fn seed_current(
        &self,
        client: &CdpClient,
        session: &str,
        owned: bool,
    ) -> Result<(), &'static str> {
        let page = client.page_of(session);
        let Some(target) = client.target_for_session(&page) else {
            // Preparation may precede the attachment event. It still installs
            // Fetch; an unobserved target gains no history proof from this.
            return Ok(());
        };
        let target = self
            .frames
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&target)
            .cloned()
            .unwrap_or(target);
        let page = client.session_for_target(&target).unwrap_or(page);
        let tree = client
            .send_command_no_params("Page.getFrameTree", Some(session))
            .await
            .map_err(|_| storage::UNCLEARED)?;
        let history = client
            .send_command_no_params("Page.getNavigationHistory", Some(&page))
            .await
            .map_err(|_| storage::UNCLEARED)?;
        let entries = history["entries"].as_array().ok_or(storage::UNCLEARED)?;
        let fresh = entries.len() == 1
            && entries[0]["url"] == "about:blank"
            && tree["frameTree"]["frame"]["url"] == "about:blank";
        let mut ids = Vec::new();
        frame_ids(&tree["frameTree"], &mut ids);
        let mut nodes = vec![&tree["frameTree"]];
        while let Some(node) = nodes.pop() {
            if let Some(children) = node["childFrames"].as_array() {
                nodes.extend(children);
            }
            if frame_origin(&node["frame"]).is_some() {
                let frame = node["frame"]["id"].as_str().ok_or(storage::UNCLEARED)?;
                let own_session = client
                    .session_for_target(frame)
                    .unwrap_or_else(|| session.to_owned());
                self.retain_key(client, &own_session, &node["frame"])
                    .await?;
            }
        }
        let mut frames = self
            .frames
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        frames.extend(ids.into_iter().map(|id| (id, target.clone())));
        let mut tabs = self.tabs.lock().unwrap_or_else(|error| error.into_inner());
        let tab = tabs.entry(target).or_default();
        // Only an initially blank target establishes complete history. A
        // current frame tree cannot attest old embedded/cached documents.
        tab.known |= owned && fresh;
        frame_origins(&tree["frameTree"], &mut tab.origins);
        for entry in entries {
            if let Some(value) = entry["url"].as_str().and_then(origin) {
                tab.origins.insert(value);
            }
        }
        Ok(())
    }

    pub(crate) fn origins(&self, site: &str) -> Result<Vec<String>, &'static str> {
        let boundary = state::site_url(site)?;
        let tabs = self.tabs.lock().unwrap_or_else(|error| error.into_inner());
        let mut origins = tabs
            .values()
            .flat_map(|tab| tab.origins.iter())
            .filter(|origin| state::origin_in_site(origin, &boundary))
            .cloned()
            .collect::<HashSet<_>>();
        origins.extend(
            self.keys(site)?
                .iter()
                .filter_map(|key| storage_origin(key)),
        );
        Ok(origins.into_iter().collect())
    }

    async fn sessions(
        &self,
        client: &CdpClient,
        site: &str,
        destructive: bool,
    ) -> Result<Vec<String>, &'static str> {
        let boundary = state::site_url(site)?;
        let targets = client
            .send_command_no_params("Target.getTargets", None)
            .await
            .map_err(|_| storage::UNCLEARED)?;
        let contexts = client
            .send_command_no_params("Target.getBrowserContexts", None)
            .await
            .map_err(|_| storage::UNCLEARED)?;
        let isolated = contexts["browserContextIds"]
            .as_array()
            .ok_or(storage::UNCLEARED)?;
        let mut sessions = Vec::new();
        for target in targets["targetInfos"]
            .as_array()
            .ok_or(storage::UNCLEARED)?
        {
            let id = target["targetId"].as_str().ok_or(storage::UNCLEARED)?;
            if target["type"] != "page"
                || client.site_context().private_target(id)
                || isolated
                    .iter()
                    .any(|context| context == &target["browserContextId"])
            {
                continue;
            }
            let session = client.session_for_target(id).ok_or(storage::UNCLEARED)?;
            // Admission can own a Fetch pause between documents. Renderer
            // reads cannot answer then; preparation already seeded the tab
            // before navigation and the native reader retains its footprint.
            let tracked = self
                .tabs
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .contains_key(id);
            if !tracked {
                self.seed(client, &session).await?;
            }
            let tabs = self.tabs.lock().unwrap_or_else(|error| error.into_inner());
            let tab = tabs.get(id).ok_or(storage::UNCLEARED)?;
            if destructive && !tab.known {
                return Err(storage::UNCLEARED);
            }
            if tab
                .origins
                .iter()
                .any(|origin| state::origin_in_site(origin, &boundary))
            {
                sessions.push(session);
            }
        }
        Ok(sessions)
    }

    /// Freeze before cookie enumeration or origin mutation. The physical
    /// effect owner awaits resume too, including an abandoned request waiter.
    pub(crate) async fn freeze(
        &self,
        client: &CdpClient,
        site: &str,
    ) -> Result<Vec<String>, &'static str> {
        let sessions = self.sessions(client, site, false).await?;
        let mut frozen = Vec::new();
        for session in sessions {
            if client
                .send_command(
                    "Page.setWebLifecycleState",
                    Some(json!({"state":"frozen"})),
                    Some(&session),
                )
                .await
                .is_err()
            {
                self.resume(client, &frozen).await?;
                return Err(storage::UNCLEARED);
            }
            frozen.push(session);
        }
        Ok(frozen)
    }

    pub(crate) async fn resume(
        &self,
        client: &CdpClient,
        sessions: &[String],
    ) -> Result<(), &'static str> {
        for session in sessions {
            client
                .send_command(
                    "Page.setWebLifecycleState",
                    Some(json!({"state":"active"})),
                    Some(session),
                )
                .await
                .map_err(|_| storage::UNCLEARED)?;
        }
        Ok(())
    }

    async fn workers(&self, client: &CdpClient, site: &str) -> Result<Vec<Worker>, &'static str> {
        let boundary = state::site_url(site)?;
        let targets = client
            .send_command_no_params("Target.getTargets", None)
            .await
            .map_err(|_| storage::UNCLEARED)?;
        let contexts = client
            .send_command_no_params("Target.getBrowserContexts", None)
            .await
            .map_err(|_| storage::UNCLEARED)?;
        let isolated = contexts["browserContextIds"]
            .as_array()
            .ok_or(storage::UNCLEARED)?;
        let mut workers = Vec::new();
        for target in targets["targetInfos"]
            .as_array()
            .ok_or(storage::UNCLEARED)?
        {
            if !matches!(
                target["type"].as_str(),
                Some("worker" | "shared_worker" | "service_worker")
            ) || isolated
                .iter()
                .any(|context| context == &target["browserContextId"])
            {
                continue;
            }
            let dedicated = target["type"] == "worker";
            let target = target["targetId"].as_str().ok_or(storage::UNCLEARED)?;
            let (session, borrowed) = self.worker_session(client, target).await?;
            // Script URLs are not namespace authority: a worker may load a
            // script from another origin. Keep Chrome's complete key verbatim.
            let reply = client
                .send_command_no_params("Storage.getStorageKey", Some(&session))
                .await
                .map_err(|error| {
                    #[cfg(test)]
                    eprintln!("worker_namespace_query_refused={error}");
                    let _ = error;
                    storage::UNCLEARED
                })?;
            let key = reply["storageKey"].as_str().ok_or(storage::UNCLEARED)?;
            if let Some(origin) = storage_origin(key) {
                if state::origin_in_site(&origin, &boundary) {
                    workers.push(Worker {
                        target: target.into(),
                        key: key.into(),
                        origin,
                        dedicated,
                    });
                    continue;
                }
            } else if url::Url::parse(key.split('^').next().unwrap_or_default())
                .map_or(true, |url| matches!(url.scheme(), "http" | "https"))
            {
                return Err(storage::UNCLEARED);
            }
            // A nested worker need not already have a native attachment.
            // Unselected namespaces release only the metadata attachment
            // borrowed above; their execution and original sessions stay.
            if borrowed {
                client
                    .send_command(
                        "Target.detachFromTarget",
                        Some(json!({"sessionId":session})),
                        None,
                    )
                    .await
                    .map_err(|_| storage::UNCLEARED)?;
            }
        }
        Ok(workers)
    }

    async fn worker_session(
        &self,
        client: &CdpClient,
        target: &str,
    ) -> Result<(String, bool), &'static str> {
        if let Some(session) = client.session_for_target(target) {
            return Ok((session, false));
        }
        let reply = client
            .send_command(
                "Target.attachToTarget",
                Some(json!({"targetId":target,"flatten":true})),
                None,
            )
            .await
            .map_err(|_| storage::UNCLEARED)?;
        Ok((
            reply["sessionId"]
                .as_str()
                .ok_or(storage::UNCLEARED)?
                .to_owned(),
            true,
        ))
    }

    async fn retire_worker(&self, client: &CdpClient, worker: &Worker) -> Result<(), &'static str> {
        #[cfg(test)]
        eprintln!("worker_stop_stage=inspect dedicated={}", worker.dedicated);
        let mut events = client.subscribe();
        let targets = client
            .send_command_no_params("Target.getTargets", None)
            .await
            .map_err(|_| storage::UNCLEARED)?;
        if !targets["targetInfos"]
            .as_array()
            .ok_or(storage::UNCLEARED)?
            .iter()
            .any(|target| target["targetId"] == worker.target)
        {
            return Ok(());
        }
        // Navigation can retire a worker's original parent attachment while
        // execution remains. Rebind transport to the same target, then reprove
        // Chrome's complete namespace; a session is not worker authority.
        let (session, _) = self.worker_session(client, &worker.target).await?;
        if client.target_for_session(&session).as_deref() != Some(&worker.target) {
            return Err(storage::UNCLEARED);
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
        let current = client
            .send_command_no_params("Storage.getStorageKey", Some(&session))
            .await
            .map_err(|error| {
                #[cfg(test)]
                eprintln!("worker_stop_namespace_query_refused={error}");
                let _ = error;
                storage::UNCLEARED
            })?;
        if current["storageKey"] != worker.key {
            #[cfg(test)]
            eprintln!("worker_stop_namespace_changed=true");
            return Err(storage::UNCLEARED);
        }
        if client.target_for_session(&session).as_deref() != Some(&worker.target) {
            return Err(storage::UNCLEARED);
        }
        // Dedicated execution can outlive its retired document in Chrome,
        // which does not support closeTarget for it. Request the standard
        // worker close only on the current namespace-bound session. A replaced
        // or throwing close cannot settle without the browser's stop receipt.
        if worker.dedicated {
            let reply = client
                .send_command(
                    "Runtime.evaluate",
                    // close() marks the scope; Chrome's normal task completion
                    // performs shutdown. Inspector evaluation is not that task.
                    Some(
                        json!({"expression":"setTimeout(()=>self.close(),0)","returnByValue":true}),
                    ),
                    Some(&session),
                )
                .await
                .map_err(|_| storage::UNCLEARED)?;
            if reply.get("exceptionDetails").is_some() {
                return Err(storage::UNCLEARED);
            }
        } else {
            let reply = client
                .send_command(
                    "Target.closeTarget",
                    Some(json!({"targetId":worker.target})),
                    None,
                )
                .await
                .map_err(|error| {
                    #[cfg(test)]
                    eprintln!("worker_stop_close_refused={error}");
                    let _ = error;
                    storage::UNCLEARED
                })?;
            if reply["success"] != true {
                return Err(storage::UNCLEARED);
            }
        }
        // Service workers stop asynchronously; a close acknowledgement alone
        // is not retirement. Chrome reports the matching execution's demise.
        tokio::time::timeout_at(deadline, async {
            loop {
                tokio::select! {
                    event=events.recv()=>{
                        let event=event.map_err(|_|storage::UNCLEARED)?;
                        #[cfg(test)]
                        if event.session_id.as_deref()==Some(&session)
                            || event.params["sessionId"]==session
                            || event.params["targetId"]==worker.target
                            || event.params["targetInfo"]["targetId"]==worker.target
                        { eprintln!("worker_stop_browser_event={}", event.method); }
                        if (event.method=="Inspector.targetCrashed" && event.session_id.as_deref()==Some(&session))
                            || (event.method=="Target.targetDestroyed" && event.params["targetId"]==worker.target)
                        {return Ok::<_,&'static str>(());}
                    },
                    _=client.closed()=>return Err(storage::UNCLEARED),
                }
            }
        }).await.map_err(|_|storage::UNCLEARED)??;
        // Retired DevTools hosts can remain as crashed inspection sessions.
        // Release only this native attachment after its stopped receipt.
        if client.target_for_session(&session).as_deref() == Some(&worker.target) {
            let detached = client
                .send_command(
                    "Target.detachFromTarget",
                    Some(json!({"sessionId":session})),
                    None,
                )
                .await;
            #[cfg(test)]
            if let Err(error) = &detached {
                eprintln!("worker_stopped_attachment_release_refused={error}");
            }
            // A stopped target may already have discarded the attachment
            // before the native event cache observes it. Actual browser
            // absence below settles teardown, never this transport reply.
            let _ = detached;
        }
        // Stop notification can precede the manager's live-target removal.
        // Wait on matching browser events and reread actual absence; do not
        // make immediate visibility or a renderer return a timing requirement.
        tokio::time::timeout_at(deadline, async {
            loop {
                let remaining = client
                    .send_command_no_params("Target.getTargets", None)
                    .await
                    .map_err(|_| storage::UNCLEARED)?;
                if !remaining["targetInfos"]
                    .as_array()
                    .ok_or(storage::UNCLEARED)?
                    .iter()
                    .any(|target| target["targetId"] == worker.target)
                {
                    return Ok::<_, &'static str>(());
                }
                loop {
                    tokio::select! {
                        event=events.recv()=> {
                            let event=event.map_err(|_|storage::UNCLEARED)?;
                            if event.params["targetId"]==worker.target
                                || event.params["targetInfo"]["targetId"]==worker.target
                                || event.params["sessionId"]==session
                                || event.session_id.as_deref()==Some(&session)
                            {break;}
                        },
                        _=client.closed()=>return Err(storage::UNCLEARED),
                    }
                }
            }
        })
        .await
        .map_err(|_| storage::UNCLEARED)??;
        Ok(())
    }

    /// The user approved losing history only on affected retained tabs. Blank
    /// commits before reset; storage clears afterward, including pagehide writes.
    pub(crate) async fn retire(
        &self,
        client: &CdpClient,
        site: &str,
    ) -> Result<Vec<String>, &'static str> {
        let sessions = self.sessions(client, site, true).await?;
        self.settle_keys(site).await?;
        let scripts = sessions
            .iter()
            .map(|session| {
                client
                    .script_execution_disabled(session)
                    .map(|disabled| (session.clone(), disabled))
                    .ok_or(storage::UNCLEARED)
            })
            .collect::<Result<Vec<_>, _>>()?;
        #[cfg(test)]
        eprintln!("worker_retirement_stage=initial_inventory");
        // Resolve worker namespaces before any document/history mutation.
        let mut workers = self.workers(client, site).await?;
        let mut origins = self.origins(site)?;
        let retiring = async {
        // Quiesce creators without freezing worker task queues. Dedicated
        // close must finish while its creator still owns an active document.
        for (session, disabled) in &scripts {
            // An equal-value setter is a no-op in that DevTools attachment.
            // Reassert an already-enabled baseline before disabling, without
            // ever enabling a creator the caller had kept disabled.
            if !disabled {
                client.send_command("Emulation.setScriptExecutionDisabled", Some(json!({"value":false})), Some(session)).await.map_err(|_|storage::UNCLEARED)?;
            }
            client.send_command("Emulation.setScriptExecutionDisabled", Some(json!({"value":true})), Some(session)).await.map_err(|_|storage::UNCLEARED)?;
            if client.script_execution_disabled(session) != Some(true) { return Err(storage::UNCLEARED); }
        }
        for worker in workers.drain(..) {
            self.retire_worker(client, &worker).await?;
            self.keys.lock().unwrap_or_else(|error|error.into_inner()).insert(worker.key);
            if !origins.contains(&worker.origin) {origins.push(worker.origin);}
        }
        for session in &sessions {
            let mut events = client.subscribe();
            let reply = client
                .send_command(
                    "Page.navigate",
                    Some(json!({"url":"about:blank"})),
                    Some(&session),
                )
                .await
                .map_err(|_| storage::UNCLEARED)?;
            if reply.get("errorText").is_some() {
                return Err(storage::UNCLEARED);
            }
            let mut tree = client
                .send_command_no_params("Page.getFrameTree", Some(&session))
                .await
                .map_err(|_| storage::UNCLEARED)?;
            if tree["frameTree"]["frame"]["url"] != "about:blank" {
                let loader = reply["loaderId"].as_str().ok_or(storage::UNCLEARED)?;
                tokio::time::timeout(std::time::Duration::from_secs(8), async {
                    loop { tokio::select! {
                        event=events.recv()=>{let event=event.map_err(|_|storage::UNCLEARED)?; if event.session_id.as_deref()==Some(session.as_str()) && event.method=="Page.frameNavigated" && event.params["frame"]["parentId"].as_str().is_none_or(str::is_empty) && event.params["frame"]["loaderId"]==loader && event.params["frame"]["url"]=="about:blank" {return Ok::<_,&'static str>(());}},
                        _=client.closed()=>return Err(storage::UNCLEARED),
                    }}
                }).await.map_err(|_|storage::UNCLEARED)??;
                tree = client
                    .send_command_no_params("Page.getFrameTree", Some(&session))
                    .await
                    .map_err(|_| storage::UNCLEARED)?;
                if tree["frameTree"]["frame"]["url"] != "about:blank" {
                    return Err(storage::UNCLEARED);
                }
            }
            client
                .send_command_no_params("Page.resetNavigationHistory", Some(&session))
                .await
                .map_err(|_| storage::UNCLEARED)?;
            let history = client
                .send_command_no_params("Page.getNavigationHistory", Some(&session))
                .await
                .map_err(|_| storage::UNCLEARED)?;
            let entries = history["entries"].as_array().ok_or(storage::UNCLEARED)?;
            if entries.len() != 1 || entries[0]["url"] != "about:blank" {
                return Err(storage::UNCLEARED);
            }
            let target = client
                .target_for_session(&session)
                .ok_or(storage::UNCLEARED)?;
            self.tabs
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(
                    target.clone(),
                    Tab {
                        known: true,
                        ..Default::default()
                    },
                );
            self.frames
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .retain(|_, owner| owner != &target);
            self.resume(client, std::slice::from_ref(session)).await?;
        }
        // Retire any execution born during teardown and prove a final empty
        // selected set before erasing. Persistent churn remains unsettled.
        #[cfg(test)]
        eprintln!("worker_retirement_stage=execution_stop");
        tokio::time::timeout(std::time::Duration::from_secs(8), async {
            loop {
                let workers = self.workers(client, site).await?;
                if workers.is_empty() { return Ok::<_, &'static str>(()); }
                for worker in workers {
                    self.retire_worker(client, &worker).await?;
                    self.keys
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .insert(worker.key);
                    if !origins.contains(&worker.origin) {
                        origins.push(worker.origin);
                    }
                }
            }
        }).await.map_err(|_|storage::UNCLEARED)??;
        Ok(origins)
        }.await;
        self.restore_scripts(client, &scripts).await.and(retiring)
    }

    async fn restore_scripts(
        &self,
        client: &CdpClient,
        scripts: &[(String, bool)],
    ) -> Result<(), &'static str> {
        let mut restored = true;
        for (session, disabled) in scripts {
            // Restore every reachable creator even when an earlier target
            // disappeared or refuses. The aggregate still fences erasure.
            restored &= client
                .send_command(
                    "Emulation.setScriptExecutionDisabled",
                    Some(json!({"value":disabled})),
                    Some(session),
                )
                .await
                .is_ok()
                && client.script_execution_disabled(session) == Some(*disabled);
        }
        restored.then_some(()).ok_or(storage::UNCLEARED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn creator_restore_attempts_every_affected_session_and_preserves_disabled_state() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            for target in ["A-gone", "A-live", "B"] {
                socket.send(Message::Text(json!({"method":"Target.attachedToTarget","params":{"sessionId":target,"waitingForDebugger":true,"targetInfo":{"targetId":target,"type":"page","url":"about:blank"}}}).to_string())).await.unwrap();
            }
            let Message::Text(body) = socket.next().await.unwrap().unwrap() else {
                panic!("a barrier");
            };
            let request: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(request["method"], "Browser.getVersion");
            socket
                .send(Message::Text(
                    json!({"id":request["id"],"result":{}}).to_string(),
                ))
                .await
                .unwrap();
            for (session, value, accepted) in [
                ("B", true, true),
                ("A-gone", false, false),
                ("A-live", true, true),
            ] {
                let Message::Text(body) = socket.next().await.unwrap().unwrap() else {
                    panic!("a restore");
                };
                let request: Value = serde_json::from_str(&body).unwrap();
                assert_eq!(request["method"], "Emulation.setScriptExecutionDisabled");
                assert_eq!(request["sessionId"], session);
                assert_eq!(request["params"]["value"], value);
                let reply = if accepted {
                    json!({"id":request["id"],"result":{}})
                } else {
                    json!({"id":request["id"],"error":{"code":-32000,"message":"target gone"}})
                };
                socket.send(Message::Text(reply.to_string())).await.unwrap();
            }
            while let Some(Ok(_)) = socket.next().await {}
        });
        let client = CdpClient::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        client
            .send_command_no_params("Browser.getVersion", None)
            .await
            .unwrap();
        client
            .send_command(
                "Emulation.setScriptExecutionDisabled",
                Some(json!({"value":true})),
                Some("B"),
            )
            .await
            .unwrap();
        let result = Documents::default()
            .restore_scripts(
                &client,
                &[("A-gone".into(), false), ("A-live".into(), true)],
            )
            .await;
        assert_eq!(result, Err(storage::UNCLEARED));
        assert_eq!(
            client.script_execution_disabled("A-live"),
            Some(true),
            "later creators are restored despite the first failure"
        );
        assert_eq!(
            client.script_execution_disabled("B"),
            Some(true),
            "unselected execution stays disabled as requested"
        );
        client.disconnect();
        drop(client);
        server.await.unwrap();
    }

    #[test]
    fn retained_origins_belong_to_target_until_its_history_is_retired() {
        let owner = Documents::default();
        owner.observe(
            "Page.frameNavigated",
            &json!({"frame":{"url":"https://private.example/account"}}),
            Some("affected"),
        );
        owner.observe(
            "Page.frameNavigated",
            &json!({"frame":{"url":"https://other.example/frame","parentId":"page"}}),
            Some("affected"),
        );
        owner.observe(
            "Page.frameNavigated",
            &json!({"frame":{"url":"https://other.example/next"}}),
            Some("affected"),
        );
        owner.observe(
            "Page.frameNavigated",
            &json!({"frame":{"url":"https://other.example/only"}}),
            Some("unrelated"),
        );
        assert_eq!(
            owner.origins("https://private.example").unwrap(),
            vec!["https://private.example"]
        );
        assert!(!owner.tabs.lock().unwrap()["affected"].known);
        owner.observe(
            "Target.targetDestroyed",
            &json!({"targetId":"affected"}),
            None,
        );
        assert!(owner.origins("https://private.example").unwrap().is_empty());
        assert_eq!(owner.tabs.lock().unwrap()["unrelated"].origins.len(), 1);
    }

    #[test]
    fn only_a_new_paused_blank_target_establishes_complete_history() {
        let owner = Documents::default();
        for (id, paused, url, known) in [
            ("new", true, "about:blank", true),
            ("adopted", false, "about:blank", false),
            ("loaded", true, "https://example.com", false),
        ] {
            owner.observe(
                "Target.attachedToTarget",
                &json!({"waitingForDebugger":paused,"targetInfo":{"targetId":id,"url":url}}),
                None,
            );
            assert_eq!(
                owner
                    .tabs
                    .lock()
                    .unwrap()
                    .get(id)
                    .is_some_and(|tab| tab.known),
                known
            );
        }
    }

    #[test]
    fn cached_oop_frame_origin_stays_with_its_parent_after_frame_target_destroyed() {
        let owner = Documents::default();
        owner.observe(
            "Page.frameAttached",
            &json!({"frameId":"inner","parentFrameId":"outer"}),
            Some("parent-tab"),
        );
        owner.observe(
            "Page.frameNavigated",
            &json!({"frame":{"id":"inner","url":"https://private.example/frame"}}),
            Some("inner"),
        );
        owner.observe("Target.targetDestroyed", &json!({"targetId":"inner"}), None);
        let tabs = owner.tabs.lock().unwrap();
        assert!(tabs["parent-tab"]
            .origins
            .contains("https://private.example"));
        assert!(!tabs.contains_key("inner"));
        drop(tabs);
        owner.observe(
            "Target.targetDestroyed",
            &json!({"targetId":"parent-tab"}),
            None,
        );
        assert!(owner.origins("https://private.example").unwrap().is_empty());
        assert!(owner.frames.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn worker_retirement_reproves_namespace_and_current_target_after_reattachment() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        for case in ["changed-key", "changed-target", "same-target-reattached"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let calls = std::sync::Arc::new(Mutex::new(Vec::new()));
            let observed = calls.clone();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                let attached = |session: &str, target: &str| json!({"method":"Target.attachedToTarget","params":{"sessionId":session,"waitingForDebugger":false,"targetInfo":{"targetId":target,"type":"shared_worker","url":"https://other.example/script.js"}}});
                socket
                    .send(Message::Text(
                        attached("old-session", "selected-target")
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                if case == "same-target-reattached" {
                    socket.send(Message::Text(json!({"method":"Target.detachedFromTarget","params":{"sessionId":"old-session","targetId":"selected-target"}}).to_string().into())).await.unwrap();
                }
                let mut stopped = false;
                while let Some(Ok(message)) = socket.next().await {
                    let Message::Text(body) = message else {
                        continue;
                    };
                    let request: Value = serde_json::from_str(&body).unwrap();
                    let method = request["method"].as_str().unwrap();
                    observed.lock().unwrap().push(method.to_owned());
                    let result = match method {
                        "Browser.getVersion" => json!({"product":"Chrome/154"}),
                        "Target.getTargets" => {
                            if stopped {
                                json!({"targetInfos":[]})
                            } else {
                                json!({"targetInfos":[{"targetId":"selected-target","type":"shared_worker"},{"targetId":"unrelated-target","type":"shared_worker"}]})
                            }
                        }
                        "Target.attachToTarget" => {
                            assert_eq!(request["params"]["targetId"], "selected-target");
                            socket
                                .send(Message::Text(
                                    attached("current-session", "selected-target")
                                        .to_string()
                                        .into(),
                                ))
                                .await
                                .unwrap();
                            json!({"sessionId":"current-session"})
                        }
                        "Storage.getStorageKey" => {
                            if case == "changed-target" {
                                socket.send(Message::Text(json!({"method":"Target.detachedFromTarget","params":{"sessionId":"old-session","targetId":"selected-target"}}).to_string().into())).await.unwrap();
                                socket
                                    .send(Message::Text(
                                        attached("old-session", "unrelated-target")
                                            .to_string()
                                            .into(),
                                    ))
                                    .await
                                    .unwrap();
                            }
                            json!({"storageKey":if case=="changed-key" {"https://other.example/^0https://private.example"} else {"https://private.example/^0https://other.example"}})
                        }
                        "Target.closeTarget" => {
                            assert_eq!(case, "same-target-reattached");
                            assert_eq!(request["params"]["targetId"], "selected-target");
                            socket.send(Message::Text(json!({"method":"Inspector.targetCrashed","sessionId":"current-session","params":{}}).to_string().into())).await.unwrap();
                            stopped = true;
                            json!({"success":true})
                        }
                        "Target.detachFromTarget" => {
                            assert_eq!(request["params"]["sessionId"], "current-session");
                            socket.send(Message::Text(json!({"method":"Target.detachedFromTarget","params":{"sessionId":"current-session","targetId":"selected-target"}}).to_string().into())).await.unwrap();
                            json!({})
                        }
                        _ => panic!("unexpected worker command {method}"),
                    };
                    socket
                        .send(Message::Text(
                            json!({"id":request["id"],"result":result})
                                .to_string()
                                .into(),
                        ))
                        .await
                        .unwrap();
                }
            });
            let client = CdpClient::connect(&format!("ws://{address}"))
                .await
                .unwrap();
            client
                .send_command_no_params("Browser.getVersion", None)
                .await
                .unwrap();
            let worker = Worker {
                target: "selected-target".into(),
                key: "https://private.example/^0https://other.example".into(),
                origin: "https://private.example".into(),
                dedicated: false,
            };
            let result = Documents::default().retire_worker(&client, &worker).await;
            assert_eq!(result.is_ok(), case == "same-target-reattached", "{case}");
            client.disconnect();
            drop(client);
            server.await.unwrap();
            let calls = calls.lock().unwrap();
            assert_eq!(
                calls
                    .iter()
                    .filter(|method| method.as_str() == "Target.closeTarget")
                    .count(),
                usize::from(case == "same-target-reattached"),
                "{case}"
            );
            assert!(
                !calls.iter().any(|method| method == "Runtime.evaluate"),
                "the wire authority test requests no renderer execution"
            );
        }
    }
}
