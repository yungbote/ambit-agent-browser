use super::*;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;

const OWNER: &str = "aabbccdd-1111-4222-8333-123456789abc";
const OTHER: &str = "aabbccdd-1111-4222-8333-123456789abd";

fn expires() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 20_000
}

fn command(op: &str, owner: &str) -> Value {
    let mut request = json!({ "action": ACTION, "op": op, "controllerId": owner });
    if matches!(op, "acquire" | "renew") {
        request["expiresAt"] = json!(expires());
    }
    request
}

fn parse(value: Value) -> ControlRequest {
    ControlRequest::parse(&value).unwrap()
}

#[tokio::test]
async fn expired_native_input_retains_custody_until_reset_is_proven() {
    let mut control = BrowserControl {
        lease: Some(Lease {
            controller_id: OWNER.into(),
            expires_at: 0,
            deadline: Instant::now() - Duration::from_millis(1),
            last_sequence: 1,
            outcome_unknown: false,
            held: HeldInputs::default(),
            native_input_pending: true,
            download_cursor: 0,
            sign_in: None,
        }),
        ..BrowserControl::default()
    };
    assert!(control.agent_error().is_some());
    // A lost helper cannot acknowledge release of a possibly held input.
    assert_eq!(
        control.expire(None).await.unwrap_err().code,
        "browser_control_outcome_unknown"
    );
    assert_eq!(
        control.agent_error().unwrap().code,
        "browser_control_outcome_unknown"
    );
    assert_eq!(
        control
            .execute(parse(command("release", OWNER)), None)
            .await
            .unwrap_err()
            .code,
        "browser_control_outcome_unknown"
    );
    assert_eq!(
        control
            .execute(parse(command("acquire", OTHER)), None)
            .await
            .unwrap_err()
            .code,
        "browser_control_outcome_unknown"
    );
    assert_eq!(control.lease.as_ref().unwrap().controller_id, OWNER);
    // The existing successful-reset path clears this pending state before
    // expired custody is released. Retiring Chrome also removes the lease.
    control.reset_browser();
    assert!(control.agent_error().is_none());
}

fn input(sequence: u64) -> ControlRequest {
    parse(
        json!({ "action": ACTION, "op": "input", "controllerId": OWNER, "sequence": sequence,
        "events": [{ "type": "input_keyboard", "eventType": "char", "text": "x" }] }),
    )
}

struct Browser {
    client: Arc<CdpClient>,
    commands: mpsc::Receiver<Value>,
    responses: mpsc::Sender<Value>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Browser {
    async fn new() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "ws://{}/devtools/browser/test",
            listener.local_addr().unwrap()
        );
        let (commands_tx, commands) = mpsc::channel(512);
        let (responses, mut responses_rx) = mpsc::channel::<Value>(512);
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut tx, mut rx) = ws.split();
            loop {
                tokio::select! {
                    response = responses_rx.recv() => {
                        let Some(response) = response else { break; };
                        if tx.send(Message::Text(response.to_string())).await.is_err() { break; }
                    }
                    command = rx.next() => {
                        match command {
                            Some(Ok(Message::Text(text))) => {
                                if commands_tx.send(serde_json::from_str(&text).unwrap()).await.is_err() { break; }
                            }
                            Some(Ok(Message::Ping(_))) => {}
                            _ => break,
                        }
                    }
                }
            }
        });
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        Self {
            client,
            commands,
            responses,
            task,
        }
    }

    async fn next(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(2), self.commands.recv())
            .await
            .unwrap()
            .unwrap()
    }

    async fn ack(&self, command: &Value) {
        self.responses
            .send(json!({ "id": command["id"], "result": {} }))
            .await
            .unwrap();
    }
}

#[test]
fn control_requests_enforce_bounds_and_reject_untrusted_fields() {
    let valid = json!({ "action": ACTION, "op": "input", "controllerId": OWNER, "sequence": 1,
        "events": [{ "type": "input_mouse", "eventType": "mousePressed", "x": 20.5, "y": 30,
            "button": "left", "buttons": 1, "modifiers": 15 }] });
    assert!(ControlRequest::parse(&valid).is_ok());
    let mut cases = Vec::new();
    for (key, value) in [
        ("controllerId", json!(OWNER.to_uppercase())),
        (
            "controllerId",
            json!("00000000-0000-0000-0000-000000000000"),
        ),
        ("sequence", json!(0)),
        ("sequence", json!(1.5)),
        ("sequence", json!(MAX_SAFE_INTEGER + 1)),
        ("expiresAt", Value::Null),
        ("script", json!("1")),
        ("events", json!([])),
        ("events", json!(vec![valid["events"][0].clone(); 65])),
    ] {
        let mut value_case = valid.clone();
        value_case[key] = value;
        cases.push(value_case);
    }
    for (key, value) in [
        ("x", json!(-0.1)),
        ("x", json!(32769)),
        ("x", json!("1")),
        ("y", Value::Null),
        ("modifiers", json!(16)),
        ("buttons", json!(32)),
        ("clickCount", json!(4)),
        ("button", json!("sideways")),
        ("eventType", json!("click")),
        ("deltaY", json!(32769)),
        ("method", json!("Runtime.evaluate")),
    ] {
        let mut value_case = valid.clone();
        value_case["events"][0][key] = value;
        cases.push(value_case);
    }
    let mut too_large = command("acquire", OWNER);
    too_large["id"] = json!("x".repeat(MAX_REQUEST_BYTES));
    cases.push(too_large);
    for case in cases {
        assert_eq!(
            ControlRequest::parse(&case).unwrap_err().code,
            "browser_control_invalid",
            "{case}"
        );
    }
    for op in ["acquire", "renew", "release"] {
        assert!(ControlRequest::parse(&command(op, OWNER)).is_ok());
    }
    let mut inspect = json!({ "action": ACTION, "op": "inspect" });
    assert!(ControlRequest::parse(&inspect).is_ok());
    inspect["events"] = Value::Null;
    assert!(ControlRequest::parse(&inspect).is_err());
}

