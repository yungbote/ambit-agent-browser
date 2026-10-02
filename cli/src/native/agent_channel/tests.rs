//! The channel's protocol over real connections, with a scripted browser:
//! what a frame does from its arrival to its reply and its ledger entry.
//! The daemon's own browser behind it is proven against a real Chrome by
//! `e2e_agent_channel` in `cli/tests`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::Notify;

use super::*;

const CHANNEL_A: &str = "5b0c0b1e-6f0a-4c1c-9d59-0e3f2b7a9c11";
const CHANNEL_B: &str = "5b0c0b1e-6f0a-4c1c-9d59-0e3f2b7a9c22";
const CHANNEL_C: &str = "5b0c0b1e-6f0a-4c1c-9d59-0e3f2b7a9c33";
const ACTION: &str = "8c1f0e2a-3b4c-4d5e-8f60-718293a4b5c6";

/// A browser whose steps do what the test says: `agent_browser_get_url`
/// fails, `agent_browser_wait_ms` waits for the test's go-ahead, and
/// `agent_browser_get_html` answers with as many bytes as its selector says.
#[derive(Default)]
struct Scripted {
    values: crate::native::site_sessions::redaction::Redaction,
    landed: Option<Value>,
    result_text: Option<String>,
    custody: Option<tokio::sync::broadcast::Sender<(ChannelId, Value)>>,
    ran: StdMutex<Vec<String>>,
    started: Notify,
    custody_closed: Notify,
    go: Notify,
    /// Each channel told it ended, with how many steps had run by then.
    ended: StdMutex<Vec<(String, usize)>>,
}

impl Scripted {
    fn ran(&self) -> Vec<String> {
        self.ran.lock().unwrap().clone()
    }

    fn ended(&self) -> Vec<(String, usize)> {
        self.ended.lock().unwrap().clone()
    }
}

impl Browser for Scripted {
    fn redact(&self, value: &mut Value) {
        self.values.scrub_protocol(value);
    }

    fn redact_step(&self, op: &str, value: &mut Value) {
        self.values.scrub_tool(value, op);
    }
    async fn action_files(
        &self,
        _: ChannelId,
        owner: Owner,
        files: Option<Vec<crate::native::playwright::files::Receipt>>,
        _: &Ledger,
    ) -> Result<Value, String> {
        self.go.notify_one();
        Ok(match files {
            Some(files) => json!({"actionId":owner.action.to_string(),"registered":files.len()}),
            None => json!({"actionId":owner.action.to_string(),"released":true}),
        })
    }
    async fn program_request(
        &self,
        _: ChannelId,
        _: Owner,
        request: crate::native::playwright::remote::Request,
        _: &Ledger,
    ) -> Result<Value, String> {
        match request {
            crate::native::playwright::remote::Request::Status { .. }
            | crate::native::playwright::remote::Request::Close { .. } => {
                self.go.notify_one();
                Ok(json!({"state":"closed","nativeInputSettled":true}))
            }
            _ => Err("The scripted browser serves only the tested metadata request.".into()),
        }
    }
    fn custody_events(&self) -> Option<tokio::sync::broadcast::Receiver<(ChannelId, Value)>> {
        self.custody.as_ref().map(|events| events.subscribe())
    }

    async fn site_request(
        &self,
        _: ChannelId,
        request: SiteRequest,
        _: Arc<Ledger>,
    ) -> Result<Value, &'static str> {
        match request {
            SiteRequest::Refuse { site, .. } => {
                self.go.notify_one();
                Ok(json!({"site":site,"attached":false}))
            }
            SiteRequest::Attach { site, .. } => Ok(json!({"site":site,"attached":true})),
            _ => Err("The scripted browser serves only the tested custody request."),
        }
    }

    async fn step(&self, frame: &FrameContext<'_>, step: &PreparedStep) -> StepRecord {
        self.ran.lock().unwrap().push(step.op.clone());
        if step.op == "agent_browser_wait_ms" {
            if let Some(events) = &self.custody {
                let _=events.send((frame.channel,json!({"type":"site_session.need",
                    "requestId":"11111111-1111-4111-8111-111111111111","site":"https://example.com","pageGeneration":"g"})));
            }
        }
        let text = match step.op.as_str() {
            "agent_browser_wait_ms" => {
                self.started.notify_one();
                self.go.notified().await;
                "waited".to_string()
            }
            "agent_browser_get_html" => {
                let size: usize = step.call.command["selector"]
                    .as_str()
                    .and_then(|size| size.trim_start_matches('#').parse().ok())
                    .unwrap_or(1);
                "h".repeat(size)
            }
            op => op.to_string(),
        };
        let text = self.result_text.as_ref().cloned().unwrap_or(text);
        let succeeded = step.op != "agent_browser_get_url";
        let response = if succeeded {
            json!({ "success": true, "data": { "text": text } })
        } else {
            json!({ "success": false, "error": "No page.", "code": "browser_operation_failed" })
        };
        StepRecord {
            op: step.op.clone(),
            result: json!({ "isError": !succeeded,
                "content": [{ "type": "text", "text": text }],
                "structuredContent": { "response": response } }),
            succeeded,
            landed: self.landed.clone(),
            timing: StepTiming {
                queue_us: 10,
                op_us: 100,
                motion_us: 0,
            },
        }
    }

    async fn finish(&self, frame: &FrameContext<'_>, asks: &Asks<'_>) -> Finish {
        Finish {
            browser: json!({ "namespace": frame.binding.namespace,
                "session": frame.binding.session,
                "page": { "targetId": "T", "loaderId": "L", "pageGeneration": "G",
                    "url": "https://example.test/", "title": "Example" },
                "capture": { "status": if asks.capture { "taken" } else { "not_requested" } } }),
            observation: asks.observe.then(|| {
                json!({ "viewport": { "width": 800, "height": 600 },
                    "candidates": [], "omitted": 0, "frames": [], "canvasCoverage": 0 })
            }),
            resolved: (!asks.resolve.is_empty()).then(|| json!([])),
            queue_us: 5,
            observe_us: 50,
        }
    }

    async fn channel_closed(&self, _: ChannelId) {
        self.custody_closed.notify_one();
    }

    async fn end(&self, channel: ChannelId) {
        let ran = self.ran.lock().unwrap().len();
        self.ended.lock().unwrap().push((channel.to_string(), ran));
    }
}

