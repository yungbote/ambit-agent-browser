//! Real Chromium qualification for Product's internal human-file bridge.
use super::actions::{execute_command, DaemonState};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const OWNER: &str = "aabbccdd-1111-4222-8333-123456789abc";
const PAGE: &str = r#"<!doctype html><html><body style="margin:0">
<input id="one" type="file" accept=".bin" style="position:absolute;left:20px;top:20px;width:250px;height:40px">
<input id="many" type="file" multiple style="position:absolute;left:20px;top:80px;width:250px;height:40px">
<input id="directory" type="file" webkitdirectory style="position:absolute;left:20px;top:140px;width:250px;height:40px">
<div id="zone" style="position:absolute;left:300px;top:20px;width:200px;height:160px;background:#ddd">Drop</div>
<a id="download" style="position:absolute;left:20px;top:210px" download="receipt.bin">Download</a>
<script>
window.receipts=[];
async function capture(files,kind,trusted){receipts.push({kind,trusted,files:await Promise.all(Array.from(files,async f=>({name:f.name,bytes:Array.from(new Uint8Array(await f.arrayBuffer()))})))})}
for(const n of document.querySelectorAll('input'))n.addEventListener('change',e=>capture(e.target.files,n.id,e.isTrusted));
zone.addEventListener('dragover',e=>e.preventDefault());
zone.addEventListener('drop',e=>{e.preventDefault();capture(e.dataTransfer.files,'drop',e.isTrusted)});
download.href=URL.createObjectURL(new Blob([new Uint8Array([0,1,127,128,255])],{type:'application/octet-stream'}));
</script></body></html>"#;

async fn command(state: &mut DaemonState, request: Value) -> Value {
    tokio::time::timeout(
        Duration::from_secs(15),
        Box::pin(execute_command(&request, state)),
    )
    .await
    .expect("command timeout")
}

fn success(response: &Value) -> &Value {
    assert_eq!(response["success"], true, "{response}");
    &response["data"]
}

async fn control(state: &mut DaemonState, mut request: Value) -> Value {
    request["action"] = json!("ambit_browser_control");
    request["controllerId"] = json!(OWNER);
    command(state, request).await
}

async fn evaluate(state: &DaemonState, expression: &str) -> Value {
    let browser = state.browser.as_ref().unwrap();
    let result = browser
        .client
        .send_command(
            "Runtime.evaluate",
            Some(json!({"expression":expression,"returnByValue":true,"awaitPromise":true})),
            Some(browser.active_session_id().unwrap()),
        )
        .await
        .unwrap();
    assert!(result.get("exceptionDetails").is_none(), "{result}");
    result["result"]["value"].clone()
}

fn expiry() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 29_000
}

async fn launch() -> (DaemonState, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let mut state = DaemonState::new();
    success(
        &command(
            &mut state,
            json!({"action":"launch","headless":true,"downloadPath":directory.path()}),
        )
        .await,
    );
    success(&command(&mut state,json!({"action":"navigate","url":format!("data:text/html;base64,{}",STANDARD.encode(PAGE))})).await);
    let capabilities = command(
        &mut state,
        json!({"action":"ambit_browser_control","op":"inspect"}),
    )
    .await;
    assert_eq!(success(&capabilities)["filesSupported"], true);
    (state, directory)
}