#[test]
fn controller_viewport_is_a_bounded_css_geometry_event() {
    for (width, height) in [(1, 1), (640, 480), (32768, 32768)] {
        assert!(
            validate_event(&json!({ "type": "viewport", "width": width, "height": height }))
                .is_ok()
        );
    }
    for value in [
        json!(0),
        json!(-1),
        json!(1.5),
        json!(32769),
        json!("640"),
        Value::Null,
    ] {
        assert!(
            validate_event(&json!({ "type": "viewport", "width": value, "height": 480 })).is_err()
        );
        assert!(
            validate_event(&json!({ "type": "viewport", "width": 640, "height": value })).is_err()
        );
    }
    assert!(validate_event(
        &json!({ "type": "viewport", "width": 640, "height": 480, "deviceScaleFactor": 1 })
    )
    .is_err());
}

#[test]
fn controller_navigation_and_copy_keep_their_declared_fields() {
    for action in ["back", "forward", "reload"] {
        assert!(validate_event(&json!({ "type": "navigation", "action": action })).is_ok());
        assert!(validate_event(
            &json!({ "type": "navigation", "action": action, "url": "https://example.test/" })
        )
        .is_err());
    }
    for url in [
        "https://example.test/",
        "example.test",
        "about:blank",
        "data:text/plain,local",
    ] {
        assert!(
            validate_event(&json!({ "type": "navigation", "action": "navigate", "url": url }))
                .is_ok()
        );
    }
    for url in [json!(""), json!("https://"), json!(false), Value::Null] {
        assert!(
            validate_event(&json!({ "type": "navigation", "action": "navigate", "url": url }))
                .is_err()
        );
    }
    assert!(validate_event(
        &json!({ "type": "navigation", "action": "script", "url": "https://example.test/" })
    )
    .is_err());
    assert!(ControlRequest::parse(&command("copy", OWNER)).is_ok());
    let mut copy = command("copy", OWNER);
    copy["sequence"] = json!(1);
    assert!(ControlRequest::parse(&copy).is_err());
}

#[tokio::test]
async fn native_activity_follows_browser_acknowledgements_and_page_identity() {
    let mut browser = Browser::new().await;
    let mut events = browser.client.subscribe();
    let params = json!({ "type": "mouseMoved", "x": 20, "y": 30 });
    let mut pending = browser
        .client
        .enqueue_command(
            "Input.dispatchMouseEvent",
            Some(params.clone()),
            Some("page"),
        )
        .await
        .unwrap();
    let sent = browser.next().await;
    assert!(
        events.try_recv().is_err(),
        "unacknowledged input became visible"
    );
    browser
        .responses
        .send(json!({ "id": sent["id"], "error": { "code": -1, "message": "refused" }}))
        .await
        .unwrap();
    assert!(pending.acknowledgment().await.unwrap().error.is_some());
    assert!(events.try_recv().is_err(), "refused input became visible");
    let mut pending = browser
        .client
        .enqueue_command_from(
            "Input.dispatchMouseEvent",
            Some(params),
            Some("page"),
            InputSource::Human,
        )
        .await
        .unwrap();
    let sent = browser.next().await;
    let before = crate::native::stream::monotonic_us();
    browser.ack(&sent).await;
    pending.acknowledgment().await.unwrap();
    let observed = events.recv().await.unwrap();
    assert_eq!(observed.method, super::super::activity::EVENT);
    assert_eq!(observed.params["source"], "human");
    // The executed sample is stamped on the media clock when acknowledged.
    let ts = observed.params["ts"].as_u64().unwrap();
    assert!(
        (before..=crate::native::stream::monotonic_us()).contains(&ts),
        "{observed:?}"
    );
    browser.responses.send(json!({ "method": "Page.frameNavigated", "sessionId": "page", "params": { "frame": { "id": "main", "loaderId": "new" } } })).await.unwrap();
    let reset = events.recv().await.unwrap();
    assert_eq!(reset.params["eventType"], "reset");
    assert!(reset.params["ts"].as_u64() >= Some(ts));
    assert_ne!(
        reset.params["pageGeneration"],
        observed.params["pageGeneration"]
    );
    events.recv().await.unwrap(); // Original navigation event remains available.
    browser.responses.send(json!({ "method": "Page.screencastFrame", "sessionId": "page", "params": { "data": "frame", "sessionId": 1 } })).await.unwrap();
    let frame = events.recv().await.unwrap();
    assert_eq!(
        frame.params[super::super::activity::FRAME_GENERATION],
        reset.params["pageGeneration"]
    );
}

#[test]
fn control_keyboard_and_touch_validation_covers_optional_types_and_multi_touch() {
    let mut insertion =
        json!({ "type": "input_keyboard", "eventType": "insertText", "text": "Human input" });
    assert!(validate_event(&insertion).is_ok());
    assert_eq!(
        input_command("input_keyboard", &insertion).unwrap(),
        ("Input.insertText", json!({ "text": "Human input" }))
    );
    insertion["modifiers"] = json!(0);
    assert!(validate_event(&insertion).is_err());
    let mut keyboard = json!({ "type": "input_keyboard", "eventType": "char", "text": "é" });
    assert!(validate_event(&keyboard).is_ok());
    keyboard["text"] = json!("abcd");
    assert!(validate_event(&keyboard).is_err());
    keyboard["text"] = json!("abc");
    assert!(validate_event(&keyboard).is_ok());
    keyboard["text"] = json!("𝄞a");
    assert!(validate_event(&keyboard).is_ok());
    keyboard["text"] = json!("𝄞ab");
    assert!(validate_event(&keyboard).is_err());
    keyboard["text"] = json!("x".repeat(4097));
    assert!(validate_event(&keyboard).is_err());
    keyboard["text"] = json!("");
    assert!(validate_event(&keyboard).is_err());
    keyboard["key"] = json!("Escape");
    assert!(validate_event(&keyboard).is_ok());
    keyboard["code"] = Value::Null;
    assert!(validate_event(&keyboard).is_err());
    let point = json!({ "id": 1, "x": 0, "y": 32768, "force": 1, "radiusX": 2 });
    let mut touch =
        json!({ "type": "input_touch", "eventType": "touchStart", "touchPoints": [point.clone()] });
    assert!(validate_event(&touch).is_ok());
    touch["touchPoints"] = json!([point.clone(), point]);
    assert!(validate_event(&touch).is_err());
    touch["touchPoints"][1]["id"] = json!(2);
    assert!(validate_event(&touch).is_ok());
    touch["touchPoints"][1]["force"] = json!(1.1);
    assert!(validate_event(&touch).is_err());
    touch["touchPoints"] = json!([]);
    assert!(validate_event(&touch).is_err());
    touch["eventType"] = json!("touchEnd");
    assert!(validate_event(&touch).is_ok());
}

