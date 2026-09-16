//! Real Chromium downloads through the daemon's public command boundary.
//!
//! Run with a supplied Chrome executable, serially:
//! `cargo test downloads_e2e_tests -- --ignored --test-threads=1`.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, watch};
use tokio::task::{JoinHandle, JoinSet};

use super::actions::{execute_command, DaemonState};
use super::cdp::types::CdpEvent;

const RECEIPT: &[u8] = b"download receipt\n\0\x01\x7f\x80\xff\r\n";
const EARLIER: &[u8] = &[0xea; 16384];
const SELECTED: &[u8] = &[0x5e; 16384];
const BROKEN: &[u8] = &[0x42; 16384];
const DEADLINE: Duration = Duration::from_secs(15);

struct DownloadFixture {
    url: String,
    release_earlier: watch::Sender<bool>,
    release_selected: watch::Sender<bool>,
    server: JoinHandle<()>,
}

impl DownloadFixture {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (release_earlier, earlier) = watch::channel(false);
        let (release_selected, selected) = watch::channel(false);
        let server = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    connection = listener.accept() => {
                        let Ok((stream, _)) = connection else { break };
                        let mut earlier = earlier.clone();
                        let mut selected = selected.clone();
                        connections.spawn(async move {
                            let mut reader = BufReader::new(stream);
                            let mut line = String::new();
                            if reader.read_line(&mut line).await.is_err() {
                                return;
                            }
                            let path = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
                            loop {
                                line.clear();
                                match reader.read_line(&mut line).await {
                                    Ok(0) | Err(_) => return,
                                    Ok(_) if line == "\r\n" => break,
                                    _ => {}
                                }
                            }
                            let stream = reader.get_mut();
                            if path == "/" {
                                let page = b"<!doctype html><title>Download fixture</title><a id=receipt href=/receipt.bin download>Receipt</a><a id=earlier href=/earlier.bin download>Earlier</a><a id=broken href=/broken.bin download>Broken</a><a id=selected href=/selected.bin download>Selected</a>";
                                let header = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", page.len());
                                let _ = stream.write_all(header.as_bytes()).await;
                                let _ = stream.write_all(page).await;
                            } else if matches!(path.as_str(), "/receipt.bin" | "/earlier.bin" | "/selected.bin" | "/broken.bin") {
                                let body = match path.as_str() {
                                    "/earlier.bin" => EARLIER,
                                    "/selected.bin" => SELECTED,
                                    "/broken.bin" => BROKEN,
                                    _ => RECEIPT,
                                };
                                let size = if path == "/broken.bin" { body.len() + 1024 } else { body.len() };
                                // Supply enough bytes for Chromium to admit a streamed response.
                                let prefix = body.len().min(4096);
                                let header = format!("HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"{}\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", &path[1..], size);
                                if stream.write_all(header.as_bytes()).await.is_err()
                                    || stream.write_all(&body[..prefix]).await.is_err()
                                    || stream.flush().await.is_err()
                                {
                                    return;
                                }
                                match path.as_str() {
                                    "/earlier.bin" | "/broken.bin" => {
                                        if earlier.wait_for(|ready| *ready).await.is_err() { return; }
                                    }
                                    "/selected.bin" => {
                                        if selected.wait_for(|ready| *ready).await.is_err() { return; }
                                    }
                                    _ => {}
                                }
                                // An incomplete response makes Chromium report a failed download.
                                // The test observes the actual canceled GUID before releasing
                                // the selected response; no filesystem or delay stands in for it.
                                if path != "/broken.bin" {
                                    let _ = stream.write_all(&body[prefix..]).await;
                                }
                            } else {
                                let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                            }
                            let _ = stream.shutdown().await;
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Self {
            url,
            release_earlier,
            release_selected,
            server,
        }
    }
}

impl Drop for DownloadFixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn command(state: &mut DaemonState, request: Value) -> Value {
    // Keep the daemon's large command future off the test thread's stack.
    tokio::time::timeout(DEADLINE, Box::pin(execute_command(&request, state)))
        .await
        .unwrap_or_else(|_| panic!("command exceeded its deadline: {request}"))
}

// A read-only CDP subscription provides fixture rendezvous. Every command and
// download assertion still crosses execute_command, never the observer internals.
async fn download_event(
    events: &mut broadcast::Receiver<CdpEvent>,
    method: &str,
    matches: impl Fn(&Value) -> bool,
) -> Value {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let event = events.recv().await.expect("native CDP event stream");
            if event.method == method && matches(&event.params) {
                return event.params;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("Chromium did not emit the expected {method}"))
}

fn success(response: &Value) -> &Value {
    assert_eq!(response["success"], true, "{response}");
    &response["data"]
}

async fn launch() -> (DaemonState, DownloadFixture, TempDir) {
    let fixture = DownloadFixture::start().await;
    let downloads = tempfile::tempdir().unwrap();
    let mut state = DaemonState::new();
    success(
        &command(
            &mut state,
            json!({"action": "launch", "headless": true, "downloadPath": downloads.path()}),
        )
        .await,
    );
    success(
        &command(
            &mut state,
            json!({"action": "navigate", "url": fixture.url}),
        )
        .await,
    );
    (state, fixture, downloads)
}

async fn observe_file(mut find: impl FnMut() -> Option<PathBuf>) -> PathBuf {
    tokio::time::timeout(DEADLINE, async {
        loop {
            if let Some(path) = find() {
                return path;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("download filesystem state did not settle")
}

async fn completed_file(directory: &Path, expected: &[u8]) -> PathBuf {
    observe_file(|| {
        std::fs::read_dir(directory).unwrap().find_map(|entry| {
            let path = entry.unwrap().path();
            (path
                .extension()
                .is_none_or(|extension| extension != "crdownload")
                && std::fs::read(&path).is_ok_and(|bytes| bytes == expected))
            .then_some(path)
        })
    })
    .await
}

fn assert_download(response: &Value, expected: &[u8], name: &str) -> PathBuf {
    let data = success(response);
    assert_eq!(data["status"], "completed", "{response}");
    assert_eq!(data["suggestedFilename"], name, "{response}");
    assert_eq!(data["receivedBytes"].as_u64(), Some(expected.len() as u64));
    assert!(uuid::Uuid::parse_str(data["guid"].as_str().unwrap()).is_ok());
    let path = PathBuf::from(data["path"].as_str().expect("actual download path"));
    assert!(
        path.is_absolute(),
        "download path must be absolute: {path:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), expected);
    path
}

#[tokio::test]
#[ignore = "requires a supplied real Chromium executable"]
async fn e2e_download_explicit_saves_exact_binary_bytes() {
    let (mut state, _fixture, downloads) = launch().await;
    let destination = downloads.path().join("saved receipt.bin");
    let response = command(
        &mut state,
        json!({"action": "download", "selector": "#receipt", "path": destination}),
    )
    .await;
    assert_eq!(
        assert_download(&response, RECEIPT, "receipt.bin"),
        destination
    );
    success(&command(&mut state, json!({"action": "close"})).await);
}

#[tokio::test]
#[ignore = "requires a supplied real Chromium executable"]
async fn e2e_download_wait_observes_already_completed_download() {
    let (mut state, _fixture, downloads) = launch().await;
    success(
        &command(
            &mut state,
            json!({"action": "click", "selector": "#receipt"}),
        )
        .await,
    );
    let completed = completed_file(downloads.path(), RECEIPT).await;
    let response = command(
        &mut state,
        json!({"action": "waitfordownload", "timeout": 5000}),
    )
    .await;
    assert_eq!(
        assert_download(&response, RECEIPT, "receipt.bin"),
        completed
    );
    success(&command(&mut state, json!({"action": "close"})).await);
}

#[tokio::test]
#[ignore = "requires a supplied real Chromium executable"]
async fn e2e_download_wait_saves_to_requested_destination() {
    let (mut state, _fixture, downloads) = launch().await;
    success(
        &command(
            &mut state,
            json!({"action": "click", "selector": "#receipt"}),
        )
        .await,
    );
    completed_file(downloads.path(), RECEIPT).await;
    let destination = downloads.path().join("exports").join("chosen name.bin");
    let response = command(
        &mut state,
        json!({"action": "waitfordownload", "path": destination, "timeout": 5000}),
    )
    .await;
    assert_eq!(
        assert_download(&response, RECEIPT, "receipt.bin"),
        destination
    );
    success(&command(&mut state, json!({"action": "close"})).await);
}

#[tokio::test]
#[ignore = "requires a supplied real Chromium executable"]
async fn e2e_download_explicit_ignores_earlier_completion() {
    let (mut state, fixture, downloads) = launch().await;
    let mut events = state.browser.as_ref().unwrap().client.subscribe();
    success(
        &command(
            &mut state,
            json!({"action": "click", "selector": "#earlier"}),
        )
        .await,
    );
    let earlier = download_event(&mut events, "Browser.downloadWillBegin", |event| {
        event["suggestedFilename"] == "earlier.bin"
    })
    .await;
    let destination = downloads.path().join("selected-result.bin");
    let selected = async {
        let response = command(
            &mut state,
            json!({"action": "download", "selector": "#selected", "path": destination}),
        )
        .await;
        success(&response);
        response
    };
    let completion_order = async {
        download_event(&mut events, "Browser.downloadWillBegin", |event| {
            event["suggestedFilename"] == "selected.bin"
        })
        .await;
        fixture.release_earlier.send_replace(true);
        download_event(&mut events, "Browser.downloadProgress", |event| {
            event["guid"] == earlier["guid"] && event["state"] == "completed"
        })
        .await;
        fixture.release_selected.send_replace(true);
    };
    let (response, ()) = tokio::join!(selected, completion_order);
    assert_eq!(
        assert_download(&response, SELECTED, "selected.bin"),
        destination
    );
    let earlier = command(
        &mut state,
        json!({"action": "waitfordownload", "timeout": 5000}),
    )
    .await;
    assert_download(&earlier, EARLIER, "earlier.bin");
    assert_ne!(response["data"]["guid"], earlier["data"]["guid"]);
    success(&command(&mut state, json!({"action": "close"})).await);
}

#[tokio::test]
#[ignore = "requires a supplied real Chromium executable"]
async fn e2e_download_explicit_ignores_earlier_cancellation() {
    let (mut state, fixture, downloads) = launch().await;
    let mut events = state.browser.as_ref().unwrap().client.subscribe();
    success(
        &command(
            &mut state,
            json!({"action": "click", "selector": "#broken"}),
        )
        .await,
    );
    let earlier = download_event(&mut events, "Browser.downloadWillBegin", |event| {
        event["suggestedFilename"] == "broken.bin"
    })
    .await;
    let destination = downloads.path().join("selected-result.bin");
    let selected = async {
        let response = command(
            &mut state,
            json!({"action": "download", "selector": "#selected", "path": destination}),
        )
        .await;
        success(&response);
        response
    };
    let cancellation_order = async {
        download_event(&mut events, "Browser.downloadWillBegin", |event| {
            event["suggestedFilename"] == "selected.bin"
        })
        .await;
        fixture.release_earlier.send_replace(true);
        download_event(&mut events, "Browser.downloadProgress", |event| {
            event["guid"] == earlier["guid"] && event["state"] == "canceled"
        })
        .await;
        fixture.release_selected.send_replace(true);
    };
    let (response, ()) = tokio::join!(selected, cancellation_order);
    assert_eq!(
        assert_download(&response, SELECTED, "selected.bin"),
        destination
    );
    let canceled = command(
        &mut state,
        json!({"action": "waitfordownload", "timeout": 5000}),
    )
    .await;
    assert_eq!(canceled["success"], false, "{canceled}");
    assert!(
        canceled["error"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase()
            .contains("cancel"),
        "{canceled}"
    );
    success(
        &command(
            &mut state,
            json!({"action": "click", "selector": "#receipt"}),
        )
        .await,
    );
    let next = command(
        &mut state,
        json!({"action": "waitfordownload", "timeout": 5000}),
    )
    .await;
    assert_download(&next, RECEIPT, "receipt.bin");
    success(&command(&mut state, json!({"action": "close"})).await);
}

#[tokio::test]
#[ignore = "requires a supplied real Chromium executable"]
async fn e2e_download_tracks_created_and_retired_child_frames() {
    let (mut state, _fixture, downloads) = launch().await;
    success(&command(&mut state, json!({
        "action": "evaluate",
        "script": "document.body.innerHTML='<button id=export>Export in a new frame</button>'; document.getElementById('export').onclick=()=>{const frame=document.createElement('iframe');frame.hidden=true;frame.src='/receipt.bin';document.body.append(frame)}"
    })).await);
    let destination = downloads.path().join("iframe-result.bin");
    let explicit = command(
        &mut state,
        json!({"action": "download", "selector": "#export", "path": destination}),
    )
    .await;
    assert_eq!(
        assert_download(&explicit, RECEIPT, "receipt.bin"),
        destination
    );
    let mut events = state.browser.as_ref().unwrap().client.subscribe();
    success(
        &command(
            &mut state,
            json!({"action": "click", "selector": "#export"}),
        )
        .await,
    );
    let started = download_event(&mut events, "Browser.downloadWillBegin", |event| {
        event["suggestedFilename"] == "receipt.bin"
    })
    .await;
    download_event(&mut events, "Browser.downloadProgress", |event| {
        event["guid"] == started["guid"] && event["state"] == "completed"
    })
    .await;
    let removed = command(&mut state, json!({
        "action": "evaluate",
        "script": "document.querySelectorAll('iframe').forEach(frame=>frame.remove());document.querySelectorAll('iframe').length"
    })).await;
    assert_eq!(success(&removed)["result"], 0);
    let retained = command(
        &mut state,
        json!({"action": "waitfordownload", "timeout": 5000}),
    )
    .await;
    assert_download(&retained, RECEIPT, "receipt.bin");
    assert_eq!(retained["data"]["guid"], started["guid"]);
    assert_ne!(retained["data"]["guid"], explicit["data"]["guid"]);
    success(&command(&mut state, json!({"action": "close"})).await);
}

#[tokio::test]
#[ignore = "requires a supplied real Chromium executable"]
async fn e2e_download_observes_new_browser_context() {
    let (mut state, fixture, downloads) = launch().await;
    let window = command(&mut state, json!({"action": "window_new"})).await;
    assert_eq!(success(&window)["total"], 2);
    success(
        &command(
            &mut state,
            json!({"action": "navigate", "url": fixture.url}),
        )
        .await,
    );
    // A plain click and wait exercise context initialization before an explicit
    // download has any opportunity to configure that context itself.
    success(
        &command(
            &mut state,
            json!({"action": "click", "selector": "#receipt"}),
        )
        .await,
    );
    let retained = command(
        &mut state,
        json!({"action": "waitfordownload", "timeout": 5000}),
    )
    .await;
    assert_download(&retained, RECEIPT, "receipt.bin");
    let destination = downloads.path().join("new-context.bin");
    let explicit = command(
        &mut state,
        json!({"action": "download", "selector": "#receipt", "path": destination}),
    )
    .await;
    assert_eq!(
        assert_download(&explicit, RECEIPT, "receipt.bin"),
        destination
    );
    assert_ne!(retained["data"]["guid"], explicit["data"]["guid"]);
    success(&command(&mut state, json!({"action": "close"})).await);
}

#[tokio::test]
#[ignore = "requires a supplied real Chromium executable"]
async fn e2e_download_commands_preserve_human_control_custody() {
    let (mut state, _fixture, downloads) = launch().await;
    success(
        &command(
            &mut state,
            json!({"action": "click", "selector": "#receipt"}),
        )
        .await,
    );
    completed_file(downloads.path(), RECEIPT).await;
    let owner = uuid::Uuid::new_v4().to_string();
    success(&command(&mut state, json!({"action": "ambit_browser_control", "op": "acquire", "controllerId": owner, "expiresAt": super::stream::timestamp_ms() + 25000})).await);
    for request in [
        json!({"action": "waitfordownload", "timeout": 500}),
        json!({"action": "download", "selector": "#receipt", "path": downloads.path().join("forbidden.bin")}),
    ] {
        let response = command(&mut state, request).await;
        assert_eq!(response["success"], false, "{response}");
        assert_eq!(response["code"], "browser_controlled_by_user", "{response}");
    }
    assert!(!downloads.path().join("forbidden.bin").exists());
    success(
        &command(
            &mut state,
            json!({"action": "ambit_browser_control", "op": "release", "controllerId": owner}),
        )
        .await,
    );
    let response = command(
        &mut state,
        json!({"action": "waitfordownload", "timeout": 5000}),
    )
    .await;
    assert_download(&response, RECEIPT, "receipt.bin");
    success(&command(&mut state, json!({"action": "close"})).await);
}
