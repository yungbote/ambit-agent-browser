//! Per-browser custody, independent of command custody. A document pause can
//! receive its host attach while the operation awaiting that document runs.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::{broadcast, Mutex};
use url::Url;

use super::{
    protocol::{self, Mode, Offer, Request},
    state, storage, Context,
};
use crate::native::agent_channel::frame::ChannelId;
use crate::native::cdp::client::CdpClient;

#[path = "transfer.rs"]
mod transfer;

const REFUSED: &str = "The browser site custody request is no longer authorized for this channel.";
const DEADLINE: Duration = Duration::from_secs(2);

struct Pause {
    ready: tokio::sync::oneshot::Sender<bool>,
    session: String,
    generation: String,
    frame: Option<String>,
}
struct Pending {
    channel: ChannelId,
    site: String,
    mode: Mode,
    expires: Instant,
    offer_generation: u64,
    pauses: Vec<Pause>,
    admitted: bool,
    expected_use: Option<String>,
}
struct Held {
    use_id: String,
    mode: Mode,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
    origins: HashSet<String>,
}
struct ExpectedHeld {
    channel: Option<ChannelId>,
    use_id: Option<String>,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
}
impl ExpectedHeld {
    fn matches(&self, state: &State, site: &str) -> bool {
        self.channel
            .is_none_or(|channel| state.channel == Some(channel))
            && state.held.get(site).map(|held| held.use_id.clone()) == self.use_id
            && self.expires_at.is_none_or(|expiry| {
                state
                    .held
                    .get(site)
                    .is_some_and(|held| held.expires_at == Some(expiry))
                    && expiry <= chrono::Utc::now()
            })
    }
}

#[derive(Default)]
struct State {
    channel: Option<ChannelId>,
    ledger: Option<Arc<crate::native::agent_channel::ledger::Ledger>>,
    offers: HashMap<String, Offer>,
    pending: HashMap<String, Pending>,
    resolved: HashSet<String>,
    held: HashMap<String, Held>,
    transfers: HashMap<String, transfer::Transfer>,
    activated: bool,
    offer_generation: u64,
    prepared: HashSet<String>,
    client: Option<Arc<CdpClient>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

pub(crate) struct Custody {
    state: Mutex<State>,
    effects: Mutex<()>,
    effect_site: std::sync::RwLock<Option<String>>,
    effect_changed: tokio::sync::Notify,
    context: Context,
    events: broadcast::Sender<(ChannelId, Value)>,
}

struct Effect<'a> {
    owner: &'a Custody,
    _lock: tokio::sync::MutexGuard<'a, ()>,
}
impl Drop for Effect<'_> {
    fn drop(&mut self) {
        *self
            .owner
            .effect_site
            .write()
            .unwrap_or_else(|error| error.into_inner()) = None;
        self.owner.effect_changed.notify_waiters();
    }
}

impl Custody {
    #[cfg(test)]
    pub(crate) fn with_files(files: crate::native::playwright::files::StagedFiles) -> Arc<Self> {
        let mut owner = Self::new();
        Arc::get_mut(&mut owner).unwrap().context.files = files;
        owner
    }
    pub(crate) fn files(&self) -> crate::native::playwright::files::StagedFiles {
        self.context.files.clone()
    }
    pub(crate) fn new() -> Arc<Self> {
        let (events, _) = broadcast::channel(64);
        Arc::new(Self {
            state: Mutex::new(State::default()),
            effects: Mutex::new(()),
            effect_site: std::sync::RwLock::default(),
            effect_changed: tokio::sync::Notify::new(),
            context: Context::default(),
            events,
        })
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<(ChannelId, Value)> {
        self.events.subscribe()
    }
    async fn effect(&self, site: &str) -> Effect<'_> {
        let lock = self.effects.lock().await;
        *self
            .effect_site
            .write()
            .unwrap_or_else(|error| error.into_inner()) = Some(site.into());
        Effect {
            owner: self,
            _lock: lock,
        }
    }
    pub(crate) fn scrub(&self, value: &mut Value) {
        self.context.values.scrub_protocol(value);
    }

