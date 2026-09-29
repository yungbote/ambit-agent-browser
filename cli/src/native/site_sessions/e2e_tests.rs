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
