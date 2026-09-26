//! Operation-local CDP transport. Playwright keeps its complete protocol and
//! session identities. Actual input alone joins the existing native owner;
//! DOM evaluation never manufactures pointer movement or user-input events.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch, Mutex};
use tokio::task::{JoinHandle, JoinSet};
use tokio_tungstenite::tungstenite::Message;

use crate::native::browser_control::BrowserControl;
use crate::native::cdp::client::CdpClient;

pub(super) struct Tunnel {
    endpoint: String,
    pub(super) client: Arc<CdpClient>,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<Result<(), String>>>,
}

impl Tunnel {
    pub(super) async fn start(
        client: Arc<CdpClient>,
        control: Arc<Mutex<BrowserControl>>,
    ) -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|_| {
            "browser_operation_rejected: The local Playwright transport could not start."
        })?;
        let path = format!("/{}", uuid::Uuid::new_v4().simple());
        let endpoint = format!(
            "ws://127.0.0.1:{}{path}",
            listener
                .local_addr()
                .map_err(|error| error.to_string())?
                .port()
        );
        let (stop, _) = watch::channel(false);
        if control.lock().await.has_native_display() {
            client.enable_window_pointer();
        }
        let task = tokio::spawn(serve(listener, client.clone(), control, stop.clone(), path));
        Ok(Self {
            endpoint,
            client,
            stop,
            task: Some(task),
        })
    }

    pub(super) fn endpoint(&self) -> &str {
        &self.endpoint
    }
    pub(super) fn stop(&self) {
        self.stop.send_replace(true);
    }

    pub(super) async fn finish(&mut self) -> Result<(), String> {
        self.stop();
        if let Some(mut task) = self.task.take() {
            match tokio::time::timeout(Duration::from_secs(6), &mut task).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err("The Playwright transport stopped without settlement.".into()),
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                    Err("The final native Playwright input could not be acknowledged.".into())
                }
            }
        } else {
            Ok(())
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.stop();
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.client.disconnect();
    }
}

/// Program commands written to the browser and not yet acknowledged.
const MAX_PENDING_COMMANDS: usize = 256;

fn is_input(method: &str) -> bool {
    matches!(
        method,
        "Input.dispatchMouseEvent"
            | "Input.dispatchKeyEvent"
            | "Input.insertText"
            | "Input.dispatchTouchEvent"
    )
}

struct InputWorker(JoinHandle<()>);

