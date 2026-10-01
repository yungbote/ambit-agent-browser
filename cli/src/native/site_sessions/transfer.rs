//! Bounded bytes inside the existing need/custody lifecycle. Admission is not
//! a new grant: every use still stands on the same channel, page and offer.

use super::super::{
    bytes::{Bytes, CHUNK_BYTES},
    protocol::TransferRequest,
    state::SiteState,
};
use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};

const IDLE: Duration = Duration::from_secs(8);

#[derive(Default)]
struct Settled {
    done: AtomicU8,
    ready: tokio::sync::Notify,
}
impl Settled {
    fn complete(&self, clean: bool) {
        self.done
            .store(if clean { 1 } else { 2 }, Ordering::Release);
        self.ready.notify_waiters();
    }
    async fn wait(&self) -> bool {
        loop {
            let notified = self.ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let done = self.done.load(Ordering::Acquire);
            if done != 0 {
                return done == 1;
            }
            notified.await;
        }
    }
}

struct Applying {
    settled: Arc<Settled>,
    client: Arc<CdpClient>,
    site: String,
}
impl Drop for Applying {
    fn drop(&mut self) {
        if self.settled.done.load(Ordering::Acquire) == 0 {
            self.client.site_profile().mark_uncleared(&self.site);
            self.settled.complete(false);
        }
    }
}

pub(super) struct Transfer {
    channel: ChannelId,
    pub(super) site: String,
    use_id: Option<String>,
    expected_use: Option<String>,
    mode: Option<Mode>,
    request_id: Option<String>,
    generation: u64,
    client: Arc<CdpClient>,
    deadline: chrono::DateTime<chrono::Utc>,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
    bytes: Option<Bytes>,
    previous: Option<(u64, usize, [u8; 32])>,
    next: u64,
    progress: Option<Instant>,
    changed: Arc<tokio::sync::Notify>,
    cancel: tokio::sync::watch::Sender<bool>,
    applying: Option<Arc<Settled>>,
    exporting: bool,
}

impl Transfer {
    pub(super) fn cancel(&self) {
        let _ = self.cancel.send_replace(true);
        self.changed.notify_one();
    }
}

fn held_use(state: &State, site: &str) -> Option<String> {
    state.held.get(site).map(|held| held.use_id.clone())
}
fn current(state: &State, transfer: &Transfer) -> bool {
    !*transfer.cancel.borrow()
        && transfer.client.site_profile().require_clear().is_ok()
        && state.channel == Some(transfer.channel)
        && state
            .ledger
            .as_ref()
            .is_none_or(|ledger| ledger.may_continue(transfer.channel))
        && state.offer_generation == transfer.generation
        && state
            .client
            .as_ref()
            .is_some_and(|client| Arc::ptr_eq(client, &transfer.client))
        && chrono::Utc::now() < transfer.deadline
        && transfer.expires_at.is_none_or(|at| chrono::Utc::now() < at)
        && held_use(state, &transfer.site) == transfer.expected_use
        && transfer.progress.is_none_or(|at| at.elapsed() < IDLE)
        && transfer.request_id.as_ref().is_none_or(|id| {
            state.pending.get(id).is_some_and(|pending| {
                pending.channel == transfer.channel
                    && pending.site == transfer.site
                    && pending.offer_generation == transfer.generation
                    && pending.pauses.iter().all(|pause| {
                        transfer
                            .client
                            .document_generation(&pause.session, pause.frame.as_deref())
                            == pause.generation
                    })
            })
        })
        && transfer.mode.is_none_or(|mode| {
            state.offers.get(&transfer.site).is_some_and(|offer| {
                offer.mode == mode
                    && protocol::expiry(offer.expires_at.as_deref())
                        .ok()
                        .flatten()
                        .is_none_or(|at| chrono::Utc::now() < at)
            })
        })
}

