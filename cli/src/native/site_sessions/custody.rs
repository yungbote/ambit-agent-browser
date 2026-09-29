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

const REFUSED: &str = "The browser site custody request is no longer authorized for this channel.";
const DEADLINE: Duration = Duration::from_secs(2);

struct Pause {
    ready: tokio::sync::oneshot::Sender<()>,
}
struct Pending {
    channel: ChannelId,
    site: String,
    mode: Mode,
    expires: Instant,
    offer_generation: u64,
    pauses: Vec<Pause>,
}
struct Held {
    use_id: String,
    mode: Mode,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
}
#[derive(Default)]
struct State {
    channel: Option<ChannelId>,
    offers: HashMap<String, Offer>,
    pending: HashMap<String, Pending>,
    resolved: HashSet<String>,
    held: HashMap<String, Held>,
    activated: bool,
    offer_generation: u64,
    origins: HashSet<String>,
    prepared: HashSet<String>,
    client: Option<Arc<CdpClient>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

pub(crate) struct Custody {
    state: Mutex<State>,
    context: Context,
    events: broadcast::Sender<(ChannelId, Value)>,
}

impl Custody {
    pub(crate) fn new() -> Arc<Self> {
        let (events, _) = broadcast::channel(64);
        Arc::new(Self {
            state: Mutex::new(State::default()),
            context: Context::default(),
            events,
        })
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<(ChannelId, Value)> {
        self.events.subscribe()
    }
    pub(crate) fn scrub(&self, value: &mut Value) {
        self.context.values.scrub_protocol(value);
    }

    pub(crate) fn scrub_tool(&self, op: &str, value: &mut Value) {
        self.context.values.scrub_tool(
            value,
            matches!(
                op,
                "agent_browser_eval" | "agent_browser_evaluate" | "agent_browser_run_playwright"
            ),
        );
    }

    /// Runs after the canonical launch and before its first command. A new
    /// native connection gets the same value registry, but fresh page gates.
    pub(crate) async fn browser_ready(
        self: &Arc<Self>,
        client: Arc<CdpClient>,
        sessions: Vec<String>,
    ) -> Result<(), &'static str> {
        {
            let mut state = self.state.lock().await;
            if state.channel.is_none() {
                return Ok(());
            }
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
                            event=events.recv()=>match event { Ok(event)=>event, Err(_)=>break },
                            _=connection.closed()=>break,
                        };
                        let Some(owner) = owner.upgrade() else {
                            break;
                        };
                        if event.method == "Page.frameNavigated" {
                            if let Some(url) =
                                event.params.pointer("/frame/url").and_then(Value::as_str)
                            {
                                if let Ok(url) = Url::parse(url) {
                                    if matches!(url.scheme(), "http" | "https") {
                                        owner
                                            .state
                                            .lock()
                                            .await
                                            .origins
                                            .insert(url.origin().ascii_serialization());
                                    }
                                }
                            }
                        }
                    }
                }));
            }
        }
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
        {
            let state = self.state.lock().await;
            if state.channel.is_none() || state.prepared.contains(session) {
                return Ok(());
            }
        }
        client.send_command("Fetch.enable",Some(json!({"patterns":[{"urlPattern":"*","resourceType":"Document","requestStage":"Request"}]})),Some(session)).await.map_err(|_|REFUSED)?;
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
    ) {
        let Some(_) = params["requestId"].as_str() else {
            return;
        };
        let url = params
            .pointer("/request/url")
            .and_then(Value::as_str)
            .and_then(|url| Url::parse(url).ok());
        let mut state = self.state.lock().await;
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
            return;
        };
        if protocol::expiry(state.offers[&site].expires_at.as_deref())
            .ok()
            .flatten()
            .is_some_and(|at| at <= chrono::Utc::now())
        {
            drop(state);
            let _ = self.detach(&site).await;
            return;
        }
        let (ready, waiting) = tokio::sync::oneshot::channel();
        if let Some((_, pending)) = state
            .pending
            .iter_mut()
            .find(|(_, pending)| pending.site == site)
        {
            pending.pauses.push(Pause { ready });
            drop(state);
            let _ = waiting.await;
            return;
        }
        let Some(channel) = state.channel else {
            return;
        };
        let request_id = uuid::Uuid::new_v4().to_string();
        let mode = state.offers[&site].mode;
        let offer_generation = state.offer_generation;
        state.pending.insert(
            request_id.clone(),
            Pending {
                channel,
                site: site.clone(),
                mode,
                expires: Instant::now() + DEADLINE,
                offer_generation,
                pauses: vec![Pause { ready }],
            },
        );
        drop(state);
        let _=self.events.send((channel,json!({"type":"site_session.need","requestId":request_id,"site":site,"pageGeneration":client.page_generation(&session)})));
        let owner = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::time::sleep(DEADLINE).await;
            if let Some(owner) = owner.upgrade() {
                owner.release(&request_id).await;
            }
        });
        let _ = waiting.await;
    }

    async fn release(&self, request_id: &str) {
        let (pending, client, had_state, origins) = {
            let mut state = self.state.lock().await;
            let Some(pending) = state.pending.remove(request_id) else {
                return;
            };
            state.resolved.insert(pending.site.clone());
            let had_state = state.held.remove(&pending.site).is_some();
            let boundary = state::site_url(&pending.site).expect("an admitted pending site");
            let origins = state
                .origins
                .iter()
                .filter(|origin| state::origin_in_site(origin, &boundary))
                .cloned()
                .collect::<Vec<_>>();
            (pending, state.client.clone(), had_state, origins)
        };
        if let Some(client) = client {
            if had_state {
                // A refused revalidation must really continue signed out,
                // including cookies retained across a native relaunch.
                let _ = storage::clear_site(&client, &pending.site, &origins).await;
            }
            for pause in pending.pauses {
                let _ = pause.ready.send(());
            }
        }
    }

    pub(crate) async fn request(
        self: &Arc<Self>,
        channel: ChannelId,
        request: Request,
    ) -> Result<Value, &'static str> {
        if let Request::Offer(offers) = request {
            {
                let mut state = self.state.lock().await;
                state.offer_generation = state.offer_generation.checked_add(1).ok_or(REFUSED)?;
            }
            let pending = {
                let state = self.state.lock().await;
                state.pending.keys().cloned().collect::<Vec<_>>()
            };
            for request in pending {
                self.release(&request).await;
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
                    .map(|(site, _)| site.clone())
                    .collect::<Vec<_>>()
            };
            for site in discard {
                self.detach(&site).await?;
            }
            let mut state = self.state.lock().await;
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
                let (pending, client) = {
                    let mut state = self.state.lock().await;
                    let pending = state.pending.get(&request_id).ok_or(REFUSED)?;
                    if pending.channel != channel
                        || pending.site != site
                        || pending.mode != mode
                        || pending.expires <= Instant::now()
                    {
                        return Err(REFUSED);
                    }
                    let deadline = protocol::expiry(expires_at.as_deref())?;
                    let offered = protocol::expiry(
                        state
                            .offers
                            .get(&site)
                            .ok_or(REFUSED)?
                            .expires_at
                            .as_deref(),
                    )?;
                    if deadline.is_some_and(|at| at <= chrono::Utc::now())
                        || offered.is_some_and(|limit| deadline.is_none_or(|at| at > limit))
                    {
                        return Err(REFUSED);
                    }
                    let client = state.client.clone().ok_or(REFUSED)?;
                    (state.pending.remove(&request_id).ok_or(REFUSED)?, client)
                };
                let expiry = tokio::time::Instant::from_std(pending.expires);
                let imported = tokio::time::timeout_at(expiry, storage::import(&client, &attached))
                    .await
                    .map_err(|_| REFUSED)
                    .and_then(|result| result);
                if imported.is_err() {
                    let _ = storage::clear(&client, &attached).await;
                }
                {
                    let mut state = self.state.lock().await;
                    // Offer replacement or channel close wins over late import.
                    if state.channel != Some(channel)
                        || state.offers.get(&site).map(|offer| offer.mode) != Some(mode)
                        || state.offer_generation != pending.offer_generation
                    {
                        drop(state);
                        let _ = storage::clear(&client, &attached).await;
                        for pause in pending.pauses {
                            let _ = pause.ready.send(());
                        }
                        return Err(REFUSED);
                    }
                    state.resolved.insert(site.clone());
                    if imported.is_ok() {
                        state
                            .origins
                            .extend(attached.origins.iter().map(|origin| origin.origin.clone()));
                        state.held.insert(
                            site.clone(),
                            Held {
                                use_id: use_id.clone(),
                                mode,
                                expires_at: protocol::expiry(expires_at.as_deref())?,
                            },
                        );
                    }
                }
                for pause in pending.pauses {
                    let _ = pause.ready.send(());
                }
                imported?;
                if let Some(deadline) = protocol::expiry(expires_at.as_deref())? {
                    self.schedule_expiry(site.clone(), use_id, deadline);
                }
                Ok(json!({"site":site,"attached":true}))
            }
            Request::Refuse { request_id, site } => {
                let state = self.state.lock().await;
                let pending = state.pending.get(&request_id).ok_or(REFUSED)?;
                if pending.channel != channel || pending.site != site {
                    return Err(REFUSED);
                }
                drop(state);
                self.release(&request_id).await;
                Ok(json!({"site":site,"attached":false}))
            }
            Request::Export(site) => {
                let (client, origins) = {
                    let state = self.state.lock().await;
                    let boundary = state::site_url(&site)?;
                    let origins = state
                        .origins
                        .iter()
                        .filter(|origin| state::origin_in_site(origin, &boundary))
                        .cloned()
                        .collect::<Vec<_>>();
                    (state.client.clone().ok_or(REFUSED)?, origins)
                };
                let captured = storage::capture(&client, &site, &origins).await?;
                Ok(json!({"states":[captured]}))
            }
            Request::Detach { sites, revoked } => {
                let mut cleared = Vec::new();
                for site in sites {
                    self.detach(&site).await?;
                    cleared.push(json!({"site":site,"cleared":true}));
                }
                Ok(json!({"sites":cleared,"reason":if revoked{"revoked"}else{"ended"}}))
            }
            Request::Offer(_) => unreachable!(),
        }
    }

    async fn detach(&self, site: &str) -> Result<(), &'static str> {
        let (requests, client, origins) = {
            let mut state = self.state.lock().await;
            state.offers.remove(site);
            state.resolved.insert(site.into());
            let requests = state
                .pending
                .iter()
                .filter(|(_, pending)| pending.site == site)
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            state.held.remove(site);
            let boundary = state::site_url(site)?;
            let origins = state
                .origins
                .iter()
                .filter(|origin| state::origin_in_site(origin, &boundary))
                .cloned()
                .collect::<Vec<_>>();
            (requests, state.client.clone(), origins)
        };
        for request in requests {
            self.release(&request).await;
        }
        if let Some(client) = client {
            storage::clear_site(&client, site, &origins).await?;
            let targets = client
                .send_command("Target.getTargets", Some(json!({})), None)
                .await
                .map_err(|_| REFUSED)?;
            for target in targets["targetInfos"].as_array().ok_or(REFUSED)? {
                let matches = target["url"]
                    .as_str()
                    .and_then(|url| Url::parse(url).ok())
                    .is_some_and(|url| {
                        state::site_url(site).is_ok_and(|boundary| {
                            url.scheme() == boundary.scheme()
                                && url
                                    .host_str()
                                    .is_some_and(|host| state::host_in_site(host, &boundary))
                        })
                    });
                if matches {
                    if let Some(session) = target["targetId"]
                        .as_str()
                        .and_then(|target| client.session_for_target(target))
                    {
                        client
                            .send_command(
                                "Page.reload",
                                Some(json!({"ignoreCache":true})),
                                Some(&session),
                            )
                            .await
                            .map_err(|_| REFUSED)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn end(&self, channel: ChannelId) {
        if self.state.lock().await.channel != Some(channel) {
            return;
        }
        let pending = {
            let state = self.state.lock().await;
            state.pending.keys().cloned().collect::<Vec<_>>()
        };
        for request in pending {
            self.release(&request).await;
        }
        let mut state = self.state.lock().await;
        state.channel = None;
        state.offers.clear();
        state.resolved.clear();
    }

    pub(crate) async fn admits_channel(&self, channel: ChannelId) -> bool {
        let state = self.state.lock().await;
        !state.activated || state.channel == Some(channel)
    }

    async fn expire_due(&self) -> Result<(), &'static str> {
        let due = {
            let state = self.state.lock().await;
            state
                .held
                .iter()
                .filter(|(_, held)| held.expires_at.is_some_and(|at| at <= chrono::Utc::now()))
                .map(|(site, _)| site.clone())
                .collect::<Vec<_>>()
        };
        for site in due {
            self.detach(&site).await?;
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
                let _ = owner.detach(&site).await;
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