#[tokio::test]
async fn control_lease_is_exclusive_idempotent_bounded_and_recoverable() {
    let browser = Browser::new().await;
    let connection = Some((browser.client.as_ref(), "page"));
    let mut control = BrowserControl::default();
    let acquire = parse(command("acquire", OWNER));
    let original_expiry = acquire.expires_at.unwrap();
    let response = control.execute(acquire, connection).await.unwrap();
    assert_eq!(response["lastSequence"], 0);
    assert_eq!(
        control.agent_error().unwrap().code,
        "browser_controlled_by_user"
    );
    let response = control
        .execute(parse(command("acquire", OWNER)), connection)
        .await
        .unwrap();
    assert_eq!(response["expiresAt"], original_expiry);
    assert_eq!(
        control
            .execute(parse(command("acquire", OTHER)), connection)
            .await
            .unwrap_err()
            .code,
        "browser_control_conflict"
    );
    for op in ["renew", "release"] {
        assert_eq!(
            control
                .execute(parse(command(op, OTHER)), connection)
                .await
                .unwrap_err()
                .code,
            "browser_control_stale"
        );
    }
    let mut invalid = command("renew", OWNER);
    invalid["expiresAt"] = json!(expires() + 20_000);
    assert_eq!(
        control
            .execute(parse(invalid), connection)
            .await
            .unwrap_err()
            .code,
        "browser_control_invalid"
    );
    control
        .execute(parse(command("renew", OWNER)), connection)
        .await
        .unwrap();
    let released = control
        .execute(parse(command("release", OWNER)), connection)
        .await
        .unwrap();
    assert_eq!(
        released,
        control
            .execute(parse(command("release", OWNER)), connection)
            .await
            .unwrap()
    );
    assert!(control.agent_error().is_none());
    assert_eq!(
        control
            .execute(parse(command("acquire", OWNER)), connection)
            .await
            .unwrap_err()
            .code,
        "browser_control_stale"
    );
    control
        .execute(parse(command("acquire", OTHER)), connection)
        .await
        .unwrap();
    assert_eq!(
        released,
        control
            .execute(parse(command("release", OWNER)), connection)
            .await
            .unwrap()
    );
    assert_eq!(control.lease.as_ref().unwrap().controller_id, OTHER);
    control.lease.as_mut().unwrap().deadline = Instant::now();
    assert!(control.agent_error().is_none());
    for op in ["renew", "acquire"] {
        assert_eq!(
            control
                .execute(parse(command(op, OTHER)), connection)
                .await
                .unwrap_err()
                .code,
            "browser_control_expired"
        );
    }
    let fresh = uuid::Uuid::new_v4().to_string();
    control
        .execute(parse(command("acquire", &fresh)), connection)
        .await
        .unwrap();
    assert_eq!(control.lease.as_ref().unwrap().controller_id, fresh);
}

#[tokio::test]
async fn control_acquire_waits_for_every_prior_stream_ack_and_rejects_other_viewers() {
    let mut browser = Browser::new().await;
    let gate = Arc::new(Mutex::new(BrowserControl::default()));
    let mouse = json!({ "eventType": "mouseMoved", "x": 1, "y": 2 });
    for _ in 0..2 {
        gate.lock()
            .await
            .stream_input("input_mouse", &mouse, &browser.client, Some("page"))
            .await
            .unwrap();
    }
    let first = browser.next().await;
    let second = browser.next().await;
    let client = browser.client.clone();
    let task_gate = gate.clone();
    let mut acquire = tokio::spawn(async move {
        task_gate
            .lock()
            .await
            .execute(
                parse(command("acquire", OWNER)),
                Some((client.as_ref(), "page")),
            )
            .await
    });
    browser.ack(&second).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut acquire)
            .await
            .is_err()
    );
    browser.ack(&first).await;
    assert_eq!(acquire.await.unwrap().unwrap()["status"], "controlled");
    assert_eq!(
        gate.lock()
            .await
            .stream_input("input_mouse", &mouse, &browser.client, Some("page"))
            .await
            .unwrap_err()
            .code,
        "browser_controlled_by_user"
    );
    assert!(browser.commands.try_recv().is_err());
}

#[tokio::test]
async fn control_input_acknowledges_once_and_never_replays_a_duplicate_or_gap() {
    let mut browser = Browser::new().await;
    let gate = Arc::new(Mutex::new(BrowserControl::default()));
    gate.lock()
        .await
        .execute(
            parse(command("acquire", OWNER)),
            Some((browser.client.as_ref(), "page")),
        )
        .await
        .unwrap();
    let task_gate = gate.clone();
    let client = browser.client.clone();
    let input_task = tokio::spawn(async move {
        task_gate
            .lock()
            .await
            .execute(input(1), Some((client.as_ref(), "page")))
            .await
    });
    let sent = browser.next().await;
    assert_eq!(sent["method"], "Input.dispatchKeyEvent");
    assert_eq!(sent["params"]["text"], "x");
    assert!(sent["params"].get("key").is_none());
    browser.ack(&sent).await;
    assert_eq!(input_task.await.unwrap().unwrap()["status"], "applied");
    let mut control = gate.lock().await;
    assert_eq!(
        control
            .execute(input(1), Some((browser.client.as_ref(), "page")))
            .await
            .unwrap()["status"],
        "duplicate"
    );
    assert_eq!(
        control
            .execute(input(3), Some((browser.client.as_ref(), "page")))
            .await
            .unwrap_err()
            .code,
        "browser_control_sequence_gap"
    );
    assert!(browser.commands.try_recv().is_err());
}