// A compositor readback establishes that the fixture can receive input.
// Optional screenshots are task evidence, never runtime configuration.
async fn capture_fixture(state: &DaemonState, name: &str) {
    let browser = state.browser.as_ref().unwrap();
    let screenshot = browser
        .client
        .send_command(
            "Page.captureScreenshot",
            Some(json!({"format":"png"})),
            Some(browser.active_session_id().unwrap()),
        )
        .await
        .unwrap();
    if let Ok(directory) = std::env::var("AMBIT_FILE_TEST_ARTIFACT_DIR") {
        let directory = std::path::Path::new(&directory);
        std::fs::write(
            directory.join(format!("{name}-page.png")),
            STANDARD
                .decode(screenshot["data"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        if let Some(display) = browser.display_client() {
            let (capture, _) = display.capture().await.unwrap();
            std::fs::write(
                directory.join(format!("{name}-window.jpg")),
                STANDARD.decode(capture.data).unwrap(),
            )
            .unwrap();
        }
    }
}

async fn acquire(state: &mut DaemonState) {
    success(&control(state, json!({"op":"acquire","expiresAt":expiry()})).await);
}

// Fixture-only calibration uses a trusted CDP hover's screen coordinates.
// The tested control still receives full-window pixels and uses native input.
async fn surface_point(state: &DaemonState, x: f64, y: f64) -> (f64, f64, String) {
    let browser = state.browser.as_ref().unwrap();
    let session = browser.active_session_id().unwrap();
    if let Some(display) = browser.display_client() {
        let install="(function arm(w){w.__fileTestPointer=null;w.addEventListener('pointermove',e=>{w.__fileTestPointer=[e.screenX,e.screenY];try{w.top.__fileTestPointer=w.__fileTestPointer}catch{}},{once:true,capture:true});for(let i=0;i<w.frames.length;i++){try{arm(w.frames[i])}catch{}}})(window)";
        evaluate(state, install).await;
        for session in state.iframe_sessions.values() {
            browser
                .client
                .send_command(
                    "Runtime.evaluate",
                    Some(json!({"expression":install})),
                    Some(session),
                )
                .await
                .unwrap();
        }
        browser
            .client
            .send_command(
                "Input.dispatchMouseEvent",
                Some(json!({"type":"mouseMoved","x":x,"y":y})),
                Some(session),
            )
            .await
            .unwrap();
        let pointer = evaluate(state, "__fileTestPointer").await;
        let pointer = if pointer.is_array() {
            pointer
        } else {
            let mut found = Value::Null;
            for session in state.iframe_sessions.values() {
                let result=browser.client.send_command("Runtime.evaluate",Some(json!({"expression":"window.__fileTestPointer??null","returnByValue":true})),Some(session)).await.unwrap();
                if result["result"]["value"].is_array() {
                    found = result["result"]["value"].clone();
                }
            }
            found
        };
        assert!(pointer.is_array(), "{pointer}");
        let surface = display.surface();
        (
            pointer[0].as_f64().unwrap() * f64::from(surface.device_scale_factor),
            pointer[1].as_f64().unwrap() * f64::from(surface.device_scale_factor),
            surface.generation,
        )
    } else {
        (x, y, browser.client.page_generation(session))
    }
}

async fn click(state: &mut DaemonState, sequence: u64, x: f64, y: f64) {
    let (x, y, generation) = surface_point(state, x, y).await;
    success(&control(state,json!({"op":"input","sequence":sequence,"expectedSurfaceGeneration":generation,"events":[
        {"type":"input_mouse","eventType":"mousePressed","x":x,"y":y,"button":"left","buttons":1,"clickCount":1},
        {"type":"input_mouse","eventType":"mouseReleased","x":x,"y":y,"button":"left","buttons":0,"clickCount":1}]})).await);
}

async fn chooser(state: &mut DaemonState) -> Value {
    for _ in 0..30 {
        let response = control(state, json!({"op":"files"})).await;
        let data = success(&response);
        if !data["chooser"].is_null() {
            return data["chooser"].clone();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("chooser missing")
}

async fn close(state: &mut DaemonState) {
    success(&control(state, json!({"op":"release"})).await);
    success(&command(state, json!({"action":"close"})).await);
}

async fn drop_target(state: &mut DaemonState, sequence: u64, x: f64, y: f64) -> Value {
    let (x, y, generation) = surface_point(state, x, y).await;
    let response = control(
        state,
        json!({"op":"drop","sequence":sequence,"expectedSurfaceGeneration":generation,"x":x,"y":y}),
    )
    .await;
    success(&response)["destination"].clone()
}

async fn receipt(state: &DaemonState, index: usize) -> Value {
    for _ in 0..50 {
        let value = evaluate(state, &format!("receipts[{index}]??null")).await;
        if !value.is_null() {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("upload receipt missing")
}

#[tokio::test]
#[ignore = "requires installed Chromium"]
async fn browser_files_e2e_chooser_bytes_multiple_cancel_and_directory() {
    let (mut state, dir) = launch().await;
    let file = dir.path().join("laptop.bin");
    std::fs::write(&file, [0, 1, 127, 128, 255]).unwrap();
    acquire(&mut state).await;
    click(&mut state, 1, 35.0, 35.0).await;
    let destination = chooser(&mut state).await;
    assert_eq!(destination["accept"], ".bin");
    assert_eq!(destination["multiple"], false);
    success(&control(&mut state,json!({"op":"setfiles","sequence":2,"destinationId":destination["destinationId"],"files":[file]})).await);
    let actual = receipt(&state, 0).await;
    assert_eq!(actual["files"][0]["bytes"], json!([0, 1, 127, 128, 255]));
    assert_eq!(actual["trusted"], true);
    click(&mut state, 3, 35.0, 35.0).await;
    let canceled = chooser(&mut state).await;
    success(
        &control(
            &mut state,
            json!({"op":"dismissfiles","destinationId":canceled["destinationId"]}),
        )
        .await,
    );
    assert_eq!(evaluate(&state, "one.files[0].name").await, "laptop.bin");
    assert!(success(&control(&mut state, json!({"op":"files"})).await)["chooser"].is_null());
    let second = dir.path().join("another.bin");
    std::fs::write(&second, [255, 9]).unwrap();
    click(&mut state, 4, 35.0, 95.0).await;
    let multiple = chooser(&mut state).await;
    assert_eq!(multiple["multiple"], true);
    success(&control(&mut state,json!({"op":"setfiles","sequence":5,"destinationId":multiple["destinationId"],"files":[file,second]})).await);
    assert_eq!(
        receipt(&state, 1).await["files"].as_array().unwrap().len(),
        2
    );
    click(&mut state, 6, 35.0, 155.0).await;
    let unsupported = control(&mut state, json!({"op":"files"})).await;
    assert_eq!(
        unsupported["code"], "browser_control_file_unsupported",
        "{unsupported}"
    );
    close(&mut state).await;
}

#[tokio::test]
#[ignore = "requires installed Chromium"]
async fn browser_files_e2e_native_drop_real_bytes_and_repeated_position() {
    let (mut state, dir) = launch().await;
    let file = dir.path().join("drop.bin");
    std::fs::write(&file, [0, 255, 13, 10]).unwrap();
    acquire(&mut state).await;
    capture_fixture(&state, "drop-before").await;
    for index in 0..2 {
        let destination = drop_target(&mut state, index * 2 + 1, 340.0, 80.0).await;
        assert_eq!(destination["kind"], "drop");
        success(&control(&mut state,json!({"op":"setfiles","sequence":index*2+2,"destinationId":destination["destinationId"],"files":[file]})).await);
        let actual = receipt(&state, index as usize).await;
        assert_eq!(actual["trusted"], true, "{actual}");
        assert_eq!(actual["files"][0]["bytes"], json!([0, 255, 13, 10]));
    }
    capture_fixture(&state, "drop-after").await;
    close(&mut state).await;
}

#[tokio::test]
#[ignore = "requires installed Chromium"]
async fn browser_files_e2e_input_drop_uses_native_events_and_default_file_selection() {
    let (mut state, dir) = launch().await;
    let source = dir.path().join("input-drop.bin");
    std::fs::write(&source, [0, 1, 128, 255]).unwrap();
    evaluate(&state, "window.inputDropTrusted=false;one.addEventListener('drop',event=>window.inputDropTrusted=event.isTrusted)").await;
    acquire(&mut state).await;
    let destination = drop_target(&mut state, 1, 60.0, 35.0).await;
    assert_eq!(destination["kind"], "input");
    success(&control(&mut state, json!({"op":"setfiles","sequence":2,"destinationId":destination["destinationId"],"files":[source]})).await);
    assert_eq!(evaluate(&state, "window.inputDropTrusted").await, true);
    let selected = receipt(&state, 0).await;
    assert_eq!(selected["kind"], "one");
    assert_eq!(selected["files"][0]["bytes"], json!([0, 1, 128, 255]));
    close(&mut state).await;
}

#[tokio::test]
#[ignore = "requires installed Chromium"]
async fn browser_files_e2e_replaced_node_navigation_and_release_refuse_staged_files() {
    let (mut state, dir) = launch().await;
    let file = dir.path().join("staged.bin");
    std::fs::write(&file, [42]).unwrap();
    acquire(&mut state).await;
    click(&mut state, 1, 35.0, 35.0).await;
    let destination = chooser(&mut state).await;
    evaluate(&state, "one.replaceWith(one.cloneNode())").await;
    let refused=control(&mut state,json!({"op":"setfiles","sequence":2,"destinationId":destination["destinationId"],"files":[file]})).await;
    assert_eq!(refused["code"], "browser_control_file_stale", "{refused}");
    assert_eq!(evaluate(&state, "one.files.length").await, 0);
    click(&mut state, 2, 35.0, 35.0).await;
    let destination = chooser(&mut state).await;
    let browser = state.browser.as_ref().unwrap();
    browser
        .client
        .send_command(
            "Page.navigate",
            Some(json!({"url":"about:blank"})),
            Some(browser.active_session_id().unwrap()),
        )
        .await
        .unwrap();
    let refused=control(&mut state,json!({"op":"setfiles","sequence":3,"destinationId":destination["destinationId"],"files":[file]})).await;
    assert_eq!(refused["code"], "browser_control_file_stale", "{refused}");
    success(&control(&mut state, json!({"op":"release"})).await);
    let refused=control(&mut state,json!({"op":"setfiles","sequence":3,"destinationId":destination["destinationId"],"files":[file]})).await;
    assert_eq!(refused["code"], "browser_control_stale");
    success(&command(&mut state, json!({"action":"close"})).await);
}

#[tokio::test]
#[ignore = "requires installed Chromium"]
async fn browser_files_e2e_downloads_start_at_acquisition_and_keep_exact_paths() {
    let (mut state, dir) = launch().await;
    success(&command(&mut state,json!({"action":"download","selector":"#download","path":dir.path().join("earlier.bin")})).await);
    acquire(&mut state).await;
    assert_eq!(
        success(&control(&mut state, json!({"op":"files"})).await)["downloads"],
        json!([])
    );
    click(&mut state, 1, 35.0, 218.0).await;
    let mut found = Value::Null;
    for _ in 0..50 {
        let response = control(&mut state, json!({"op":"files"})).await;
        let downloads = &success(&response)["downloads"];
        if downloads.as_array().is_some_and(|items| !items.is_empty()) {
            found = downloads[0].clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(found["status"], "completed", "{found}");
    assert_eq!(found["id"], found["guid"]);
    assert_eq!(
        std::fs::read(found["path"].as_str().unwrap()).unwrap(),
        [0, 1, 127, 128, 255]
    );
    assert_eq!(
        success(&control(&mut state, json!({"op":"files"})).await)["downloads"][0],
        found
    );
    success(&control(&mut state, json!({"op":"release"})).await);
    let retained = command(
        &mut state,
        json!({"action":"ambit_browser_control","op":"downloads"}),
    )
    .await;
    let retained = success(&retained);
    assert_eq!(retained["controlled"], false);
    assert_eq!(retained["filesSupported"], true);
    assert!(retained.get("controllerId").is_none());
    assert!(retained["downloads"]
        .as_array()
        .unwrap()
        .iter()
        .any(|download| download["guid"] == found["guid"]));
    success(&command(&mut state, json!({"action":"snapshot"})).await);
    let waited = command(&mut state, json!({"action":"waitfordownload"})).await;
    assert_eq!(success(&waited)["guid"], found["guid"]);
    // Completed history must still name a capture after ordinary source
    // organization. Product can resume retained immutable bytes by this GUID.
    std::fs::rename(
        found["path"].as_str().unwrap(),
        dir.path().join("moved.bin"),
    )
    .unwrap();
    success(&command(&mut state, json!({"action":"tab_new","url":"about:blank"})).await);
    success(&command(&mut state, json!({"action":"tab_close","tabId":"t1"})).await);
    let after_close = command(
        &mut state,
        json!({"action":"ambit_browser_control","op":"downloads"}),
    )
    .await;
    assert!(success(&after_close)["downloads"]
        .as_array()
        .unwrap()
        .iter()
        .any(|download| download["guid"] == found["guid"]));
    success(&command(&mut state, json!({"action":"close"})).await);
}

#[tokio::test]
#[ignore = "requires installed Chromium"]
async fn browser_files_e2e_unsupported_file_engine_keeps_ordinary_control() {
    let (mut state, _dir) = launch().await;
    state.engine = "lightpanda".into();
    let inspection = command(
        &mut state,
        json!({"action":"ambit_browser_control","op":"inspect"}),
    )
    .await;
    assert_eq!(success(&inspection)["filesSupported"], false);
    acquire(&mut state).await;
    assert!(!state.browser.as_ref().unwrap().client.files.active());
    success(&control(&mut state, json!({"op":"release"})).await);
    state.engine = "chrome".into();
    success(&command(&mut state, json!({"action":"close"})).await);
}

#[tokio::test]
#[ignore = "requires installed Chromium"]
async fn browser_files_e2e_local_iframe_chooser_drop_and_detachment() {
    let (mut state, dir) = launch().await;
    evaluate(&state,&format!("{{const iframe=document.createElement('iframe');iframe.id='child';iframe.style='position:absolute;left:0;top:300px;width:600px;height:260px;border:0';iframe.srcdoc={};document.body.append(iframe)}}",serde_json::to_string(PAGE).unwrap())).await;
    let file = dir.path().join("frame.bin");
    std::fs::write(&file, [17, 0, 255]).unwrap();
    acquire(&mut state).await;
    click(&mut state, 1, 35.0, 335.0).await;
    let target = chooser(&mut state).await;
    success(&control(&mut state,json!({"op":"setfiles","sequence":2,"destinationId":target["destinationId"],"files":[file]})).await);
    assert_eq!(
        evaluate(&state, "child.contentWindow.one.files[0].name").await,
        "frame.bin"
    );
    let target = drop_target(&mut state, 3, 350.0, 365.0).await;
    success(&control(&mut state,json!({"op":"setfiles","sequence":4,"destinationId":target["destinationId"],"files":[file]})).await);
    for _ in 0..50 {
        if evaluate(&state, "child.contentWindow.receipts.length")
            .await
            .as_u64()
            == Some(2)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        evaluate(&state, "child.contentWindow.receipts[1].trusted").await,
        true
    );
    assert_eq!(
        evaluate(&state, "child.contentWindow.receipts[1].files[0].bytes").await,
        json!([17, 0, 255])
    );
    click(&mut state, 5, 35.0, 335.0).await;
    let target = chooser(&mut state).await;
    evaluate(&state, "child.remove()").await;
    let refused=control(&mut state,json!({"op":"setfiles","sequence":6,"destinationId":target["destinationId"],"files":[file]})).await;
    assert_eq!(refused["code"], "browser_control_file_stale", "{refused}");
    close(&mut state).await;
}

#[tokio::test]
#[ignore = "requires installed Chromium"]
async fn browser_files_e2e_drop_follows_same_node_layout_and_refuses_overlay() {
    let (mut state, dir) = launch().await;
    let file = dir.path().join("moving.bin");
    std::fs::write(&file, [5, 6]).unwrap();
    acquire(&mut state).await;
    let target = drop_target(&mut state, 1, 350.0, 80.0).await;
    evaluate(&state, "zone.style.left='500px'").await;
    success(&control(&mut state,json!({"op":"setfiles","sequence":2,"destinationId":target["destinationId"],"files":[file]})).await);
    assert_eq!(receipt(&state, 0).await["trusted"], true);
    let target = drop_target(&mut state, 3, 550.0, 80.0).await;
    evaluate(&state,"{const overlay=document.createElement('div');overlay.style='position:absolute;left:500px;top:20px;width:200px;height:160px;background:white';document.body.append(overlay)}").await;
    let refused=control(&mut state,json!({"op":"setfiles","sequence":4,"destinationId":target["destinationId"],"files":[file]})).await;
    assert_eq!(refused["code"], "browser_control_file_stale", "{refused}");
    assert_eq!(evaluate(&state, "receipts.length").await, 1);
    close(&mut state).await;
}

#[tokio::test]
#[ignore = "requires installed Chromium"]
async fn browser_files_e2e_cross_origin_iframe_chooser_and_trusted_drop() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let child_url = url.replace("127.0.0.1", "localhost");
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut connection, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut request = [0; 4096];
                let _ = connection.read(&mut request).await;
                let response=format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",PAGE.len(),PAGE);
                let _ = connection.write_all(response.as_bytes()).await;
            });
        }
    });
    let (mut state, dir) = launch().await;
    success(&command(&mut state, json!({"action":"navigate","url":url})).await);
    evaluate(&state,&format!("{{const iframe=document.createElement('iframe');iframe.id='child';iframe.style='position:absolute;left:0;top:300px;width:600px;height:260px;border:0';iframe.src={};document.body.append(iframe)}}",serde_json::to_string(&child_url).unwrap())).await;
    acquire(&mut state).await;
    for _ in 0..50 {
        success(&control(&mut state, json!({"op":"files"})).await);
        if let Some(session) = state.iframe_sessions.values().next() {
            let ready=state.browser.as_ref().unwrap().client.send_command("Runtime.evaluate",Some(json!({"expression":"!!document.getElementById('one')","returnByValue":true})),Some(session)).await.unwrap();
            if ready["result"]["value"] == true {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !state.iframe_sessions.is_empty(),
        "fixture must use an actual remote iframe renderer"
    );
    capture_fixture(&state, "cross-origin-before").await;
    let file = dir.path().join("remote.bin");
    std::fs::write(&file, [12, 0, 255]).unwrap();
    click(&mut state, 1, 35.0, 335.0).await;
    let target = chooser(&mut state).await;
    success(&control(&mut state,json!({"op":"setfiles","sequence":2,"destinationId":target["destinationId"],"files":[file]})).await);
    let target = drop_target(&mut state, 3, 350.0, 365.0).await;
    success(&control(&mut state,json!({"op":"setfiles","sequence":4,"destinationId":target["destinationId"],"files":[file]})).await);
    let session = state.iframe_sessions.values().next().unwrap().clone();
    let result=state.browser.as_ref().unwrap().client.send_command("Runtime.evaluate",Some(json!({"expression":"Promise.all(receipts.map(async r=>r))","returnByValue":true,"awaitPromise":true})),Some(&session)).await.unwrap();
    assert_eq!(result["result"]["value"][1]["trusted"], true, "{result}");
    assert_eq!(
        result["result"]["value"][1]["files"][0]["bytes"],
        json!([12, 0, 255]),
        "{result}"
    );
    close(&mut state).await;
    server.abort();
}

#[tokio::test]
#[ignore = "requires installed Chromium"]
async fn browser_files_e2e_expiry_during_staging_never_delivers_files() {
    let (mut state, dir) = launch().await;
    let file = dir.path().join("late.bin");
    std::fs::write(&file, [42]).unwrap();
    acquire(&mut state).await;
    click(&mut state, 1, 35.0, 35.0).await;
    let destination = chooser(&mut state).await;
    let soon = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 100;
    success(&control(&mut state, json!({"op":"renew","expiresAt":soon})).await);
    tokio::time::sleep(Duration::from_millis(120)).await;
    let refused=control(&mut state,json!({"op":"setfiles","sequence":2,"destinationId":destination["destinationId"],"files":[file]})).await;
    assert_eq!(refused["code"], "browser_control_expired", "{refused}");
    assert_eq!(evaluate(&state, "one.files.length").await, 0);
    close(&mut state).await;
}