impl Drop for InputWorker {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn serve(
    listener: TcpListener,
    client: Arc<CdpClient>,
    control: Arc<Mutex<BrowserControl>>,
    stopped: watch::Sender<bool>,
    path: String,
) -> Result<(), String> {
    let mut stop = stopped.subscribe();
    if *stop.borrow() {
        return Ok(());
    }
    let socket = loop {
        let socket = tokio::select! {
            biased;
            _ = stop.changed() => return Ok(()),
            accepted = listener.accept() => accepted.map_err(|error| error.to_string())?.0,
        };
        let handshake = tokio_tungstenite::accept_hdr_async(
            socket,
            |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
                if request.headers().contains_key("origin") || request.uri().path() != path {
                    return Err(tokio_tungstenite::tungstenite::http::Response::builder()
                        .status(403)
                        .body(Some(
                            "Browser-origin and foreign operation connections are not permitted."
                                .to_owned(),
                        ))
                        .unwrap());
                }
                Ok(response)
            },
        );
        let accepted = tokio::select! {
            biased;
            _ = stop.changed() => return Ok(()),
            accepted = tokio::time::timeout(Duration::from_secs(1), handshake) => accepted,
        };
        if let Ok(Ok(socket)) = accepted {
            break socket;
        }
    };
    drop(listener);
    let (mut writer, mut reader) = socket.split();
    // Browser output is never dropped or reordered, so it is never refused:
    // while the program's event loop is busy its messages wait here, as they
    // would in Chrome's own buffer on a direct connection.
    let (outgoing, mut replies) = mpsc::unbounded_channel::<Message>();
    let (inputs, mut input_requests) = mpsc::channel::<Value>(64);
    let mut tasks = JoinSet::new();
    tasks.spawn(async move {
        while let Some(message) = replies.recv().await {
            writer
                .send(message)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok::<_, String>(())
    });
    // Program command ids, by the reserved browser command id carrying each.
    let forwarded = Arc::new(std::sync::Mutex::new(HashMap::<u64, Value>::new()));
    // One reader forwards the browser's replies and events in the order the
    // browser sent them. Playwright registers listeners while handling some
    // replies, so a reply that overtakes or trails its surrounding events
    // loses them (for example, the main execution context).
    let mut browser = client.subscribe_raw();
    let replies = forwarded.clone();
    let browser_output = outgoing.clone();
    let connection = client.clone();
    tasks.spawn(async move {
        loop {
            let message = tokio::select! {
                biased;
                message = browser.recv() => message
                    .map_err(|_| "The Playwright protocol event stream was interrupted.".to_string())?,
                _ = connection.closed() => {
                    return Err("The browser connection closed.".to_string());
                }
            };
            let mut value: Value =
                serde_json::from_str(&message.text).map_err(|error| error.to_string())?;
            let text = match value.get("id") {
                None => message.text,
                Some(id) => {
                    let program = id
                        .as_u64()
                        .and_then(|id| replies.lock().unwrap().remove(&id));
                    // Replies to the operation's own measurement commands
                    // are not the program's.
                    let Some(program) = program else { continue };
                    value["id"] = program;
                    value.to_string()
                }
            };
            browser_output
                .send(Message::Text(text))
                .map_err(|_| "Playwright transport closed".to_string())?;
        }
    });
    // One input worker preserves wire order even when Playwright pipelines a
    // down/up pair. Other protocol replies and dialog events remain concurrent.
    let input_client = client.clone();
    let input_output = outgoing.clone();
    let input_stop = stop.clone();
    let mut input_worker = InputWorker(tokio::spawn(async move {
        while let Some(command) = input_requests.recv().await {
            if *input_stop.borrow() {
                break;
            }
            let response = input_response(&command, &input_client, &control).await;
            if input_output
                .send(Message::Text(response.to_string()))
                .is_err()
            {
                break;
            }
        }
    }));
    // Acknowledgments of program commands already written to the browser.
    let mut acknowledgments = JoinSet::new();
    let outcome: Result<(), String> = loop {
        tokio::select! {
            biased;
            _ = stop.changed() => break Ok(()),
            completed = tasks.join_next(), if !tasks.is_empty() => {
                if !matches!(completed, Some(Ok(Ok(())))) { break Err("The Playwright protocol transport failed.".into()); }
            }
            completed = acknowledgments.join_next(), if !acknowledgments.is_empty() => {
                if !matches!(completed, Some(Ok(Ok(())))) { break Err("The Playwright protocol transport failed.".into()); }
            }
            // A full window pauses reading the program's socket; the program
            // waits as it would for a slow browser.
            message = reader.next(), if acknowledgments.len() < MAX_PENDING_COMMANDS => {
                let text = match message {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Close(_))) | None => break Ok(()),
                    Some(Ok(_)) => continue,
                    Some(Err(_)) => break Ok(()),
                };
                let command: Value = match serde_json::from_str(&text) {
                    Ok(command) => command,
                    Err(_) => break Err("Invalid Playwright protocol message.".into()),
                };
                let Some(method) = command["method"].as_str() else { break Err("Missing Playwright protocol method.".into()); };
                if is_input(method) {
                    let queued = tokio::select! {
                        biased;
                        _ = stop.changed() => break Ok(()),
                        queued = inputs.send(command) => queued,
                    };
                    if queued.is_err() { break Err("The native input owner stopped.".into()); }
                } else {
                    let Some(program) = command.get("id").cloned() else { break Err("Missing Playwright protocol id.".into()); };
                    let id = client.reserve_command_id();
                    forwarded.lock().unwrap().insert(id, program.clone());
                    // Written here, one at a time, in the program's order.
                    // Chrome runs a session's commands in arrival order and
                    // Playwright pipelines on that (Runtime.enable before
                    // Runtime.runIfWaitingForDebugger); only the wait for
                    // each acknowledgment is concurrent.
                    let sent = tokio::select! {
                        biased;
                        _ = stop.changed() => break Ok(()),
                        sent = client.enqueue_reserved_command(
                            id,
                            method,
                            command.get("params").cloned(),
                            command["sessionId"].as_str(),
                        ) => sent,
                    };
                    match sent {
                        // Held until the reply so the connection's own reply
                        // effects still apply; the ordered reader forwards it.
                        Ok(mut pending) => {
                            acknowledgments.spawn(async move {
                                pending
                                    .acknowledgment()
                                    .await
                                    .map(|_| ())
                                    .map_err(|_| "The browser connection closed.".to_string())
                            });
                        }
                        Err(error) => {
                            if forwarded.lock().unwrap().remove(&id).is_none() {
                                continue;
                            }
                            let mut reply = json!({ "id": program, "error": { "code": -32000, "message": error } });
                            if let Some(session) = command.get("sessionId") {
                                reply["sessionId"] = session.clone();
                            }
                            if outgoing.send(Message::Text(reply.to_string())).is_err() {
                                break Err("Playwright transport closed".into());
                            }
                        }
                    }
                }
            }
        }
    };
    stopped.send_replace(true);
    drop(inputs);
    // A socket close is also final. Do not replay queued input or cancel a
    // display RPC whose outcome the native owner is still observing.
    match tokio::time::timeout(Duration::from_secs(5), &mut input_worker.0).await {
        Ok(_) => {}
        Err(_) => {
            input_worker.0.abort();
            let _ = (&mut input_worker.0).await;
            acknowledgments.shutdown().await;
            tasks.shutdown().await;
            return Err(
                "The native input owner did not settle before Playwright disconnect.".into(),
            );
        }
    }
    acknowledgments.shutdown().await;
    tasks.shutdown().await;
    outcome
}