impl Custody {
    pub(super) async fn transfer(
        self: &Arc<Self>,
        channel: ChannelId,
        request: TransferRequest,
    ) -> Result<Value, &'static str> {
        match request {
            TransferRequest::Begin {
                request_id,
                use_id,
                site,
                mode,
                page_generation,
                deadline,
                expires_at,
            } => {
                let mut state = self.state.lock().await;
                let pending = state.pending.get(&request_id).ok_or(REFUSED)?;
                let offer = state.offers.get(&site).ok_or(REFUSED)?;
                let offered = protocol::expiry(offer.expires_at.as_deref())?;
                let now = chrono::Utc::now();
                let client = state.client.clone().ok_or(REFUSED)?;
                client.site_profile().require_clear()?;
                if state
                    .ledger
                    .as_ref()
                    .is_some_and(|ledger| !ledger.may_continue(channel))
                {
                    return Err(REFUSED);
                }
                if state.channel != Some(channel)
                    || pending.channel != channel
                    || pending.site != site
                    || pending.mode != mode
                    || pending.expires <= Instant::now()
                    || pending.admitted
                    || pending.offer_generation != state.offer_generation
                    || pending
                        .pauses
                        .first()
                        .is_none_or(|pause| pause.generation != page_generation)
                    || pending.pauses.iter().any(|pause| {
                        client.document_generation(&pause.session, pause.frame.as_deref())
                            != pause.generation
                    })
                    || deadline <= now
                    || expires_at.is_some_and(|at| at <= now || deadline > at)
                    || offered.is_some_and(|at| {
                        at <= now || deadline > at || expires_at.is_none_or(|expiry| expiry > at)
                    })
                {
                    return Err(REFUSED);
                }
                let id = uuid::Uuid::new_v4().to_string();
                let (cancel, _) = tokio::sync::watch::channel(false);
                let transfer = Transfer {
                    channel,
                    site: site.clone(),
                    use_id: Some(use_id.clone()),
                    expected_use: held_use(&state, &site),
                    mode: Some(mode),
                    request_id: Some(request_id.clone()),
                    generation: state.offer_generation,
                    client,
                    deadline,
                    expires_at,
                    bytes: Some(Bytes::new().map_err(|_| REFUSED)?),
                    previous: None,
                    next: 0,
                    progress: None,
                    changed: Arc::default(),
                    cancel,
                    applying: None,
                    exporting: false,
                };
                state.pending.get_mut(&request_id).ok_or(REFUSED)?.admitted = true;
                state.transfers.insert(id.clone(), transfer);
                drop(state);
                self.watch_transfer(id.clone());
                Ok(
                    json!({"transferId":id,"site":site,"requestId":request_id,"useId":use_id,"chunkBytes":CHUNK_BYTES}),
                )
            }
            TransferRequest::Part {
                transfer_id,
                offset,
                data,
            } => {
                let mut state = self.state.lock().await;
                let transfer = state.transfers.get(&transfer_id).ok_or(REFUSED)?;
                if transfer.channel != channel
                    || transfer.exporting
                    || transfer.applying.is_some()
                    || !current(&state, transfer)
                {
                    return Err(REFUSED);
                }
                let hash: [u8; 32] = Sha256::digest(&data).into();
                let transfer = state.transfers.get_mut(&transfer_id).unwrap();
                if transfer.previous == Some((offset, data.len(), hash)) {
                    return Ok(json!({"transferId":transfer_id,"nextOffset":transfer.next}));
                }
                if offset != transfer.next {
                    return Err(REFUSED);
                }
                transfer
                    .bytes
                    .as_mut()
                    .ok_or(REFUSED)?
                    .write_all(&data)
                    .map_err(|_| REFUSED)?;
                transfer.previous = Some((offset, data.len(), hash));
                transfer.next = transfer.bytes.as_ref().unwrap().length();
                transfer.progress = Some(Instant::now());
                transfer.changed.notify_one();
                Ok(json!({"transferId":transfer_id,"nextOffset":transfer.next}))
            }
            TransferRequest::Finish {
                transfer_id,
                bytes,
                sha256,
            } => {
                let mut state = self.state.lock().await;
                let transfer = state.transfers.get(&transfer_id).ok_or(REFUSED)?;
                if transfer.channel != channel
                    || transfer.exporting
                    || transfer.applying.is_some()
                    || !current(&state, transfer)
                {
                    return Err(REFUSED);
                }
                let transfer = state.transfers.get_mut(&transfer_id).unwrap();
                let body = transfer.bytes.as_mut().ok_or(REFUSED)?;
                if body.length() != bytes || body.digest().map_err(|_| REFUSED)? != sha256 {
                    drop(state);
                    let _ = self.retire_transfer(&transfer_id).await;
                    return Err(REFUSED);
                }
                let value = serde_json::from_reader(body.reader().map_err(|_| REFUSED)?)
                    .map_err(|_| REFUSED);
                let attached =
                    value.and_then(|value| SiteState::read_streamed(value, &transfer.site));
                let attached = match attached {
                    Ok(attached) => attached,
                    Err(error) => {
                        drop(state);
                        let _ = self.retire_transfer(&transfer_id).await;
                        return Err(error);
                    }
                };
                let settled = Arc::new(Settled::default());
                transfer.applying = Some(settled.clone());
                transfer.bytes = None;
                transfer.progress = None;
                let client = transfer.client.clone();
                let cancel = transfer.cancel.subscribe();
                drop(state);
                let owner = self.clone();
                // Own applying work beyond the request future. Retirement signals
                // cancellation and waits for close+clear, never drops an import.
                tokio::spawn(async move {
                    let _applying = Applying {
                        settled: settled.clone(),
                        client: client.clone(),
                        site: attached.site.clone(),
                    };
                    let _effect = owner.effect(&attached.site).await;
                    let still_current = {
                        let state = owner.state.lock().await;
                        state
                            .transfers
                            .get(&transfer_id)
                            .is_some_and(|transfer| current(&state, transfer))
                    };
                    let frozen = if still_current {
                        let admitted = client
                            .site_profile()
                            .documents
                            .admit_mutation(&client, &attached.site)
                            .await;
                        if let Err(error) = admitted {
                            Err(error)
                        } else {
                            client
                                .site_profile()
                                .documents
                                .freeze(&client, &attached.site)
                                .await
                        }
                    } else {
                        Err(REFUSED)
                    };
                    let attempted = frozen.is_ok() && {
                        let state = owner.state.lock().await;
                        state
                            .transfers
                            .get(&transfer_id)
                            .is_some_and(|transfer| current(&state, transfer))
                    };
                    let imported = if attempted {
                        storage::import_current(&client, &attached, Some(cancel)).await
                    } else {
                        Err(frozen.as_ref().err().copied().unwrap_or(REFUSED))
                    };
                    let valid = {
                        let state = owner.state.lock().await;
                        state
                            .transfers
                            .get(&transfer_id)
                            .is_some_and(|transfer| current(&state, transfer))
                    };
                    let resumed = if (imported.is_ok() && valid) || (!attempted && frozen.is_ok()) {
                        client
                            .site_profile()
                            .documents
                            .resume(&client, frozen.as_ref().unwrap())
                            .await
                    } else {
                        Ok(())
                    };
                    let mut state = owner.state.lock().await;
                    let valid = state
                        .transfers
                        .get(&transfer_id)
                        .is_some_and(|transfer| current(&state, transfer));
                    let result = if imported.is_ok() && resumed.is_ok() && valid {
                        let transfer = state.transfers.remove(&transfer_id).unwrap();
                        state.resolved.insert(transfer.site.clone());
                        let use_id = transfer.use_id.unwrap();
                        state.held.insert(
                            transfer.site.clone(),
                            Held {
                                use_id: use_id.clone(),
                                mode: transfer.mode.unwrap(),
                                expires_at: transfer.expires_at,
                                origins: attached
                                    .origins
                                    .iter()
                                    .map(|origin| origin.origin.clone())
                                    .collect(),
                            },
                        );
                        let pending = state.pending.remove(transfer.request_id.as_ref().unwrap());
                        drop(state);
                        settled.complete(true);
                        if let Some(pending) = pending {
                            for pause in pending.pauses {
                                let _ = pause.ready.send(true);
                            }
                        }
                        if let Some(expiry) = transfer.expires_at {
                            owner.schedule_expiry(transfer.site.clone(), use_id, expiry);
                        }
                        Ok(json!({"site":transfer.site,"attached":true}))
                    } else {
                        drop(state);
                        let origins = attached
                            .origins
                            .iter()
                            .map(|origin| origin.origin.clone())
                            .collect::<Vec<_>>();
                        let clean = imported != Err(storage::UNCLEARED)
                            && resumed.is_ok()
                            && (!attempted
                                || owner
                                    .clear_documents(&client, &attached.site, &origins)
                                    .await
                                    .is_ok());
                        if !clean {
                            client.site_profile().mark_uncleared(&attached.site);
                        }
                        settled.complete(clean);
                        drop(_effect);
                        let _ = owner.retire_transfer(&transfer_id).await;
                        Err(if clean { REFUSED } else { storage::UNCLEARED })
                    };
                    result
                })
                .await
                .map_err(|_| REFUSED)?
            }
            TransferRequest::Read {
                transfer_id,
                offset,
            } => {
                let mut state = self.state.lock().await;
                let transfer = state.transfers.get(&transfer_id).ok_or(REFUSED)?;
                if transfer.channel != channel
                    || !transfer.exporting
                    || transfer.applying.is_some()
                    || !current(&state, transfer)
                {
                    return Err(REFUSED);
                }
                let transfer = state.transfers.get_mut(&transfer_id).unwrap();
                let replay = transfer
                    .previous
                    .is_some_and(|(previous, _, _)| previous == offset);
                if offset != transfer.next && !replay {
                    return Err(REFUSED);
                }
                let body = transfer.bytes.as_mut().ok_or(REFUSED)?;
                let data = body.chunk(offset, CHUNK_BYTES).map_err(|_| REFUSED)?;
                let next = offset + data.len() as u64;
                if !replay && !data.is_empty() {
                    transfer.previous = Some((offset, data.len(), Sha256::digest(&data).into()));
                    transfer.next = next;
                    transfer.progress = Some(Instant::now());
                    transfer.changed.notify_one();
                }
                Ok(
                    json!({"transferId":transfer_id,"offset":offset,"data":STANDARD.encode(&data),"nextOffset":next,"done":next==body.length()}),
                )
            }
            TransferRequest::Close { transfer_id } => {
                let state = self.state.lock().await;
                if state
                    .transfers
                    .get(&transfer_id)
                    .is_some_and(|transfer| transfer.channel != channel)
                {
                    return Err(REFUSED);
                }
                drop(state);
                self.retire_transfer(&transfer_id).await?;
                Ok(json!({"transferId":transfer_id,"closed":true}))
            }
            TransferRequest::Detach {
                site,
                expected_use,
                revoked,
            } => {
                self.detach_expected(
                    &site,
                    Some(ExpectedHeld {
                        channel: Some(channel),
                        use_id: expected_use,
                        expires_at: None,
                    }),
                )
                .await?;
                Ok(
                    json!({"sites":[{"site":site,"cleared":true}],"reason":if revoked{"revoked"}else{"ended"}}),
                )
            }
            TransferRequest::Export {
                site,
                expected_use,
                deadline,
            } => {
                let mut state = self.state.lock().await;
                let now = chrono::Utc::now();
                let held_expiry = state.held.get(&site).and_then(|held| held.expires_at);
                let offered_expiry = protocol::expiry(
                    state
                        .offers
                        .get(&site)
                        .and_then(|offer| offer.expires_at.as_deref()),
                )?;
                let expires_at = held_expiry.into_iter().chain(offered_expiry).min();
                if state.channel != Some(channel)
                    || held_use(&state, &site) != expected_use
                    || deadline <= now
                    || expires_at.is_some_and(|at| at <= now || deadline > at)
                {
                    return Err(REFUSED);
                }
                let client = state.client.clone().ok_or(REFUSED)?;
                let origins = self.site_origins(&state, &site)?;
                let (cancel, _) = tokio::sync::watch::channel(false);
                let settled = Arc::new(Settled::default());
                let receiver = cancel.subscribe();
                let id = uuid::Uuid::new_v4().to_string();
                let transfer = Transfer {
                    channel,
                    site: site.clone(),
                    use_id: expected_use.clone(),
                    expected_use,
                    mode: None,
                    request_id: None,
                    generation: state.offer_generation,
                    client: client.clone(),
                    deadline,
                    expires_at,
                    bytes: None,
                    previous: None,
                    next: 0,
                    progress: None,
                    changed: Arc::default(),
                    cancel,
                    applying: Some(settled.clone()),
                    exporting: true,
                };
                state.transfers.insert(id.clone(), transfer);
                drop(state);
                self.watch_transfer(id.clone());
                let owner = self.clone();
                tokio::spawn(async move {
                    let _applying=Applying {settled:settled.clone(),client:client.clone(),site:site.clone()};
                    let _effect=owner.effect(&site).await;
                    let still_current={let state=owner.state.lock().await;state.transfers.get(&id).is_some_and(|transfer|current(&state,transfer))};
                    let captured=if still_current {
                        match client.site_profile().documents.freeze(&client,&site).await {
                            Ok(frozen)=>{
                                let captured=storage::capture_current(&client,&site,&origins,Some(receiver)).await;
                                let resumed=client.site_profile().documents.resume(&client,&frozen).await;
                                resumed.and(captured)
                            },Err(error)=>Err(error)
                        }
                    } else {Err(REFUSED)};
                    let clean=captured.as_ref().err()!=Some(&storage::UNCLEARED);
                    if !clean {client.site_profile().mark_uncleared(&site);}
                    let body=captured.and_then(|captured| {let mut body=Bytes::new().map_err(|_|REFUSED)?;body.write_json(&captured).map_err(|_|REFUSED)?;let digest=body.digest().map_err(|_|REFUSED)?;Ok((body,digest))});
                    let mut state=owner.state.lock().await;
                    let valid=state.transfers.get(&id).is_some_and(|transfer|current(&state,transfer));
                    let result=match body {
                        Ok((body,sha256)) if valid => {
                            let bytes=body.length();
                            let transfer=state.transfers.get_mut(&id).unwrap();transfer.bytes=Some(body);transfer.applying=None;
                            Ok(json!({"transferId":id,"site":site,"useId":transfer.use_id,"bytes":bytes,"sha256":sha256,"chunkBytes":CHUNK_BYTES}))
                        }, _=>Err(REFUSED),
                    };
                    drop(state);settled.complete(clean);drop(_effect);if result.is_err(){let _=owner.retire_transfer(&id).await;}result
                }).await.map_err(|_|REFUSED)?
            }
        }
    }

    fn watch_transfer(self: &Arc<Self>, id: String) {
        let owner = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let Some(owner) = owner.upgrade() else {
                    return;
                };
                let state = owner.state.lock().await;
                let Some(transfer) = state.transfers.get(&id) else {
                    return;
                };
                if !current(&state, transfer) {
                    drop(state);
                    let _ = owner.retire_transfer(&id).await;
                    return;
                }
                let authority = (transfer.deadline - chrono::Utc::now())
                    .to_std()
                    .unwrap_or_default();
                let delay = transfer
                    .progress
                    .map(|at| IDLE.saturating_sub(at.elapsed()))
                    .map_or(authority, |idle| idle.min(authority));
                let changed = transfer.changed.clone();
                drop(state);
                drop(owner);
                tokio::select! { _=tokio::time::sleep(delay)=>{}, _=changed.notified()=>{} }
            }
        });
    }

    pub(super) async fn retire_transfer(self: &Arc<Self>, id: &str) -> Result<(), &'static str> {
        let applying = {
            let state = self.state.lock().await;
            let Some(transfer) = state.transfers.get(id) else {
                return Ok(());
            };
            let _ = transfer.cancel.send_replace(true);
            transfer.changed.notify_one();
            transfer.applying.clone()
        };
        let owner = self.clone();
        let id = id.to_owned();
        // Preserve the existing entry/pause until close+clear is proven. The
        // cleanup job survives cancellation of its request waiter.
        tokio::spawn(async move {
            if let Some(applying) = applying {
                if !applying.wait().await {
                    return Err(storage::UNCLEARED);
                }
            }
            let request_id = {
                let state = owner.state.lock().await;
                let Some(transfer) = state.transfers.get(&id) else {
                    return Ok(());
                };
                transfer.request_id.clone()
            };
            if let Some(request_id) = request_id {
                if let Err(error) = owner.release(&request_id).await {
                    // An uncleared profile keeps the existing entry/pause until
                    // recovery. A stale-use abort already retired its document.
                    if error == storage::UNCLEARED
                        || owner.state.lock().await.pending.contains_key(&request_id)
                    {
                        return Err(error);
                    }
                }
            }
            owner.state.lock().await.transfers.remove(&id);
            Ok(())
        })
        .await
        .map_err(|_| storage::UNCLEARED)?
    }

    pub(super) async fn retire_stale(self: &Arc<Self>) {
        let ids = {
            let state = self.state.lock().await;
            state
                .transfers
                .iter()
                .filter(|(_, transfer)| !current(&state, transfer))
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>()
        };
        for id in ids {
            let _ = self.retire_transfer(&id).await;
        }
    }

    /// EOF fences immediately; cleanup can wait for the owned applying job.
    pub(crate) async fn channel_closed(&self, channel: ChannelId) {
        let mut state = self.state.lock().await;
        if state.channel == Some(channel) {
            state.channel = None;
            state.offers.clear();
        }
        for transfer in state
            .transfers
            .values()
            .filter(|transfer| transfer.channel == channel)
        {
            let _ = transfer.cancel.send_replace(true);
            transfer.changed.notify_one();
        }
    }
}