const DIGEST: &str = "sha256:540f433f3536f8432ca701d0bfea8b176fc630ca2120ff7ac22ac1082621c4a8";

fn endpoint() -> Arc<Endpoint> {
    endpoint_role(false)
}

fn endpoint_role(browser_host: bool) -> Arc<Endpoint> {
    Arc::new(Endpoint::with_digest(
        Identity {
            namespace: "thread".into(),
            session: "browser".into(),
            require_sandbox: true,
            browser_host,
        },
        DIGEST,
    ))
}

/// The host's side of one connection to the endpoint.
struct Host {
    lines: BufReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
    served: tokio::task::JoinHandle<()>,
}

impl Host {
    /// Opens a connection whose first line is `first`.
    async fn open<B: Browser + Send + Sync + 'static>(
        endpoint: &Arc<Endpoint>,
        browser: &Arc<B>,
        first: Value,
    ) -> Self {
        let (host, daemon) = tokio::io::duplex(16 << 20);
        let (lines, mut writer) = tokio::io::split(host);
        writer.write_all(line(&first).as_bytes()).await.unwrap();
        let (endpoint, browser) = (endpoint.clone(), browser.clone());
        let served = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(daemon);
            let mut reader = BufReader::new(read);
            let mut first = String::new();
            reader.read_line(&mut first).await.unwrap();
            endpoint
                .serve(
                    browser.as_ref(),
                    &crate::native::stream::IdleActivity::new(),
                    first,
                    &mut reader,
                    &mut write,
                    &mut VecDeque::new(),
                    &mut Vec::new(),
                )
                .await;
        });
        Self {
            lines: BufReader::new(lines),
            writer,
            served,
        }
    }

    /// Opens a channel and says hello, answering the hello's reply.
    async fn hello<B: Browser + Send + Sync + 'static>(
        endpoint: &Arc<Endpoint>,
        browser: &Arc<B>,
        extra: Value,
    ) -> (Self, Value) {
        let mut host = Self::open(endpoint, browser, hello(CHANNEL_A, extra)).await;
        let reply = host.reply().await;
        (host, reply)
    }

    async fn send(&mut self, frame: Value) {
        self.writer
            .write_all(line(&frame).as_bytes())
            .await
            .unwrap();
    }

    async fn reply(&mut self) -> Value {
        let mut text = String::new();
        tokio::time::timeout(Duration::from_secs(10), self.lines.read_line(&mut text))
            .await
            .expect("a reply in time")
            .unwrap();
        assert!(text.ends_with('\n'), "one line: {text:?}");
        serde_json::from_str(&text).unwrap()
    }

    /// Whether the endpoint closed the connection without another line.
    async fn closed(&mut self) -> bool {
        let mut text = String::new();
        matches!(
            tokio::time::timeout(Duration::from_secs(10), self.lines.read_line(&mut text)).await,
            Ok(Ok(0))
        )
    }
}

fn line(frame: &Value) -> String {
    let mut frame = frame.clone();
    if frame.get("action").is_none() {
        let mut forwarded = serde_json::Map::new();
        forwarded.insert("action".into(), json!(super::ACTION));
        forwarded.extend(frame.as_object().unwrap().clone());
        frame = Value::Object(forwarded);
    }
    format!("{frame}\n")
}

fn hello(channel: &str, extra: Value) -> Value {
    let mut frame = json!({ "type": "hello", "id": 1, "protocol": 1, "channel": channel,
        "binding": { "version": 1, "namespace": "thread", "session": "browser",
            "requireSandbox": true } });
    frame
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    frame
}

fn sequence(id: u64, generation: &str, steps: Value, directory: &Path) -> Value {
    json!({ "type": "sequence", "id": id, "actionId": ACTION, "ownerGeneration": generation,
        "directory": directory, "steps": steps })
}

fn op_status(id: u64, generation: &str, of: Option<(&str, u64)>, wait_ms: u64) -> Value {
    let mut frame = json!({ "type": "op_status", "id": id, "actionId": ACTION,
        "ownerGeneration": generation, "waitMs": wait_ms });
    if let Some((channel, frame_id)) = of {
        frame["of"] = json!({ "channel": channel, "id": frame_id });
    }
    frame
}

fn step(op: &str, arguments: Value) -> Value {
    json!({ "op": op, "arguments": arguments })
}

fn title() -> Value {
    step("agent_browser_get_title", json!({}))
}