async fn input_response(
    command: &Value,
    client: &CdpClient,
    control: &Mutex<BrowserControl>,
) -> Value {
    let mut response = json!({ "id": command["id"] });
    if let Some(session) = command.get("sessionId") {
        response["sessionId"] = session.clone();
    }
    let result = native_input(command, client, control).await;
    match result {
        Ok(()) => response["result"] = json!({}),
        Err(error) => {
            response["error"] = json!({ "code": -32000, "message": error });
        }
    }
    response
}

async fn native_input(
    command: &Value,
    client: &CdpClient,
    control: &Mutex<BrowserControl>,
) -> Result<(), String> {
    let method = command["method"].as_str().unwrap();
    let session = command["sessionId"]
        .as_str()
        .ok_or("Playwright input has no page session")?;
    let params = command.get("params").cloned().unwrap_or_else(|| json!({}));
    let mut control = control.lock().await;
    if let Some(error) = control.agent_error() {
        return Err(format!("{}: {}", error.code, error.message));
    }
    if control.has_native_display() && method != "Input.dispatchTouchEvent" {
        let target = client
            .send_command_no_params("Target.getTargetInfo", Some(session))
            .await?;
        if target["targetInfo"]["type"] == "page" {
            client
                .send_command(
                    "Target.activateTarget",
                    Some(json!({ "targetId": target["targetInfo"]["targetId"] })),
                    None,
                )
                .await?;
        }
        match method {
            "Input.dispatchMouseEvent" => {
                // A new unheld gesture remeasures its page/window mapping;
                // held gestures keep the native owner's established mapping.
                control.begin_agent_command();
                control.agent_native_mouse(native_mouse_params(&params), client, session, &[session]).await?;
            }
            "Input.insertText" => control.agent_native_keys(&[json!({ "type": "input_keyboard", "eventType": "insertText", "text": params["text"] })]).await?,
            "Input.dispatchKeyEvent" => {
                let event = native_keyboard_event(&params);
                // CDP commands are browser editing commands, not key names.
                // Keep composition/IME traffic on its original protocol path.
                if params.get("commands").is_some_and(|commands| commands.as_array().is_some_and(|values| !values.is_empty())) {
                    client.send_command(method, Some(params), Some(session)).await?;
                } else {
                    control.agent_native_keys(&[event]).await?;
                }
            }
            _ => unreachable!(),
        }
    } else {
        let kind = match method {
            "Input.dispatchMouseEvent" => "input_mouse",
            "Input.dispatchKeyEvent" => "input_keyboard",
            "Input.dispatchTouchEvent" => "input_touch",
            _ => {
                client
                    .send_command(method, Some(params), Some(session))
                    .await?;
                return Ok(());
            }
        };
        control.agent_input(kind, params, client, session).await?;
    }
    Ok(())
}