#[tokio::test]
async fn control_cancelled_input_is_unknown_until_release_or_finite_expiry() {
    let mut browser = Browser::new().await;
    let gate = Arc::new(Mutex::new(BrowserControl::default()));
    gate.lock()
        .await
        .execute(
            parse(command("acquire", OWNER)),
            Some((browser.client.as_ref(), "page")),
        )
        .await
        .unwrap();
    let task_gate = gate.clone();
    let client = browser.client.clone();
    let task = tokio::spawn(async move {
        task_gate
            .lock()
            .await
            .execute(input(1), Some((client.as_ref(), "page")))
            .await
    });
    browser.next().await;
    task.abort();
    let _ = task.await;
    let mut control = gate.lock().await;
    assert_eq!(
        control.agent_error().unwrap().code,
        "browser_control_outcome_unknown"
    );
    for request in [
        input(1),
        input(2),
        parse(command("renew", OWNER)),
        parse(command("acquire", OWNER)),
    ] {
        assert_eq!(
            control
                .execute(request, Some((browser.client.as_ref(), "page")))
                .await
                .unwrap_err()
                .code,
            "browser_control_outcome_unknown"
        );
    }
    assert_eq!(control.lease.as_ref().unwrap().last_sequence, 0);
    assert!(browser.commands.try_recv().is_err());
    control.lease.as_mut().unwrap().deadline = Instant::now();
    assert!(control.agent_error().is_none());
    assert_eq!(
        control
            .execute(input(1), Some((browser.client.as_ref(), "page")))
            .await
            .unwrap_err()
            .code,
        "browser_control_expired"
    );
    assert_eq!(
        control
            .execute(
                parse(command("acquire", OTHER)),
                Some((browser.client.as_ref(), "page"))
            )
            .await
            .unwrap()["status"],
        "controlled"
    );
}

#[tokio::test]
async fn control_partial_batch_and_missing_ack_report_unknown_without_replay() {
    let mut browser = Browser::new().await;
    let gate = Arc::new(Mutex::new(BrowserControl::default()));
    gate.lock()
        .await
        .execute(
            parse(command("acquire", OWNER)),
            Some((browser.client.as_ref(), "page")),
        )
        .await
        .unwrap();
    gate.lock().await.lease.as_mut().unwrap().deadline =
        Instant::now() + Duration::from_millis(200);
    let mut request = input(1);
    request
        .events
        .as_mut()
        .unwrap()
        .push(json!({ "type": "input_keyboard", "eventType": "char", "text": "y" }));
    let task_gate = gate.clone();
    let client = browser.client.clone();
    let task = tokio::spawn(async move {
        task_gate
            .lock()
            .await
            .execute(request, Some((client.as_ref(), "page")))
            .await
    });
    let first = browser.next().await;
    browser.ack(&first).await;
    browser.next().await;
    assert_eq!(
        task.await.unwrap().unwrap_err().code,
        "browser_control_outcome_unknown"
    );
    assert_eq!(gate.lock().await.lease.as_ref().unwrap().last_sequence, 0);
    assert!(browser.commands.try_recv().is_err());
}

