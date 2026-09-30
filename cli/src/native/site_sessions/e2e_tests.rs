//! Real Chrome proof of the state adapter. Values here are synthetic nosecret
//! canaries, never an existing profile or a customer's sign-in.

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{state::SiteState, storage};
use crate::native::browser::{BrowserManager, WaitUntil};
use crate::native::cdp::chrome::LaunchOptions;

async fn browser() -> BrowserManager {
    BrowserManager::launch(
        LaunchOptions {
            executable_path: Some(
                std::env::var("AMBIT_TEST_CHROME_EXECUTABLE")
                    .unwrap_or("/usr/bin/google-chrome".into()),
            ),
            headless: true,
            ..LaunchOptions::default()
        },
        None,
    )
    .await
    .expect("a tool-owned test Chrome launches")
}

async fn evaluate(browser: &BrowserManager, expression: &str) -> Value {
    let result = browser
        .client
        .send_command(
            "Runtime.evaluate",
            Some(json!({"expression":expression,
        "returnByValue":true,"awaitPromise":true})),
            Some(browser.active_session_id().unwrap()),
        )
        .await
        .expect("fixture evaluation succeeds");
    assert!(result.get("exceptionDetails").is_none());
    result["result"]["value"].clone()
}

#[tokio::test]
#[ignore]
async fn e2e_site_state_roundtrips_native_cookie_storage_and_revocation() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{port}");
    let (requests, mut observed) = tokio::sync::mpsc::unbounded_channel::<String>();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let requests = requests.clone();
            tokio::spawn(async move {
                let mut data = vec![0; 8192];
                let count = socket.read(&mut data).await.unwrap_or(0);
                let _ = requests.send(String::from_utf8_lossy(&data[..count]).into_owned());
                let body = "<!doctype html><title>Identity fixture</title><p>Ready</p>";
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    let state = SiteState::read(json!({"format":"ambit.browser-site-state.v1","site":"http://127.0.0.1",
        "capturedAt":"2026-09-29T00:00:00.000Z","chromeMajor":154,
        "cookies":[{"name":"ambit_fixture","value":"nosecret-cookie-canary","domain":"127.0.0.1","path":"/",
            "expires":1900000000,"httpOnly":true,"secure":false,"sameSite":"lax","priority":"medium",
            "sourceScheme":"non_secure","partitionKey":null}],
        "origins":[{"origin":origin,"localStorage":[["fixture-key","nosecret-storage-canary"]],
            "indexedDB":[{"name":"fixture-db","version":3,"stores":[{"name":"records","keyPath":null,
                "autoIncrement":false,"indexes":[],"records":[{"key":"entry","value":{"$":"object","entries":[
                    ["text","nosecret-idb-canary"],["date",{"$":"date","v":1000}],
                    ["binary",{"$":"binary","kind":"Uint8Array","base64":"AQID"}],
                    ["map",{"$":"map","entries":[["answer",42]]}]]}}]}]}]}],"omitted":[]}), "http://127.0.0.1").unwrap();
    let mut first = browser().await;
    storage::import(&first.client, &state)
        .await
        .expect("native state import succeeds");
    assert!(
        observed.try_recv().is_err(),
        "a synthetic origin runs no site request"
    );
    first.navigate(&origin, WaitUntil::Load).await.unwrap();
    let requested = tokio::time::timeout(std::time::Duration::from_secs(2), observed.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        requested.contains("ambit_fixture=nosecret-cookie-canary"),
        "the document request carries the imported HttpOnly cookie"
    );
    assert_eq!(
        evaluate(&first, "document.cookie.includes('ambit_fixture')").await,
        false
    );
    assert_eq!(
        evaluate(
            &first,
            "localStorage.getItem('fixture-key')==='nosecret-storage-canary'"
        )
        .await,
        true
    );
    assert_eq!(evaluate(&first, "new Promise(resolve=>{const r=indexedDB.open('fixture-db');r.onsuccess=()=>{const d=r.result;const q=d.transaction('records').objectStore('records').get('entry');q.onsuccess=()=>{const v=q.result;resolve(v.text==='nosecret-idb-canary'&&v.date.getTime()===1000&&v.binary[2]===3&&v.map.get('answer')===42);d.close();};};})").await, true);
    let captured = storage::capture(&first.client, "http://127.0.0.1", &[origin.clone()])
        .await
        .unwrap();
    assert_eq!(captured.cookies.len(), 1);
    assert_eq!(captured.origins[0].indexed_db[0].version, 3);
    if let Ok(directory) = std::env::var("AMBIT_SITE_CUSTODY_EVIDENCE") {
        std::fs::create_dir_all(&directory).unwrap();
        // Only these synthetic nosecret canaries enter a qualification file.
        std::fs::write(
            std::path::Path::new(&directory).join("synthetic-site-state.json"),
            serde_json::to_vec_pretty(&captured).unwrap(),
        )
        .unwrap();
    }
    let mut model_value = json!({"cookie":"nosecret-cookie-canary","storage":"nosecret-storage-canary","record":"nosecret-idb-canary"});
    first.client.site_context().values.scrub(&mut model_value);
    assert!(model_value
        .as_object()
        .unwrap()
        .values()
        .all(|value| value == "[redacted credential]"));
    first.close().await.unwrap();
    let mut second = browser().await;
    storage::import(&second.client, &captured).await.unwrap();
    second.navigate(&origin, WaitUntil::Load).await.unwrap();
    assert_eq!(
        evaluate(
            &second,
            "localStorage.getItem('fixture-key')==='nosecret-storage-canary'"
        )
        .await,
        true
    );
    second.client.send_command("Network.setCookie",Some(json!({"name":"rotated-fixture","value":"nosecret-rotated-cookie","domain":"127.0.0.1","path":"/","httpOnly":true})),Some(second.active_session_id().unwrap())).await.unwrap();
    storage::clear(&second.client, &captured).await.unwrap();
    let revoked = storage::capture(&second.client, "http://127.0.0.1", &[origin.clone()])
        .await
        .unwrap();
    assert!(revoked.cookies.is_empty());
    assert!(revoked.origins[0].local_storage.is_empty());
    assert!(revoked.origins[0].indexed_db.is_empty());
    let foreign = storage::capture(&second.client, "http://localhost", &[])
        .await
        .unwrap();
    assert!(foreign.cookies.is_empty());
    generator_proof(&second, &origin).await;
    second.close().await.unwrap();
    server.abort();
    let _ = server.await;
}

async fn generator_proof(browser: &BrowserManager, origin: &str) {
    assert_eq!(evaluate(browser,r#"(async()=>{
      const request=r=>new Promise((resolve,reject)=>{r.onsuccess=()=>resolve(r.result);r.onerror=()=>reject(r.error)});
      await request(indexedDB.deleteDatabase('generator-fixture'));
      const opening=indexedDB.open('generator-fixture',1);
      opening.onupgradeneeded=()=>{
        for(const name of ['empty','deleted','large','exhausted','aborted'])opening.result.createObjectStore(name,{autoIncrement:true});
        opening.result.createObjectStore('inline',{autoIncrement:true,keyPath:'identity.id'});
      };
      const database=await request(opening);
      const writing=database.transaction([...database.objectStoreNames],'readwrite');
      const finished=new Promise((resolve,reject)=>{writing.oncomplete=resolve;writing.onabort=()=>reject(writing.error)});
      writing.objectStore('deleted').put({keep:'kept'},2);writing.objectStore('deleted').put({},7);writing.objectStore('deleted').delete(7);
      writing.objectStore('inline').put({identity:{id:17},keep:'kept'});writing.objectStore('inline').put({identity:{id:101}});writing.objectStore('inline').delete(101);
      writing.objectStore('large').put({},2**53-1);writing.objectStore('large').delete(2**53-1);
      writing.objectStore('exhausted').put({},2**53);writing.objectStore('exhausted').delete(2**53);
      await finished;
      const aborted=database.transaction('aborted','readwrite');
      const ended=new Promise(resolve=>{aborted.onabort=resolve});
      const provisional=aborted.objectStore('aborted').put({},1001);provisional.onsuccess=()=>aborted.abort();await ended;
      database.close();return true;
    })()"#).await,true);
    let first = storage::capture(&browser.client, "http://127.0.0.1", &[origin.into()])
        .await
        .unwrap();
    let second = storage::capture(&browser.client, "http://127.0.0.1", &[origin.into()])
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&first.origins).unwrap(),
        serde_json::to_value(&second.origins).unwrap(),
        "capture abort preserves every actual row and generator"
    );
    let stores = &first.origins[0]
        .indexed_db
        .iter()
        .find(|database| database.name == "generator-fixture")
        .unwrap()
        .stores;
    for (name, next) in [
        ("empty", json!(1)),
        ("deleted", json!(8)),
        ("inline", json!(102)),
        ("large", json!(9007199254740992u64)),
        ("exhausted", json!("exhausted")),
        ("aborted", json!(1)),
    ] {
        assert_eq!(
            stores
                .iter()
                .find(|store| store.name == name)
                .unwrap()
                .next_key
                .as_ref(),
            Some(&next)
        );
    }
    for _ in 0..2 {
        storage::import(&browser.client, &first).await.unwrap();
        let restored = storage::capture(&browser.client, "http://127.0.0.1", &[origin.into()])
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(&first.origins).unwrap(),
            serde_json::to_value(&restored.origins).unwrap(),
            "restoration preserves rows and generator idempotently"
        );
    }
    assert_eq!(evaluate(browser,r#"(async()=>{
      const request=r=>new Promise((resolve,reject)=>{r.onsuccess=()=>resolve(r.result);r.onerror=()=>reject(r.error)});
      const database=await request(indexedDB.open('generator-fixture'));
      const writing=database.transaction([...database.objectStoreNames],'readwrite');
      const finished=new Promise((resolve,reject)=>{writing.oncomplete=resolve;writing.onabort=()=>reject(writing.error)});
      const keys={};const results=[];
      for(const name of database.objectStoreNames){
        results.push(new Promise((resolve,reject)=>{const r=writing.objectStore(name).add({});
          r.onsuccess=()=>{keys[name]=r.result;resolve()};r.onerror=event=>{event.preventDefault();if(r.error.name==='ConstraintError'){keys[name]='exhausted';resolve()}else reject(r.error)};
        }));
      }
      await Promise.all(results);await finished;database.close();return keys;
    })()"#).await,json!({"empty":1,"deleted":8,"inline":102,"large":9007199254740992u64,"exhausted":"exhausted","aborted":1}));
    if let Ok(directory) = std::env::var("AMBIT_SITE_CUSTODY_EVIDENCE") {
        std::fs::write(
            std::path::Path::new(&directory).join("synthetic-generator-state.json"),
            serde_json::to_vec_pretty(&first).unwrap(),
        )
        .unwrap();
    }
}