fn semantic_landing(texts: Value) -> Value {
    json!({"changedNodes": 2, "target":{"backendNodeId":42},
        "changedText":{"texts":texts,"omitted":0,"pageGeneration":"G","backendNodeId":42}})
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore]
async fn e2e_semantic_registered_values_survive_real_dispatch_only_as_redacted_bounded_data() {
    use tokio::io::AsyncReadExt;
    let (_d, path) = directory();
    let env = crate::test_utils::EnvGuard::new(&[
        "AGENT_BROWSER_WINDOW_STREAM",
        "DISPLAY",
        "AGENT_BROWSER_SESSION",
        "AGENT_BROWSER_NAMESPACE",
        "AGENT_BROWSER_SOCKET_DIR",
    ]);
    env.set("AGENT_BROWSER_WINDOW_STREAM", "1");
    env.set("DISPLAY", "");
    env.set("AGENT_BROWSER_SESSION", "browser");
    env.set("AGENT_BROWSER_NAMESPACE", "thread");
    env.set("AGENT_BROWSER_SOCKET_DIR", path.to_str().unwrap());
    // nosecret: synthetic values in an isolated tool-owned fixture/browser.
    let canary = "fixture-registered-credential-canary";
    let html = format!("<!doctype html><title>Report</title><h2 id=result>Waiting</h2><button id=run type=button onclick=\"result.textContent='Report ready {canary}'\">Generate report</button><!--{}-->","h".repeat(70_000));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let html = html.clone();
            tokio::spawn(async move {
                let mut request = [0; 4096];
                let _ = stream.read(&mut request).await;
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",html.len());
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    let state = Arc::new(tokio::sync::Mutex::new(
        crate::native::actions::DaemonState::new(),
    ));
    let browser = Arc::new(super::dispatch::DaemonBrowser::new(state.clone()));
    let endpoint = endpoint_role(true);
    let (mut host, admitted) = Host::hello(&endpoint, &browser,
        json!({"binding":{"version":1,"namespace":"thread","session":"browser","requireSandbox":true,"browserHost":true}})).await;
    assert_eq!(admitted["success"], true, "{admitted}");
    host.send(json!({"type":"site_sessions.offer","id":2,"sites":[]}))
        .await;
    let offered = host.reply().await;
    assert_eq!(offered["success"], true, "{offered}");
    let mut open = sequence(
        3,
        "41",
        json!([step("agent_browser_open", json!({"url":url}))]),
        &path,
    );
    open["observe"] = json!(true);
    host.send(open).await;
    let opened = host.reply().await;
    assert_eq!(opened["success"], true, "{opened}");
    let generation = opened["browser"]["page"]["pageGeneration"]
        .as_str()
        .unwrap()
        .to_owned();
    let target = opened["observation"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| candidate["name"] == "Generate report")
        .unwrap()["backendNodeId"]
        .clone();
    {
        let state = state.lock().await;
        let client = &state.browser.as_ref().unwrap().client;
        let values = client.site_context().values;
        values.register(canary);
        values.register(&generation);
    }
    let mut click = step("agent_browser_click", json!({"selector":"#run"}));
    click["preconditions"] =
        json!({"pageGeneration":generation,"backendNodeId":target,"effects":"read"});
    host.send(sequence(4, "41", json!([click.clone()]), &path))
        .await;
    let refused = host.reply().await;
    assert_eq!(
        refused["steps"][0]["result"]["structuredContent"]["response"]["code"],
        "browser_effect_refused"
    );
    assert_eq!(refused["steps"][0]["timing"]["motionUs"], 0);
    // The fixture's named commit grants only this observed node/document.
    click["preconditions"]["effects"] = json!("commit");
    host.send(sequence(
        5,
        "41",
        json!([
            click,
            step("agent_browser_get_html", json!({"selector":"body"}))
        ]),
        &path,
    ))
    .await;
    let acted = host.reply().await;
    assert_eq!(acted["success"], true, "{acted}");
    assert!(!acted.to_string().contains(canary));
    assert_eq!(acted["browser"]["page"]["pageGeneration"], generation);
    let landed = &acted["steps"][0]["landed"];
    assert_eq!(landed["changedText"]["pageGeneration"], generation);
    assert_eq!(
        landed["changedText"]["backendNodeId"],
        landed["target"]["backendNodeId"]
    );
    assert_eq!(
        landed["changedText"]["texts"],
        json!([format!(
            "Report ready {}",
            crate::native::site_sessions::redaction::MARKER
        )])
    );
    host.send(op_status(6, "41", Some((CHANNEL_A, 5)), 0)).await;
    let status = host.reply().await;
    assert!(!status.to_string().contains(canary));
    assert_eq!(status["data"]["result"]["steps"][0]["landed"], *landed);
    let saved = std::fs::read_to_string(
        status["data"]["result"]["steps"][1]["result"]["path"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert!(!saved.contains(canary));
    assert!(saved.contains(crate::native::site_sessions::redaction::MARKER));
    drop(host.writer);
    drop(host.lines);
    tokio::time::timeout(Duration::from_secs(10), host.served)
        .await
        .unwrap()
        .unwrap();
    let mut state = state.lock().await;
    let closed =
        crate::native::actions::execute_command(&json!({"action":"close"}), &mut state).await;
    assert_eq!(closed["success"], true);
    server.abort();
}

#[tokio::test]
async fn semantic_registry_scrubs_before_reply_ledger_and_retained_result_without_erasing_identity()
{
    let canary = "synthetic-credential-canary";
    let browser = Arc::new(Scripted {
        landed: Some(semantic_landing(json!(["Report ready", canary]))),
        result_text: Some(format!("{} {canary}", "h".repeat(70_000))),
        ..Scripted::default()
    });
    browser.values.register(canary);
    // Even when a stored value equals an identity, typed native identity survives.
    browser.values.register("G");
    let endpoint = endpoint();
    let (_d, path) = directory();
    let (mut host, _) = Host::hello(&endpoint, &browser, json!({})).await;
    host.send(sequence(2, "41", json!([title()]), &path)).await;
    let reply = host.reply().await;
    assert!(!reply.to_string().contains(canary));
    assert_eq!(reply["browser"]["page"]["pageGeneration"], "G");
    let landing = &reply["steps"][0]["landed"];
    assert_eq!(landing["changedText"]["pageGeneration"], "G");
    assert_eq!(landing["changedText"]["backendNodeId"], 42);
    assert_eq!(
        landing["changedText"]["texts"],
        json!([
            "Report ready",
            crate::native::site_sessions::redaction::MARKER
        ])
    );
    host.send(op_status(3, "41", Some((CHANNEL_A, 2)), 0)).await;
    let status = host.reply().await;
    assert!(!status.to_string().contains(canary));
    let kept = &status["data"]["result"]["steps"][0];
    assert_eq!(kept["landed"], *landing);
    let saved = std::fs::read_to_string(kept["result"]["path"].as_str().unwrap()).unwrap();
    assert!(!saved.contains(canary));
    assert!(saved.contains(crate::native::site_sessions::redaction::MARKER));
    drop(host.writer);
    drop(host.lines);
    tokio::time::timeout(Duration::from_secs(10), host.served)
        .await
        .unwrap()
        .unwrap();
    let mut reopened = Host::open(&endpoint, &browser, hello(CHANNEL_B, json!({}))).await;
    assert_eq!(reopened.reply().await["success"], true);
    reopened
        .send(op_status(2, "41", Some((CHANNEL_A, 2)), 0))
        .await;
    let recovered = reopened.reply().await;
    assert!(!recovered.to_string().contains(canary));
    assert_eq!(recovered["data"]["result"]["steps"][0]["landed"], *landing);
}

#[tokio::test]
async fn semantic_registry_expansion_drops_only_optional_text_before_retention() {
    // 4 x 113 chars / 972 UTF-8 bytes before scrub; expansion stays below
    // each 200-char cap but exceeds the aggregate 1024-byte contract.
    let secret = "canary88";
    let original = format!("{}{}", secret.repeat(6), "界".repeat(65));
    assert!(original.chars().count() <= 200 && original.len() * 4 <= 1024);
    let browser = Arc::new(Scripted {
        landed: Some(semantic_landing(json!([
            original, original, original, original
        ]))),
        ..Scripted::default()
    });
    browser.values.register(secret);
    let endpoint = endpoint();
    let (_d, path) = directory();
    let (mut host, _) = Host::hello(&endpoint, &browser, json!({})).await;
    host.send(sequence(2, "41", json!([title()]), &path)).await;
    let reply = host.reply().await;
    assert!(
        reply["steps"][0]["landed"].get("changedText").is_none(),
        "{reply}"
    );
    assert_eq!(reply["steps"][0]["landed"]["changedNodes"], 2);
    assert_eq!(reply["steps"][0]["landed"]["target"]["backendNodeId"], 42);
    assert_eq!(reply["browser"]["page"]["pageGeneration"], "G");
    host.send(op_status(3, "41", Some((CHANNEL_A, 2)), 0)).await;
    assert!(host.reply().await["data"]["result"]["steps"][0]["landed"]
        .get("changedText")
        .is_none());
}

#[tokio::test]
async fn semantic_registry_remains_scrubbed_when_a_new_owner_fences_the_running_channel() {
    let canary = "cancelled-credential-canary";
    let browser = Arc::new(Scripted {
        landed: Some(semantic_landing(json!([canary]))),
        result_text: Some(canary.into()),
        ..Scripted::default()
    });
    browser.values.register(canary);
    let endpoint = endpoint();
    let (_d, path) = directory();
    let (mut old, _) = Host::hello(&endpoint, &browser, json!({})).await;
    old.send(sequence(2, "41", json!([wait(), title()]), &path))
        .await;
    browser.started.notified().await;
    let mut new = Host::open(&endpoint, &browser, hello(CHANNEL_B, json!({}))).await;
    assert_eq!(new.reply().await["success"], true);
    new.send(op_status(2, "42", Some((CHANNEL_A, 2)), 0)).await;
    assert_eq!(new.reply().await["data"]["state"], "running");
    browser.go.notify_one();
    let stopped = old.reply().await;
    assert_eq!(stopped["steps"].as_array().unwrap().len(), 1);
    assert!(!stopped.to_string().contains(canary));
    new.send(op_status(3, "42", Some((CHANNEL_A, 2)), 0)).await;
    let status = new.reply().await;
    assert_eq!(status["data"]["state"], "settled");
    assert!(!status.to_string().contains(canary));
    assert_eq!(browser.ran(), ["agent_browser_wait_ms"]);
}

fn wait() -> Value {
    step("agent_browser_wait_ms", json!({ "ms": 1 }))
}

fn directory() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().canonicalize().unwrap();
    (directory, path)
}

#[tokio::test]
async fn a_hello_answers_the_catalog_the_driver_digest_and_the_channels_bounds() {
    let (endpoint, browser) = (endpoint(), Arc::new(Scripted::default()));
    let (_host, reply) = Host::hello(&endpoint, &browser, json!({})).await;
    assert_eq!(reply["id"], 1);
    assert_eq!(reply["success"], true, "{reply}");
    let data = &reply["data"];
    assert_eq!(data["protocol"], 1);
    assert_eq!(data["sessionCustody"], 4);
    assert_eq!(data["catalog"], crate::mcp::host_bound::catalog_identity());
    assert!(data["catalog"].get("tools").is_none());
    assert_eq!(data["driverArtifactDigest"], DIGEST);
    assert_eq!(data["features"], json!(["motion"]));
    assert_eq!(data["excludedOps"], json!(["agent_browser_run_playwright"]));
    assert_eq!(
        data["limits"],
        json!({ "maxInFlight": 4, "maxRequestBytes": 2097152, "maxReplyBytes": 4194304,"maxCustodyBytes":8389632,"maxCustodyChunkBytes":1048576,
            "ledgerEntries": 256, "ledgerBytes": 16777216, "candidates": 60, "resolve": 30 })
    );
}

#[tokio::test]
async fn a_hello_is_refused_for_another_binding_protocol_channel_or_owner() {
    let (endpoint, browser) = (endpoint(), Arc::new(Scripted::default()));
    for (extra, code) in [
        (
            json!({ "binding": { "version": 1, "namespace": "other", "session": "browser", "requireSandbox": true } }),
            "agent_channel_binding",
        ),
        (
            json!({ "binding": { "version": 1, "namespace": "thread", "session": "other", "requireSandbox": true } }),
            "agent_channel_binding",
        ),
        (
            json!({ "binding": { "version": 1, "namespace": "thread", "session": "browser", "requireSandbox": false } }),
            "agent_channel_binding",
        ),
        (json!({ "protocol": 2 }), "agent_channel_protocol"),
    ] {
        let (_host, reply) = Host::hello(&endpoint, &browser, extra.clone()).await;
        assert_eq!(reply["success"], false, "{extra}");
        assert_eq!(reply["code"], code, "{extra}");
        assert_eq!(reply["id"], 1);
    }
    // A channel id is used once, and a hello once per channel.
    let (mut first, reply) = Host::hello(&endpoint, &browser, json!({})).await;
    assert_eq!(reply["success"], true);
    first.send(hello(CHANNEL_B, json!({ "id": 2 }))).await;
    assert_eq!(first.reply().await["code"], "agent_channel_protocol");
    let (_reused, reply) = Host::hello(&endpoint, &browser, json!({})).await;
    assert_eq!(reply["code"], "agent_channel_protocol");
    // An owner older than one the daemon saw for the Action is fenced.
    let mut newer = Host::open(
        &endpoint,
        &browser,
        hello(
            CHANNEL_B,
            json!({ "actionId": ACTION, "ownerGeneration": "42" }),
        ),
    )
    .await;
    assert_eq!(newer.reply().await["success"], true);
    let mut older = Host::open(
        &endpoint,
        &browser,
        hello(
            CHANNEL_C,
            json!({ "actionId": ACTION, "ownerGeneration": "41" }),
        ),
    )
    .await;
    assert_eq!(older.reply().await["code"], "agent_channel_fenced");
    // A connection whose hello was refused opened no channel to end.
    drop(older.writer);
    drop(older.lines);
    tokio::time::timeout(Duration::from_secs(10), older.served)
        .await
        .unwrap()
        .unwrap();
    assert!(browser.ended().is_empty(), "{:?}", browser.ended());
}

#[tokio::test]
async fn frames_before_a_hello_or_off_the_agent_action_are_refused_in_step() {
    let (endpoint, browser) = (endpoint(), Arc::new(Scripted::default()));
    let (_d, path) = directory();
    let mut host = Host::open(
        &endpoint,
        &browser,
        sequence(1, "1", json!([title()]), &path),
    )
    .await;
    let refused = host.reply().await;
    assert_eq!(refused["code"], "agent_channel_protocol", "{refused}");
    assert_eq!(refused["steps"], json!([]));
    host.send(hello(CHANNEL_A, json!({ "id": 2 }))).await;
    assert_eq!(host.reply().await["success"], true);
    // A control operation on the agent connection never runs.
    host.send(
        json!({ "action": "ambit_browser_control", "op": "acquire", "id": 3,
        "controllerId": "aabbccdd-1111-4222-8333-123456789abc", "expiresAt": 1 }),
    )
    .await;
    let control = host.reply().await;
    assert_eq!(control["id"], 3);
    assert_eq!(control["code"], "agent_channel_protocol", "{control}");
    assert!(browser.ran().is_empty());
    // A line that is not a frame at all ends the connection.
    host.writer.write_all(b"not json\n").await.unwrap();
    assert!(host.closed().await);
}

#[tokio::test]
async fn a_sequence_runs_its_steps_in_order_and_answers_each_with_the_frame_timing() {
    let (endpoint, browser) = (endpoint(), Arc::new(Scripted::default()));
    let (_d, path) = directory();
    let (mut host, _) = Host::hello(&endpoint, &browser, json!({})).await;
    let mut frame = sequence(
        7,
        "41",
        json!([
            step(
                "agent_browser_open",
                json!({ "url": "https://example.com/pricing" })
            ),
            step("agent_browser_get_text", json!({ "selector": "main" }))
        ]),
        &path,
    );
    frame["observe"] = json!(true);
    let frame_steps = frame["steps"].clone();
    host.send(frame).await;
    let reply = host.reply().await;
    assert_eq!(reply["id"], 7);
    assert_eq!(reply["success"], true, "{reply}");
    assert!(reply.get("code").is_none());
    let steps = reply["steps"].as_array().unwrap();
    assert_eq!(
        steps
            .iter()
            .map(|step| step["op"].clone())
            .collect::<Vec<_>>(),
        [json!("agent_browser_open"), json!("agent_browser_get_text")]
    );
    assert_eq!(
        steps[0]["timing"],
        json!({ "queueUs": 10, "opUs": 100, "motionUs": 0 })
    );
    assert_eq!(reply["browser"]["page"]["pageGeneration"], "G");
    assert_eq!(reply["observation"]["candidates"], json!([]));
    assert_eq!(
        reply["timing"],
        json!({ "queueUs": 25, "opUs": 200, "motionUs": 0, "observeUs": 50 })
    );
    assert_eq!(
        browser.ran(),
        ["agent_browser_open", "agent_browser_get_text"]
    );
    // Its outcome is in the ledger, as the reply carried it, with each
    // step's arguments beside its result.
    host.send(op_status(8, "41", Some((CHANNEL_A, 7)), 0)).await;
    let status = host.reply().await;
    assert_eq!(status["data"]["state"], "settled", "{status}");
    let kept = status["data"]["result"]["steps"].as_array().unwrap();
    for (index, (kept, answered)) in kept.iter().zip(steps).enumerate() {
        let mut answered = answered.clone();
        answered["arguments"] = frame_steps[index]["arguments"].clone();
        assert_eq!(*kept, answered);
    }
    assert_eq!(kept.len(), 2);
    assert_eq!(status["data"]["result"]["browser"], reply["browser"]);
    assert_eq!(status["data"]["result"]["success"], true);
}

#[tokio::test]
async fn a_sequence_stops_at_its_first_step_that_does_not_succeed() {
    let (endpoint, browser) = (endpoint(), Arc::new(Scripted::default()));
    let (_d, path) = directory();
    let (mut host, _) = Host::hello(&endpoint, &browser, json!({})).await;
    host.send(sequence(
        2,
        "41",
        json!([title(), step("agent_browser_get_url", json!({})), title()]),
        &path,
    ))
    .await;
    let reply = host.reply().await;
    assert_eq!(reply["success"], false);
    assert!(reply.get("code").is_none(), "{reply}");
    assert_eq!(reply["steps"].as_array().unwrap().len(), 2);
    assert_eq!(reply["steps"][1]["result"]["isError"], true);
    assert_eq!(
        browser.ran(),
        ["agent_browser_get_title", "agent_browser_get_url"]
    );
}

#[tokio::test]
async fn a_frame_with_an_invalid_step_is_refused_before_any_step_runs() {
    let (endpoint, browser) = (endpoint(), Arc::new(Scripted::default()));
    let (_d, path) = directory();
    let (mut host, _) = Host::hello(&endpoint, &browser, json!({})).await;
    for (id, bad, message) in [
        (
            2,
            step(
                "agent_browser_run_playwright",
                json!({ "code": "return 1" }),
            ),
            "agent_browser_run_playwright runs on the file protocol, not on the agent channel.",
        ),
        (
            3,
            step("agent_browser_click", json!({ "selector": "#go" })),
            "A step that acts on an element carries the pageGeneration and effects preconditions.",
        ),
        (
            4,
            step("agent_browser_batch", json!({})),
            "Tool is not in the host-bound browser profile.",
        ),
    ] {
        host.send(sequence(id, "41", json!([title(), bad]), &path))
            .await;
        let reply = host.reply().await;
        assert_eq!(reply["code"], "browser_operation_rejected", "{reply}");
        assert_eq!(reply["error"], message);
        assert_eq!(reply["data"], json!({ "step": 1 }));
        assert_eq!(reply["steps"], json!([]));
        host.send(op_status(100 + id, "41", Some((CHANNEL_A, id)), 0))
            .await;
        assert_eq!(host.reply().await["data"]["state"], "not_started");
    }
    assert!(browser.ran().is_empty());
}

#[tokio::test]
async fn a_second_sequence_sent_while_one_is_unanswered_is_refused_and_never_starts() {
    let (endpoint, browser) = (endpoint(), Arc::new(Scripted::default()));
    let (_d, path) = directory();
    let (mut host, _) = Host::hello(&endpoint, &browser, json!({})).await;
    host.send(sequence(2, "41", json!([wait()]), &path)).await;
    browser.started.notified().await;
    host.send(sequence(3, "41", json!([title()]), &path)).await;
    host.send(op_status(4, "41", Some((CHANNEL_A, 3)), 0)).await;
    // Let the endpoint read both while the first runs.
    tokio::time::sleep(Duration::from_millis(50)).await;
    browser.go.notify_one();
    assert_eq!(host.reply().await["success"], true);
    let second = host.reply().await;
    assert_eq!(second["id"], 3);
    assert_eq!(second["code"], "agent_channel_protocol", "{second}");
    assert_eq!(host.reply().await["data"]["state"], "not_started");
    assert_eq!(browser.ran(), ["agent_browser_wait_ms"]);
}

#[tokio::test]
async fn a_connection_that_ends_mid_sequence_keeps_the_prefix_and_starts_nothing_more() {
    let (endpoint, browser) = (endpoint(), Arc::new(Scripted::default()));
    let (_d, path) = directory();
    let (mut host, _) = Host::hello(&endpoint, &browser, json!({})).await;
    host.send(sequence(2, "41", json!([title(), wait(), title()]), &path))
        .await;
    host.send(sequence(3, "41", json!([title()]), &path)).await;
    browser.started.notified().await;
    // The host goes away while the second step runs.
    drop(host.writer);
    drop(host.lines);
    tokio::time::sleep(Duration::from_millis(50)).await;
    browser.go.notify_one();
    tokio::time::timeout(Duration::from_secs(10), host.served)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        browser.ran(),
        ["agent_browser_get_title", "agent_browser_wait_ms"]
    );
    // The channel ended once its running step was done, and only then.
    assert_eq!(browser.ended(), [(CHANNEL_A.to_string(), 2)]);
    // A new channel of the same owner reads what happened.
    let mut reopened = Host::open(&endpoint, &browser, hello(CHANNEL_B, json!({}))).await;
    assert_eq!(reopened.reply().await["success"], true);
    reopened
        .send(op_status(2, "41", Some((CHANNEL_A, 2)), 0))
        .await;
    let settled = reopened.reply().await;
    assert_eq!(settled["data"]["state"], "settled", "{settled}");
    assert_eq!(settled["data"]["result"]["success"], true);
    let steps = settled["data"]["result"]["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2, "the acknowledged prefix: {settled}");
    reopened
        .send(op_status(3, "41", Some((CHANNEL_A, 3)), 0))
        .await;
    assert_eq!(reopened.reply().await["data"]["state"], "not_started");
    reopened
        .send(op_status(4, "41", Some((CHANNEL_A, 9)), 0))
        .await;
    assert_eq!(reopened.reply().await["data"]["state"], "not_started");
}

#[tokio::test]
async fn a_newer_owner_fences_the_older_ones_channel_between_its_steps() {
    let (endpoint, browser) = (endpoint(), Arc::new(Scripted::default()));
    let (_d, path) = directory();
    let (mut old, _) = Host::hello(&endpoint, &browser, json!({})).await;
    old.send(sequence(2, "41", json!([wait(), title()]), &path))
        .await;
    browser.started.notified().await;
    let mut new = Host::open(&endpoint, &browser, hello(CHANNEL_B, json!({}))).await;
    assert_eq!(new.reply().await["success"], true);
    // Asking what happened registers the newer owner, which stops the old
    // channel before its next step.
    new.send(op_status(2, "42", Some((CHANNEL_A, 2)), 0)).await;
    assert_eq!(new.reply().await["data"]["state"], "running");
    browser.go.notify_one();
    let stopped = old.reply().await;
    assert_eq!(stopped["success"], true, "{stopped}");
    assert_eq!(stopped["steps"].as_array().unwrap().len(), 1);
    assert_eq!(browser.ran(), ["agent_browser_wait_ms"]);
    // Nothing more starts on the old channel, and its owner is refused.
    old.send(sequence(3, "41", json!([title()]), &path)).await;
    assert_eq!(old.reply().await["code"], "agent_channel_fenced");
    old.send(op_status(4, "41", None, 0)).await;
    assert_eq!(old.reply().await["code"], "agent_channel_fenced");
    new.send(op_status(3, "42", Some((CHANNEL_A, 2)), 0)).await;
    let settled = new.reply().await;
    assert_eq!(settled["data"]["state"], "settled");
    assert_eq!(
        settled["data"]["result"]["steps"].as_array().unwrap().len(),
        1
    );
    assert_eq!(browser.ran(), ["agent_browser_wait_ms"]);
}

#[tokio::test]
async fn op_status_answers_each_state_and_can_wait_for_a_running_frame() {
    let (endpoint, browser) = (endpoint(), Arc::new(Scripted::default()));
    let (_d, path) = directory();
    let (mut runner, _) = Host::hello(&endpoint, &browser, json!({})).await;
    runner.send(sequence(2, "41", json!([wait()]), &path)).await;
    browser.started.notified().await;
    let mut asker = Host::open(&endpoint, &browser, hello(CHANNEL_B, json!({}))).await;
    assert_eq!(asker.reply().await["success"], true);
    asker
        .send(op_status(2, "41", Some((CHANNEL_A, 2)), 0))
        .await;
    assert_eq!(asker.reply().await["data"]["state"], "running");
    // A frame not yet received on a live channel may still arrive.
    asker
        .send(op_status(3, "41", Some((CHANNEL_A, 9)), 0))
        .await;
    assert_eq!(asker.reply().await["data"]["state"], "running");
    // A channel this daemon never saw.
    asker
        .send(op_status(4, "41", Some((CHANNEL_C, 1)), 0))
        .await;
    assert_eq!(asker.reply().await["data"]["state"], "unknown");
    // A held answer comes as soon as the frame settles.
    asker
        .send(op_status(5, "41", Some((CHANNEL_A, 2)), 10_000))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    browser.go.notify_one();
    let settled = asker.reply().await;
    assert_eq!(settled["data"]["state"], "settled", "{settled}");
    assert_eq!(runner.reply().await["success"], true);
    // Without `of`: every entry of the Action, in the order received.
    asker.send(op_status(6, "41", None, 0)).await;
    let entries = asker.reply().await;
    let listed = entries["data"]["entries"].as_array().unwrap();
    assert_eq!(listed.len(), 1, "{entries}");
    assert_eq!(listed[0]["channel"], CHANNEL_A);
    assert_eq!(listed[0]["id"], 2);
    assert_eq!(listed[0]["state"], "settled");
    // Another Action's frame is not answered.
    asker
        .send(json!({ "type": "op_status", "id": 7, "actionId": "11111111-2222-4333-8444-555555555555",
            "ownerGeneration": "1", "of": { "channel": CHANNEL_A, "id": 2 } }))
        .await;
    assert_eq!(asker.reply().await["code"], "agent_channel_protocol");
}

#[tokio::test]
async fn a_reply_over_its_bound_sends_its_largest_results_to_files_and_cuts_nothing() {
    let (endpoint, browser) = (endpoint(), Arc::new(Scripted::default()));
    let (_d, path) = directory();
    let (mut host, _) = Host::hello(&endpoint, &browser, json!({})).await;
    let html = |size: usize| {
        step(
            "agent_browser_get_html",
            json!({ "selector": format!("#{size}") }),
        )
    };
    // Each result carries its text twice (the model's text and the data).
    host.send(sequence(
        2,
        "41",
        json!([html(3 << 19), html(3 << 19), html(50_000), html(10)]),
        &path,
    ))
    .await;
    let mut text = String::new();
    host.lines.read_line(&mut text).await.unwrap();
    assert!(text.len() <= reply::MAX_REPLY_BYTES, "{}", text.len());
    let reply: Value = serde_json::from_str(&text).unwrap();
    let steps = reply["steps"].as_array().unwrap();
    // One of the two 3 MiB results had to leave the line; the rest fit.
    let referenced: Vec<usize> = (0..4)
        .filter(|index| reply::is_reference(&steps[*index]["result"]))
        .collect();
    assert_eq!(referenced.len(), 1, "{referenced:?}");
    let reference = &steps[referenced[0]]["result"];
    let file = std::fs::read(reference["path"].as_str().unwrap()).unwrap();
    assert_eq!(reference["sizeBytes"], file.len());
    let written: Value = serde_json::from_slice(&file).unwrap();
    assert_eq!(
        written["content"][0]["text"].as_str().unwrap().len(),
        3 << 19
    );
    assert_eq!(steps[3]["result"]["content"][0]["text"], "h".repeat(10));
    // The ledger keeps every result over 64 KiB as a reference, one file each.
    host.send(op_status(3, "41", Some((CHANNEL_A, 2)), 0)).await;
    let status = host.reply().await;
    let kept = status["data"]["result"]["steps"].as_array().unwrap();
    for (index, step) in kept.iter().take(3).enumerate() {
        assert!(reply::is_reference(&step["result"]), "{index}");
        assert!(step["result"]["path"].is_string());
    }
    assert_eq!(kept[3]["result"], steps[3]["result"]);
    assert_eq!(std::fs::read_dir(path.join("steps")).unwrap().count(), 3);
}

#[tokio::test]
async fn frames_past_the_read_ahead_bound_close_the_connection() {
    let (endpoint, browser) = (endpoint(), Arc::new(Scripted::default()));
    let (_d, path) = directory();
    let (mut host, _) = Host::hello(&endpoint, &browser, json!({})).await;
    host.send(sequence(2, "41", json!([wait()]), &path)).await;
    browser.started.notified().await;
    let mut pad = op_status(3, "41", None, 0);
    pad["pad"] = json!("p".repeat(3 << 20));
    let _ = host.writer.write_all(line(&pad).as_bytes()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    browser.go.notify_one();
    // The running frame still answers; then the connection ends.
    assert_eq!(host.reply().await["success"], true);
    assert!(host.closed().await);
}

#[tokio::test]
async fn a_paused_sequence_emits_need_and_accepts_the_reply_without_command_deadlock() {
    let (events, _) = tokio::sync::broadcast::channel(16);
    let browser = Arc::new(Scripted {
        custody: Some(events),
        ..Scripted::default()
    });
    let endpoint = endpoint_role(true);
    let (_directory, path) = directory();
    let (mut host, hello) = Host::hello(&endpoint, &browser, json!({"binding":{"version":1,"namespace":"thread","session":"browser","requireSandbox":true,"browserHost":true}})).await;
    assert_eq!(hello["data"]["sessionCustody"], 4);
    assert_eq!(hello["data"]["limits"]["maxRequestBytes"], 2 << 20);
    host.send(sequence(2, "41", json!([wait()]), &path)).await;
    let need = host.reply().await;
    assert_eq!(need["type"], "site_session.need");
    assert!(need.get("id").is_none());
    host.send(
        json!({"type":"site_session.refuse","id":3,"requestId":need["requestId"],
        "site":need["site"],"reason":"denied"}),
    )
    .await;
    let attached = host.reply().await;
    assert_eq!(attached["id"], 3);
    assert_eq!(attached["success"], true);
    let sequence = host.reply().await;
    assert_eq!(sequence["id"], 2);
    assert_eq!(sequence["success"], true);
}

#[tokio::test]
async fn immutable_host_binding_gates_custody_and_remote_programs() {
    let browser = Arc::new(Scripted::default());
    let forged = json!({"binding":{"version":1,"namespace":"thread","session":"browser","requireSandbox":true,"browserHost":true}});
    let (_, denied) = Host::hello(&endpoint(), &browser, forged.clone()).await;
    assert_eq!(denied["code"], "agent_channel_binding");
    let (_, denied) = Host::hello(&endpoint_role(true), &browser, json!({})).await;
    assert_eq!(denied["code"], "agent_channel_binding");
    let (mut standalone, accepted) = Host::hello(&endpoint(), &browser, json!({})).await;
    assert_eq!(accepted["success"], true);
    standalone
        .send(json!({"type":"site_sessions.offer","id":2,"sites":[]}))
        .await;
    assert_eq!(standalone.reply().await["success"], false);
    standalone.send(json!({"type":"program.status","id":3,"programId":"6b87fd14-4712-45e1-829e-95ee008fd783","actionId":ACTION,"ownerGeneration":"1"})).await;
    assert_eq!(standalone.reply().await["code"], "agent_channel_binding");
}

#[tokio::test]
async fn remote_status_settles_independently_beside_an_unanswered_sequence() {
    let endpoint = endpoint_role(true);
    let browser = Arc::new(Scripted::default());
    let (_directory, path) = directory();
    let (mut host,hello)=Host::hello(&endpoint,&browser,json!({"binding":{"version":1,"namespace":"thread","session":"browser","requireSandbox":true,"browserHost":true}})).await;
    assert_eq!(hello["success"], true);
    host.send(sequence(2, "41", json!([wait()]), &path)).await;
    browser.started.notified().await;
    host.send(json!({"type":"program.status","id":3,"programId":"6b87fd14-4712-45e1-829e-95ee008fd783","actionId":ACTION,"ownerGeneration":"41"})).await;
    let reply = host.reply().await;
    assert_eq!(
        reply["id"], 3,
        "metadata must settle without waiting behind sequence2"
    );
    assert_eq!(reply["data"]["nativeInputSettled"], true);
    assert_eq!(host.reply().await["id"], 2);
}

#[tokio::test]
async fn native_file_registration_answers_without_holding_a_program_slot_or_sequence() {
    let endpoint = endpoint_role(true);
    let browser = Arc::new(Scripted::default());
    let (_directory, path) = directory();
    let(mut host,hello)=Host::hello(&endpoint,&browser,json!({"binding":{"version":1,"namespace":"thread","session":"browser","requireSandbox":true,"browserHost":true}})).await;
    assert_eq!(hello["success"], true);
    host.send(sequence(2, "41", json!([wait()]), &path)).await;
    browser.started.notified().await;
    host.send(
        json!({"type":"action.files","id":3,"actionId":ACTION,"ownerGeneration":"41","files":[]}),
    )
    .await;
    let staged = host.reply().await;
    assert_eq!(staged["id"], 3);
    assert_eq!(staged["data"]["registered"], 0);
    assert_eq!(host.reply().await["id"], 2);
    host.send(
        json!({"type":"action.files.release","id":4,"actionId":ACTION,"ownerGeneration":"41"}),
    )
    .await;
    assert_eq!(host.reply().await["data"]["released"], true);
}

#[tokio::test]
async fn a_need_while_the_channel_is_idle_is_delivered_only_to_its_owner() {
    let (events, _) = tokio::sync::broadcast::channel(16);
    let browser = Arc::new(Scripted {
        custody: Some(events.clone()),
        ..Scripted::default()
    });
    let endpoint = endpoint();
    let (mut host, _) = Host::hello(&endpoint, &browser, json!({})).await;
    let _ = events.send((
        ChannelId::parse(CHANNEL_B).unwrap(),
        json!({"type":"site_session.need","site":"https://other.com"}),
    ));
    let _ = events.send((
        ChannelId::parse(CHANNEL_A).unwrap(),
        json!({"type":"site_session.need","site":"https://example.com"}),
    ));
    assert_eq!(host.reply().await["site"], "https://example.com");
}

#[tokio::test]
async fn a_channel_that_ends_is_told_to_the_browser_after_its_last_step() {
    let (endpoint, browser) = (endpoint(), Arc::new(Scripted::default()));
    let (_d, path) = directory();
    let (mut host, _) = Host::hello(&endpoint, &browser, json!({})).await;
    host.send(sequence(2, "41", json!([title(), title()]), &path))
        .await;
    assert_eq!(host.reply().await["success"], true);
    assert!(browser.ended().is_empty(), "a live channel has not ended");
    drop(host.writer);
    drop(host.lines);
    tokio::time::timeout(Duration::from_secs(10), host.served)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(browser.ended(), [(CHANNEL_A.to_string(), 2)]);
}

#[tokio::test]
async fn channel_eof_fences_custody_before_the_running_step_settles() {
    let (endpoint, browser) = (endpoint(), Arc::new(Scripted::default()));
    let (_directory, path) = directory();
    let (mut host, _) = Host::hello(&endpoint, &browser, json!({})).await;
    host.send(sequence(2, "41", json!([wait()]), &path)).await;
    browser.started.notified().await;
    drop(host.writer);
    drop(host.lines);
    let fenced =
        tokio::time::timeout(Duration::from_secs(1), browser.custody_closed.notified()).await;
    browser.go.notify_one();
    tokio::time::timeout(Duration::from_secs(10), host.served)
        .await
        .unwrap()
        .unwrap();
    assert!(
        fenced.is_ok(),
        "custody authority must end on EOF while ordinary input still settles"
    );
}