#[tokio::test]
async fn control_cancelled_acquire_keeps_pending_stream_receipts() {
    let mut browser = Browser::new().await;
    let gate = Arc::new(Mutex::new(BrowserControl::default()));
    gate.lock()
        .await
        .stream_input(
            "input_keyboard",
            &json!({ "text": "x", "eventType": "char" }),
            &browser.client,
            Some("page"),
        )
        .await
        .unwrap();
    let event = browser.next().await;
    let task_gate = gate.clone();
    let client = browser.client.clone();
    let task = tokio::spawn(async move {
        task_gate
            .lock()
            .await
            .execute(
                parse(command("acquire", OWNER)),
                Some((client.as_ref(), "page")),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    task.abort();
    let _ = task.await;
    assert_eq!(gate.lock().await.pending_stream.len(), 1);
    browser.ack(&event).await;
    assert_eq!(
        gate.lock()
            .await
            .execute(
                parse(command("acquire", OWNER)),
                Some((browser.client.as_ref(), "page"))
            )
            .await
            .unwrap()["status"],
        "controlled"
    );
}

#[tokio::test]
async fn control_stream_receipts_are_bounded_and_disconnect_cannot_imply_settlement() {
    let mut browser = Browser::new().await;
    let mut control = BrowserControl::default();
    for _ in 0..MAX_PENDING_STREAM_INPUTS {
        control
            .stream_input(
                "input_keyboard",
                &json!({ "text": "x", "eventType": "char" }),
                &browser.client,
                Some("page"),
            )
            .await
            .unwrap();
        browser.next().await;
    }
    assert_eq!(control.pending_stream.len(), MAX_PENDING_STREAM_INPUTS);
    assert_eq!(
        control
            .stream_input(
                "input_keyboard",
                &json!({ "text": "x" }),
                &browser.client,
                Some("page")
            )
            .await
            .unwrap_err()
            .code,
        "browser_control_input_busy"
    );
    browser.task.abort();
    assert_eq!(
        control
            .execute(
                parse(command("acquire", OWNER)),
                Some((browser.client.as_ref(), "page"))
            )
            .await
            .unwrap_err()
            .code,
        "browser_control_outcome_unknown"
    );
    assert!(control.lease.is_none());
}

#[tokio::test]
async fn control_acquire_neutralizes_legacy_held_input_on_its_original_page() {
    let mut browser = Browser::new().await;
    let gate = Arc::new(Mutex::new(BrowserControl::default()));
    gate.lock()
        .await
        .stream_input(
            "input_keyboard",
            &json!({ "eventType": "keyDown", "key": "Shift", "code": "ShiftLeft", "modifiers": 8 }),
            &browser.client,
            Some("original-page"),
        )
        .await
        .unwrap();
    let down = browser.next().await;
    browser.ack(&down).await;
    let task_gate = gate.clone();
    let client = browser.client.clone();
    let acquire = tokio::spawn(async move {
        task_gate
            .lock()
            .await
            .execute(
                parse(command("acquire", OWNER)),
                Some((client.as_ref(), "new-active-page")),
            )
            .await
    });
    let up = browser.next().await;
    assert_eq!(up["method"], "Input.dispatchKeyEvent");
    assert_eq!(up["sessionId"], "original-page");
    assert_eq!(up["params"]["type"], "keyUp");
    assert_eq!(up["params"]["modifiers"], 0);
    browser.ack(&up).await;
    assert_eq!(acquire.await.unwrap().unwrap()["status"], "controlled");
    assert!(gate.lock().await.stream_held.next_release().is_none());
}

#[tokio::test]
async fn control_cancelled_transfer_never_replays_an_unacknowledged_release() {
    let mut browser = Browser::new().await;
    let gate = Arc::new(Mutex::new(BrowserControl::default()));
    gate.lock()
        .await
        .execute(
            parse(command("acquire", OWNER)),
            Some((browser.client.as_ref(), "page")),
        )
        .await
        .unwrap();
    let mut down = input(1);
    down.events = Some(vec![
        json!({ "type": "input_keyboard", "eventType": "keyDown", "key": "Shift", "code": "ShiftLeft", "modifiers": 8 }),
    ]);
    let task_gate = gate.clone();
    let client = browser.client.clone();
    let sent = tokio::spawn(async move {
        task_gate
            .lock()
            .await
            .execute(down, Some((client.as_ref(), "page")))
            .await
    });
    let event = browser.next().await;
    browser.ack(&event).await;
    sent.await.unwrap().unwrap();
    let task_gate = gate.clone();
    let client = browser.client.clone();
    let release = tokio::spawn(async move {
        task_gate
            .lock()
            .await
            .execute(
                parse(command("release", OWNER)),
                Some((client.as_ref(), "page")),
            )
            .await
    });
    let up = browser.next().await;
    assert_eq!(up["params"]["type"], "keyUp");
    release.abort();
    let _ = release.await;
    assert_eq!(
        gate.lock().await.agent_error().unwrap().code,
        "browser_control_outcome_unknown"
    );
    let released = gate
        .lock()
        .await
        .execute(
            parse(command("release", OWNER)),
            Some((browser.client.as_ref(), "page")),
        )
        .await
        .unwrap();
    assert_eq!(released["status"], "released");
    assert!(
        browser.commands.try_recv().is_err(),
        "lost release must never be sent again"
    );
}

#[test]
fn internal_file_requests_require_bounded_identity_and_typed_paths() {
    let destination = "11223344-1111-4222-8333-123456789abc";
    for request in [
        command("files", OWNER),
        json!({"action":ACTION,"op":"dismissfiles","controllerId":OWNER,"destinationId":destination}),
        json!({"action":ACTION,"op":"setfiles","controllerId":OWNER,"destinationId":destination,"sequence":1,"files":["/workspace/work/input.bin"]}),
        json!({"action":ACTION,"op":"drop","controllerId":OWNER,"sequence":1,"expectedSurfaceGeneration":destination,"x":0,"y":21.5}),
    ] {
        assert!(ControlRequest::parse(&request).is_ok(), "{request}");
    }
    let valid = json!({"action":ACTION,"op":"setfiles","controllerId":OWNER,"destinationId":destination,"sequence":1,"files":["/workspace/work/input.bin"]});
    for (key, value) in [
        ("files", json!([])),
        ("files", json!(["relative"])),
        ("files", json!(["/x\u{0}y"])),
        ("files", json!(vec!["/x"; 65])),
        ("sequence", json!(0)),
        ("destinationId", json!("wrong")),
        ("bytes", json!("AA==")),
        ("path", json!("/etc/passwd")),
    ] {
        let mut request = valid.clone();
        request[key] = value;
        assert!(ControlRequest::parse(&request).is_err(), "{request}");
    }
    let mut status = command("files", OWNER);
    status["path"] = json!("/arbitrary");
    assert!(ControlRequest::parse(&status).is_err());
}

#[test]
fn completed_download_snapshot_is_readonly_and_has_no_controller_fields() {
    let request = json!({"action":ACTION,"op":"downloads"});
    assert!(ControlRequest::parse(&request).is_ok());
    for field in ["controllerId", "sequence", "path", "files", "destinationId"] {
        let mut invalid = request.clone();
        invalid[field] = Value::Null;
        assert!(ControlRequest::parse(&invalid).is_err(), "{invalid}");
    }
}

#[tokio::test]
async fn inspect_does_not_infer_file_support_without_a_chromium_page() {
    let mut control = BrowserControl::default();
    let result = control
        .execute(parse(json!({"action":ACTION,"op":"inspect"})), None)
        .await
        .unwrap();
    assert_eq!(result["supported"], true);
    assert_eq!(result["filesSupported"], false);
    assert!(result.get("controllerId").is_none());
}

#[tokio::test]
async fn resumed_controller_rejects_late_input_and_starts_a_fresh_sequence() {
    let mut browser = Browser::new().await;
    let gate = Arc::new(Mutex::new(BrowserControl::default()));
    {
        let mut control = gate.lock().await;
        let connection = Some((browser.client.as_ref(), "page"));
        control
            .execute(parse(command("acquire", OWNER)), connection)
            .await
            .unwrap();
        control
            .execute(parse(command("release", OWNER)), connection)
            .await
            .unwrap();
        let fresh = control
            .execute(parse(command("acquire", OTHER)), connection)
            .await
            .unwrap();
        assert_eq!(fresh["lastSequence"], 0);
        for sequence in [1, 2] {
            assert_eq!(
                control
                    .execute(input(sequence), connection)
                    .await
                    .unwrap_err()
                    .code,
                "browser_control_stale"
            );
        }
        assert!(
            browser.commands.try_recv().is_err(),
            "late input must never reach the browser"
        );
    }
    let task_gate = gate.clone();
    let client = browser.client.clone();
    let task = tokio::spawn(async move {
        let mut next = input(1);
        next.controller_id = OTHER.into();
        task_gate
            .lock()
            .await
            .execute(next, Some((client.as_ref(), "page")))
            .await
    });
    let sent = browser.next().await;
    assert_eq!(sent["method"], "Input.dispatchKeyEvent");
    browser.ack(&sent).await;
    let applied = task.await.unwrap().unwrap();
    assert_eq!(applied["status"], "applied");
    assert_eq!(applied["lastSequence"], 1);
    assert!(browser.commands.try_recv().is_err());
}

fn sign_in_request(sequence: u64, events: Value) -> ControlRequest {
    parse(
        json!({ "action": ACTION, "op": "input", "controllerId": OWNER,
        "sequence": sequence, "events": events }),
    )
}

fn sign_in_event() -> Value {
    json!([{ "type": "sign_in", "idleTimeoutMs": 600000 }])
}

fn lease_for(controller_id: &str, deadline: Instant, last_sequence: u64) -> Lease {
    Lease {
        controller_id: controller_id.into(),
        expires_at: expires(),
        deadline,
        last_sequence,
        outcome_unknown: false,
        held: HeldInputs::default(),
        native_input_pending: false,
        download_cursor: 0,
        sign_in: None,
    }
}

/// A display helper that acknowledges every control operation and reports
/// each operation it received.
fn acknowledging_display() -> (
    std::sync::Arc<crate::native::display::DisplayClient>,
    mpsc::UnboundedReceiver<Value>,
    tokio::net::UnixStream,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let (display, peer, frames) = crate::native::display::DisplayClient::test_channel();
    let (seen, received) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut peer = tokio::io::BufReader::new(peer);
        let mut line = String::new();
        while peer.read_line(&mut line).await.is_ok_and(|read| read > 0) {
            let request: Value = serde_json::from_str(&line).unwrap();
            line.clear();
            let reply = json!({ "id": request["id"], "success": true, "data": {} });
            let _ = seen.send(request);
            if peer
                .get_mut()
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .is_err()
            {
                break;
            }
        }
    });
    (display, received, frames)
}

#[test]
fn sign_in_event_shape_is_exact_and_judged_after_the_sequence() {
    for (idle, expected) in [(10_000, 10), (600_000, 600), (3_600_000, 3600)] {
        let request = sign_in_request(1, json!([{ "type": "sign_in", "idleTimeoutMs": idle }]));
        assert_eq!(
            request.sign_in().unwrap().unwrap(),
            Duration::from_secs(expected)
        );
    }
    let generation = uuid::Uuid::new_v4().to_string();
    let mut surfaced = json!({ "action": ACTION, "op": "input", "controllerId": OWNER,
        "sequence": 1, "events": sign_in_event() });
    surfaced["expectedSurfaceGeneration"] = json!(generation);
    assert!(parse(surfaced).sign_in().unwrap().is_ok());
    assert!(input(1).sign_in().is_none());
    // Variants still parse, so a replayed sequence reads as a duplicate; the
    // event itself is refused only after the sequence is ordered.
    for events in [
        json!([{ "type": "sign_in" }]),
        json!([{ "type": "sign_in", "idleTimeoutMs": 9_999 }]),
        json!([{ "type": "sign_in", "idleTimeoutMs": 3_600_001 }]),
        json!([{ "type": "sign_in", "idleTimeoutMs": 600000.5 }]),
        json!([{ "type": "sign_in", "idleTimeoutMs": "600000" }]),
        json!([{ "type": "sign_in", "idleTimeoutMs": -1 }]),
        json!([{ "type": "sign_in", "idleTimeoutMs": 600000, "url": "https://example.com" }]),
        json!([{ "type": "sign_in", "idleTimeoutMs": 600000 }, { "type": "sign_in", "idleTimeoutMs": 600000 }]),
        json!([{ "type": "sign_in", "idleTimeoutMs": 600000 }, { "type": "input_keyboard", "eventType": "char", "text": "x" }]),
        json!(["sign_in", { "type": "sign_in", "idleTimeoutMs": 600000 }]),
    ] {
        let error = sign_in_request(1, events.clone())
            .sign_in()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code, "browser_control_invalid", "{events}");
    }
    // Envelope fields keep their ordinary parse-time rules.
    for invalid in [
        json!({ "action": ACTION, "op": "input", "controllerId": OWNER, "events": sign_in_event() }),
        json!({ "action": ACTION, "op": "input", "controllerId": OWNER, "sequence": 1,
            "events": sign_in_event(), "expiresAt": expires() }),
        json!({ "action": ACTION, "op": "input", "controllerId": OWNER, "sequence": 1,
            "events": sign_in_event(), "expectedSurfaceGeneration": "not-a-uuid" }),
    ] {
        assert!(ControlRequest::parse(&invalid).is_err(), "{invalid}");
    }
}

