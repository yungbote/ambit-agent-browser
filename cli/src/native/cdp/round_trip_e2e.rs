//! What one command costs on the driver's CDP connection to a real Chrome:
//! a trivial evaluation and an input dispatch, one at a time, and
//! evaluations twenty at once, each timed from its send to its reply.
//! Prints `CDP <json>`; with `$AMBIT_CDP_PROOF` set to a file, writes the
//! numbers there.
//!
//! `cargo test --profile ci e2e_cdp_round_trips -- --ignored` with
//! `AGENT_BROWSER_EXECUTABLE_PATH` set.

use std::time::Instant;

use serde_json::{json, Value};

use crate::native::actions::{execute_command, DaemonState};

fn stats(mut values: Vec<f64>) -> Value {
    values.sort_by(f64::total_cmp);
    let at = |share: f64| values[((values.len() - 1) as f64 * share).round() as usize];
    let round = |value: f64| (value * 1000.0).round() / 1000.0;
    json!({"n": values.len(), "p50": round(at(0.5)), "p95": round(at(0.95)),
        "max": round(values[values.len() - 1])})
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_cdp_round_trips() {
    let mut state = DaemonState::new();
    for command in [
        json!({"id":"1","action":"launch","headless":true}),
        json!({"action":"navigate","url":"data:text/html,<title>Round trip</title>"}),
    ] {
        let response = Box::pin(execute_command(&command, &mut state)).await;
        assert_eq!(response["success"], true, "{response}");
    }
    let browser = state.browser.as_ref().unwrap();
    let client = browser.client.clone();
    let session = browser.active_session_id().unwrap().to_string();
    let evaluate = json!({"expression": "1", "returnByValue": true});
    let evaluation = || {
        let (client, session, evaluate) = (client.clone(), session.clone(), evaluate.clone());
        async move {
            let started = Instant::now();
            client
                .send_command("Runtime.evaluate", Some(evaluate), Some(&session))
                .await
                .unwrap();
            elapsed_ms(started)
        }
    };
    for _ in 0..20 {
        evaluation().await;
    }
    let mut evaluations = Vec::new();
    for _ in 0..200 {
        evaluations.push(evaluation().await);
    }
    let mut dispatches = Vec::new();
    for index in 0..200u32 {
        let started = Instant::now();
        let mouse = json!({"type": "mouseMoved", "x": 100 + index % 200, "y": 100});
        client
            .send_command("Input.dispatchMouseEvent", Some(mouse), Some(&session))
            .await
            .unwrap();
        dispatches.push(elapsed_ms(started));
    }
    let mut together = Vec::new();
    for _ in 0..10 {
        together.extend(futures_util::future::join_all((0..20).map(|_| evaluation())).await);
    }
    // A command written while another is still unanswered: the page is busy
    // for 30 ms, and the browser process answers `Browser.getVersion` at once
    // unless the write itself was held back.
    let busy = json!({"returnByValue": true, "expression":
        "(() => { const t = performance.now(); while (performance.now() - t < 30) {} return 1 })()"});
    let mut behind = Vec::new();
    for _ in 0..50 {
        let pending = {
            let (client, session, busy) = (client.clone(), session.clone(), busy.clone());
            tokio::spawn(async move {
                client
                    .send_command("Runtime.evaluate", Some(busy), Some(&session))
                    .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let started = Instant::now();
        client
            .send_command("Browser.getVersion", None, None)
            .await
            .unwrap();
        behind.push(elapsed_ms(started));
        pending.await.unwrap().unwrap();
    }
    let report = json!({
        "evaluate": stats(evaluations),
        "dispatchMouseEvent": stats(dispatches),
        "evaluateTwentyAtOnce": stats(together),
        "behindAPendingCommand": stats(behind),
    });
    println!("CDP {report}");
    if let Ok(path) = std::env::var("AMBIT_CDP_PROOF") {
        std::fs::write(path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    }
    Box::pin(execute_command(&json!({"action":"close"}), &mut state)).await;
}
