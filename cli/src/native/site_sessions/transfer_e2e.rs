//! Synthetic ordinary native/byte-consumer proof, outside HOST activation.

use super::*;
use crate::native::browser::WaitUntil;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn ask(
    owner: &Arc<Custody>,
    channel: ChannelId,
    kind: &str,
    value: Value,
) -> Result<Value, &'static str> {
    owner.request(channel, Request::read(kind, value)?).await
}
async fn need(
    owner: &Arc<Custody>,
    client: &Arc<CdpClient>,
    session: &str,
    origin: &str,
) -> (Value, tokio::task::JoinHandle<()>) {
    let mut events = owner.subscribe();
    let owner = owner.clone();
    let client = client.clone();
    let session = session.to_owned();
    let origin = origin.to_owned();
    let pause = tokio::spawn(async move {
        owner
            .paused(
                &client,
                session,
                json!({"requestId":"synthetic-transfer-document","request":{"url":origin}}),
            )
            .await;
    });
    let (_, need) = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap();
    (need, pause)
}

async fn embedded_storage(client: &CdpClient, page: &str) -> Value {
    let mut events = client.subscribe();
    let (frame, session) = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let tree = client
                .send_command_no_params("Page.getFrameTree", Some(page))
                .await
                .unwrap();
            if let Some(frame) = tree["frameTree"]["childFrames"][0]["frame"]["id"].as_str() {
                return (
                    frame.to_owned(),
                    client
                        .session_for_target(frame)
                        .unwrap_or_else(|| page.to_owned()),
                );
            }
            let targets = client
                .send_command_no_params("Target.getTargets", None)
                .await
                .unwrap();
            if let Some((frame, session)) = targets["targetInfos"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|target| target["type"] == "iframe")
                .find_map(|target| {
                    let frame = target["targetId"].as_str()?;
                    let session = client.session_for_target(frame)?;
                    (client.page_of(&session) == client.page_of(page))
                        .then(|| (frame.to_owned(), session))
                })
            {
                return (frame, session);
            }
            let event = events.recv().await.unwrap();
            if event.method == "Target.targetDestroyed"
                && event.params["targetId"] == client.target_for_session(page).unwrap()
            {
                panic!("the fixture parent closed");
            }
        }
    })
    .await
    .expect("the real embedded frame becomes observable on its own native session");
    let world = client
        .send_command(
            "Page.createIsolatedWorld",
            Some(json!({"frameId":frame,"worldName":"nosecret-retirement-proof"})),
            Some(&session),
        )
        .await
        .unwrap();
    let value=client.send_command("Runtime.evaluate",Some(json!({"expression":"localStorage.getItem('retained-frame')","contextId":world["executionContextId"],"returnByValue":true})),Some(&session)).await.unwrap();
    assert!(value.get("exceptionDetails").is_none());
    value["result"]["value"].clone()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_site_state_transfer_large_unicode_digest_and_current_lifecycle() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = [0; 4096];
                let _ = socket.read(&mut request).await;
                let body = "<!doctype html><title>Transfer</title>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length:{}\r\nConnection:close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    let mut daemon = crate::native::actions::DaemonState::new();
    let launched=Box::pin(crate::native::actions::execute_command(&json!({"action":"launch","headless":true,
            "executablePath":std::env::var("AMBIT_TEST_CHROME_EXECUTABLE").unwrap_or("/usr/bin/google-chrome".into())}),&mut daemon)).await;
    assert_eq!(
        launched["success"], true,
        "the ordinary fixture launch succeeds"
    );
    let browser = daemon.browser.as_mut().unwrap();
    browser.navigate(&origin, WaitUntil::Load).await.unwrap();
    let client = browser.client.clone();
    let session = browser.active_session_id().unwrap().to_owned();
    let channel = ChannelId::parse("11111111-1111-4111-8111-111111111111").unwrap();
    let foreign = ChannelId::parse("33333333-3333-4333-8333-333333333333").unwrap();
    let owner = Custody::new();
    ask(
        &owner,
        channel,
        "site_sessions.offer",
        json!({"sites":[{"site":"http://127.0.0.1","mode":"act"}]}),
    )
    .await
    .unwrap();
    owner
        .browser_ready(client.clone(), vec![session.clone()])
        .await
        .unwrap();
    let (need, pause) = need(&owner, &client, &session, &origin).await;
    let deadline = (chrono::Utc::now() + chrono::Duration::seconds(45)).to_rfc3339();
    let begin = json!({"requestId":need["requestId"],"useId":"22222222-2222-4222-8222-222222222222","site":"http://127.0.0.1","mode":"act","pageGeneration":need["pageGeneration"],"deadlineAt":deadline});
    let mut wrong = begin.clone();
    wrong["pageGeneration"] = json!("retired");
    assert!(ask(&owner, channel, "site_session.attach.begin", wrong)
        .await
        .is_err());
    assert!(
        ask(&owner, foreign, "site_session.attach.begin", begin.clone())
            .await
            .is_err()
    );
    let begun = ask(&owner, channel, "site_session.attach.begin", begin.clone())
        .await
        .unwrap();
    let id = begun["transferId"].as_str().unwrap();
    assert!(
        ask(&owner, channel, "site_session.attach.begin", begin.clone())
            .await
            .is_err()
    );
    // BEGIN consumes only need freshness, not a new authority lifetime.
    owner
        .state
        .lock()
        .await
        .pending
        .get_mut(need["requestId"].as_str().unwrap())
        .unwrap()
        .expires = Instant::now() - Duration::from_secs(1);
    let first = "nosecret 雪😀\"\\\n\t\0".repeat(400_000);
    let second = "nosecret 𝄞é\r\\雪\"".repeat(400_000);
    let expected = vec![
        hex::encode(Sha256::digest(first.as_bytes())),
        hex::encode(Sha256::digest(second.as_bytes())),
    ];
    let document = json!({"format":state::FORMAT,"site":"http://127.0.0.1","capturedAt":"2026-09-30T00:00:00.000Z","chromeMajor":154,"cookies":[],
            "origins":[{"origin":origin,"localStorage":[["transfer-marker","nosecret-transfer"]],"indexedDB":[{"name":"transfer-rows","version":1,"stores":[{"name":"rows","keyPath":null,"autoIncrement":true,"nextKey":3,"indexes":[],"records":[{"key":1,"value":first},{"key":2,"value":second}]}]}]}],"omitted":[]});
    let body = serde_json::to_vec(&document).unwrap();
    assert!(body.len() > state::MAX_BYTES);
    let mut offset = 0;
    for chunk in body.chunks(CHUNK_BYTES) {
        let part = json!({"transferId":id,"offset":offset,"data":STANDARD.encode(chunk)});
        assert!(
            ask(&owner, foreign, "site_session.state.part", part.clone())
                .await
                .is_err()
        );
        let written = ask(&owner, channel, "site_session.state.part", part.clone())
            .await
            .unwrap();
        assert_eq!(written["nextOffset"], offset + chunk.len());
        let progress = owner.state.lock().await.transfers[id].progress;
        assert_eq!(
            ask(&owner, channel, "site_session.state.part", part)
                .await
                .unwrap()["nextOffset"],
            offset + chunk.len()
        );
        assert_eq!(
            owner.state.lock().await.transfers[id].progress,
            progress,
            "replay does not extend idle"
        );
        if offset == 0 {
            assert!(ask(
                &owner,
                channel,
                "site_session.state.part",
                json!({"transferId":id,"offset":0,"data":"eA=="})
            )
            .await
            .is_err());
        }
        offset += chunk.len();
    }
    assert!(
        !pause.is_finished(),
        "no part makes imported state visible or unpauses the document"
    );
    ask(
        &owner,
        channel,
        "site_session.attach.finish",
        json!({"transferId":id,"bytes":body.len(),"sha256":hex::encode(Sha256::digest(&body))}),
    )
    .await
    .unwrap();
    pause.await.unwrap();
    let verified=client.send_command("Runtime.evaluate",Some(json!({"expression":r#"new Promise((resolve,reject)=>{const opening=indexedDB.open('transfer-rows');opening.onerror=()=>reject(opening.error);opening.onsuccess=()=>{const db=opening.result;const transaction=db.transaction('rows','readwrite');const store=transaction.objectStore('rows');const values=store.getAll();const next=store.add('nosecret-next');let key;next.onsuccess=()=>key=next.result;transaction.oncomplete=async()=>{db.close();const hashes=await Promise.all(values.result.map(async value=>Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256',new TextEncoder().encode(value))),v=>v.toString(16).padStart(2,'0')).join('')));resolve({hashes,next:key})};transaction.onerror=()=>reject(transaction.error)}})"#,"awaitPromise":true,"returnByValue":true})),Some(&session)).await.unwrap();
    assert_eq!(verified["result"]["value"]["hashes"], json!(expected));
    assert_eq!(verified["result"]["value"]["next"], 3);
    let export=ask(&owner,channel,"site_session.export",json!({"sites":["http://127.0.0.1"],"transfer":true,"expectedUseId":"22222222-2222-4222-8222-222222222222","deadlineAt":deadline})).await.unwrap();
    let export_id = export["transferId"].as_str().unwrap();
    let mut exported = Vec::new();
    let mut offset = 0;
    loop {
        let part = ask(
            &owner,
            channel,
            "site_session.state.read",
            json!({"transferId":export_id,"offset":offset}),
        )
        .await
        .unwrap();
        let chunk = STANDARD.decode(part["data"].as_str().unwrap()).unwrap();
        assert!(chunk.len() <= CHUNK_BYTES);
        exported.extend(chunk);
        if part["done"] == true {
            break;
        }
        offset = part["nextOffset"].as_u64().unwrap();
    }
    assert_eq!(export["bytes"], exported.len());
    assert_eq!(export["sha256"], hex::encode(Sha256::digest(&exported)));
    let captured = SiteState::read_streamed(
        serde_json::from_slice(&exported).unwrap(),
        "http://127.0.0.1",
    )
    .unwrap();
    let store = &captured.origins[0].indexed_db[0].stores[0];
    assert_eq!(store.next_key, Some(json!(4)));
    assert_eq!(
        hex::encode(Sha256::digest(
            store.records[0].value.as_str().unwrap().as_bytes()
        )),
        expected[0]
    );
    assert_eq!(
        hex::encode(Sha256::digest(
            store.records[1].value.as_str().unwrap().as_bytes()
        )),
        expected[1]
    );
    assert!(ask(
        &owner,
        channel,
        "site_session.detach",
        json!({"sites":["http://127.0.0.1"],"reason":"ended","expectedUseId":null})
    )
    .await
    .is_err());
    ask(
        &owner,
        channel,
        "site_session.state.close",
        json!({"transferId":export_id}),
    )
    .await
    .unwrap();
    owner.end(channel).await;
    assert!(owner.state.lock().await.transfers.is_empty());
    storage::clear_site(&client, "http://127.0.0.1", &[origin])
        .await
        .unwrap();
    browser.close().await.unwrap();
    server.abort();
    let _ = server.await;
    eprintln!(
        "transfer_receipt={}",
        json!({"documentBytes":body.len(),"rowSha256":expected,"exportBytes":exported.len(),"nextKeyAfterImport":3,"nextKeyAfterExport":4,"chunkBytes":CHUNK_BYTES})
    );
}
/// Cancel only after real Chrome reports a partial origin write, then abandon
/// the caller. The effect owner must still retire the importer before release.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_site_state_transfer_cancelled_waiter_settles_real_partial_import() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = [0; 4096];
                let _ = socket.read(&mut request).await;
                let body = "<!doctype html><title>Cancellation</title>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length:{}\r\nConnection:close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    let mut daemon = crate::native::actions::DaemonState::new();
    let launched=Box::pin(crate::native::actions::execute_command(&json!({"action":"launch","headless":true,
        "executablePath":std::env::var("AMBIT_TEST_CHROME_EXECUTABLE").unwrap_or("/usr/bin/google-chrome".into())}),&mut daemon)).await;
    assert_eq!(launched["success"], true);
    let browser = daemon.browser.as_mut().unwrap();
    browser.navigate(&origin, WaitUntil::Load).await.unwrap();
    let client = browser.client.clone();
    let session = browser.active_session_id().unwrap().to_owned();
    client
        .send_command_no_params("DOMStorage.enable", Some(&session))
        .await
        .unwrap();
    let channel = ChannelId::parse("11111111-1111-4111-8111-111111111111").unwrap();
    let owner = Custody::new();
    ask(
        &owner,
        channel,
        "site_sessions.offer",
        json!({"sites":[{"site":"http://127.0.0.1","mode":"act"}]}),
    )
    .await
    .unwrap();
    owner
        .browser_ready(client.clone(), vec![session.clone()])
        .await
        .unwrap();
    let (requested, pause) = need(&owner, &client, &session, &origin).await;
    let begin=ask(&owner,channel,"site_session.attach.begin",json!({"requestId":requested["requestId"],
        "site":"http://127.0.0.1","useId":"22222222-2222-4222-8222-222222222222","mode":"act",
        "pageGeneration":requested["pageGeneration"],"deadlineAt":(chrono::Utc::now()+chrono::Duration::seconds(45)).to_rfc3339()})).await.unwrap();
    let id = begin["transferId"].as_str().unwrap().to_owned();
    let records = (0..50_000)
        .map(|key| json!({"key":key,"value":format!("nosecret partial 雪😀 row {key}")}))
        .collect::<Vec<_>>();
    let document = json!({"format":state::FORMAT,"site":"http://127.0.0.1","capturedAt":"2026-09-30T00:00:00Z","chromeMajor":154,
        "cookies":[],"origins":[{"origin":origin,"localStorage":[["partial-import","nosecret-partial"]],
        "indexedDB":[{"name":"partial-import","version":1,"stores":[{"name":"rows","keyPath":null,"autoIncrement":false,"indexes":[],"records":records}]}]}],"omitted":[]});
    let body = serde_json::to_vec(&document).unwrap();
    let mut offset = 0;
    for chunk in body.chunks(CHUNK_BYTES) {
        ask(
            &owner,
            channel,
            "site_session.state.part",
            json!({"transferId":id,"offset":offset,"data":STANDARD.encode(chunk)}),
        )
        .await
        .unwrap();
        offset += chunk.len();
    }
    let mut observed = client.subscribe();
    let mut cancelled = owner.state.lock().await.transfers[&id].cancel.subscribe();
    let finish = tokio::spawn({
        let owner = owner.clone();
        let id = id.clone();
        async move {
            ask(&owner,channel,"site_session.attach.finish",json!({"transferId":id,"bytes":body.len(),"sha256":hex::encode(Sha256::digest(&body))})).await
        }
    });
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let event = observed.recv().await.unwrap();
            if event.method == "DOMStorage.domStorageItemAdded"
                && event.params["storageId"]["securityOrigin"] == origin
                && event.params["key"] == "partial-import"
            {
                break;
            }
        }
    })
    .await
    .expect("actual Chrome reports the import's partial origin write");
    let settled = owner.state.lock().await.transfers[&id]
        .applying
        .clone()
        .expect("import remains owned after its first write");
    assert!(
        !pause.is_finished(),
        "partial state never releases the paused document"
    );
    let retirement = tokio::spawn({
        let owner = owner.clone();
        let id = id.clone();
        async move { owner.retire_transfer(&id).await }
    });
    tokio::time::timeout(Duration::from_secs(2), cancelled.changed())
        .await
        .unwrap()
        .unwrap();
    assert!(*cancelled.borrow());
    retirement.abort();
    let _ = retirement.await;
    if settled.done.load(Ordering::Acquire) == 0 {
        assert!(
            !pause.is_finished(),
            "abandoned cancellation waiter cannot release before settlement"
        );
    }
    assert!(
        tokio::time::timeout(Duration::from_secs(15), settled.wait())
            .await
            .unwrap(),
        "real importer closes and partial state clears"
    );
    assert!(finish.await.unwrap().is_err());
    tokio::time::timeout(Duration::from_secs(2), pause)
        .await
        .unwrap()
        .unwrap();
    assert!(
        owner.context.owned_targets().is_empty(),
        "private importer mask remains until actual target absence"
    );
    browser.navigate(&origin, WaitUntil::Load).await.unwrap();
    let after=client.send_command("Runtime.evaluate",Some(json!({"expression":"(async()=>({storage:localStorage.getItem('partial-import'),databases:(await indexedDB.databases()).map(database=>database.name)}))()","awaitPromise":true,"returnByValue":true})),Some(browser.active_session_id().unwrap())).await.unwrap();
    browser.close().await.unwrap();
    server.abort();
    let _ = server.await;
    assert_eq!(after["result"]["value"]["storage"], Value::Null);
    assert!(!after["result"]["value"]["databases"]
        .as_array()
        .unwrap()
        .iter()
        .any(|database| database == "partial-import"));
    assert!(owner.state.lock().await.transfers.is_empty());
    eprintln!(
        "partial_import_cancel_receipt={}",
        json!({"actualPartialWriteObserved":true,"cancelledWaiter":true,"settledBeforePauseRelease":true,"privateTargets":0,"stateAbsentInFreshDocument":true})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_site_state_first_capture_includes_existing_document_without_saved_use() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = [0; 4096];
                let _ = socket.read(&mut request).await;
                let body = "<!doctype html><title>First capture</title>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length:{}\r\nConnection:close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    let mut daemon = crate::native::actions::DaemonState::new();
    let launched=Box::pin(crate::native::actions::execute_command(&json!({"action":"launch","headless":true,
        "executablePath":std::env::var("AMBIT_TEST_CHROME_EXECUTABLE").unwrap_or("/usr/bin/google-chrome".into())}),&mut daemon)).await;
    assert_eq!(launched["success"], true);
    let browser = daemon.browser.as_mut().unwrap();
    browser.navigate(&origin, WaitUntil::Load).await.unwrap();
    let client = browser.client.clone();
    let session = browser.active_session_id().unwrap().to_owned();
    let seeded=client.send_command("Runtime.evaluate",Some(json!({"expression":r#"(async()=>{localStorage.setItem('first-capture','nosecret 雪😀\\\"');document.cookie='first_capture=nosecret-cookie;path=/';const opening=indexedDB.open('first-capture',1);const db=await new Promise((resolve,reject)=>{opening.onupgradeneeded=()=>opening.result.createObjectStore('rows',{autoIncrement:true});opening.onsuccess=()=>resolve(opening.result);opening.onerror=()=>reject(opening.error)});const tx=db.transaction('rows','readwrite');tx.objectStore('rows').add('nosecret 雪😀\\\" interior');tx.objectStore('rows').add('nosecret 𝄞é\n escaped');await new Promise((resolve,reject)=>{tx.oncomplete=resolve;tx.onerror=()=>reject(tx.error)});db.close();return true})()"#,"awaitPromise":true,"returnByValue":true})),Some(&session)).await.unwrap();
    assert_eq!(seeded["result"]["value"], true);
    let channel = ChannelId::parse("11111111-1111-4111-8111-111111111111").unwrap();
    let owner = Custody::new();
    ask(&owner, channel, "site_sessions.offer", json!({"sites":[]}))
        .await
        .unwrap();
    owner
        .browser_ready(client.clone(), vec![session])
        .await
        .unwrap();
    assert!(owner.state.lock().await.held.is_empty());
    let export=ask(&owner,channel,"site_session.export",json!({"sites":["http://127.0.0.1"],"transfer":true,"expectedUseId":null,"deadlineAt":(chrono::Utc::now()+chrono::Duration::seconds(30)).to_rfc3339()})).await.unwrap();
    let id = export["transferId"].as_str().unwrap();
    let mut body = Vec::new();
    let mut offset = 0;
    loop {
        let part = ask(
            &owner,
            channel,
            "site_session.state.read",
            json!({"transferId":id,"offset":offset}),
        )
        .await
        .unwrap();
        body.extend(STANDARD.decode(part["data"].as_str().unwrap()).unwrap());
        if part["done"] == true {
            break;
        }
        offset = part["nextOffset"].as_u64().unwrap();
    }
    ask(
        &owner,
        channel,
        "site_session.state.close",
        json!({"transferId":id}),
    )
    .await
    .unwrap();
    let captured =
        SiteState::read_streamed(serde_json::from_slice(&body).unwrap(), "http://127.0.0.1")
            .unwrap();
    browser.close().await.unwrap();
    server.abort();
    let _ = server.await;
    assert_eq!(export["useId"], Value::Null);
    assert_eq!(export["bytes"], body.len());
    assert_eq!(export["sha256"], hex::encode(Sha256::digest(&body)));
    assert!(captured
        .cookies
        .iter()
        .any(|cookie| cookie.name == "first_capture"));
    let stored=captured.origins.iter().find(|stored|stored.origin==origin).expect("first capture retains the actual current document origin without a saved use or a later navigation");
    assert!(stored
        .local_storage
        .iter()
        .any(|pair| pair.0 == "first-capture"));
    let rows = &stored
        .indexed_db
        .iter()
        .find(|database| database.name == "first-capture")
        .unwrap()
        .stores[0];
    assert_eq!(rows.records.len(), 2);
    assert_eq!(rows.next_key, Some(json!(3)));
    assert!(captured.omitted.is_empty());
    eprintln!(
        "first_capture_receipt={}",
        json!({"expectedUseId":null,"originAlreadyOpenBeforeAdmission":true,"origins":captured.origins.len(),"localStorageCanary":true,"rows":2,"nextKey":3,"omissions":0})
    );
}

struct NativeFixture {
    custody: Arc<Custody>,
    client: Arc<CdpClient>,
    session: String,
    origin: String,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_site_state_revocation_retires_cached_frames_and_preserves_unrelated_history() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{port}");
    let other = format!("http://localhost:{port}");
    let frame = origin.clone();
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let frame = frame.clone();
            tokio::spawn(async move {
                let mut request = [0; 4096];
                let count = socket.read(&mut request).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..count]);
                let body = if request.starts_with("GET /container ") {
                    format!("<!doctype html><title>Container</title><iframe src='{frame}/writer'></iframe>")
                } else if request.starts_with("GET /probe-container ") {
                    format!("<!doctype html><title>Readonly container</title><iframe src='{frame}/reader'></iframe>")
                } else if request.starts_with("GET /writer ") {
                    "<!doctype html><title>Writer</title><script>globalThis.nosecretOldWriter=()=>{localStorage.setItem('retained-frame','nosecret-retained-frame');document.cookie='retained_frame=nosecret-retained;path=/'};nosecretOldWriter();addEventListener('pagehide',nosecretOldWriter)</script>".into()
                } else {
                    "<!doctype html><title>Unrelated</title><script>globalThis.nosecretUnrelated=true</script>".into()
                };
                let response=format!("HTTP/1.1 200 OK\r\nContent-Type:text/html\r\nContent-Length:{}\r\nConnection:close\r\n\r\n{body}",body.len());
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    let mut daemon = crate::native::actions::DaemonState::new();
    let launched=Box::pin(crate::native::actions::execute_command(&json!({"action":"launch","headless":true,"executablePath":std::env::var("AMBIT_TEST_CHROME_EXECUTABLE").unwrap_or("/usr/bin/google-chrome".into())}),&mut daemon)).await;
    assert_eq!(launched["success"], true);
    let browser = daemon.browser.as_mut().unwrap();
    let client = browser.client.clone();
    let owner = Custody::new();
    let affected = browser.active_session_id().unwrap().to_owned();
    let affected_target = browser.active_target_id().unwrap().to_owned();
    owner
        .browser_ready(client.clone(), vec![affected.clone()])
        .await
        .unwrap();
    browser
        .navigate(&format!("{other}/container"), WaitUntil::Load)
        .await
        .unwrap();
    assert_eq!(
        embedded_storage(&client, &affected).await,
        "nosecret-retained-frame",
        "the actual cross-origin frame ran its writer"
    );
    browser
        .navigate(&format!("{other}/after-frame"), WaitUntil::Load)
        .await
        .unwrap();
    let before_affected = client
        .send_command_no_params("Page.getNavigationHistory", Some(&affected))
        .await
        .unwrap();
    assert!(before_affected["entries"].as_array().unwrap().len() > 1);
    let own=client.send_command("Runtime.evaluate",Some(json!({"expression":"localStorage.setItem('B-own','nosecret-B-own');document.cookie='B_own=nosecret-B-cookie;path=/';true","returnByValue":true})),Some(&affected)).await.unwrap();
    assert_eq!(own["result"]["value"], true);
    browser.tab_new(None, None).await.unwrap();
    let unrelated = browser.active_session_id().unwrap().to_owned();
    let unrelated_target = browser.active_target_id().unwrap().to_owned();
    browser
        .navigate(&format!("{other}/unrelated-0"), WaitUntil::Load)
        .await
        .unwrap();
    browser
        .navigate(&format!("{other}/unrelated-1"), WaitUntil::Load)
        .await
        .unwrap();
    let before_unrelated = client
        .send_command_no_params("Page.getNavigationHistory", Some(&unrelated))
        .await
        .unwrap();
    let channel = ChannelId::parse("11111111-1111-4111-8111-111111111111").unwrap();
    let receipt = ask(
        &owner,
        channel,
        "site_session.detach",
        json!({"sites":["http://127.0.0.1"],"reason":"revoked"}),
    )
    .await
    .unwrap();
    assert_eq!(receipt["sites"][0]["cleared"], true);
    assert_eq!(
        client.target_for_session(&affected).as_deref(),
        Some(affected_target.as_str())
    );
    assert_eq!(
        client.target_for_session(&unrelated).as_deref(),
        Some(unrelated_target.as_str())
    );
    let after_affected = client
        .send_command_no_params("Page.getNavigationHistory", Some(&affected))
        .await
        .unwrap();
    assert_eq!(after_affected["entries"].as_array().unwrap().len(), 1);
    assert_eq!(after_affected["entries"][0]["url"], "about:blank");
    let after_unrelated = client
        .send_command_no_params("Page.getNavigationHistory", Some(&unrelated))
        .await
        .unwrap();
    assert_eq!(
        after_unrelated, before_unrelated,
        "unrelated history and current index are exact"
    );
    let unaffected = client
        .send_command(
            "Runtime.evaluate",
            Some(json!({"expression":"nosecretUnrelated===true && localStorage.getItem('B-own')==='nosecret-B-own' && document.cookie.includes('B_own=nosecret-B-cookie')","returnByValue":true})),
            Some(&unrelated),
        )
        .await
        .unwrap();
    assert_eq!(unaffected["result"]["value"], true);
    let clean = storage::capture(&client, "http://127.0.0.1", std::slice::from_ref(&origin))
        .await
        .unwrap();
    assert!(clean.cookies.is_empty());
    assert!(clean.origins[0].local_storage.is_empty());
    assert!(
        client
            .send_command(
                "Page.navigateToHistoryEntry",
                Some(json!({"entryId":before_affected["entries"][1]["id"]})),
                Some(&affected)
            )
            .await
            .is_err(),
        "the cached writer entry cannot be restored"
    );
    browser
        .navigate(&format!("{other}/probe-container"), WaitUntil::Load)
        .await
        .unwrap();
    assert_eq!(
        embedded_storage(&client, &unrelated).await,
        Value::Null,
        "readonly fresh frame cannot recover prior partitioned storage"
    );
    browser.close().await.unwrap();
    server.abort();
    let _ = server.await;
    eprintln!(
        "retained_history_receipt={}",
        json!({"cachedCrossOriginFrameRetired":true,"affectedTargetRetained":true,"affectedHistoryEntries":1,"unrelatedTargetRetained":true,"unrelatedHistoryExact":true,"BownDataExact":true,"oldEntryRejected":true,"partitionedAStateAbsent":true})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_site_state_unknown_adopted_history_refuses_before_destructive_cleanup() {
    let mut browser = crate::native::browser::BrowserManager::launch(
        crate::native::cdp::chrome::LaunchOptions {
            headless: true,
            executable_path: Some(
                std::env::var("AMBIT_TEST_CHROME_EXECUTABLE")
                    .unwrap_or("/usr/bin/google-chrome".into()),
            ),
            ..Default::default()
        },
        None,
    )
    .await
    .unwrap();
    browser
        .navigate(
            "data:text/html,<title>Unknown history</title>",
            WaitUntil::Load,
        )
        .await
        .unwrap();
    let session = browser.active_session_id().unwrap().to_owned();
    let before = browser
        .client
        .send_command_no_params("Page.getNavigationHistory", Some(&session))
        .await
        .unwrap();
    browser.client.send_command("Storage.setCookies",Some(json!({"cookies":[{"name":"unknown_history","value":"nosecret-retained-cookie","domain":"example.com","path":"/"}]})),None).await.unwrap();
    let adopted = Arc::new(CdpClient::connect(browser.get_cdp_url()).await.unwrap());
    let attach = adopted
        .send_command(
            "Target.attachToTarget",
            Some(json!({"targetId":browser.active_target_id().unwrap(),"flatten":true})),
            None,
        )
        .await
        .unwrap();
    let adopted_session = attach["sessionId"].as_str().unwrap().to_owned();
    let owner = Custody::new();
    owner
        .browser_ready(adopted.clone(), vec![adopted_session])
        .await
        .unwrap();
    let channel = ChannelId::parse("11111111-1111-4111-8111-111111111111").unwrap();
    assert_eq!(
        ask(
            &owner,
            channel,
            "site_session.detach",
            json!({"sites":["https://example.com"],"reason":"revoked"})
        )
        .await,
        Err(storage::UNCLEARED)
    );
    let after = browser
        .client
        .send_command_no_params("Page.getNavigationHistory", Some(&session))
        .await
        .unwrap();
    assert_eq!(after, before);
    let cookies = browser
        .client
        .send_command_no_params("Storage.getCookies", None)
        .await
        .unwrap();
    assert!(cookies["cookies"]
        .as_array()
        .unwrap()
        .iter()
        .any(|cookie| cookie["name"] == "unknown_history"));
    adopted.disconnect();
    browser.close().await.unwrap();
    eprintln!(
        "unknown_history_receipt={}",
        json!({"refusedBeforeMutation":true,"historyExact":true,"cookiePreserved":true})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_site_state_partition_inventory_after_actual_native_process_restart() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{port}");
    let other = format!("http://localhost:{port}");
    let frame = origin.clone();
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let frame = frame.clone();
            tokio::spawn(async move {
                let mut request = [0; 4096];
                let count = socket.read(&mut request).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..count]);
                let body = if request.starts_with("GET /container ") {
                    format!("<!doctype html><iframe src='{frame}/writer'></iframe>")
                } else if request.starts_with("GET /probe-container ") {
                    format!("<!doctype html><iframe src='{frame}/reader'></iframe>")
                } else if request.starts_with("GET /writer ") {
                    "<!doctype html><script>localStorage.setItem('retained-frame','nosecret-retained-frame')</script>".into()
                } else {
                    "<!doctype html><title>Readonly</title>".into()
                };
                let response=format!("HTTP/1.1 200 OK\r\nContent-Type:text/html\r\nContent-Length:{}\r\nConnection:close\r\n\r\n{body}",body.len());
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    let mut daemon = crate::native::actions::DaemonState::new();
    let launched=Box::pin(crate::native::actions::execute_command(&json!({"action":"launch","headless":true,"executablePath":std::env::var("AMBIT_TEST_CHROME_EXECUTABLE").unwrap_or("/usr/bin/google-chrome".into())}),&mut daemon)).await;
    assert_eq!(launched["success"], true);
    let browser = daemon.browser.as_mut().unwrap();
    let owner = Custody::new();
    let client = browser.client.clone();
    let session = browser.active_session_id().unwrap().to_owned();
    owner
        .browser_ready(client.clone(), vec![session.clone()])
        .await
        .unwrap();
    browser
        .navigate(&format!("{other}/container"), WaitUntil::Load)
        .await
        .unwrap();
    assert_eq!(
        embedded_storage(&client, &session).await,
        "nosecret-retained-frame"
    );
    client
        .site_profile()
        .documents
        .settle_keys("http://127.0.0.1")
        .await
        .unwrap();
    let own=client.send_command("Runtime.evaluate",Some(json!({"expression":"localStorage.setItem('B-own','nosecret-B-own');document.cookie='B_own=nosecret-B-cookie;path=/;max-age=600';true","returnByValue":true})),Some(&session)).await.unwrap();
    assert_eq!(own["result"]["value"], true);
    let retained = browser.relaunch_options().unwrap();
    let path = retained
        .retained_profile
        .as_ref()
        .unwrap()
        .path()
        .to_owned();
    browser.close().await.unwrap();
    let output=tokio::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","native::site_sessions::custody::transfer::tests::e2e_site_state_partition_restart_child","--ignored","--nocapture","--test-threads=1"]).env("AMBIT_NATIVE_RESTART_PROFILE",path).env("AMBIT_NATIVE_RESTART_PORT",port.to_string()).output().await.unwrap();
    eprintln!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "the independent native process proves refusal without mutation"
    );
    server.abort();
    let _ = server.await;
    drop(retained);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_site_state_partition_restart_child() {
    let Some(path) = std::env::var("AMBIT_NATIVE_RESTART_PROFILE").ok() else {
        return;
    };
    let port = std::env::var("AMBIT_NATIVE_RESTART_PORT").unwrap();
    let other = format!("http://localhost:{port}");
    let mut daemon = crate::native::actions::DaemonState::new();
    let launched=Box::pin(crate::native::actions::execute_command(&json!({"action":"launch","headless":true,"profile":path,"executablePath":std::env::var("AMBIT_TEST_CHROME_EXECUTABLE").unwrap_or("/usr/bin/google-chrome".into())}),&mut daemon)).await;
    assert_eq!(launched["success"], true);
    let browser = daemon.browser.as_mut().unwrap();
    let client = browser.client.clone();
    let session = browser.active_session_id().unwrap().to_owned();
    let owner = Custody::new();
    owner
        .browser_ready(client.clone(), vec![session.clone()])
        .await
        .unwrap();
    browser
        .navigate(&format!("{other}/probe-container"), WaitUntil::Load)
        .await
        .unwrap();
    assert_eq!(
        embedded_storage(&client, &session).await,
        "nosecret-retained-frame",
        "the partition persisted from the earlier native process"
    );
    let before = client
        .send_command_no_params("Page.getNavigationHistory", Some(&session))
        .await
        .unwrap();
    let channel = ChannelId::parse("11111111-1111-4111-8111-111111111111").unwrap();
    assert_eq!(
        ask(
            &owner,
            channel,
            "site_session.detach",
            json!({"sites":["http://127.0.0.1"],"reason":"revoked"})
        )
        .await,
        Err(storage::UNCLEARED)
    );
    assert_eq!(
        client
            .send_command_no_params("Page.getNavigationHistory", Some(&session))
            .await
            .unwrap(),
        before
    );
    assert_eq!(
        embedded_storage(&client, &session).await,
        "nosecret-retained-frame"
    );
    let own=client.send_command("Runtime.evaluate",Some(json!({"expression":"localStorage.getItem('B-own')==='nosecret-B-own' && document.cookie.includes('B_own=nosecret-B-cookie')","returnByValue":true})),Some(&session)).await.unwrap();
    assert_eq!(own["result"]["value"], true);
    browser.close().await.unwrap();
    eprintln!(
        "native_restart_partition_receipt={}",
        json!({"actualIndependentNativeProcess":true,"persistedAPartitionProven":true,"unknownInventoryRefused":true,"historyExact":true,"ADataUnchangedAfterRefusal":true,"BownDataExact":true})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_site_state_cleanup_failure_fences_actual_reconnect_and_retained_relaunch() {
    let mut browser = crate::native::browser::BrowserManager::launch(
        crate::native::cdp::chrome::LaunchOptions {
            headless: true,
            executable_path: Some(
                std::env::var("AMBIT_TEST_CHROME_EXECUTABLE")
                    .unwrap_or("/usr/bin/google-chrome".into()),
            ),
            ..Default::default()
        },
        None,
    )
    .await
    .unwrap();
    let relaunch = browser.relaunch_options().unwrap();
    let retained_clone = relaunch.clone();
    browser.client.send_command("Storage.setCookies",Some(json!({"cookies":[{"name":"cleanup_fixture","value":"nosecret-persisted-cookie","domain":"127.0.0.1","path":"/","expires":1900000000}]})),None).await.unwrap();
    browser.close().await.unwrap();
    let mut browser = crate::native::browser::BrowserManager::launch(relaunch.clone(), None)
        .await
        .unwrap();
    let client = browser.client.clone();
    let persisted = client
        .send_command_no_params("Storage.getCookies", None)
        .await
        .unwrap();
    assert!(
        persisted["cookies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|cookie| cookie["name"] == "cleanup_fixture"),
        "the canary persists through the actual retained-profile relaunch baseline"
    );
    let channel = ChannelId::parse("11111111-1111-4111-8111-111111111111").unwrap();
    let owner = Custody::new();
    ask(
        &owner,
        channel,
        "site_sessions.offer",
        json!({"sites":[{"site":"http://127.0.0.1","mode":"act"}]}),
    )
    .await
    .unwrap();
    owner
        .browser_ready(
            client.clone(),
            vec![browser.active_session_id().unwrap().to_owned()],
        )
        .await
        .unwrap();
    let use_id = "22222222-2222-4222-8222-222222222222";
    owner.state.lock().await.held.insert(
        "http://127.0.0.1".into(),
        Held {
            use_id: use_id.into(),
            mode: Mode::Act,
            origins: HashSet::new(),
            expires_at: None,
        },
    );
    let target = browser.active_target_id().unwrap().to_owned();
    let mut events = client.subscribe();
    let close = client
        .send_command("Target.closeTarget", Some(json!({"targetId":target})), None)
        .await
        .unwrap();
    assert_eq!(close["success"], true);
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.method == "Target.targetDestroyed" && event.params["targetId"] == target {
                break;
            }
        }
    })
    .await
    .unwrap();
    // No page session is left to delete cookies: this is a real CDP cleanup
    // failure, not a test flag or a claim that browser closure erased them.
    assert_eq!(
        ask(
            &owner,
            channel,
            "site_session.detach",
            json!({"sites":["http://127.0.0.1"],"reason":"ended","expectedUseId":use_id})
        )
        .await,
        Err(storage::UNCLEARED)
    );
    assert!(owner
        .state
        .lock()
        .await
        .held
        .contains_key("http://127.0.0.1"));
    assert!(client.site_profile().require_clear().is_err());
    assert!(browser.relaunch_options().is_err());
    assert!(
        crate::native::browser::BrowserManager::launch(retained_clone.clone(), None)
            .await
            .is_err()
    );
    let reconnected = Arc::new(CdpClient::connect(browser.get_cdp_url()).await.unwrap());
    assert!(owner
        .browser_ready(reconnected.clone(), vec![])
        .await
        .is_err());
    let cookies = client
        .send_command_no_params("Storage.getCookies", None)
        .await
        .unwrap();
    assert!(cookies["cookies"]
        .as_array()
        .unwrap()
        .iter()
        .any(|cookie| cookie["name"] == "cleanup_fixture"));
    // Only this existing owner may create a blank cleanup document and attest
    // scoped deletion. The newly dialled connection cannot settle its receipt.
    let mut attached = client.subscribe();
    let created = client
        .send_command(
            "Target.createTarget",
            Some(json!({"url":"about:blank","background":true})),
            None,
        )
        .await
        .unwrap();
    let cleanup_target = created["targetId"].as_str().unwrap();
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let event = attached.recv().await.unwrap();
            if event.method == "Target.attachedToTarget"
                && event.params["targetInfo"]["targetId"] == cleanup_target
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    owner.recover_cleanup(&client).await.unwrap();
    assert!(client.site_profile().require_clear().is_ok());
    assert!(owner.state.lock().await.held.is_empty());
    let cookies = client
        .send_command_no_params("Storage.getCookies", None)
        .await
        .unwrap();
    assert!(!cookies["cookies"]
        .as_array()
        .unwrap()
        .iter()
        .any(|cookie| cookie["name"] == "cleanup_fixture"));
    reconnected.disconnect();
    browser.close().await.unwrap();
    let mut reopened = crate::native::browser::BrowserManager::launch(relaunch, None)
        .await
        .expect("retained relaunch succeeds only after actual owner cleanup");
    let cookies = reopened
        .client
        .send_command_no_params("Storage.getCookies", None)
        .await
        .unwrap();
    assert!(!cookies["cookies"]
        .as_array()
        .unwrap()
        .iter()
        .any(|cookie| cookie["name"] == "cleanup_fixture"));
    reopened.close().await.unwrap();
    eprintln!(
        "profile_cleanup_receipt={}",
        json!({"persistedCookieRelaunchBaseline":true,"actualClearFailure":true,"heldReceiptRetained":true,"freshConnectionDenied":true,"retainedCloneRelaunchDenied":true,"cookieSurvivedTargetRetirement":true,"existingOwnerProvedClear":true,"actualRetainedRelaunchAfterProof":true,"cookieAbsentAfterRelaunch":true})
    );
}

impl crate::native::agent_channel::Browser for NativeFixture {
    async fn site_request(
        &self,
        channel: ChannelId,
        request: Request,
        ledger: Arc<crate::native::agent_channel::ledger::Ledger>,
    ) -> Result<Value, &'static str> {
        let offered = matches!(&request, Request::Offer(_));
        let result = self
            .custody
            .request_current(channel, request, ledger)
            .await?;
        if offered {
            self.custody
                .browser_ready(self.client.clone(), vec![self.session.clone()])
                .await?;
            let (owner, client, session, origin) = (
                self.custody.clone(),
                self.client.clone(),
                self.session.clone(),
                self.origin.clone(),
            );
            tokio::spawn(async move {
                owner.paused(&client,session,json!({"requestId":"synthetic-native-consumer-document","request":{"url":origin}})).await;
            });
        }
        Ok(result)
    }
    fn custody_events(&self) -> Option<tokio::sync::broadcast::Receiver<(ChannelId, Value)>> {
        Some(self.custody.subscribe())
    }
    async fn channel_closed(&self, channel: ChannelId) {
        self.custody.channel_closed(channel).await;
    }
    async fn end(&self, channel: ChannelId) {
        self.custody.end(channel).await;
    }
    async fn step(
        &self,
        _: &crate::native::agent_channel::FrameContext<'_>,
        _: &crate::native::agent_channel::step::PreparedStep,
    ) -> crate::native::agent_channel::StepRecord {
        panic!("synthetic fixture never serves model operations")
    }
    async fn finish(
        &self,
        _: &crate::native::agent_channel::FrameContext<'_>,
        _: &crate::native::agent_channel::Asks<'_>,
    ) -> crate::native::agent_channel::Finish {
        panic!("synthetic fixture never serves model observations")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_site_state_transfer_backend_native_consumer() {
    use crate::native::agent_channel::{Endpoint, Identity};
    use tokio::io::AsyncBufReadExt;
    let backend = std::env::var("AMBIT_SITE_TRANSFER_BACKEND")
        .expect("assigned helper checkout is required for real consumer qualification");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = [0; 4096];
                let _ = socket.read(&mut request).await;
                let body = "<!doctype html><title>Native consumer</title>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length:{}\r\nConnection:close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    let mut daemon = crate::native::actions::DaemonState::new();
    let launched=Box::pin(crate::native::actions::execute_command(&json!({"action":"launch","headless":true,
            "executablePath":std::env::var("AMBIT_TEST_CHROME_EXECUTABLE").unwrap_or("/usr/bin/google-chrome".into())}),&mut daemon)).await;
    assert_eq!(
        launched["success"], true,
        "the ordinary fixture launch succeeds"
    );
    let browser = daemon.browser.as_mut().unwrap();
    browser.navigate(&origin, WaitUntil::Load).await.unwrap();
    let client = browser.client.clone();
    let session = browser.active_session_id().unwrap().to_owned();
    let custody = Custody::new();
    let fixture = NativeFixture {
        custody: custody.clone(),
        client: client.clone(),
        session: session.clone(),
        origin: origin.clone(),
    };
    let wire = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = wire.local_addr().unwrap().port();
    let endpoint = Endpoint::with_digest(
        Identity {
            namespace: "3f2a0000000040008000000000000000".into(),
            session: "browser".into(),
            require_sandbox: false,
            browser_host: true,
        },
        &format!("sha256:{}", "a".repeat(64)),
    );
    let serving = tokio::spawn(async move {
        let (socket, _) = wire.accept().await.unwrap();
        let (read, mut write) = socket.into_split();
        let mut reader = tokio::io::BufReader::new(read);
        let mut first = String::new();
        reader.read_line(&mut first).await.unwrap();
        endpoint
            .serve(
                &fixture,
                &crate::native::stream::IdleActivity::new(),
                first,
                &mut reader,
                &mut write,
                &mut std::collections::VecDeque::new(),
                &mut Vec::new(),
            )
            .await;
    });
    let pinned_descriptor = tempfile::NamedTempFile::new().unwrap();
    serde_json::to_writer(
        pinned_descriptor.as_file(),
        &crate::mcp::host_bound::descriptor(),
    )
    .unwrap();
    let output = tokio::process::Command::new("node")
        .args([
            "-r",
            "ts-node/register/transpile-only",
            "src/agent-workspaces/browser-site-custody-native.fixture.ts",
            &port.to_string(),
            &origin,
            pinned_descriptor.path().to_str().unwrap(),
        ])
        .current_dir(backend)
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    let native_ended = tokio::time::timeout(Duration::from_secs(5), serving).await;
    let result = String::from_utf8_lossy(&output.stdout).to_string();
    let error = String::from_utf8_lossy(&output.stderr).to_string();
    let checked = if output.status.success() {
        client.send_command("Runtime.evaluate",Some(json!({"expression":r#"new Promise((resolve,reject)=>{const opening=indexedDB.open('transfer-rows');opening.onsuccess=()=>{const db=opening.result;const transaction=db.transaction('rows','readwrite');const store=transaction.objectStore('rows');const added=store.add('nosecret-next');let key;added.onsuccess=()=>key=added.result;transaction.oncomplete=()=>{db.close();resolve(key)};transaction.onerror=()=>reject(transaction.error)};opening.onerror=()=>reject(opening.error)})"#,"awaitPromise":true,"returnByValue":true})),Some(&session)).await.ok()
    } else {
        None
    };
    storage::clear_site(&client, "http://127.0.0.1", &[origin])
        .await
        .unwrap();
    browser.close().await.unwrap();
    server.abort();
    let _ = server.await;
    assert!(
        native_ended.is_ok(),
        "native endpoint exits after its owned channel EOF"
    );
    assert!(
        output.status.success(),
        "backend/native consumer failed: {error}"
    );
    let receipt: Value = serde_json::from_str(result.trim()).unwrap();
    assert_eq!(receipt["nativeConsumer"], true);
    assert_eq!(receipt["preparationPastNeedWindow"], true);
    assert_eq!(receipt["nextKey"], 3);
    assert!(receipt["maximumFrameBytes"].as_u64().unwrap() < 2 * 1024 * 1024);
    assert_eq!(
        checked.unwrap()["result"]["value"],
        3,
        "actual Chrome preserves the imported key generator after byte-helper export"
    );
    assert!(custody.state.lock().await.transfers.is_empty());
    eprintln!("backend_native_transfer_receipt={receipt}");
}

/// A surviving worker can repopulate cleared storage after every A document
/// has retired. B worker execution, registrations and history are independent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_site_state_revocation_retires_workers_and_preserves_other_site() {
    Box::pin(worker_retirement_case(None)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_site_state_worker_close_override_holds_without_erasure() {
    Box::pin(worker_retirement_case(Some("noop"))).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_site_state_worker_close_throw_holds_without_erasure() {
    Box::pin(worker_retirement_case(Some("throw"))).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_site_state_worker_close_refusal_preserves_user_disabled_scripts() {
    Box::pin(worker_retirement_case(Some("predisabled"))).await;
}

async fn worker_retirement_case(fault: Option<&str>) {
    const PUT: &str = r#"self.put=value=>new Promise((resolve,reject)=>{const opening=indexedDB.open('worker-custody',1);opening.onupgradeneeded=()=>opening.result.createObjectStore('values');opening.onerror=()=>reject(opening.error);opening.onsuccess=()=>{const db=opening.result;const tx=db.transaction('values','readwrite');tx.objectStore('values').put(value,'credential');tx.oncomplete=()=>{db.close();resolve(value)};tx.onerror=()=>{db.close();reject(tx.error)}}});"#;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{port}");
    let other = format!("http://localhost:{port}");
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = [0; 4096];
                let count = socket.read(&mut request).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..count]);
                let (mime, body) = if request.starts_with("GET /shared.js ") {
                    ("application/javascript", format!("{PUT}onconnect=e=>{{const port=e.ports[0];port.onmessage=async e=>port.postMessage(await put(e.data));port.start()}};"))
                } else if request.starts_with("GET /dedicated-parent.js ") {
                    ("application/javascript", format!("{PUT}self.child=new Worker('/dedicated.js');onmessage=async e=>{{const value=await new Promise(resolve=>{{child.onmessage=e=>resolve(e.data);child.postMessage(e.data)}});postMessage(await put(value))}};"))
                } else if request.starts_with("GET /dedicated.js ") {
                    (
                        "application/javascript",
                        format!("{PUT}onmessage=async e=>postMessage(await put(e.data));"),
                    )
                } else if request.starts_with("GET /service.js ") {
                    ("application/javascript", "self.addEventListener('install',e=>e.waitUntil(self.skipWaiting()));self.addEventListener('activate',e=>e.waitUntil(clients.claim()));".into())
                } else {
                    (
                        "text/html",
                        "<!doctype html><title>Worker custody</title>".into(),
                    )
                };
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type:{mime}\r\nContent-Length:{}\r\nConnection:close\r\n\r\n{body}", body.len());
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    let mut daemon = crate::native::actions::DaemonState::new();
    let launched = Box::pin(crate::native::actions::execute_command(
        &json!({"action":"launch","headless":true,"executablePath":std::env::var("AMBIT_TEST_CHROME_EXECUTABLE").unwrap_or("/usr/bin/google-chrome".into())}),
        &mut daemon,
    )).await;
    assert_eq!(launched["success"], true);
    let browser = daemon.browser.as_mut().unwrap();
    let client = browser.client.clone();
    let owner = Custody::new();
    owner
        .browser_ready(
            client.clone(),
            vec![browser.active_session_id().unwrap().into()],
        )
        .await
        .unwrap();
    let resume_workers = |client: &Arc<CdpClient>| {
        let mut events = client.subscribe();
        let resuming = client.clone();
        tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                if event.method == "Target.attachedToTarget"
                    && event.params["waitingForDebugger"] == true
                    && matches!(
                        event.params["targetInfo"]["type"].as_str(),
                        Some("shared_worker" | "service_worker" | "worker")
                    )
                {
                    let session = event.params["sessionId"].as_str().unwrap();
                    eprintln!(
                        "fixture_worker_attached={}",
                        event.params["targetInfo"]["type"]
                    );
                    resuming
                        .send_command_no_wait("Target.setAutoAttach", Some(json!({"autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true})), Some(session))
                        .await
                        .unwrap();
                    resuming
                        .send_command_no_wait(
                            "Runtime.runIfWaitingForDebugger",
                            None,
                            Some(session),
                        )
                        .await
                        .unwrap();
                }
            }
        })
    };
    let resume = resume_workers(&client);
    let create = r#"(async()=>{await navigator.serviceWorker.register('/service.js');await navigator.serviceWorker.ready;const value=location.hostname==='localhost'?'nosecret-B-worker':'nosecret-A-worker';window.dedicated=new Worker('/dedicated-parent.js');await new Promise(resolve=>{dedicated.onmessage=e=>resolve(e.data);dedicated.postMessage(value)});window.worker=new SharedWorker('/shared.js',{name:'custody',extendedLifetime:true});worker.port.start();return await new Promise(resolve=>{worker.port.onmessage=e=>resolve(e.data);worker.port.postMessage(value)})})()"#;
    // Exercise the exact successful createTarget owner too; merely attaching
    // an adopted paused blank page must not invent script-state provenance.
    browser.tab_new(None, None).await.unwrap();
    browser
        .navigate(&format!("{origin}/writer"), WaitUntil::Load)
        .await
        .unwrap();
    let a_page = browser.active_session_id().unwrap().to_owned();
    let made = client
        .send_command(
            "Runtime.evaluate",
            Some(json!({"expression":create,"awaitPromise":true,"returnByValue":true})),
            Some(&a_page),
        )
        .await
        .unwrap();
    assert_eq!(made["result"]["value"], "nosecret-A-worker", "{made}");
    client
        .site_profile()
        .documents
        .settle_keys("http://127.0.0.1")
        .await
        .unwrap();
    browser.tab_new(None, None).await.unwrap();
    browser
        .navigate(&format!("{other}/writer"), WaitUntil::Load)
        .await
        .unwrap();
    let b_page = browser.active_session_id().unwrap().to_owned();
    let made = client
        .send_command(
            "Runtime.evaluate",
            Some(json!({"expression":create,"awaitPromise":true,"returnByValue":true})),
            Some(&b_page),
        )
        .await
        .unwrap();
    assert_eq!(made["result"]["value"], "nosecret-B-worker", "{made}");
    let b_own = client.send_command("Runtime.evaluate", Some(json!({"expression":"localStorage.setItem('B-own','nosecret-B-own');document.cookie='B_own=nosecret-B-cookie;path=/;max-age=600';true","returnByValue":true})), Some(&b_page)).await.unwrap();
    assert_eq!(b_own["result"]["value"], true);
    let targets = client
        .send_command_no_params("Target.getTargets", None)
        .await
        .unwrap();
    let worker = |kind: &str, url: &str| {
        targets["targetInfos"]
            .as_array()
            .unwrap()
            .iter()
            .find(|target| target["type"] == kind && target["url"] == url)
            .unwrap()["targetId"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let a_worker = worker("shared_worker", &format!("{origin}/shared.js"));
    let b_worker = worker("shared_worker", &format!("{other}/shared.js"));
    let a_child = worker("worker", &format!("{origin}/dedicated.js"));
    let b_child = worker("worker", &format!("{other}/dedicated.js"));
    let a_session = client.session_for_target(&a_worker).unwrap();
    let b_session = client.session_for_target(&b_worker).unwrap();
    let a_child_session = client.session_for_target(&a_child).unwrap();
    let b_child_session = client.session_for_target(&b_child).unwrap();
    let a_key = client
        .send_command_no_params("Storage.getStorageKey", Some(&a_session))
        .await
        .unwrap();
    assert_eq!(a_key["storageKey"], format!("{origin}/"));
    let child_close = client
        .send_command(
            "Target.closeTarget",
            Some(json!({"targetId":a_child})),
            None,
        )
        .await;
    assert!(
        child_close.is_err(),
        "Chrome cannot directly close a dedicated worker: {child_close:?}"
    );
    eprintln!("dedicated_worker_direct_close_supported=false");
    let b_history = client
        .send_command_no_params("Page.getNavigationHistory", Some(&b_page))
        .await
        .unwrap();
    let a_history = client
        .send_command_no_params("Page.getNavigationHistory", Some(&a_page))
        .await
        .unwrap();
    let a_parent = worker("worker", &format!("{origin}/dedicated-parent.js"));
    if fault == Some("predisabled") {
        let control = client.send_command("Runtime.evaluate",Some(json!({"expression":"window.nosecretScriptClicked=false;const button=document.createElement('button');button.style.cssText='position:fixed;left:0;top:0;width:100px;height:100px';button.onclick=()=>{window.nosecretScriptClicked=true};document.body.append(button);true","returnByValue":true})),Some(&a_page)).await.unwrap();
        assert_eq!(control["result"]["value"], true);
        for kind in ["mousePressed", "mouseReleased"] {
            client
                .send_command(
                    "Input.dispatchMouseEvent",
                    Some(json!({"type":kind,"x":20,"y":20,"button":"left","clickCount":1})),
                    Some(&a_page),
                )
                .await
                .unwrap();
        }
        let control = client.send_command("Runtime.evaluate",Some(json!({"expression":"const clicked=window.nosecretScriptClicked;window.nosecretScriptClicked=false;clicked","returnByValue":true})),Some(&a_page)).await.unwrap();
        assert_eq!(
            control["result"]["value"], true,
            "the actual click probe works before scripts are disabled"
        );
        client
            .send_command(
                "Emulation.setScriptExecutionDisabled",
                Some(json!({"value":true})),
                Some(&a_page),
            )
            .await
            .unwrap();
    }
    if let Some(fault) = fault {
        let session = match client.session_for_target(&a_parent) {
            Some(session) => session,
            None => client
                .send_command(
                    "Target.attachToTarget",
                    Some(json!({"targetId":a_parent,"flatten":true})),
                    None,
                )
                .await
                .unwrap()["sessionId"]
                .as_str()
                .unwrap()
                .to_owned(),
        };
        let key = client
            .send_command_no_params("Storage.getStorageKey", Some(&session))
            .await
            .unwrap();
        assert_eq!(key, a_key);
        let replacement = if fault == "throw" {
            "()=>{throw new Error('nosecret close refused')}"
        } else {
            "()=>undefined"
        };
        let expression =
            format!("globalThis.nosecretOriginalClose=self.close;self.close={replacement};true");
        let changed = client
            .send_command(
                "Runtime.evaluate",
                Some(json!({"expression":expression,"returnByValue":true})),
                Some(&session),
            )
            .await
            .unwrap();
        assert_eq!(changed["result"]["value"], true);
    }
    let channel = ChannelId::parse("11111111-1111-4111-8111-111111111111").unwrap();
    let detached = ask(
        &owner,
        channel,
        "site_session.detach",
        json!({"sites":["http://127.0.0.1"],"reason":"revoked"}),
    )
    .await;
    if let Some(fault) = fault {
        assert_eq!(detached, Err(storage::UNCLEARED));
        assert!(client.site_profile().require_clear().is_err());
        if fault == "predisabled" {
            for kind in ["mousePressed", "mouseReleased"] {
                client
                    .send_command(
                        "Input.dispatchMouseEvent",
                        Some(json!({"type":kind,"x":20,"y":20,"button":"left","clickCount":1})),
                        Some(&a_page),
                    )
                    .await
                    .unwrap();
            }
            let clicked = client
                .send_command(
                    "Runtime.evaluate",
                    Some(json!({"expression":"window.nosecretScriptClicked","returnByValue":true})),
                    Some(&a_page),
                )
                .await
                .unwrap();
            assert_eq!(
                clicked["result"]["value"], false,
                "failed worker cleanup must preserve the user's disabled scripts"
            );
        }
        let names = client
            .send_command(
                "IndexedDB.requestDatabaseNames",
                Some(json!({"storageKey":a_key["storageKey"]})),
                Some(&a_page),
            )
            .await
            .unwrap();
        assert_eq!(names["databaseNames"], json!(["worker-custody"]));
        assert_eq!(
            client
                .send_command_no_params("Page.getNavigationHistory", Some(&a_page))
                .await
                .unwrap(),
            a_history
        );
        assert_eq!(
            client
                .send_command_no_params("Page.getNavigationHistory", Some(&b_page))
                .await
                .unwrap(),
            b_history
        );
        let b_alive = client.send_command("Runtime.evaluate",Some(json!({"expression":"put('nosecret-B-failure-after')","awaitPromise":true,"returnByValue":true})),Some(&b_child_session)).await.unwrap();
        assert_eq!(b_alive["result"]["value"], "nosecret-B-failure-after");
        let session = client
            .session_for_target(&a_parent)
            .expect("the refused A execution remains owned");
        let key = client
            .send_command_no_params("Storage.getStorageKey", Some(&session))
            .await
            .unwrap();
        assert_eq!(key, a_key);
        let restored = client.send_command("Runtime.evaluate",Some(json!({"expression":"self.close=globalThis.nosecretOriginalClose;true","returnByValue":true})),Some(&session)).await.unwrap();
        assert_eq!(restored["result"]["value"], true);
        owner.recover_cleanup(&client).await.unwrap();
        assert!(client.site_profile().require_clear().is_ok());
        let cleared = storage::capture(&client, "http://127.0.0.1", std::slice::from_ref(&origin))
            .await
            .unwrap();
        assert!(cleared
            .origins
            .iter()
            .all(|origin| origin.indexed_db.is_empty()));
        assert_eq!(
            client
                .send_command_no_params("Page.getNavigationHistory", Some(&b_page))
                .await
                .unwrap(),
            b_history
        );
        let b_data = client.send_command("Runtime.evaluate",Some(json!({"expression":"localStorage.getItem('B-own')==='nosecret-B-own'&&document.cookie.includes('B_own=nosecret-B-cookie')","returnByValue":true})),Some(&b_page)).await.unwrap();
        assert_eq!(b_data["result"]["value"], true);
        browser.close().await.unwrap();
        resume.abort();
        let _ = resume.await;
        server.abort();
        let _ = server.await;
        eprintln!(
            "worker_close_refusal_receipt={}",
            json!({"fault":fault,"cleanupRefused":true,"AStoredRowsPreserved":true,"AHistoryExact":true,"BExecutionDataHistoryExact":true,"sameOwnerRecoveryProvedClear":true})
        );
        return;
    }
    detached.unwrap();
    // An actual surviving execution rewrites bytes after native settlement;
    // a closed worker must not acknowledge this write.
    let repopulation = client.send_command("Runtime.evaluate", Some(json!({"expression":"put('nosecret-repopulated-A')","awaitPromise":true,"returnByValue":true})), Some(&a_session)).await;
    let child_repopulation = client.send_command("Runtime.evaluate", Some(json!({"expression":"put('nosecret-repopulated-A-child')","awaitPromise":true,"returnByValue":true})), Some(&a_child_session)).await;
    let a_state = storage::capture(&client, "http://127.0.0.1", std::slice::from_ref(&origin))
        .await
        .unwrap();
    let b_write = client.send_command("Runtime.evaluate", Some(json!({"expression":"put('nosecret-B-after')","awaitPromise":true,"returnByValue":true})), Some(&b_session)).await.unwrap();
    assert_eq!(b_write["result"]["value"], "nosecret-B-after");
    let b_child_write = client.send_command("Runtime.evaluate", Some(json!({"expression":"put('nosecret-B-after')","awaitPromise":true,"returnByValue":true})), Some(&b_child_session)).await.unwrap();
    assert_eq!(b_child_write["result"]["value"], "nosecret-B-after");
    assert_eq!(
        client
            .send_command_no_params("Page.getNavigationHistory", Some(&b_page))
            .await
            .unwrap(),
        b_history
    );
    let b_own = client.send_command("Runtime.evaluate", Some(json!({"expression":"(async()=>({data:localStorage.getItem('B-own')==='nosecret-B-own'&&document.cookie.includes('B_own=nosecret-B-cookie'),registrations:(await navigator.serviceWorker.getRegistrations()).map(r=>r.scope)}))()","awaitPromise":true,"returnByValue":true})), Some(&b_page)).await.unwrap();
    assert_eq!(b_own["result"]["value"]["data"], true);
    assert_eq!(
        b_own["result"]["value"]["registrations"],
        json!([format!("{other}/")])
    );
    let a_empty = a_state
        .origins
        .iter()
        .all(|stored| stored.indexed_db.is_empty());
    let retired_history = client
        .send_command_no_params("Page.getNavigationHistory", Some(&a_page))
        .await
        .unwrap();
    assert_eq!(retired_history["entries"].as_array().unwrap().len(), 1);
    assert_eq!(retired_history["entries"][0]["url"], "about:blank");
    let relaunch = browser.relaunch_options().unwrap();
    browser.close().await.unwrap();
    resume.abort();
    let _ = resume.await;
    let mut reopened = crate::native::browser::BrowserManager::launch(relaunch, None)
        .await
        .unwrap();
    let resume = resume_workers(&reopened.client);
    reopened
        .navigate(&format!("{other}/read"), WaitUntil::Load)
        .await
        .unwrap();
    let b_persisted = reopened.client.send_command("Runtime.evaluate",Some(json!({"expression":"(async()=>({data:localStorage.getItem('B-own')==='nosecret-B-own'&&document.cookie.includes('B_own=nosecret-B-cookie'),registrations:(await navigator.serviceWorker.getRegistrations()).map(r=>r.scope),row:await new Promise((resolve,reject)=>{const opening=indexedDB.open('worker-custody');opening.onsuccess=()=>{const db=opening.result;const tx=db.transaction('values');const read=tx.objectStore('values').get('credential');read.onsuccess=()=>resolve(read.result);read.onerror=()=>reject(read.error);tx.oncomplete=()=>db.close()};opening.onerror=()=>reject(opening.error)})}))()","awaitPromise":true,"returnByValue":true})),Some(reopened.active_session_id().unwrap())).await.unwrap();
    assert_eq!(b_persisted["result"]["value"]["data"], true);
    assert_eq!(b_persisted["result"]["value"]["row"], "nosecret-B-after");
    assert_eq!(
        b_persisted["result"]["value"]["registrations"],
        json!([format!("{other}/")])
    );
    reopened
        .navigate(&format!("{origin}/read"), WaitUntil::Load)
        .await
        .unwrap();
    let a_persisted = reopened.client.send_command("Runtime.evaluate",Some(json!({"expression":"(async()=>({registrations:(await navigator.serviceWorker.getRegistrations()).map(r=>r.scope),databases:(await indexedDB.databases()).map(db=>db.name)}))()","awaitPromise":true,"returnByValue":true})),Some(reopened.active_session_id().unwrap())).await.unwrap();
    assert_eq!(a_persisted["result"]["value"]["registrations"], json!([]));
    assert_eq!(a_persisted["result"]["value"]["databases"], json!([]));
    reopened.close().await.unwrap();
    resume.abort();
    let _ = resume.await;
    eprintln!(
        "worker_retirement_receipt={}",
        json!({"AExecutionRefused":repopulation.is_err(),"AChildExecutionRefused":child_repopulation.is_err(),"AStoredRowsAbsent":a_empty,"BWorkerStillWrites":true,"BChildWorkerStillWrites":true,"BRegistrationPreserved":true,"BDataAndHistoryExact":true,"AAffectedTabRetainedAndHistoryReset":true,"ARelaunchRegistrationAndIDBAbsent":true,"BRelaunchRegistrationDataAndWorkerWritePersisted":true})
    );
    server.abort();
    let _ = server.await;
    assert!(
        repopulation.is_err(),
        "the retired A worker still executes and repopulates storage: {repopulation:?}"
    );
    assert!(
        a_empty,
        "worker credentials survived after native settlement"
    );
    assert!(
        child_repopulation.is_err(),
        "the retired A child worker still executes: {child_repopulation:?}"
    );
}
