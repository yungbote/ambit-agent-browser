//! Real loopback CDP failures, current binding and owned cleanup.

use super::*;
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio_tungstenite::tungstenite::Message;

struct Fixture {
    client: Arc<CdpClient>,
    server: tokio::task::JoinHandle<()>,
    port: u16,
    fault: Arc<AtomicBool>,
    cookie: Arc<AtomicBool>,
    close_refused: Arc<AtomicBool>,
    private: Arc<AtomicBool>,
}
impl Fixture {
    async fn new() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let fault = Arc::new(AtomicBool::new(true));
        let cookie = Arc::new(AtomicBool::new(false));
        let close_refused = Arc::new(AtomicBool::new(true));
        let private = Arc::new(AtomicBool::new(false));
        let states = (
            fault.clone(),
            cookie.clone(),
            close_refused.clone(),
            private.clone(),
        );
        let server = tokio::spawn(async move {
            let mut peers = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted=listener.accept()=>{let (socket,_)=accepted.unwrap();let (fault,cookie,close_refused,private)=states.clone();peers.spawn(async move {
                        let mut socket=tokio_tungstenite::accept_async(socket).await.unwrap();
                        socket.send(Message::Text(json!({"method":"Target.attachedToTarget","params":{"sessionId":"page","waitingForDebugger":false,"targetInfo":{"targetId":"public-page","type":"page","url":"https://example.com","browserContextId":"default"}}}).to_string())).await.unwrap();
                        while let Some(Ok(Message::Text(line)))=socket.next().await {
                            let frame:Value=serde_json::from_str(&line).unwrap();let method=frame["method"].as_str().unwrap();
                            if method=="Test.frameCommit" {
                                socket.send(Message::Text(json!({"method":"Page.frameNavigated","sessionId":"page","params":{"frame":{"id":frame["params"]["frame"],"parentId":"public-frame","url":"https://example.com/frame"}}}).to_string())).await.unwrap();
                            }
                            let reply=match method {
                                "Storage.setCookies"=>{cookie.store(true,Ordering::Release);json!({"id":frame["id"],"error":{"code":-32000,"message":"Synthetic dependency failed after a partial write"}})},
                                "Storage.getCookies"=>json!({"id":frame["id"],"result":{"cookies":if cookie.load(Ordering::Acquire){vec![json!({"name":"fixture","value":"nosecret-partial","domain":"example.com","path":"/","expires":1900000000,"httpOnly":true,"secure":true,"sameSite":"Lax","priority":"Medium","sourceScheme":"Secure","session":false})]}else{Vec::new()}}}),
                                "Target.getTargets"=>{let mut targets=if fault.load(Ordering::Acquire){Vec::new()}else{vec![json!({"targetId":"public-page","type":"page","url":"https://example.com","browserContextId":"default"})]};if private.load(Ordering::Acquire){targets.push(json!({"targetId":"private-document","type":"page","url":"about:blank","browserContextId":"default"}));}json!({"id":frame["id"],"result":{"targetInfos":targets}})},
                                "Page.getFrameTree"=>json!({"id":frame["id"],"result":{"frameTree":{"frame":{"id":"public-frame","url":"about:blank"}}}}),
                                "Page.getNavigationHistory"=>json!({"id":frame["id"],"result":{"entries":[{"id":1,"url":"about:blank"}],"currentIndex":0}}),
                                "Target.getBrowserContexts"=>json!({"id":frame["id"],"result":{"browserContextIds":[]}}),
                                "Network.deleteCookies"=>{cookie.store(false,Ordering::Release);json!({"id":frame["id"],"result":{}})},
                                "Target.closeTarget"=>{let refused=close_refused.load(Ordering::Acquire);if !refused{private.store(false,Ordering::Release);}json!({"id":frame["id"],"result":{"success":!refused}})},
                                _=>json!({"id":frame["id"],"result":{}}),
                            };if socket.send(Message::Text(reply.to_string())).await.is_err(){break;}
                        }
                    });},
                    _=peers.join_next(),if !peers.is_empty()=>{},
                }
            }
        });
        let client = Arc::new(
            CdpClient::connect(&format!("ws://127.0.0.1:{port}"))
                .await
                .unwrap(),
        );
        client
            .send_command_no_params("Browser.getVersion", None)
            .await
            .unwrap();
        client.site_profile().documents.fresh();
        client
            .site_profile()
            .documents
            .seed_fresh(&client, "page")
            .await
            .unwrap();
        Self {
            client,
            server,
            port,
            fault,
            cookie,
            close_refused,
            private,
        }
    }
    async fn end(self) {
        self.client.disconnect();
        self.server.abort();
        let _ = self.server.await;
    }
}
async fn seeded(
    client: Arc<CdpClient>,
    settled: Arc<Settled>,
) -> (Arc<Custody>, String, tokio::sync::oneshot::Receiver<bool>) {
    let owner = Custody::new();
    let channel = ChannelId::parse("11111111-1111-4111-8111-111111111111").unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let request_id = uuid::Uuid::new_v4().to_string();
    let site = "https://example.com".to_owned();
    let (ready, waiting) = tokio::sync::oneshot::channel();
    let (cancel, _) = tokio::sync::watch::channel(false);
    let mut state = owner.state.lock().await;
    state.client = Some(client.clone());
    state.channel = Some(channel);
    state.activated = true;
    state.offers.insert(
        site.clone(),
        Offer {
            site: site.clone(),
            mode: Mode::Act,
            use_id: None,
            expires_at: None,
        },
    );
    state.pending.insert(
        request_id.clone(),
        Pending {
            channel,
            site: site.clone(),
            mode: Mode::Act,
            expires: Instant::now() + DEADLINE,
            offer_generation: 0,
            pauses: vec![Pause {
                ready,
                session: "page".into(),
                generation: client.page_generation("page"),
                frame: None,
            }],
            admitted: true,
            expected_use: None,
        },
    );
    state.transfers.insert(
        id.clone(),
        Transfer {
            channel,
            site,
            use_id: Some("22222222-2222-4222-8222-222222222222".into()),
            expected_use: None,
            mode: Some(Mode::Act),
            request_id: Some(request_id),
            generation: 0,
            client,
            deadline: chrono::Utc::now() + chrono::Duration::seconds(30),
            expires_at: None,
            bytes: None,
            previous: None,
            next: 0,
            progress: None,
            changed: Arc::default(),
            cancel,
            applying: Some(settled),
            exporting: false,
        },
    );
    drop(state);
    (owner, id, waiting)
}

