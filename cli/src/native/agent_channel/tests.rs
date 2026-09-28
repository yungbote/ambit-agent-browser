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
    ran: StdMutex<Vec<String>>,
    started: Notify,
    go: Notify,
}

impl Scripted {
    fn ran(&self) -> Vec<String> {
        self.ran.lock().unwrap().clone()
    }
}

impl Browser for Scripted {
    async fn step(&self, _: &FrameContext<'_>, step: &PreparedStep) -> StepRecord {
        self.ran.lock().unwrap().push(step.op.clone());
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
            landed: None,
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
}

const DIGEST: &str = "sha256:540f433f3536f8432ca701d0bfea8b176fc630ca2120ff7ac22ac1082621c4a8";

fn endpoint() -> Arc<Endpoint> {
    Arc::new(Endpoint::with_digest(
        Identity {
            namespace: "thread".into(),
            session: "browser".into(),
            require_sandbox: true,
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
    async fn open(endpoint: &Arc<Endpoint>, browser: &Arc<Scripted>, first: Value) -> Self {
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
    async fn hello(
        endpoint: &Arc<Endpoint>,
        browser: &Arc<Scripted>,
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
    assert_eq!(data["catalog"], crate::mcp::host_bound::catalog_identity());
    assert!(data["catalog"].get("tools").is_none());
    assert_eq!(data["driverArtifactDigest"], DIGEST);
    assert_eq!(data["features"], json!(["motion"]));
    assert_eq!(data["excludedOps"], json!(["agent_browser_run_playwright"]));
    assert_eq!(
        data["limits"],
        json!({ "maxInFlight": 4, "maxRequestBytes": 2097152, "maxReplyBytes": 4194304,
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
    // Its outcome is in the ledger, as the reply carried it.
    host.send(op_status(8, "41", Some((CHANNEL_A, 7)), 0)).await;
    let status = host.reply().await;
    assert_eq!(status["data"]["state"], "settled", "{status}");
    assert_eq!(status["data"]["result"]["steps"], reply["steps"]);
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