#[tokio::test]
#[ignore]
async fn e2e_lazy_site_attach_applies_before_the_first_real_document_request() {
    use super::{custody::Custody, protocol::Request};
    use crate::native::agent_channel::frame::ChannelId;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let requests = requests.clone();
            tokio::spawn(async move {
                let mut bytes = vec![0; 8192];
                let count = socket.read(&mut bytes).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&bytes[..count]).to_string();
                let _ = requests.send(request);
                let body = "<!doctype html><title>Lazy identity</title><p>Ready</p>";
                let response=format!("HTTP/1.1 200 OK\r\nContent-Type:text/html\r\nContent-Length:{}\r\nConnection:close\r\n\r\n{body}",body.len());
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    // Exercise the real daemon's generic Fetch resolver as well as custody.
    // A BrowserManager alone cannot reveal competing paused-request owners.
    let mut daemon_state = crate::native::actions::DaemonState::new();
    let launched = Box::pin(crate::native::actions::execute_command(
        &json!({"action":"launch","headless":true}),
        &mut daemon_state,
    ))
    .await;
    assert_eq!(launched["success"], true, "{launched}");
    let browser = daemon_state.browser.as_mut().unwrap();
    let client = browser.client.clone();
    let session = browser.active_session_id().unwrap().to_owned();
    let channel = ChannelId::parse("11111111-1111-4111-8111-111111111111").unwrap();
    let custody = Custody::new();
    let mut events = custody.subscribe();
    custody
        .request(
            channel,
            Request::read(
                "site_sessions.offer",
                json!({"sites":[{"site":"http://127.0.0.1","mode":"act"}]}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    custody
        .browser_ready(client.clone(), vec![session.clone()])
        .await
        .unwrap();
    let navigation = tokio::spawn({
        let client = client.clone();
        let session = session.clone();
        let origin = origin.clone();
        async move {
            client
                .send_command("Page.navigate", Some(json!({"url":origin})), Some(&session))
                .await
        }
    });
    let (owner, need) = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owner, channel);
    assert_eq!(need["type"], "site_session.need");
    assert!(need.get("id").is_none());
    assert!(
        received.try_recv().is_err(),
        "the document remains paused before host release"
    );
    let state = json!({"format":"ambit.browser-site-state.v1","site":"http://127.0.0.1","capturedAt":"2026-09-29T00:00:00.000Z","chromeMajor":154,
        "cookies":[{"name":"lazy_fixture","value":"nosecret-lazy-cookie","domain":"127.0.0.1","path":"/","expires":1900000000,"httpOnly":true,"secure":false,"sameSite":"lax","priority":"medium","sourceScheme":"non_secure","partitionKey":null}],
        "origins":[{"origin":origin,"localStorage":[["lazy","nosecret-lazy-storage"]],"indexedDB":[]}],"omitted":[]});
    let attach = json!({"requestId":need["requestId"],"useId":"22222222-2222-4222-8222-222222222222","site":need["site"],"mode":"act","state":state});
    let foreign = ChannelId::parse("33333333-3333-4333-8333-333333333333").unwrap();
    assert!(custody
        .request(
            foreign,
            Request::read("site_session.attach", attach.clone()).unwrap()
        )
        .await
        .is_err());
    let mut wrong_mode = attach.clone();
    wrong_mode["mode"] = json!("read");
    assert!(custody
        .request(
            channel,
            Request::read("site_session.attach", wrong_mode).unwrap()
        )
        .await
        .is_err());
    let mut wrong_request = attach.clone();
    wrong_request["requestId"] = json!("44444444-4444-4444-8444-444444444444");
    assert!(custody
        .request(
            channel,
            Request::read("site_session.attach", wrong_request).unwrap()
        )
        .await
        .is_err());
    assert!(
        received.try_recv().is_err(),
        "refused grants never release the paused request"
    );
    assert_eq!(
        custody
            .request(
                channel,
                Request::read("site_session.attach", attach.clone()).unwrap()
            )
            .await
            .unwrap()["attached"],
        true
    );
    assert!(
        custody
            .request(
                channel,
                Request::read("site_session.attach", attach).unwrap()
            )
            .await
            .is_err(),
        "the consumed need cannot replay"
    );
    navigation.await.unwrap().unwrap();
    let request = tokio::time::timeout(std::time::Duration::from_secs(2), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        request.contains("lazy_fixture=nosecret-lazy-cookie"),
        "the first real document request must carry the admitted cookie"
    );
    let value=client.send_command("Runtime.evaluate",Some(json!({"expression":"localStorage.getItem('lazy')==='nosecret-lazy-storage'","returnByValue":true})),Some(&session)).await.unwrap();
    assert_eq!(value["result"]["value"], true);
    // Socket/execution ends do not erase a site's accepted state. Exact
    // revalidated use adoption is required before a new model channel acts.
    custody.end(channel).await;
    assert!(!custody.admits_channel(foreign).await);
    custody.request(foreign,Request::read("site_sessions.offer",json!({"sites":[{"site":"http://127.0.0.1","mode":"act","useId":"22222222-2222-4222-8222-222222222222"}]})).unwrap()).await.unwrap();
    assert!(custody.admits_channel(foreign).await);
    let snapshot = custody
        .request(
            foreign,
            Request::read("site_session.export", json!({"sites":["http://127.0.0.1"]})).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        snapshot["states"][0]["cookies"].as_array().unwrap().len(),
        1
    );
    assert_eq!(evaluate(&browser,r#"new Promise((resolve,reject)=>{const opening=indexedDB.open('large-cache',1);opening.onupgradeneeded=()=>opening.result.createObjectStore('cache');opening.onsuccess=()=>{const database=opening.result;const writing=database.transaction('cache','readwrite');writing.objectStore('cache').put(new Blob([new Uint8Array(16*1024*1024)]),'large');writing.oncomplete=()=>{database.close();resolve(true)};writing.onerror=()=>reject(writing.error)};opening.onerror=()=>reject(opening.error)})"#).await,true);
    let deadline = chrono::Utc::now() + chrono::Duration::milliseconds(200);
    let expires_at = deadline.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    custody.request(foreign,Request::read("site_sessions.offer",json!({"sites":[{"site":"http://127.0.0.1","mode":"act","useId":"22222222-2222-4222-8222-222222222222","expiresAt":expires_at}]})).unwrap()).await.unwrap();
    let mut changed = client.subscribe();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let event = changed.recv().await.unwrap();
            if event.method == "Page.frameNavigated"
                && event
                    .params
                    .pointer("/frame/url")
                    .and_then(Value::as_str)
                    .is_some_and(|url| url.starts_with(&origin))
            {
                break;
            }
        }
    })
    .await
    .expect("expiry clears the site and reloads its real page");
    let expired = custody
        .request(
            foreign,
            Request::read("site_session.export", json!({"sites":["http://127.0.0.1"]})).unwrap(),
        )
        .await
        .unwrap();
    assert!(expired["states"][0]["cookies"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(expired["states"][0]["origins"][0]["localStorage"]
        .as_array()
        .unwrap()
        .is_empty());
    custody
        .request(
            foreign,
            Request::read(
                "site_session.detach",
                json!({"sites":["http://127.0.0.1"],"reason":"revoked"}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    custody.end(foreign).await;
    let closed = Box::pin(crate::native::actions::execute_command(
        &json!({"action":"close"}),
        &mut daemon_state,
    ))
    .await;
    assert_eq!(closed["success"], true, "{closed}");
    server.abort();
    let _ = server.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_lazy_custody_prepares_iframes_and_new_native_targets() {
    use super::{custody::Custody, protocol::Request};
    use crate::native::actions::{execute_command, DaemonState};
    use crate::native::agent_channel::frame::ChannelId;
    use std::sync::Arc;
    use std::time::Duration;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let origin = format!("http://localhost:{port}");
    let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let requests = requests.clone();
            tokio::spawn(async move {
                let mut input = [0u8; 8192];
                let count = socket.read(&mut input).await.unwrap_or(0);
                let _ = requests.send(String::from_utf8_lossy(&input[..count]).into_owned());
                let body = "<!doctype html><title>Target custody fixture</title>";
                let reply=format!("HTTP/1.1 200 OK\r\nContent-Type:text/html\r\nContent-Length:{}\r\nConnection:close\r\n\r\n{body}",body.len());
                let _ = socket.write_all(reply.as_bytes()).await;
            });
        }
    });
    let mut state = DaemonState::new();
    for command in [
        json!({"action":"launch","headless":true}),
        json!({"action":"navigate","url":format!("http://localhost:{port}/top")}),
    ] {
        let reply = Box::pin(execute_command(&command, &mut state)).await;
        assert_eq!(reply["success"], true, "{reply}");
    }
    let client = state.browser.as_ref().unwrap().client.clone();
    let session = state
        .browser
        .as_ref()
        .unwrap()
        .active_session_id()
        .unwrap()
        .to_owned();
    let channel = ChannelId::parse("11111111-1111-4111-8111-111111111111").unwrap();
    let custody = Custody::new();
    let mut events = custody.subscribe();
    custody
        .request(
            channel,
            Request::read(
                "site_sessions.offer",
                json!({"sites":[{"site":"http://localhost","mode":"act"}]}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    custody
        .browser_ready(client.clone(), vec![session.clone()])
        .await
        .unwrap();
    let expression=format!("(()=>{{const iframe=document.createElement('iframe');iframe.src={};document.body.append(iframe);return true}})()",serde_json::to_string(&format!("{origin}/frame")).unwrap());
    client
        .send_command(
            "Runtime.evaluate",
            Some(json!({"expression":expression,"returnByValue":true})),
            Some(&session),
        )
        .await
        .unwrap();
    let (_, need) = tokio::time::timeout(Duration::from_secs(3), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(need["site"], "http://localhost");
    while let Ok(request) = received.try_recv() {
        assert!(
            !request.starts_with("GET /frame "),
            "iframe document must not reach the server before custody"
        );
    }
    let snapshot = json!({"format":"ambit.browser-site-state.v1","site":"http://localhost","capturedAt":"2026-09-30T00:00:00.000Z","chromeMajor":154,
        "cookies":[{"name":"target_fixture","value":"nosecret-target-cookie","domain":"localhost","path":"/","expires":1900000000,"httpOnly":true,"secure":false,"sameSite":"lax","priority":"medium","sourceScheme":"non_secure","partitionKey":null}],
        "origins":[{"origin":origin,"localStorage":[["target","nosecret-target-storage"]],"indexedDB":[]}],"omitted":[]});
    custody.request(channel,Request::read("site_session.attach",json!({"requestId":need["requestId"],"useId":"22222222-2222-4222-8222-222222222222","site":"http://localhost","mode":"act","state":snapshot})).unwrap()).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let request = received.recv().await.unwrap();
            if request.starts_with("GET /frame ") {
                break request;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        first.contains("target_fixture=nosecret-target-cookie"),
        "first iframe request carries the imported cookie"
    );
    custody
        .request(
            channel,
            Request::read(
                "site_session.detach",
                json!({"sites":["http://localhost"],"reason":"revoked"}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    // Leave the prior frame behind before offering again: detach reloads
    // matching pages, and its document must not supply the new tab's need.
    let blank = Box::pin(execute_command(
        &json!({"action":"navigate","url":"about:blank"}),
        &mut state,
    ))
    .await;
    assert_eq!(blank["success"], true, "{blank}");
    custody
        .request(
            channel,
            Request::read(
                "site_sessions.offer",
                json!({"sites":[{"site":"http://localhost","mode":"act"}]}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let state = Arc::new(tokio::sync::Mutex::new(state));
    let opening = {
        let state = state.clone();
        let url = format!("{origin}/popup");
        tokio::spawn(async move {
            Box::pin(execute_command(
                &json!({"action":"tab_new","url":url}),
                &mut *state.lock().await,
            ))
            .await
        })
    };
    let (_, need) = tokio::time::timeout(Duration::from_secs(3), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(need["type"], "site_session.need");
    let mut before_attach = None;
    while let Ok(request) = received.try_recv() {
        if request.starts_with("GET /popup ") {
            before_attach = Some(request);
        }
    }
    custody.request(channel,Request::read("site_session.attach",json!({"requestId":need["requestId"],"useId":"33333333-3333-4333-8333-333333333333","site":"http://localhost","mode":"act","state":snapshot})).unwrap()).await.unwrap();
    let opened = tokio::time::timeout(Duration::from_secs(3), opening)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(opened["success"], true, "{opened}");
    let reached_before_attach = before_attach.is_some();
    let first = match before_attach {
        Some(request) => request,
        None => tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let request = received.recv().await.unwrap();
                if request.starts_with("GET /popup ") {
                    break request;
                }
            }
        })
        .await
        .unwrap(),
    };
    // A distinct site creates an actual out-of-process iframe. Its native
    // auto-attach consumer must prepare it without blocking the CDP reader;
    // ending the channel then releases the pending document signed out.
    custody
        .request(
            channel,
            Request::read(
                "site_sessions.offer",
                json!({"sites":[
                    {"site":"http://localhost","mode":"act","useId":"33333333-3333-4333-8333-333333333333"},
                    {"site":"http://127.0.0.1","mode":"act"}
                ]}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let parent = state
        .lock()
        .await
        .browser
        .as_ref()
        .unwrap()
        .active_session_id()
        .unwrap()
        .to_owned();
    let mut targets = client.subscribe();
    client
        .send_command(
            "Runtime.evaluate",
            Some(json!({"expression":format!("(()=>{{const iframe=document.createElement('iframe');iframe.src='http://127.0.0.1:{port}/oop-frame';document.body.append(iframe);return true}})()"),"returnByValue":true})),
            Some(&parent),
        )
        .await
        .unwrap();
    let (_, iframe_need) = tokio::time::timeout(Duration::from_secs(3), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(iframe_need["site"], "http://127.0.0.1");
    while let Ok(request) = received.try_recv() {
        assert!(
            !request.starts_with("GET /oop-frame "),
            "OOP iframe reached the server before custody refusal"
        );
    }
    custody
        .request(
            channel,
            Request::read(
                "site_session.refuse",
                json!({"requestId":iframe_need["requestId"],"site":"http://127.0.0.1","reason":"denied"}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let child = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = targets.recv().await.unwrap();
            if event.method == "Target.attachedToTarget"
                && event
                    .params
                    .pointer("/targetInfo/type")
                    .and_then(Value::as_str)
                    == Some("iframe")
            {
                break event.params["sessionId"].as_str().unwrap().to_owned();
            }
        }
    })
    .await
    .unwrap();
    assert_ne!(
        child, parent,
        "the fixture exercised a separate iframe session"
    );
    client
        .send_command_no_params("Runtime.enable", Some(&child))
        .await
        .unwrap();
    wait_default_context(&mut targets, &child).await;
    let responsive = tokio::time::timeout(
        Duration::from_secs(3),
        client.send_command(
            "Runtime.evaluate",
            Some(json!({"expression":"1","returnByValue":true})),
            Some(&child),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(responsive.pointer("/result/value"), Some(&json!(1)));
    custody.request(channel,Request::read("site_sessions.offer",json!({"sites":[
        {"site":"http://localhost","mode":"act","useId":"33333333-3333-4333-8333-333333333333"},
        {"site":"http://127.0.0.1","mode":"act"}
    ]})).unwrap()).await.unwrap();
    let mut contexts = client.subscribe();
    let navigating = {
        let client = client.clone();
        let child = child.clone();
        tokio::spawn(async move {
            client
                .send_command(
                    "Page.navigate",
                    Some(json!({"url":format!("http://127.0.0.1:{port}/cancel-frame")})),
                    Some(&child),
                )
                .await
        })
    };
    let (_, cancelled_need) = tokio::time::timeout(Duration::from_secs(3), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cancelled_need["site"], "http://127.0.0.1");
    while let Ok(request) = received.try_recv() {
        assert!(
            !request.starts_with("GET /cancel-frame "),
            "OOP iframe reached the server before custody cancellation"
        );
    }
    custody.end(channel).await;
    tokio::time::timeout(Duration::from_secs(3), navigating)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        custody
            .request(
                channel,
                Request::read(
                    "site_session.refuse",
                    json!({"requestId":cancelled_need["requestId"],"site":"http://127.0.0.1","reason":"denied"})
                )
                .unwrap()
            )
            .await
            .is_err(),
        "the ended channel cannot resolve a stale need"
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if received
                .recv()
                .await
                .unwrap()
                .starts_with("GET /cancel-frame ")
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    wait_default_context(&mut contexts, &child).await;
    let responsive = tokio::time::timeout(
        Duration::from_secs(3),
        client.send_command(
            "Runtime.evaluate",
            Some(json!({"expression":"1","returnByValue":true})),
            Some(&child),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(responsive.pointer("/result/value"), Some(&json!(1)));
    let closed = Box::pin(execute_command(
        &json!({"action":"close"}),
        &mut *state.lock().await,
    ))
    .await;
    assert_eq!(closed["success"], true, "{closed}");
    server.abort();
    let _ = server.await;
    assert!(
        !reached_before_attach,
        "new-target document reached the server before custody was attached"
    );
    assert!(
        first.contains("target_fixture=nosecret-target-cookie"),
        "first new-target request carries the imported cookie"
    );
}

async fn wait_default_context(
    events: &mut tokio::sync::broadcast::Receiver<crate::native::cdp::types::CdpEvent>,
    session: &str,
) {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.method == "Runtime.executionContextCreated"
                && event.session_id.as_deref() == Some(session)
                && event
                    .params
                    .pointer("/context/auxData/isDefault")
                    .and_then(Value::as_bool)
                    == Some(true)
            {
                break;
            }
        }
    })
    .await
    .expect("the iframe publishes its actual default execution context");
}

/// Capacity is state data, not the control-frame budget. This deliberately
/// exceeds the legacy envelope using valid ordinary IndexedDB string rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(target_os = "linux")]
#[ignore]
async fn e2e_site_state_capacity_retains_valid_data_above_legacy_wire_bound() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = [0u8; 4096];
                let _ = socket.read(&mut request).await;
                let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type:text/html\r\nContent-Length:38\r\nConnection:close\r\n\r\n<!doctype html><title>Capacity</title>").await;
            });
        }
    });
    let mut owned = browser().await;
    owned.navigate(&origin, WaitUntil::Load).await.unwrap();
    let session = owned.active_session_id().unwrap().to_owned();
    let populated = owned.client.send_command("Runtime.evaluate",Some(json!({
        "expression":r#"new Promise((resolve,reject)=>{const opening=indexedDB.open('capacity-fixture',1);opening.onupgradeneeded=()=>opening.result.createObjectStore('rows',{autoIncrement:true});opening.onerror=()=>reject(opening.error);opening.onsuccess=()=>{const db=opening.result;const transaction=db.transaction('rows','readwrite');const store=transaction.objectStore('rows');store.put('nosecret-capacity-first-'+ 'x'.repeat(5*1024*1024));store.put('nosecret-capacity-second-'+ 'y'.repeat(5*1024*1024));transaction.oncomplete=()=>{db.close();resolve(2)};transaction.onerror=()=>reject(transaction.error)}})"#,
        "awaitPromise":true,"returnByValue":true
    })),Some(&session)).await.unwrap();
    assert_eq!(populated.pointer("/result/value"), Some(&json!(2)));
    let before = native_memory_kib();
    let started = std::time::Instant::now();
    let captured = storage::capture(
        &owned.client,
        "http://127.0.0.1",
        std::slice::from_ref(&origin),
    )
    .await
    .unwrap();
    let elapsed_ms = started.elapsed().as_millis();
    let after = native_memory_kib();
    let records = captured
        .origins
        .iter()
        .flat_map(|origin| &origin.indexed_db)
        .flat_map(|database| &database.stores)
        .map(|store| store.records.len())
        .sum::<usize>();
    let omitted_bytes = captured
        .omitted
        .iter()
        .map(|omission| omission.bytes)
        .sum::<u64>();
    eprintln!(
        "capacity_receipt={}",
        json!({"inputRecords":2,"inputStringBytes":10*1024*1024+"nosecret-capacity-first-".len()+"nosecret-capacity-second-".len(),"capturedRecords":records,"omissions":captured.omitted.len(),"omittedSerializedBytes":omitted_bytes,"nativeBefore":before,"nativeAfter":after,"elapsedMs":elapsed_ms})
    );
    storage::clear_site(
        &owned.client,
        "http://127.0.0.1",
        std::slice::from_ref(&origin),
    )
    .await
    .unwrap();
    let cleared = owned.client.send_command("Runtime.evaluate",Some(json!({"expression":"indexedDB.databases().then(rows=>rows.length)","awaitPromise":true,"returnByValue":true})),Some(&session)).await.unwrap();
    let complete = records == 2 && captured.omitted.is_empty();
    let restored = if complete {
        storage::import(&owned.client, &captured).await
    } else {
        Err("The capacity capture was incomplete.")
    };
    let restored_receipt = if restored.is_ok() {
        owned.client.send_command("Runtime.evaluate", Some(json!({
            "expression":r#"new Promise((resolve,reject)=>{const opening=indexedDB.open('capacity-fixture');opening.onerror=()=>reject(opening.error);opening.onsuccess=()=>{const db=opening.result;const writing=db.transaction('rows','readwrite');const store=writing.objectStore('rows');const values=store.getAll();const added=store.add('nosecret-next-generator');let next;added.onsuccess=()=>{next=added.result};writing.oncomplete=()=>{db.close();resolve({rows:values.result.map(value=>({bytes:new TextEncoder().encode(value).length,prefix:value.slice(0,24),last:value.slice(-1)})),next})};writing.onerror=()=>reject(writing.error)}})"#,
            "awaitPromise":true,"returnByValue":true
        })), Some(&session)).await.unwrap()["result"]["value"].clone()
    } else {
        Value::Null
    };
    eprintln!(
        "capacity_restore_receipt={}",
        json!({"restored":restored.is_ok(),"browser":restored_receipt,"nativeAfterRestore":native_memory_kib()})
    );
    storage::clear_site(
        &owned.client,
        "http://127.0.0.1",
        std::slice::from_ref(&origin),
    )
    .await
    .unwrap();
    let closed = owned.close().await;
    server.abort();
    let _ = server.await;
    assert!(closed.is_ok());
    assert_eq!(
        cleared.pointer("/result/value"),
        Some(&json!(0)),
        "the fixture's complete database was cleared before its browser closed"
    );
    assert_eq!(
        records, 2,
        "valid state above the legacy wire bound must be preserved"
    );
    assert!(
        captured.omitted.is_empty(),
        "valid encodable state is not a resource omission"
    );
    assert!(
        restored.is_ok(),
        "complete large state restores through the native adapter"
    );
    assert_eq!(
        restored_receipt["rows"][0]["bytes"],
        5 * 1024 * 1024 + "nosecret-capacity-first-".len()
    );
    assert_eq!(
        restored_receipt["rows"][1]["bytes"],
        5 * 1024 * 1024 + "nosecret-capacity-second-".len()
    );
    assert_eq!(
        restored_receipt["rows"][0]["prefix"],
        "nosecret-capacity-first-"
    );
    assert_eq!(
        restored_receipt["rows"][1]["prefix"],
        "nosecret-capacity-second"
    );
    assert_eq!(restored_receipt["rows"][0]["last"], "x");
    assert_eq!(restored_receipt["rows"][1]["last"], "y");
    assert_eq!(
        restored_receipt["next"], 3,
        "large capture preserves the generator"
    );
}

#[cfg(target_os = "linux")]
fn native_memory_kib() -> Value {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let field = |key: &str| {
        status.lines().find_map(|line| {
            line.strip_prefix(key)
                .and_then(|value| value.split_whitespace().next())
                .and_then(|value| value.parse::<u64>().ok())
        })
    };
    json!({"rssKiB":field("VmRSS:"),"peakRssKiB":field("VmHWM:")})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_site_state_capacity_cancel_closes_its_private_document() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = [0u8; 4096];
                let _ = socket.read(&mut request).await;
                let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type:text/html\r\nContent-Length:38\r\nConnection:close\r\n\r\n<!doctype html><title>Capacity</title>").await;
            });
        }
    });
    let mut owned = browser().await;
    owned.navigate(&origin, WaitUntil::Load).await.unwrap();
    assert_eq!(evaluate(&owned,r#"new Promise((resolve,reject)=>{const opening=indexedDB.open('cancel-fixture',1);opening.onupgradeneeded=()=>opening.result.createObjectStore('rows');opening.onerror=()=>reject(opening.error);opening.onsuccess=()=>{const db=opening.result;const writing=db.transaction('rows','readwrite');const store=writing.objectStore('rows');store.put('nosecret-cancel-value',1);let stopped=false;globalThis.stopCapacityWriter=()=>{stopped=true};const hold=()=>{const pending=store.count();pending.onsuccess=()=>{if(!stopped)hold()}};hold();writing.oncomplete=()=>db.close();resolve(true)}})"#).await,true);
    let mut events = owned.client.subscribe();
    let client = owned.client.clone();
    let capture_origin = origin.clone();
    let capturing = tokio::spawn(async move {
        storage::capture(&client, "http://127.0.0.1", &[capture_origin]).await
    });
    let target = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.method == "Target.targetInfoChanged"
                && event.params["targetInfo"]["url"]
                    .as_str()
                    .is_some_and(|url| url.contains("/.ambit-site-custody-"))
            {
                break event.params["targetInfo"]["targetId"]
                    .as_str()
                    .unwrap()
                    .to_owned();
            }
        }
    })
    .await
    .unwrap();
    capturing.abort();
    match capturing.await {
        Err(error) => assert!(error.is_cancelled()),
        Ok(_) => panic!("the held fixture capture was cancelled"),
    }
    let retired = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.method == "Target.targetDestroyed" && event.params["targetId"] == target {
                break;
            }
        }
    })
    .await
    .is_ok();
    let targets = owned
        .client
        .send_command("Target.getTargets", Some(json!({})), None)
        .await
        .unwrap();
    let remains = targets["targetInfos"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["targetId"] == target);
    assert_eq!(evaluate(&owned, "stopCapacityWriter(); true").await, true);
    let database = evaluate(&owned,"new Promise((resolve,reject)=>{const opening=indexedDB.open('cancel-fixture');opening.onerror=()=>reject(opening.error);opening.onsuccess=()=>{const db=opening.result;const reading=db.transaction('rows').objectStore('rows').get(1);reading.onsuccess=()=>{db.close();resolve(reading.result==='nosecret-cancel-value')};reading.onerror=()=>reject(reading.error)}})").await;
    storage::clear_site(
        &owned.client,
        "http://127.0.0.1",
        std::slice::from_ref(&origin),
    )
    .await
    .unwrap();
    let closed = owned.close().await;
    server.abort();
    let _ = server.await;
    assert!(closed.is_ok());
    assert_eq!(
        database, true,
        "cancellation neither corrupts the site nor retains a storage transaction"
    );
    assert!(
        retired && !remains,
        "cancelled capture closes the private document before completion"
    );
}
