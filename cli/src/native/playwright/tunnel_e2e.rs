//! What one command costs a Playwright program through the tunnel to a real
//! Chrome, timed from the program's send to the reply it reads: an
//! evaluation of a constant, an evaluation whose reply follows a console
//! event (the tunnel writes the event, then the reply), and a browser
//! command sent while a 30 ms evaluation is still pending. The program's own
//! socket sends each write at once, as Node's `ws` does, so only the
//! tunnel's side is measured. Prints `TUNNEL <json>`; with
//! `$AMBIT_TUNNEL_PROOF` set to a file, writes the numbers there.
//!
//! `cargo test --profile ci e2e_tunnel_round_trips -- --ignored` with
//! `AGENT_BROWSER_EXECUTABLE_PATH` set.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::transport::Tunnel;
use crate::native::actions::{execute_command, DaemonState};
use crate::native::cdp::client::CdpClient;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A program on the tunnel: its commands, and the replies it reads, by id.
struct Program {
    sender: SplitSink<Socket, Message>,
    receiver: SplitStream<Socket>,
    next: u64,
}

impl Program {
    async fn send(&mut self, method: &str, params: Value, session: Option<&str>) -> u64 {
        self.next += 1;
        let mut command = json!({"id": self.next, "method": method, "params": params});
        if let Some(session) = session {
            command["sessionId"] = json!(session);
        }
        self.sender
            .send(Message::Text(command.to_string()))
            .await
            .unwrap();
        self.next
    }

    /// Reads until the reply to `id`, passing events and other replies.
    async fn reply(&mut self, id: u64) -> Value {
        loop {
            let message = tokio::time::timeout(Duration::from_secs(10), self.receiver.next())
                .await
                .expect("a reply within 10 s")
                .unwrap()
                .unwrap();
            if let Message::Text(text) = message {
                let value: Value = serde_json::from_str(&text).unwrap();
                if value["id"] == id {
                    assert!(value.get("error").is_none(), "{value}");
                    return value;
                }
            }
        }
    }

    async fn call(&mut self, method: &str, params: Value, session: Option<&str>) -> (Value, f64) {
        let started = Instant::now();
        let id = self.send(method, params, session).await;
        let reply = self.reply(id).await;
        (reply, started.elapsed().as_secs_f64() * 1000.0)
    }
}

fn stats(mut values: Vec<f64>) -> Value {
    values.sort_by(f64::total_cmp);
    let at = |share: f64| values[((values.len() - 1) as f64 * share).round() as usize];
    let round = |value: f64| (value * 1000.0).round() / 1000.0;
    json!({"n": values.len(), "p50": round(at(0.5)), "p95": round(at(0.95)),
        "max": round(values[values.len() - 1])})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_tunnel_round_trips() {
    let mut state = DaemonState::new();
    for command in [
        json!({"id":"1","action":"launch","headless":true}),
        json!({"action":"navigate","url":"data:text/html,<title>Tunnel</title>"}),
    ] {
        let response = Box::pin(execute_command(&command, &mut state)).await;
        assert_eq!(response["success"], true, "{response}");
    }
    let endpoint = state.browser.as_ref().unwrap().get_cdp_url().to_owned();
    let client = Arc::new(CdpClient::connect(&endpoint).await.unwrap());
    let mut tunnel = Tunnel::start(client, state.browser_control.clone())
        .await
        .unwrap();
    let (socket, _) = tokio_tungstenite::connect_async_with_config(tunnel.endpoint(), None, true)
        .await
        .unwrap();
    let (sender, receiver) = socket.split();
    let mut program = Program {
        sender,
        receiver,
        next: 0,
    };
    let (targets, _) = program.call("Target.getTargets", json!({}), None).await;
    let page = targets["result"]["targetInfos"]
        .as_array()
        .unwrap()
        .iter()
        .find(|target| target["type"] == "page")
        .unwrap()["targetId"]
        .clone();
    let (attached, _) = program
        .call(
            "Target.attachToTarget",
            json!({"targetId": page, "flatten": true}),
            None,
        )
        .await;
    let session = attached["result"]["sessionId"].as_str().unwrap().to_owned();
    let session = Some(session.as_str());
    let constant = json!({"expression": "1", "returnByValue": true});
    for _ in 0..20 {
        program
            .call("Runtime.evaluate", constant.clone(), session)
            .await;
    }
    let mut evaluations = Vec::new();
    for _ in 0..200 {
        evaluations.push(
            program
                .call("Runtime.evaluate", constant.clone(), session)
                .await
                .1,
        );
    }
    program.call("Runtime.enable", json!({}), session).await;
    let logged = json!({"expression": "console.log(1), 1", "returnByValue": true});
    let mut after_event = Vec::new();
    for _ in 0..50 {
        after_event.push(
            program
                .call("Runtime.evaluate", logged.clone(), session)
                .await
                .1,
        );
    }
    let busy = json!({"returnByValue": true, "expression":
        "(() => { const t = performance.now(); while (performance.now() - t < 30) {} return 1 })()"});
    let mut behind = Vec::new();
    for _ in 0..50 {
        let pending = program
            .send("Runtime.evaluate", busy.clone(), session)
            .await;
        tokio::time::sleep(Duration::from_millis(2)).await;
        behind.push(program.call("Browser.getVersion", json!({}), None).await.1);
        program.reply(pending).await;
    }
    let report = json!({
        "evaluate": stats(evaluations),
        "evaluateAfterAConsoleEvent": stats(after_event),
        "behindAPendingCommand": stats(behind),
    });
    println!("TUNNEL {report}");
    if let Ok(path) = std::env::var("AMBIT_TUNNEL_PROOF") {
        std::fs::write(path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    }
    let mut socket = program.sender.reunite(program.receiver).unwrap();
    socket.close(None).await.unwrap();
    tunnel.finish().await.unwrap();
    Box::pin(execute_command(&json!({"action":"close"}), &mut state)).await;
}