#[tokio::test]
async fn same_site_document_waits_for_physical_effect_without_blocking_unrelated_navigation() {
    let fixture = Fixture::new().await;
    let owner = Custody::new();
    let effect = owner.effect("https://example.com").await;
    let (started, ready) = tokio::sync::oneshot::channel();
    let waiting = tokio::spawn({
        let owner = owner.clone();
        let client = fixture.client.clone();
        async move {
            let _ = started.send(());
            owner.paused(&client,"page".into(),json!({"requestId":"same-site","request":{"url":"https://sub.example.com/new"}})).await
        }
    });
    ready.await.unwrap();
    assert!(
        !waiting.is_finished(),
        "new same-site document cannot run during capture/import/clear"
    );
    assert!(
        owner
            .paused(
                &fixture.client,
                "page".into(),
                json!({"requestId":"other-site","request":{"url":"https://unrelated.example/new"}})
            )
            .await
    );
    drop(effect);
    assert!(tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .unwrap()
        .unwrap());
    fixture.end().await;
}
#[tokio::test]
async fn failed_clear_preserves_pause_and_blocks_new_connection_until_owned_recovery() {
    let fixture = Fixture::new().await;
    let document = json!({"format":state::FORMAT,"site":"https://example.com","capturedAt":"2026-09-30T00:00:00Z","chromeMajor":154,"cookies":[],"origins":[],"omitted":[]});
    let mut body = Bytes::new().unwrap();
    body.write_json(&document).unwrap();
    let bytes = body.length();
    let sha256 = body.digest().unwrap();
    let (owner, id, mut waiting) = seeded(fixture.client.clone(), Arc::default()).await;
    {
        let mut state = owner.state.lock().await;
        let transfer = state.transfers.get_mut(&id).unwrap();
        transfer.applying = None;
        transfer.bytes = Some(body);
    }
    owner.state.lock().await.held.insert(
        "https://untouched.example".into(),
        Held {
            use_id: "44444444-4444-4444-8444-444444444444".into(),
            mode: Mode::Read,
            origins: HashSet::new(),
            expires_at: None,
        },
    );
    // Exercise the production Finish owner: a partial adapter write fails,
    // then canonical document/storage cleanup fails and retains its fence.
    let channel = ChannelId::parse("11111111-1111-4111-8111-111111111111").unwrap();
    assert_eq!(
        owner
            .transfer(
                channel,
                TransferRequest::Finish {
                    transfer_id: id.clone(),
                    bytes,
                    sha256,
                }
            )
            .await,
        Err(storage::UNCLEARED)
    );
    assert!(fixture.cookie.load(Ordering::Acquire));
    assert_eq!(owner.retire_transfer(&id).await, Err(storage::UNCLEARED));
    assert!(matches!(
        waiting.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    let other = Arc::new(
        CdpClient::connect(&format!("ws://127.0.0.1:{}", fixture.port))
            .await
            .unwrap(),
    );
    assert!(owner
        .browser_ready(other.clone(), vec!["page".into()])
        .await
        .is_err());
    assert!(owner.state.lock().await.transfers.contains_key(&id));
    assert!(fixture.cookie.load(Ordering::Acquire));
    let clone = fixture.client.site_profile();
    assert!(clone.require_clear().is_err());
    fixture.fault.store(false, Ordering::Release);
    owner.recover_cleanup(&fixture.client).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .unwrap()
        .unwrap();
    assert!(!fixture.cookie.load(Ordering::Acquire));
    assert!(clone.require_clear().is_ok());
    let state = owner.state.lock().await;
    assert!(state.transfers.is_empty());
    assert!(state.pending.is_empty());
    assert!(state.held.contains_key("https://untouched.example"));
    drop(state);
    other.disconnect();
    fixture.end().await;
}
#[tokio::test]
async fn cancelled_retirement_waiter_keeps_pause_until_cleanup_settles() {
    let fixture = Fixture::new().await;
    fixture.fault.store(false, Ordering::Release);
    let settled = Arc::new(Settled::default());
    let (owner, id, mut waiting) = seeded(fixture.client.clone(), settled.clone()).await;
    let mut cancellation = owner.state.lock().await.transfers[&id].cancel.subscribe();
    let worker = tokio::spawn({
        let owner = owner.clone();
        let id = id.clone();
        async move { owner.retire_transfer(&id).await }
    });
    tokio::time::timeout(Duration::from_secs(1), cancellation.changed())
        .await
        .unwrap()
        .unwrap();
    assert!(*cancellation.borrow());
    worker.abort();
    let _ = worker.await;
    assert!(matches!(
        waiting.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    assert!(owner.state.lock().await.transfers.contains_key(&id));
    settled.complete(true);
    tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .unwrap()
        .unwrap();
    assert!(owner.state.lock().await.transfers.is_empty());
    fixture.end().await;
}
#[tokio::test]
async fn false_close_keeps_the_private_target_mask_until_absence_is_proven() {
    let fixture = Fixture::new().await;
    fixture.fault.store(false, Ordering::Release);
    fixture.private.store(true, Ordering::Release);
    let context = super::super::super::Context::default();
    context.hold_target("private-document");
    assert_eq!(
        storage::settle_private_targets(&fixture.client, &context).await,
        Err(storage::UNCLEARED)
    );
    assert!(context.private_target("private-document"));
    fixture.close_refused.store(false, Ordering::Release);
    storage::settle_private_targets(&fixture.client, &context)
        .await
        .unwrap();
    assert!(!context.private_target("private-document"));
    fixture.end().await;
}

#[tokio::test]
async fn preparation_cancellation_survives_the_absence_of_an_import_subscriber() {
    let fixture = Fixture::new().await;
    let settled = Arc::new(Settled::default());
    let (owner, id, waiting) = seeded(fixture.client.clone(), settled).await;
    {
        let mut state = owner.state.lock().await;
        let transfer = state.transfers.get_mut(&id).unwrap();
        transfer.applying = None;
        transfer.cancel();
        assert!(*transfer.cancel.subscribe().borrow());
    }
    owner.retire_transfer(&id).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .unwrap()
        .unwrap();
    fixture.end().await;
}

#[tokio::test]
async fn current_ledger_page_use_offer_deadline_and_progress_are_exact_bindings() {
    let fixture = Fixture::new().await;
    fixture.fault.store(false, Ordering::Release);
    let settled = Arc::new(Settled::default());
    let (owner, id, waiting) = seeded(fixture.client.clone(), settled.clone()).await;
    let channel = owner.state.lock().await.transfers[&id].channel;
    let ledger = Arc::new(crate::native::agent_channel::ledger::Ledger::default());
    assert!(ledger.open(channel));
    {
        let mut state = owner.state.lock().await;
        state.ledger = Some(ledger.clone());
        state.transfers.get_mut(&id).unwrap().applying = None;
    }
    let valid = |state: &State| current(state, &state.transfers[&id]);
    {
        let mut state = owner.state.lock().await;
        assert!(valid(&state));
        state.offer_generation += 1;
        assert!(!valid(&state));
        state.offer_generation -= 1;
        state.held.insert(
            "https://example.com".into(),
            Held {
                use_id: "44444444-4444-4444-8444-444444444444".into(),
                mode: Mode::Act,
                origins: HashSet::new(),
                expires_at: None,
            },
        );
        assert!(!valid(&state));
        state.held.remove("https://example.com");
        let transfer = state.transfers.get_mut(&id).unwrap();
        let deadline = transfer.deadline;
        transfer.deadline = chrono::Utc::now() - chrono::Duration::seconds(1);
        assert!(!valid(&state));
        state.transfers.get_mut(&id).unwrap().deadline = deadline;
        state.transfers.get_mut(&id).unwrap().progress = Some(Instant::now() - IDLE);
        assert!(!valid(&state));
        state.transfers.get_mut(&id).unwrap().progress = None;
    }
    fixture.client.rotate_page_generation("page");
    {
        let state = owner.state.lock().await;
        assert!(!valid(&state));
    }
    {
        let mut state = owner.state.lock().await;
        state.pending.values_mut().next().unwrap().pauses[0].generation =
            fixture.client.page_generation("page");
        assert!(valid(&state));
    }
    ledger.end(channel);
    {
        let state = owner.state.lock().await;
        assert!(!valid(&state));
    }
    let refused = owner
        .request_current(channel, Request::Offer(Vec::new()), ledger)
        .await;
    assert!(refused.is_err());
    owner.retire_transfer(&id).await.unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .unwrap()
        .unwrap());
    fixture.end().await;
}

#[tokio::test]
async fn same_process_frame_commit_retires_its_need_without_changing_view_or_sibling_generation() {
    let fixture = Fixture::new().await;
    fixture.fault.store(false, Ordering::Release);
    let settled = Arc::new(Settled::default());
    settled.complete(true);
    let (owner, id, waiting) = seeded(fixture.client.clone(), settled).await;
    let root = fixture.client.page_generation("page");
    let sibling = fixture.client.document_generation("page", Some("sibling"));
    let frame = fixture.client.document_generation("page", Some("child"));
    {
        let mut state = owner.state.lock().await;
        let request = state.transfers[&id].request_id.clone().unwrap();
        let pause = &mut state.pending.get_mut(&request).unwrap().pauses[0];
        pause.frame = Some("child".into());
        pause.generation = frame;
        assert!(current(&state, &state.transfers[&id]));
    }
    fixture
        .client
        .send_command("Test.frameCommit", Some(json!({"frame":"child"})), None)
        .await
        .unwrap();
    {
        let state = owner.state.lock().await;
        assert!(
            !current(&state, &state.transfers[&id]),
            "same-process frame commit is an actual stale-need boundary"
        );
    }
    assert_eq!(fixture.client.page_generation("page"), root);
    assert_eq!(
        fixture.client.document_generation("page", Some("sibling")),
        sibling
    );
    owner.retire_transfer(&id).await.unwrap();
    let _ = waiting.await;
    fixture.end().await;
}

#[tokio::test]
async fn stale_pending_and_expiry_receipts_do_not_clear_replacement_uses() {
    let fixture = Fixture::new().await;
    fixture.fault.store(false, Ordering::Release);
    let settled = Arc::new(Settled::default());
    settled.complete(true);
    let (owner, id, waiting) = seeded(fixture.client.clone(), settled).await;
    let new_use = "44444444-4444-4444-8444-444444444444".to_owned();
    let expiry = chrono::Utc::now() - chrono::Duration::seconds(1);
    owner.state.lock().await.held.insert(
        "https://example.com".into(),
        Held {
            use_id: new_use.clone(),
            mode: Mode::Read,
            origins: HashSet::new(),
            expires_at: Some(expiry),
        },
    );
    owner.retire_transfer(&id).await.unwrap();
    assert!(!tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .unwrap()
        .unwrap());
    assert_eq!(
        owner.state.lock().await.held["https://example.com"].use_id,
        new_use
    );
    assert!(owner
        .detach_expected(
            "https://example.com",
            Some(ExpectedHeld {
                channel: None,
                use_id: Some("22222222-2222-4222-8222-222222222222".into()),
                expires_at: Some(expiry)
            })
        )
        .await
        .is_err());
    owner
        .state
        .lock()
        .await
        .held
        .get_mut("https://example.com")
        .unwrap()
        .expires_at = Some(expiry + chrono::Duration::hours(1));
    assert!(owner
        .detach_expected(
            "https://example.com",
            Some(ExpectedHeld {
                channel: None,
                use_id: Some(new_use.clone()),
                expires_at: Some(expiry)
            })
        )
        .await
        .is_err());
    assert_eq!(
        owner.state.lock().await.held["https://example.com"].use_id,
        new_use
    );
    fixture.end().await;
}

#[tokio::test]
async fn malformed_digest_and_utf8_never_reach_native_import() {
    for (malformed_utf8, prior_state) in
        [(false, false), (true, false), (false, true), (true, true)]
    {
        let fixture = Fixture::new().await;
        fixture.fault.store(false, Ordering::Release);
        fixture.cookie.store(prior_state, Ordering::Release);
        let settled = Arc::new(Settled::default());
        let (owner, id, waiting) = seeded(fixture.client.clone(), settled).await;
        let channel = owner.state.lock().await.transfers[&id].channel;
        {
            let mut state = owner.state.lock().await;
            let transfer = state.transfers.get_mut(&id).unwrap();
            transfer.applying = None;
            transfer.bytes = Some(Bytes::new().unwrap());
        }
        let body = if malformed_utf8 {
            vec![0xff]
        } else {
            b"{}".to_vec()
        };
        owner
            .transfer(
                channel,
                TransferRequest::Part {
                    transfer_id: id.clone(),
                    offset: 0,
                    data: body.clone(),
                },
            )
            .await
            .unwrap();
        let digest = if malformed_utf8 {
            hex::encode(Sha256::digest(&body))
        } else {
            "0".repeat(64)
        };
        assert!(owner
            .transfer(
                channel,
                TransferRequest::Finish {
                    transfer_id: id.clone(),
                    bytes: body.len() as u64,
                    sha256: digest
                }
            )
            .await
            .is_err());
        assert_eq!(
            fixture.cookie.load(Ordering::Acquire),
            prior_state,
            "unverified bytes neither import nor clear prior complete state"
        );
        assert!(tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap());
        assert!(owner.state.lock().await.transfers.is_empty());
        fixture.end().await;
    }
}

#[tokio::test]
async fn offered_expiry_limits_export_even_without_an_attached_use() {
    let fixture = Fixture::new().await;
    fixture.fault.store(false, Ordering::Release);
    let settled = Arc::new(Settled::default());
    let (owner, id, waiting) = seeded(fixture.client.clone(), settled).await;
    let channel = owner.state.lock().await.transfers[&id].channel;
    {
        let mut state = owner.state.lock().await;
        state.transfers.get_mut(&id).unwrap().applying = None;
        state
            .offers
            .get_mut("https://example.com")
            .unwrap()
            .expires_at = Some((chrono::Utc::now() + chrono::Duration::seconds(5)).to_rfc3339());
    }
    assert!(owner
        .transfer(
            channel,
            TransferRequest::Export {
                site: "https://example.com".into(),
                expected_use: None,
                deadline: chrono::Utc::now() + chrono::Duration::seconds(30)
            }
        )
        .await
        .is_err());
    owner.retire_transfer(&id).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .unwrap()
        .unwrap();
    fixture.end().await;
}

#[tokio::test]
async fn real_progress_idle_retires_incomplete_bytes_and_duplicate_does_not_extend_it() {
    let fixture = Fixture::new().await;
    fixture.fault.store(false, Ordering::Release);
    let settled = Arc::new(Settled::default());
    let (owner, id, waiting) = seeded(fixture.client.clone(), settled).await;
    let channel = owner.state.lock().await.transfers[&id].channel;
    {
        let mut state = owner.state.lock().await;
        let transfer = state.transfers.get_mut(&id).unwrap();
        transfer.applying = None;
        transfer.bytes = Some(Bytes::new().unwrap());
    }
    let part = || TransferRequest::Part {
        transfer_id: id.clone(),
        offset: 0,
        data: b"{".to_vec(),
    };
    owner.transfer(channel, part()).await.unwrap();
    let progress = owner.state.lock().await.transfers[&id].progress;
    owner.transfer(channel, part()).await.unwrap();
    assert_eq!(owner.state.lock().await.transfers[&id].progress, progress);
    owner.watch_transfer(id.clone());
    assert!(tokio::time::timeout(IDLE + Duration::from_secs(2), waiting)
        .await
        .unwrap()
        .unwrap());
    assert!(owner.state.lock().await.transfers.is_empty());
    assert!(!fixture.cookie.load(Ordering::Acquire));
    assert!(owner
        .transfer(
            channel,
            TransferRequest::Finish {
                transfer_id: id,
                bytes: 1,
                sha256: hex::encode(Sha256::digest(b"{"))
            }
        )
        .await
        .is_err());
    fixture.end().await;
}