#[cfg(test)]
#[path = "transfer_e2e.rs"]
mod tests;

impl Custody {
    /// Recovery stays with this profile's existing native connection. A newly
    /// dialled connection cannot attest which persisted profile it has opened.
    pub(super) async fn recover_cleanup(
        self: &Arc<Self>,
        client: &Arc<CdpClient>,
    ) -> Result<(), &'static str> {
        let previous = self.state.lock().await.client.clone();
        if let Some(previous) = &previous {
            if previous.site_profile().require_clear().is_err() && !Arc::ptr_eq(previous, client) {
                return Err(storage::UNCLEARED);
            }
        }
        if client.site_profile().require_clear().is_ok() {
            return Ok(());
        }
        let _effect = self.effects.lock().await;
        storage::settle_private_targets(client, &self.context).await?;
        let sites = client
            .site_profile()
            .uncleared_sites()
            .into_iter()
            .collect::<HashSet<_>>();
        for site in &sites {
            let origins = {
                let state = self.state.lock().await;
                self.site_origins(&state, site)?
            };
            self.clear_documents(client, site, &origins)
                .await
                .map_err(|_| storage::UNCLEARED)?;
        }
        let mut state = self.state.lock().await;
        for transfer in state
            .transfers
            .values()
            .filter(|transfer| sites.contains(&transfer.site))
        {
            transfer.cancel();
            if let Some(settled) = &transfer.applying {
                if settled.done.load(Ordering::Acquire) == 0 {
                    return Err(storage::UNCLEARED);
                }
                settled.complete(true);
            }
        }
        state
            .transfers
            .retain(|_, transfer| !sites.contains(&transfer.site));
        state.held.retain(|site, _| !sites.contains(site));
        state.offers.retain(|site, _| !sites.contains(site));
        state.resolved.extend(sites.iter().cloned());
        let requests = state
            .pending
            .iter()
            .filter(|(_, pending)| sites.contains(&pending.site))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let pending = requests
            .into_iter()
            .filter_map(|id| state.pending.remove(&id))
            .collect::<Vec<_>>();
        for site in &sites {
            client.site_profile().prove_clear(site);
        }
        drop(state);
        for pending in pending {
            for pause in pending.pauses {
                let _ = pause.ready.send(true);
            }
        }
        drop(_effect);
        self.retire_stale().await;
        Ok(())
    }
}

#[cfg(test)]
#[path = "transfer_tests.rs"]
mod cleanup_tests;