#[tokio::test]
async fn sign_in_admission_follows_the_contract_order_without_effect() {
    let (display, _ops, _frames) = acknowledging_display();
    let later = Instant::now() + Duration::from_secs(20);
    let mut control = BrowserControl {
        lease: Some(lease_for(OWNER, later, 1)),
        display: Some(display.clone()),
        ..BrowserControl::default()
    };
    let admit = |control: &BrowserControl, request: ControlRequest| {
        let event = request.sign_in().unwrap();
        control.admit_sign_in(&request, event)
    };
    let refused = |result: Result<SignInAdmission, ControlError>| match result {
        Err(error) => error.code,
        Ok(_) => "admitted",
    };
    let malformed = json!([{ "type": "sign_in", "idleTimeoutMs": 5 }]);
    let mut other = sign_in_request(2, malformed.clone());
    other.controller_id = OTHER.into();
    assert_eq!(refused(admit(&control, other)), "browser_control_stale");
    control.lease.as_mut().unwrap().deadline = Instant::now();
    assert_eq!(
        refused(admit(&control, sign_in_request(2, malformed.clone()))),
        "browser_control_expired"
    );
    control.lease.as_mut().unwrap().deadline = later;
    control.lease.as_mut().unwrap().outcome_unknown = true;
    assert_eq!(
        refused(admit(&control, sign_in_request(2, malformed.clone()))),
        "browser_control_outcome_unknown"
    );
    control.lease.as_mut().unwrap().outcome_unknown = false;
    // An applied sequence repeats its acknowledgment before anything else.
    match admit(&control, sign_in_request(1, malformed.clone())).unwrap() {
        SignInAdmission::Duplicate(acknowledgment) => {
            assert_eq!(acknowledgment["status"], "duplicate");
            assert_eq!(acknowledgment["lastSequence"], 1);
            assert_eq!(
                acknowledgment["surface"]["generation"],
                display.surface().generation
            );
        }
        SignInAdmission::Admitted { .. } => panic!("a duplicate was admitted"),
    }
    assert_eq!(
        refused(admit(&control, sign_in_request(3, sign_in_event()))),
        "browser_control_sequence_gap"
    );
    let mut stale = json!({ "action": ACTION, "op": "input", "controllerId": OWNER,
        "sequence": 2, "events": malformed });
    stale["expectedSurfaceGeneration"] = json!(uuid::Uuid::new_v4().to_string());
    assert_eq!(
        refused(admit(&control, parse(stale))),
        "browser_control_surface_stale"
    );
    let error = admit(&control, sign_in_request(2, json!([{ "type": "sign_in" }])))
        .err()
        .unwrap();
    assert_eq!(error.code, "browser_control_invalid");
    assert!(error.message.contains("idleTimeoutMs"), "{}", error.message);
    // No refusal consumed the sequence or changed the lease.
    assert_eq!(control.lease.as_ref().unwrap().last_sequence, 1);
    assert!(!control.signing_in());
    let SignInAdmission::Admitted {
        sequence,
        idle_timeout,
    } = admit(&control, sign_in_request(2, sign_in_event())).unwrap()
    else {
        panic!("a valid sign-in was not admitted");
    };
    assert_eq!((sequence, idle_timeout), (2, Duration::from_secs(600)));
    let applied = control
        .begin_sign_in(OWNER, sequence, idle_timeout)
        .unwrap();
    assert_eq!(applied["status"], "applied");
    assert_eq!(applied["lastSequence"], 2);
    assert_eq!(
        applied["surface"]["generation"],
        display.surface().generation
    );
    let error = admit(&control, sign_in_request(3, sign_in_event()))
        .err()
        .unwrap();
    assert_eq!(error.code, "browser_control_invalid");
    assert!(error.message.contains("already"), "{}", error.message);
    assert!(matches!(
        admit(&control, sign_in_request(2, sign_in_event())).unwrap(),
        SignInAdmission::Duplicate(_)
    ));
}

