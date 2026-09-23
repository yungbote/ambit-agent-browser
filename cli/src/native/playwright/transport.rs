//! Operation-local CDP transport. Playwright keeps its complete protocol and
//! session identities. Actual input alone joins the existing native owner;
//! DOM evaluation never manufactures pointer movement or user-input events.

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
    let (outgoing, mut replies) = mpsc::channel::<Message>(256);
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
    let mut events = client.subscribe_raw();
    let event_output = outgoing.clone();
    tasks.spawn(async move {
        loop {
            let event = events
                .recv()
                .await
                .map_err(|_| "The Playwright protocol event stream was interrupted.".to_string())?;
            let value: Value =
                serde_json::from_str(&event.text).map_err(|error| error.to_string())?;
            if value.get("id").is_none() {
                event_output
                    .send(Message::Text(event.text))
                    .await
                    .map_err(|_| "Playwright transport closed".to_string())?;
            }
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
                .await
                .is_err()
            {
                break;
            }
        }
    }));
    let outcome = loop {
        tokio::select! {
            biased;
            _ = stop.changed() => break Ok(()),
            completed = tasks.join_next(), if !tasks.is_empty() => {
                if !matches!(completed, Some(Ok(Ok(())))) { break Err("The Playwright protocol transport failed.".into()); }
            }
            message = reader.next() => {
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
                    if inputs.try_send(command).is_err() { break Err("Too many pending Playwright input events.".into()); }
                } else {
                    if tasks.len() >= 258 { break Err("Too many pending Playwright protocol commands.".into()); }
                    let client = client.clone();
                    let outgoing = outgoing.clone();
                    tasks.spawn(async move {
                        let response = protocol_response(&command, &client).await;
                        outgoing.send(Message::Text(response.to_string())).await.map_err(|_| "Playwright transport closed".to_string())
                    });
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
            tasks.shutdown().await;
            return Err(
                "The native input owner did not settle before Playwright disconnect.".into(),
            );
        }
    }
    tasks.shutdown().await;
    outcome
}

async fn protocol_response(command: &Value, client: &CdpClient) -> Value {
    let mut response = json!({ "id": command["id"] });
    if let Some(session) = command.get("sessionId") {
        response["sessionId"] = session.clone();
    }
    let result = async {
        let mut pending = client
            .enqueue_command(
                command["method"].as_str().unwrap(),
                command.get("params").cloned(),
                command["sessionId"].as_str(),
            )
            .await?;
        pending.acknowledgment().await
    }
    .await;
    match result {
        Ok(message) => {
            if let Some(error) = message.error {
                response["error"] =
                    json!({ "code": error.code.unwrap_or(-32000), "message": error.message });
                if let Some(data) = error.data {
                    response["error"]["data"] = json!(data);
                }
            } else {
                response["result"] = message.result.unwrap_or_else(|| json!({}));
            }
        }
        Err(error) => response["error"] = json!({ "code": -32000, "message": error }),
    }
    response
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