    pub(crate) fn scrub_tool(&self, op: &str, value: &mut Value) {
        self.context.values.scrub_tool(value, op);
    }

    fn site_origins(&self, state: &State, site: &str) -> Result<Vec<String>, &'static str> {
        let mut origins = state
            .client
            .as_ref()
            .map(|client| client.site_profile().documents.origins(site))
            .transpose()?
            .unwrap_or_default()
            .into_iter()
            .collect::<HashSet<_>>();
        if let Some(held) = state.held.get(site) {
            origins.extend(held.origins.iter().cloned());
        }
        Ok(origins.into_iter().collect())
    }

    async fn clear_documents(
        &self,
        client: &CdpClient,
        site: &str,
        origins: &[String],
    ) -> Result<(), &'static str> {
        let mut known = Box::pin(client.site_profile().documents.retire(client, site))
            .await?
            .into_iter()
            .collect::<HashSet<_>>();
        known.extend(origins.iter().cloned());
        storage::clear_site(client, site, &known.into_iter().collect::<Vec<_>>()).await
    }

    /// Runs after the canonical launch and before its first command. Mandatory
    /// file policy does not depend on an optional identity offer. A new native
    /// connection gets the same value registry, but fresh page gates.
    pub(crate) async fn browser_ready(
        self: &Arc<Self>,
        client: Arc<CdpClient>,
        sessions: Vec<String>,
    ) -> Result<(), &'static str> {
        self.recover_cleanup(&client).await?;
        if crate::native::workspace_role::current().is_ok_and(|role| role.is_browser_host()) {
            self.context.files.guard();
        }
        {
            let mut state = self.state.lock().await;
            if !state
                .client
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &client))
            {
                if let Some(task) = state.task.take() {
                    task.abort();
                }
                state.client = Some(client.clone());
                state.prepared.clear();
                state.resolved.clear();
                client.set_site_custody(Arc::downgrade(self), self.context.clone());
                let mut events = client.subscribe();
                let owner = Arc::downgrade(self);
                let connection = client.clone();
                state.task = Some(tokio::spawn(async move {
                    loop {
                        let event = tokio::select! {
                            event=events.recv()=>match event { Ok(event)=>event, Err(broadcast::error::RecvError::Lagged(_))=>{connection.site_profile().documents.unobserved();break;},Err(_)=>break },
                            _=connection.closed()=>break,
                        };
                        let Some(owner) = owner.upgrade() else {
                            break;
                        };
                        if event.method == "Page.frameNavigated" {
                            if let Some(session) = event.session_id.as_deref() {
                                let _ = connection
                                    .site_profile()
                                    .documents
                                    .retain_key(&connection, session, &event.params["frame"])
                                    .await;
                            }
                        }
                        owner.retire_stale().await;
                    }
                    if let Some(owner) = owner.upgrade() {
                        let channel = owner.state.lock().await.channel;
                        if let Some(channel) = channel {
                            owner.channel_closed(channel).await;
                            owner.end(channel).await;
                        }
                    }
                }));
            }
        }
        self.retire_stale().await;
        // Page-created targets must wait for the same native preparation as
        // daemon-created tabs, before their first document can escape.
        client
            .enable_browser_auto_attach()
            .await
            .map_err(|_| REFUSED)?;
        for session in sessions {
            self.prepare_session(&client, &session).await?;
        }
        self.expire_due().await?;
        Ok(())
    }

    /// The native CDP client calls this before a new target resumes or a
    /// document navigates. The program's private connection has no owner.
    pub(crate) async fn prepare_session(
        &self,
        client: &CdpClient,
        session: &str,
    ) -> Result<(), &'static str> {
        if client
            .target_for_session(session)
            .is_some_and(|target| client.site_context().private_target(&target))
        {
            return Ok(());
        }
        let document = client
            .target_for_session(session)
            .is_none_or(|target| client.site_profile().documents.is_document(&target));
        if document && client.target_for_session(session).is_some() {
            // A page manager already enables these events for its tabs. OOP
            // targets can be resumed without network controls, so custody
            // establishes the same subscription before their first document.
            client
                .send_command_no_params("Page.enable", Some(session))
                .await
                .map_err(|_| REFUSED)?;
            client
                .send_command_no_params("DOMStorage.enable", Some(session))
                .await
                .map_err(|_| REFUSED)?;
        }
        if document {
            client
                .site_profile()
                .documents
                .seed(client, session)
                .await?;
        }
        {
            let state = self.state.lock().await;
            if (state.channel.is_none() && !self.context.files.guarded())
                || state.prepared.contains(session)
            {
                return Ok(());
            }
        }
        let mut patterns =
            vec![json!({"urlPattern":"*","resourceType":"Document","requestStage":"Request"})];
        if self.context.files.guarded() {
            patterns.push(json!({"urlPattern":"file://*","requestStage":"Request"}));
            if let Some(pattern) = client.debugger_pattern() {
                patterns.push(pattern);
            }
        }
        client
            .send_command(
                "Fetch.enable",
                Some(json!({"patterns":patterns})),
                Some(session),
            )
            .await
            .map_err(|_| REFUSED)?;
        self.state.lock().await.prepared.insert(session.into());
        Ok(())
    }

    /// The canonical Fetch resolver waits here before applying its existing
    /// routes, domain and header policy. Custody never answers Fetch itself.
    pub(crate) async fn paused(
        self: &Arc<Self>,
        client: &Arc<CdpClient>,
        session: String,
        params: Value,
    ) -> bool {
        let Some(_) = params["requestId"].as_str() else {
            return true;
        };
        let url = params
            .pointer("/request/url")
            .and_then(Value::as_str)
            .and_then(|url| Url::parse(url).ok());
        // New same-site documents wait too; freezing the enumerated targets
        // alone cannot fence a popup created while an effect is in progress.
        if !client
            .target_for_session(&session)
            .is_some_and(|target| client.site_context().private_target(&target))
        {
            loop {
                let changed = self.effect_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                let blocked = self
                    .effect_site
                    .read()
                    .unwrap_or_else(|error| error.into_inner())
                    .as_ref()
                    .is_some_and(|site| {
                        url.as_ref().is_some_and(|url| {
                            state::site_url(site).is_ok_and(|site| {
                                url.scheme() == site.scheme()
                                    && url
                                        .host_str()
                                        .is_some_and(|host| state::host_in_site(host, &site))
                            })
                        })
                    });
                if !blocked {
                    break;
                }
                tokio::select! {_=changed=>{},_=client.closed()=>return false}
            }
        }
        let mut state = self.state.lock().await;
        if client.site_profile().require_clear().is_err() {
            return false;
        }
        let site = url.as_ref().and_then(|url| {
            state
                .offers
                .keys()
                .find(|site| {
                    state::site_url(site).is_ok_and(|site| {
                        url.scheme() == site.scheme()
                            && url
                                .host_str()
                                .is_some_and(|host| state::host_in_site(host, &site))
                    })
                })
                .cloned()
        });
        let Some(site) = site.filter(|site| !state.resolved.contains(site)) else {
            return true;
        };
        if protocol::expiry(state.offers[&site].expires_at.as_deref())
            .ok()
            .flatten()
            .is_some_and(|at| at <= chrono::Utc::now())
        {
            drop(state);
            return self.detach(&site).await.is_ok();
        }
        let (ready, waiting) = tokio::sync::oneshot::channel();
        let frame = params["frameId"].as_str().map(ToString::to_string);
        let generation = client.document_generation(&session, frame.as_deref());
        if let Some((_, pending)) = state
            .pending
            .iter_mut()
            .find(|(_, pending)| pending.site == site)
        {
            pending.pauses.push(Pause {
                ready,
                session: session.clone(),
                generation: generation.clone(),
                frame: frame.clone(),
            });
            drop(state);
            return waiting.await.unwrap_or(false);
        }
        let Some(channel) = state.channel else {
            return true;
        };
        let request_id = uuid::Uuid::new_v4().to_string();
        let mode = state.offers[&site].mode;
        let offer_generation = state.offer_generation;
        let expected_use = state.held.get(&site).map(|held| held.use_id.clone());
        state.pending.insert(
            request_id.clone(),
            Pending {
                channel,
                site: site.clone(),
                mode,
                expires: Instant::now() + DEADLINE,
                offer_generation,
                pauses: vec![Pause {
                    ready,
                    session: session.clone(),
                    generation: generation.clone(),
                    frame,
                }],
                admitted: false,
                expected_use,
            },
        );
        drop(state);
        let _=self.events.send((channel,json!({"type":"site_session.need","requestId":request_id,"site":site,"pageGeneration":generation})));
        let owner = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::time::sleep(DEADLINE).await;
            if let Some(owner) = owner.upgrade() {
                let due = owner
                    .state
                    .lock()
                    .await
                    .pending
                    .get(&request_id)
                    .is_some_and(|pending| !pending.admitted && pending.expires <= Instant::now());
                if due {
                    let _ = owner.release(&request_id).await;
                }
            }
        });
        waiting.await.unwrap_or(false)
    }

    async fn release(self: &Arc<Self>, request_id: &str) -> Result<(), &'static str> {
        let owner = self.clone();
        let request_id = request_id.to_owned();
        tokio::spawn(async move { owner.release_pending(&request_id).await })
            .await
            .map_err(|_| storage::UNCLEARED)?
    }
    async fn release_pending(&self, request_id: &str) -> Result<(), &'static str> {
        let pending_site = self
            .state
            .lock()
            .await
            .pending
            .get(request_id)
            .map(|pending| pending.site.clone());
        let Some(pending_site) = pending_site else {
            return Ok(());
        };
        let _effect = self.effect(&pending_site).await;
        let (client, site, had_state, origins) = {
            let state = self.state.lock().await;
            let Some(pending) = state.pending.get(request_id) else {
                return Ok(());
            };
            let client = state.client.clone().ok_or(REFUSED)?;
            client.site_profile().require_clear()?;
            if state
                .held
                .get(&pending.site)
                .map(|held| held.use_id.clone())
                != pending.expected_use
            {
                drop(state);
                let pending = self.state.lock().await.pending.remove(request_id);
                if let Some(pending) = pending {
                    for pause in pending.pauses {
                        let _ = pause.ready.send(false);
                    }
                }
                return Err(REFUSED);
            }
            (
                client,
                pending.site.clone(),
                state.held.contains_key(&pending.site),
                self.site_origins(&state, &pending.site)?,
            )
        };
        if had_state
            && self
                .clear_documents(&client, &site, &origins)
                .await
                .is_err()
        {
            client.site_profile().mark_uncleared(&site);
            return Err(storage::UNCLEARED);
        }
        let mut state = self.state.lock().await;
        let Some(pending) = state.pending.remove(request_id) else {
            return Ok(());
        };
        if had_state {
            state.held.remove(&site);
        }
        state.resolved.insert(site);
        drop(state);
        for pause in pending.pauses {
            let _ = pause.ready.send(!had_state);
        }
        Ok(())
    }

    /// Share the endpoint's current authority rather than copying a live flag.
    /// An EOF/fence is visible even while an owned custody job is finishing.
    pub(crate) async fn request_current(
        self: &Arc<Self>,
        channel: ChannelId,
        request: Request,
        ledger: Arc<crate::native::agent_channel::ledger::Ledger>,
    ) -> Result<Value, &'static str> {
        if !ledger.may_continue(channel) {
            return Err(REFUSED);
        }
        self.state.lock().await.ledger = Some(ledger);
        self.request(channel, request).await
    }

    pub(crate) async fn request(
        self: &Arc<Self>,
        channel: ChannelId,
        request: Request,
    ) -> Result<Value, &'static str> {
        if matches!(&request, Request::Offer(_)) {
            let client = { self.state.lock().await.client.clone() };
            if let Some(client) = client {
                self.recover_cleanup(&client).await?;
            }
        }
        if let Request::Transfer(request) = request {
            return self.transfer(channel, request).await;
        }
        if let Request::Offer(offers) = request {
            let generation = {
                let mut state = self.state.lock().await;
                state.offer_generation = state.offer_generation.checked_add(1).ok_or(REFUSED)?;
                state.offer_generation
            };
            self.retire_stale().await;
            let pending = {
                let state = self.state.lock().await;
                state.pending.keys().cloned().collect::<Vec<_>>()
            };
            for request in pending {
                let _ = self.release(&request).await;
            }
            let discard = {
                let state = self.state.lock().await;
                state
                    .held
                    .iter()
                    .filter(|(site, held)| {
                        !offers.iter().any(|offer| {
                            &offer.site == *site
                                && offer.use_id.as_ref() == Some(&held.use_id)
                                && offer.mode == held.mode
                                && protocol::expiry(offer.expires_at.as_deref())
                                    .ok()
                                    .flatten()
                                    .is_none_or(|at| at > chrono::Utc::now())
                        })
                    })
                    .map(|(site, held)| (site.clone(), held.use_id.clone()))
                    .collect::<Vec<_>>()
            };
            for (site, use_id) in discard {
                self.detach_expected(
                    &site,
                    Some(ExpectedHeld {
                        channel: None,
                        use_id: Some(use_id),
                        expires_at: None,
                    }),
                )
                .await?;
            }
            let _effect = self.effects.lock().await;
            let mut state = self.state.lock().await;
            if state.offer_generation != generation
                || state
                    .ledger
                    .as_ref()
                    .is_some_and(|ledger| !ledger.may_continue(channel))
            {
                return Err(REFUSED);
            }
            state.channel = Some(channel);
            state.activated = true;
            state.offers = offers
                .into_iter()
                .filter(|offer| {
                    protocol::expiry(offer.expires_at.as_deref())
                        .ok()
                        .flatten()
                        .is_none_or(|at| at > chrono::Utc::now())
                })
                .map(|offer| (offer.site.clone(), offer))
                .collect();
            state.resolved.clear();
            let held = state.held.keys().cloned().collect::<Vec<_>>();
            state.resolved.extend(held);
            let updates = state
                .offers
                .iter()
                .map(|(site, offer)| {
                    (
                        site.clone(),
                        protocol::expiry(offer.expires_at.as_deref()).ok().flatten(),
                    )
                })
                .collect::<Vec<_>>();
            let mut timers = Vec::new();
            for (site, expiry) in updates {
                if let Some(held) = state.held.get_mut(&site) {
                    held.expires_at = expiry;
                    if let Some(expiry) = expiry {
                        timers.push((site, held.use_id.clone(), expiry));
                    }
                }
            }
            let offered = state.offers.len();
            drop(state);
            for (site, use_id, expiry) in timers {
                self.schedule_expiry(site, use_id, expiry);
            }
            return Ok(json!({"offered":offered}));
        }
        if !matches!(request, Request::Export(_) | Request::Detach { .. })
            && self.state.lock().await.channel != Some(channel)
        {
            return Err(REFUSED);
        }
        match request {
            Request::Attach {
                request_id,
                use_id,
                site,
                mode,
                expires_at,
                state: attached,
            } => {
                // Inline v3 is only a wire adapter: use the same current
                // admission/import/cancellation owner, retaining its old two
                // second whole-operation deadline and 8 MiB reader budget.
                let (page_generation, deadline) = {
                    let state = self.state.lock().await;
                    let pending = state.pending.get(&request_id).ok_or(REFUSED)?;
                    let remaining = pending.expires.saturating_duration_since(Instant::now());
                    (
                        pending.pauses.first().ok_or(REFUSED)?.generation.clone(),
                        chrono::Utc::now()
                            + chrono::Duration::from_std(remaining).map_err(|_| REFUSED)?,
                    )
                };
                let begun = self
                    .transfer(
                        channel,
                        protocol::TransferRequest::Begin {
                            request_id,
                            use_id,
                            site,
                            mode,
                            page_generation,
                            deadline: protocol::expiry(expires_at.as_deref())?
                                .map_or(deadline, |at| deadline.min(at)),
                            expires_at: protocol::expiry(expires_at.as_deref())?,
                        },
                    )
                    .await?;
                let transfer_id = begun["transferId"].as_str().ok_or(REFUSED)?.to_owned();
                let mut body = super::bytes::Bytes::new().map_err(|_| REFUSED)?;
                body.write_json(&attached).map_err(|_| REFUSED)?;
                let bytes = body.length();
                let sha256 = body.digest().map_err(|_| REFUSED)?;
                let mut offset = 0;
                while offset < bytes {
                    let data = body
                        .chunk(offset, super::bytes::CHUNK_BYTES)
                        .map_err(|_| REFUSED)?;
                    let count = data.len();
                    self.transfer(
                        channel,
                        protocol::TransferRequest::Part {
                            transfer_id: transfer_id.clone(),
                            offset,
                            data,
                        },
                    )
                    .await?;
                    offset += count as u64;
                }
                self.transfer(
                    channel,
                    protocol::TransferRequest::Finish {
                        transfer_id,
                        bytes,
                        sha256,
                    },
                )
                .await
            }
            Request::Refuse { request_id, site } => {
                let state = self.state.lock().await;
                let pending = state.pending.get(&request_id).ok_or(REFUSED)?;
                if pending.channel != channel || pending.site != site {
                    return Err(REFUSED);
                }
                drop(state);
                self.release(&request_id).await?;
                Ok(json!({"site":site,"attached":false}))
            }
            Request::Export(site) => {
                let owner = self.clone();
                tokio::spawn(async move {
                    let _effect = owner.effect(&site).await;
                    let client = owner.state.lock().await.client.clone().ok_or(REFUSED)?;
                    client.site_profile().require_clear()?;
                    let frozen = client
                        .site_profile()
                        .documents
                        .freeze(&client, &site)
                        .await?;
                    let origins = owner.site_origins(&*owner.state.lock().await, &site)?;
                    let captured = storage::capture(&client, &site, &origins).await;
                    if client
                        .site_profile()
                        .documents
                        .resume(&client, &frozen)
                        .await
                        .is_err()
                    {
                        client.site_profile().mark_uncleared(&site);
                        return Err(storage::UNCLEARED);
                    }
                    let captured = captured?;
                    let mut legacy = super::bytes::Bytes::new().map_err(|_| REFUSED)?;
                    legacy.write_json(&captured).map_err(|_| REFUSED)?;
                    if legacy.length() > state::MAX_BYTES as u64 {
                        return Err(REFUSED);
                    }
                    Ok(json!({"states":[captured]}))
                })
                .await
                .map_err(|_| storage::UNCLEARED)?
            }
            Request::Detach { sites, revoked } => {
                let mut cleared = Vec::new();
                for site in sites {
                    self.detach(&site).await?;
                    cleared.push(json!({"site":site,"cleared":true}));
                }
                Ok(json!({"sites":cleared,"reason":if revoked{"revoked"}else{"ended"}}))
            }
            Request::Transfer(_) => unreachable!(),
            Request::Offer(_) => unreachable!(),
        }
    }

    async fn detach(self: &Arc<Self>, site: &str) -> Result<(), &'static str> {
        self.detach_expected(site, None).await
    }
    async fn detach_expected(
        self: &Arc<Self>,
        site: &str,
        expected: Option<ExpectedHeld>,
    ) -> Result<(), &'static str> {
        let owner = self.clone();
        let site = site.to_owned();
        tokio::spawn(async move { owner.detach_current(&site, expected).await })
            .await
            .map_err(|_| storage::UNCLEARED)?
    }
    async fn detach_current(
        self: &Arc<Self>,
        site: &str,
        expected: Option<ExpectedHeld>,
    ) -> Result<(), &'static str> {
        let transfers = {
            let state = self.state.lock().await;
            if expected
                .as_ref()
                .is_some_and(|expected| !expected.matches(&state, site))
            {
                return Err(REFUSED);
            }
            state
                .transfers
                .iter()
                .filter(|(_, transfer)| transfer.site == site)
                .map(|(id, transfer)| {
                    transfer.cancel();
                    id.clone()
                })
                .collect::<Vec<_>>()
        };
        for id in transfers {
            self.retire_transfer(&id).await?;
        }
        let _effect = self.effect(site).await;
        let (client, origins) = {
            let state = self.state.lock().await;
            if expected
                .as_ref()
                .is_some_and(|expected| !expected.matches(&state, site))
            {
                return Err(REFUSED);
            }
            let client = state.client.clone();
            if let Some(client) = &client {
                client.site_profile().require_clear()?;
            }
            (client, self.site_origins(&state, site)?)
        };
        if let Some(client) = &client {
            if self.clear_documents(client, site, &origins).await.is_err() {
                client.site_profile().mark_uncleared(site);
                return Err(storage::UNCLEARED);
            }
        }
        let mut state = self.state.lock().await;
        state.offers.remove(site);
        state.held.remove(site);
        state.resolved.insert(site.into());
        let requests = state
            .pending
            .iter()
            .filter(|(_, pending)| pending.site == site)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let pending = requests
            .into_iter()
            .filter_map(|id| state.pending.remove(&id))
            .collect::<Vec<_>>();
        drop(state);
        for pending in pending {
            for pause in pending.pauses {
                let _ = pause.ready.send(false);
            }
        }
        Ok(())
    }

    pub(crate) async fn end(self: &Arc<Self>, channel: ChannelId) {
        self.channel_closed(channel).await;
        self.retire_stale().await;
        let pending = {
            let state = self.state.lock().await;
            state
                .pending
                .iter()
                .filter(|(_, pending)| pending.channel == channel)
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>()
        };
        for request in pending {
            let _ = self.release(&request).await;
        }
        let mut state = self.state.lock().await;
        if state.channel.is_none() {
            state.offers.clear();
            state.resolved.clear();
        }
    }

    pub(crate) async fn admits_channel(&self, channel: ChannelId) -> bool {
        let state = self.state.lock().await;
        state
            .client
            .as_ref()
            .is_none_or(|client| client.site_profile().require_clear().is_ok())
            && (!state.activated || state.channel == Some(channel))
    }

    async fn expire_due(self: &Arc<Self>) -> Result<(), &'static str> {
        let due = {
            let state = self.state.lock().await;
            state
                .held
                .iter()
                .filter(|(_, held)| held.expires_at.is_some_and(|at| at <= chrono::Utc::now()))
                .map(|(site, held)| (site.clone(), held.use_id.clone(), held.expires_at.unwrap()))
                .collect::<Vec<_>>()
        };
        for (site, use_id, expiry) in due {
            self.detach_expected(
                &site,
                Some(ExpectedHeld {
                    channel: None,
                    use_id: Some(use_id),
                    expires_at: Some(expiry),
                }),
            )
            .await?;
        }
        Ok(())
    }

    fn schedule_expiry(
        self: &Arc<Self>,
        site: String,
        use_id: String,
        deadline: chrono::DateTime<chrono::Utc>,
    ) {
        let owner = Arc::downgrade(self);
        let delay = (deadline - chrono::Utc::now())
            .to_std()
            .unwrap_or(Duration::ZERO);
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let Some(owner) = owner.upgrade() else {
                return;
            };
            let due = {
                let state = owner.state.lock().await;
                state.held.get(&site).is_some_and(|held| {
                    held.use_id == use_id
                        && held.expires_at == Some(deadline)
                        && deadline <= chrono::Utc::now()
                })
            };
            if due {
                let _ = owner
                    .detach_expected(
                        &site,
                        Some(ExpectedHeld {
                            channel: None,
                            use_id: Some(use_id),
                            expires_at: Some(deadline),
                        }),
                    )
                    .await;
            }
        });
    }
}

impl Drop for Custody {
    fn drop(&mut self) {
        if let Ok(state) = self.state.try_lock() {
            if let Some(task) = &state.task {
                task.abort();
            }
        }
    }
}