#[tokio::test]
async fn sign_in_custody_outlasts_the_lease_deadline_until_the_browser_is_handed_back() {
    let (display, _ops, _frames) = acknowledging_display();
    let mut control = BrowserControl {
        lease: Some(lease_for(
            OWNER,
            Instant::now() + Duration::from_secs(20),
            0,
        )),
        display: Some(display),
        ..BrowserControl::default()
    };
    control
        .begin_sign_in(OWNER, 1, Duration::from_secs(600))
        .unwrap();
    assert!(control.signs_in_for(OWNER));
    assert!(!control.signs_in_for(OTHER));
    let refusal = control.agent_error().unwrap();
    assert_eq!(refusal.code, "browser_controlled_by_user");
    assert!(
        refusal.message.contains("signing in"),
        "{}",
        refusal.message
    );
    let inspected = control
        .execute(parse(json!({"action":ACTION,"op":"inspect"})), None)
        .await
        .unwrap();
    assert_eq!(inspected["supported"], true);
    assert_eq!(inspected["controlled"], true);
    assert!(inspected.get("filesSupported").is_none(), "{inspected}");

    // The lease lapses: the watchdog is due, and until it hands the browser
    // back no agent command and no other controller may take the browser.
    control.lease.as_mut().unwrap().deadline = Instant::now() - Duration::from_millis(1);
    assert!(control.sign_in_due(Instant::now()));
    assert_eq!(
        control.agent_error().unwrap().code,
        "browser_controlled_by_user"
    );
    let mut other = command("acquire", OTHER);
    other["expiresAt"] = json!(expires());
    assert_eq!(
        control.execute(parse(other), None).await.unwrap_err().code,
        "browser_control_conflict"
    );

    let released = control.end_lease().unwrap();
    assert_eq!(released["status"], "released");
    assert!(control.agent_error().is_none());
    assert!(control.needs_observation());
    assert!(!control.sign_in_due(Instant::now()));
    assert_eq!(
        control
            .execute(parse(command("release", OWNER)), None)
            .await
            .unwrap()["status"],
        "released"
    );
    assert_eq!(
        control
            .execute(parse(command("renew", OWNER)), None)
            .await
            .unwrap_err()
            .code,
        "browser_control_stale"
    );
}

/// The idle clock follows the person, not the dock: renewing keeps it,
/// applied input restarts it, and a window without DevTools takes native
/// input with no page session at all.
#[tokio::test]
async fn sign_in_idle_clock_restarts_on_applied_input_only() {
    let (display, mut ops, _frames) = acknowledging_display();
    let mut control = BrowserControl {
        lease: Some(lease_for(
            OWNER,
            Instant::now() + Duration::from_secs(20),
            0,
        )),
        display: Some(display.clone()),
        ..BrowserControl::default()
    };
    control
        .begin_sign_in(OWNER, 1, Duration::from_secs(10))
        .unwrap();
    let idle = |control: &BrowserControl| {
        control
            .lease
            .as_ref()
            .unwrap()
            .sign_in
            .as_ref()
            .unwrap()
            .idle_deadline
    };
    let started = idle(&control);
    assert!(!control.sign_in_due(Instant::now()));
    assert!(control.sign_in_due(started));
    control
        .execute(parse(command("renew", OWNER)), None)
        .await
        .unwrap();
    assert_eq!(idle(&control), started, "renewal is not presence");

    let generation = display.surface().generation;
    let typed = parse(
        json!({ "action": ACTION, "op": "input", "controllerId": OWNER,
        "sequence": 2, "expectedSurfaceGeneration": generation,
        "events": [{ "type": "input_keyboard", "eventType": "insertText", "text": "person@example.com" }] }),
    );
    let applied = control.execute(typed, None).await.unwrap();
    assert_eq!(applied["status"], "applied");
    assert_eq!(applied["lastSequence"], 2);
    assert_eq!(ops.recv().await.unwrap()["op"], "input");
    assert!(
        idle(&control) > started,
        "applied input restarts the idle clock"
    );

    let navigate = parse(
        json!({ "action": ACTION, "op": "input", "controllerId": OWNER,
        "sequence": 3, "expectedSurfaceGeneration": generation,
        "events": [{ "type": "navigation", "action": "navigate", "url": "https://example.com" }] }),
    );
    let refused = control.execute(navigate, None).await.unwrap_err();
    assert_eq!(refused.code, "browser_control_invalid");
    assert!(
        refused.message.contains("address bar"),
        "{}",
        refused.message
    );
    assert_eq!(control.lease.as_ref().unwrap().last_sequence, 2);
    assert!(
        ops.try_recv().is_err(),
        "a refused batch never reaches the window"
    );

    control
        .lease
        .as_mut()
        .unwrap()
        .sign_in
        .as_mut()
        .unwrap()
        .idle_deadline = Instant::now();
    assert!(control.sign_in_due(Instant::now()));
}