fn native_mouse_params(params: &Value) -> Value {
    let mut value = json!({});
    for field in [
        "type",
        "x",
        "y",
        "button",
        "buttons",
        "modifiers",
        "clickCount",
        "deltaX",
        "deltaY",
    ] {
        if let Some(item) = params.get(field) {
            value[field] = item.clone();
        }
    }
    value
}

fn native_keyboard_event(params: &Value) -> Value {
    let mut value = json!({ "type": "input_keyboard", "eventType": params["type"], "modifiers": params["modifiers"].as_i64().unwrap_or(0) });
    for field in ["key", "code", "text", "windowsVirtualKeyCode"] {
        if let Some(item) = params.get(field) {
            value[field] = item.clone();
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_real_input_commands_use_the_native_input_owner() {
        assert!(is_input("Input.dispatchMouseEvent"));
        assert!(is_input("Input.insertText"));
        assert!(!is_input("Runtime.evaluate"));
        assert!(!is_input("Runtime.callFunctionOn"));
        assert!(!is_input("Input.imeSetComposition"));
    }

    #[test]
    fn native_projection_preserves_input_and_excludes_protocol_only_fields() {
        assert_eq!(
            native_mouse_params(
                &json!({"type":"mousePressed","x":5,"y":8,"force":0.5,"button":"left"})
            ),
            json!({"type":"mousePressed","x":5,"y":8,"button":"left"})
        );
        assert_eq!(
            native_keyboard_event(
                &json!({"type":"keyDown","key":"A","code":"KeyA","text":"A","modifiers":8,"commands":[],"autoRepeat":false,"location":0,"unmodifiedText":"A","windowsVirtualKeyCode":65})
            ),
            json!({"type":"input_keyboard","eventType":"keyDown","key":"A","code":"KeyA","text":"A","modifiers":8,"windowsVirtualKeyCode":65})
        );
    }
    /// Playwright pipelines commands and relies on Chrome running them in
    /// the order sent, and registers context listeners while handling a
    /// reply, so a reply must never overtake or trail events the browser
    /// sent around it. The daemon runs on a multi-threaded runtime; so does
    /// this test. The browser here answers only once the program has paused,
    /// so more commands are outstanding than the transport keeps in flight:
    /// the program is slowed down, never failed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn program_commands_reach_the_browser_and_replies_return_in_order() {
        const COMMANDS: u64 = 3 * MAX_PENDING_COMMANDS as u64;
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = upstream.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = upstream.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
            let mut arrived = Vec::new();
            let mut unanswered = Vec::new();
            let mut most_outstanding = 0;
            loop {
                let next = tokio::time::timeout(Duration::from_millis(50), socket.next()).await;
                let text = match next {
                    Ok(Some(Ok(Message::Text(text)))) => Some(text),
                    Ok(Some(Ok(_))) => continue,
                    Ok(_) => break,
                    Err(_) => None,
                };
                if let Some(text) = text {
                    let command: Value = serde_json::from_str(&text).unwrap();
                    arrived.push(command["params"]["n"].as_u64().unwrap());
                    unanswered.push(command);
                    most_outstanding = most_outstanding.max(unanswered.len());
                    continue;
                }
                for command in unanswered.drain(..) {
                    let n = &command["params"]["n"];
                    for message in [
                        json!({"method":"Test.before","params":{"n":n},"sessionId":"S"}),
                        json!({"id":command["id"],"result":{"n":n},"sessionId":"S"}),
                        json!({"method":"Test.after","params":{"n":n},"sessionId":"S"}),
                    ] {
                        socket
                            .send(Message::Text(message.to_string()))
                            .await
                            .unwrap();
                    }
                }
                if arrived.len() as u64 == COMMANDS {
                    break;
                }
            }
            // The browser stays connected until the operation ends.
            while socket.next().await.is_some() {}
            (arrived, most_outstanding)
        });
        let client = Arc::new(
            CdpClient::connect(&format!("ws://{address}"))
                .await
                .unwrap(),
        );
        let mut tunnel = Tunnel::start(client, Arc::new(Mutex::new(BrowserControl::default())))
            .await
            .unwrap();
        let (program, _) = tokio_tungstenite::connect_async(tunnel.endpoint())
            .await
            .unwrap();
        let (mut program_tx, mut program_rx) = program.split();
        let sender = tokio::spawn(async move {
            for n in 0..COMMANDS {
                let command =
                    json!({"id":1000 + n,"method":"Test.command","params":{"n":n},"sessionId":"S"});
                program_tx
                    .send(Message::Text(command.to_string()))
                    .await
                    .unwrap();
            }
            program_tx
        });
        let mut received = Vec::new();
        while received.len() < 3 * COMMANDS as usize {
            let message = tokio::time::timeout(Duration::from_secs(10), program_rx.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if let Message::Text(text) = message {
                received.push(serde_json::from_str::<Value>(&text).unwrap());
            }
        }
        let expected: Vec<Value> = (0..COMMANDS)
            .flat_map(|n| {
                [
                    json!({"method":"Test.before","params":{"n":n},"sessionId":"S"}),
                    json!({"id":1000 + n,"result":{"n":n},"sessionId":"S"}),
                    json!({"method":"Test.after","params":{"n":n},"sessionId":"S"}),
                ]
            })
            .collect();
        assert_eq!(received, expected);
        let mut program = sender.await.unwrap().reunite(program_rx).unwrap();
        program.close(None).await.unwrap();
        tunnel.finish().await.unwrap();
        drop(tunnel);
        let (arrived, most_outstanding) = server.await.unwrap();
        assert_eq!(arrived, (0..COMMANDS).collect::<Vec<_>>());
        assert!((1..=MAX_PENDING_COMMANDS).contains(&most_outstanding));
    }

    /// A program's coordinates are page geometry it read earlier. A layout
    /// that lands while the program runs (a person dragging the dock) is not
    /// proven until the next command, so each later tunnel mouse event is
    /// refused before any pre-hover or native input, instead of starting
    /// under the new layout; once proven, the same event goes through.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_layout_during_a_program_refuses_its_later_mouse_events() {
        use crate::native::display::DisplayClient;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = upstream.local_addr().unwrap();
        let (reached, mut page) = mpsc::unbounded_channel::<String>();
        let server = tokio::spawn(async move {
            let (socket, _) = upstream.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
            while let Some(Ok(message)) = socket.next().await {
                let Message::Text(text) = message else {
                    continue;
                };
                let command: Value = serde_json::from_str(&text).unwrap();
                let method = command["method"].as_str().unwrap().to_owned();
                let reply = match method.as_str() {
                    "Target.getTargetInfo" => json!({"id":command["id"],
                        "result":{"targetInfo":{"type":"page","targetId":"T"}}}),
                    "Target.activateTarget" => json!({"id":command["id"],"result":{}}),
                    _ => {
                        let _ = reached.send(method);
                        json!({"id":command["id"],"error":{"code":-32000,"message":"not in this test"}})
                    }
                };
                socket.send(Message::Text(reply.to_string())).await.unwrap();
            }
        });
        let client = CdpClient::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        let (display, helper, _frames) = DisplayClient::test_channel();
        let mut helper = BufReader::new(helper);
        let control = Mutex::new(BrowserControl::default());
        control.lock().await.set_display(Some(display.clone()));
        // The run_playwright command proved the page before the program.
        display.record_proof(display.layout_epoch(), false);
        control.lock().await.begin_agent_command();

        let owner = display.clone();
        let resized = tokio::spawn(async move {
            let layout = owner.layout().await;
            owner
                .resize(&layout, 1560, 1200, Some(7), false)
                .await
                .map(|_| ())
        });
        let mut line = String::new();
        helper.read_line(&mut line).await.unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["op"], "resize");
        let reply = json!({"id":request["id"],"success":true,"data":{"width":1560,"height":1200,"windows":[]}});
        helper
            .get_mut()
            .write_all(format!("{reply}\n").as_bytes())
            .await
            .unwrap();
        resized.await.unwrap().unwrap();

        // `page.mouse.click` at a point read before the layout.
        let moved = json!({"id":3,"sessionId":"S","method":"Input.dispatchMouseEvent",
            "params":{"type":"mouseMoved","x":605,"y":105,"button":"none"}});
        for _ in 0..2 {
            let error = native_input(&moved, &client, &control).await.unwrap_err();
            assert!(error.contains("was resized"), "{error}");
        }
        assert!(page.try_recv().is_err(), "no pre-hover reached the page");
        line.clear();
        let quiet =
            tokio::time::timeout(Duration::from_millis(100), helper.read_line(&mut line)).await;
        assert!(quiet.is_err(), "no native input was sent: {line}");

        // The next command's proof lets the same event through: it measures
        // the page (the helper's window info, then a CDP pre-hover).
        display.record_proof(display.layout_epoch(), false);
        let helper_side = async {
            line.clear();
            helper.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["op"], "info");
            let info = json!({"id":request["id"],"success":true,"data":{"width":1560,"height":1200,
                "windows":[{"id":7,"pid":1,"x":0,"y":0,"width":1560,"height":1200,"mapped":true,
                "focused":true,"overrideRedirect":false,"windowType":"normal"}]}});
            helper
                .get_mut()
                .write_all(format!("{info}\n").as_bytes())
                .await
                .unwrap();
        };
        let (measured, ()) = tokio::join!(native_input(&moved, &client, &control), helper_side);
        assert!(!measured.unwrap_err().contains("was resized"));
        assert_eq!(page.recv().await.unwrap(), "Input.dispatchMouseEvent");
        server.abort();
    }

    #[tokio::test]
    async fn browser_origins_and_foreign_paths_cannot_consume_the_native_connection() {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = upstream.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = upstream.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
            while socket.next().await.is_some() {}
        });
        let client = Arc::new(
            CdpClient::connect(&format!("ws://{address}"))
                .await
                .unwrap(),
        );
        let mut tunnel = Tunnel::start(client, Arc::new(Mutex::new(BrowserControl::default())))
            .await
            .unwrap();
        let mut foreign = tunnel.endpoint().into_client_request().unwrap();
        foreign
            .headers_mut()
            .insert("Origin", "https://untrusted.example".parse().unwrap());
        assert!(tokio_tungstenite::connect_async(foreign).await.is_err());
        let mut other_path = url::Url::parse(tunnel.endpoint()).unwrap();
        other_path.set_path("/another-operation");
        assert!(tokio_tungstenite::connect_async(other_path.as_str())
            .await
            .is_err());
        let (mut native, _) = tokio_tungstenite::connect_async(tunnel.endpoint())
            .await
            .unwrap();
        native.close(None).await.unwrap();
        tunnel.finish().await.unwrap();
        drop(tunnel);
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap();
    }
}