/// Only a person's native mouse and keyboard input to an owned window skips
/// command custody. Everything else, and a sign-in the watchdog must end
/// first, is left to the command path unapplied.
#[tokio::test]
async fn window_input_fast_path_serves_only_native_window_input() {
    let (display, mut ops, _frames) = acknowledging_display();
    let generation = display.surface().generation;
    let batch = |sequence: u64, events: Value| {
        json!({ "id": "r", "action": ACTION, "op": "input", "controllerId": OWNER,
            "sequence": sequence, "expectedSurfaceGeneration": generation, "events": events })
    };
    let key = json!([{ "type": "input_keyboard", "eventType": "keyDown", "key": "a" }]);
    let control = Mutex::new(BrowserControl {
        lease: Some(lease_for(
            OWNER,
            Instant::now() + Duration::from_secs(20),
            0,
        )),
        ..BrowserControl::default()
    });
    let received = Instant::now();
    // No owned window: page input needs its DevTools session.
    assert!(
        serve_window_input(&control, &batch(1, key.clone()), received)
            .await
            .is_none()
    );
    control.lock().await.display = Some(display.clone());
    for events in [
        json!([{ "type": "navigation", "action": "reload" }]),
        json!([{ "type": "viewport", "width": 640, "height": 480 }]),
        json!([{ "type": "input_touch", "eventType": "touchStart", "touchPoints": [{ "x": 1, "y": 1, "id": 1 }] }]),
        json!([{ "type": "input_keyboard", "eventType": "keyDown", "key": "a" }, { "type": "navigation", "action": "reload" }]),
        sign_in_event(),
        json!([]),
    ] {
        assert!(
            serve_window_input(&control, &batch(1, events.clone()), received)
                .await
                .is_none(),
            "{events}"
        );
    }
    for other in [command("renew", OWNER), command("release", OWNER)] {
        assert!(serve_window_input(&control, &other, received)
            .await
            .is_none());
    }
    assert!(ops.try_recv().is_err(), "nothing reached the window");
    assert_eq!(
        control.lock().await.lease.as_ref().unwrap().last_sequence,
        0
    );

    let applied = serve_window_input(&control, &batch(1, key.clone()), received)
        .await
        .unwrap();
    assert_eq!(applied["success"], true, "{applied}");
    assert_eq!(applied["data"]["lastSequence"], 1);
    assert_eq!(ops.recv().await.unwrap()["op"], "input");
    // A replayed sequence is a duplicate, never a second keystroke.
    let duplicate = serve_window_input(&control, &batch(1, key.clone()), received)
        .await
        .unwrap();
    assert_eq!(duplicate["data"]["status"], "duplicate");
    assert!(ops.try_recv().is_err());
    // A stale surface is refused by the same lease rules.
    let mut stale = batch(2, key.clone());
    stale["expectedSurfaceGeneration"] = json!(uuid::Uuid::new_v4().to_string());
    let refused = serve_window_input(&control, &stale, received)
        .await
        .unwrap();
    assert_eq!(refused["code"], "browser_control_surface_stale");

    // A sign-in whose idle clock ran out is ended by the watchdog first.
    control
        .lock()
        .await
        .begin_sign_in(OWNER, 2, Duration::from_secs(10))
        .unwrap();
    control
        .lock()
        .await
        .lease
        .as_mut()
        .unwrap()
        .sign_in
        .as_mut()
        .unwrap()
        .idle_deadline = Instant::now();
    assert!(serve_window_input(&control, &batch(3, key), received)
        .await
        .is_none());
    assert!(ops.try_recv().is_err());
}

/// Frames name the input they include by its sequence: the watermark moves
/// only after the display acknowledged a batch, never on a refused one, and
/// clears when the lease ends.
#[tokio::test]
async fn applied_input_watermark_follows_acknowledged_input_only() {
    let (display, mut ops, _frames) = acknowledging_display();
    let generation = display.surface().generation;
    let mut control = BrowserControl {
        lease: Some(lease_for(
            OWNER,
            Instant::now() + Duration::from_secs(20),
            0,
        )),
        display: Some(display.clone()),
        ..BrowserControl::default()
    };
    let watermark = control.applied_input();
    let load = || watermark.at(u64::MAX).unwrap_or(0);
    let key = |sequence: u64, generation: &str| {
        parse(
            json!({ "action": ACTION, "op": "input", "controllerId": OWNER,
            "sequence": sequence, "expectedSurfaceGeneration": generation,
            "events": [{ "type": "input_keyboard", "eventType": "keyDown", "key": "a" }] }),
        )
    };
    assert_eq!(load(), 0);
    control.execute(key(1, &generation), None).await.unwrap();
    assert_eq!(ops.recv().await.unwrap()["op"], "input");
    assert_eq!(load(), 1);
    let stale = uuid::Uuid::new_v4().to_string();
    assert!(control.execute(key(2, &stale), None).await.is_err());
    assert_eq!(load(), 1, "a refused batch applied nothing");
    let between = crate::native::stream::monotonic_us();
    control.execute(key(2, &generation), None).await.unwrap();
    assert_eq!(load(), 2);
    // A capture that began before the second acknowledgement shows only
    // the first input, even when it is answered after it.
    assert_eq!(watermark.at(between), Some(1));
    control
        .execute(parse(command("release", OWNER)), None)
        .await
        .unwrap();
    assert_eq!(load(), 0);
    assert_eq!(watermark.at(between), Some(1));
}
