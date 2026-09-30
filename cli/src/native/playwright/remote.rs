//! The browser-host half of a program: a bounded, private transport lease.
//! Program source and Node outcomes belong to CODE, never to this daemon.

use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, Mutex};
use tokio::time::Instant;

use crate::native::actions::DaemonState;
use crate::native::agent_channel::frame::{ChannelId, Owner};
use crate::native::browser_control::InterruptReason;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ProgramId(uuid::Uuid);

impl ProgramId {
    pub(super) fn parse(value: &str) -> Result<Self, String> {
        let id = uuid::Uuid::parse_str(value)
            .map_err(|_| "programId must be a canonical lower-case UUID.")?;
        if id.is_nil() || id.hyphenated().to_string() != value {
            return Err("programId must be a canonical lower-case UUID.".into());
        }
        Ok(Self(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::actions::execute_command;
    use crate::native::agent_channel::frame::ActionId;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    fn owner(generation: u64) -> Owner {
        Owner {
            action: ActionId::for_test(71),
            generation,
        }
    }
    fn program() -> ProgramId {
        ProgramId(uuid::Uuid::new_v4())
    }
    fn channel() -> ChannelId {
        ChannelId::parse(&uuid::Uuid::new_v4().to_string()).unwrap()
    }

    #[tokio::test]
    async fn queued_open_keeps_metadata_available_and_reproves_the_action_fence() {
        let programs = Programs::default();
        let state = Arc::new(Mutex::new(DaemonState::new()));
        let ledger = crate::native::agent_channel::ledger::Ledger::default();
        let origin = channel();
        let successor = channel();
        assert!(ledger.open(origin));
        ledger.register(origin, owner(1)).unwrap();
        let held = state.lock().await;
        let opening = programs.request_current(
            &state,
            origin,
            owner(1),
            Request::Open {
                program: program(),
                timeout_ms: 1000,
                target: None,
            },
            Some(&ledger),
        );
        tokio::pin!(opening);
        tokio::select! {
            biased;
            _=&mut opening=>panic!("open cannot acquire command custody yet"),
            _=tokio::task::yield_now()=>{},
        }
        assert!(
            programs.leases.try_lock().is_ok(),
            "queued open must not prevent status/close"
        );
        assert!(ledger.open(successor));
        ledger.register(successor, owner(2)).unwrap();
        drop(held);
        let error = opening.await.unwrap_err();
        assert!(error.starts_with("agent_channel_fenced:"), "{error}");
        assert!(state.lock().await.browser.is_none());
        assert!(!state.lock().await.playwright_operations.active());
    }

    #[tokio::test]
    async fn forgotten_receipts_never_restart_their_program_id() {
        let programs = Programs::default();
        let oldest = program();
        let origin = channel();
        for id in std::iter::once(oldest).chain((0..256).map(|_| program())) {
            let (stop, _) = watch::channel(None);
            let (status, _) = watch::channel(
                json!({"programId":id.to_string(),"state":"closed","reason":"complete","nativeInputSettled":true}),
            );
            let lease = Arc::new(Lease {
                program: id,
                channel: origin,
                owner: owner(1),
                timeout_ms: 1000,
                selected: None,
                deadline: Instant::now(),
                opened: json!({}),
                stop,
                status,
                files: super::super::files::StagedFiles::default(),
            });
            let mut leases = programs.leases.lock().await;
            while leases.len() >= 256 {
                leases.pop_front();
            }
            leases.push_back(lease);
            programs.seen.lock().unwrap().insert(id);
        }
        assert_eq!(programs.leases.lock().await.len(), 256);
        assert_eq!(programs.seen.lock().unwrap().len(), 257);
        assert!(!programs
            .leases
            .lock()
            .await
            .iter()
            .any(|lease| lease.program == oldest));
        let state = Arc::new(Mutex::new(DaemonState::new()));
        let error = programs
            .request(
                &state,
                origin,
                owner(1),
                Request::Open {
                    program: oldest,
                    timeout_ms: 1000,
                    target: None,
                },
            )
            .await
            .unwrap_err();
        assert!(error.contains("not restarted"));
        assert!(!state.lock().await.playwright_operations.active());
        assert!(state.lock().await.browser.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore]
    async fn e2e_raw_debugger_rejects_site_file_and_opaque_page_origins() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let site = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut input = [0u8; 2048];
                let _ = socket.read(&mut input).await;
                let body = "<title>Raw debugger boundary fixture</title>";
                let reply=format!("HTTP/1.1 200 OK\r\nContent-Type:text/html\r\nContent-Length:{}\r\nConnection:close\r\n\r\n{body}",body.len());
                let _ = socket.write_all(reply.as_bytes()).await;
            }
        });
        let fixture = tempfile::tempdir().unwrap();
        let file = fixture.path().join("opaque.html");
        std::fs::write(&file, "<title>Opaque fixture</title>").unwrap();
        let mut state = DaemonState::new();
        let launched = Box::pin(execute_command(
            &json!({"action":"launch","headless":true}),
            &mut state,
        ))
        .await;
        assert_eq!(launched["success"], true, "{launched}");
        let endpoint = state.browser.as_ref().unwrap().get_cdp_url().to_owned();
        let port = url::Url::parse(&endpoint).unwrap().port().unwrap();
        let client = &state.browser.as_ref().unwrap().client;
        for host in [
            "127.0.0.1",
            "127.1",
            "2130706433",
            "0x7f000001",
            "localhost",
            "localhost.",
            "[::1]",
            "[::ffff:127.0.0.1]",
        ] {
            assert!(
                client.debugger_resource(&format!("http://{host}:{port}/json/new")),
                "normalized debugger alias {host}"
            );
        }
        assert!(
            !client.debugger_resource(&site),
            "an unrelated fixture service stays available"
        );
        assert!(
            !client.debugger_resource(&format!("https://example.com:{port}/")),
            "an unrelated origin using the same numeric port stays available"
        );
        assert!(
            !client.debugger_resource(&format!("http://127.0.0.1@example.com:{port}/")),
            "user-info is not the request host"
        );
        let discovery = format!("http://127.0.0.1:{port}/json/version");
        let expression = format!(
            r#"(async()=>{{const readable=await fetch({}).then(r=>r.text()).then(text=>text.includes('webSocketDebuggerUrl')).catch(()=>false);const opened=await new Promise(resolve=>{{const socket=new WebSocket({});socket.onopen=()=>{{socket.close();resolve(true)}};socket.onerror=()=>resolve(false)}});return {{readable,opened}}}})()"#,
            serde_json::to_string(&discovery).unwrap(),
            serde_json::to_string(&endpoint).unwrap()
        );
        let mut results = Vec::new();
        for (origin, url) in [
            ("site", site),
            ("opaque", "data:text/html,<title>Opaque</title>".into()),
            ("file", url::Url::from_file_path(&file).unwrap().to_string()),
            ("debugger", discovery),
        ] {
            let navigation = Box::pin(execute_command(
                &json!({"action":"navigate","url":url}),
                &mut state,
            ))
            .await;
            assert_eq!(navigation["success"], true, "{navigation}");
            let browser = state.browser.as_ref().unwrap();
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                browser.client.send_command(
                    "Runtime.evaluate",
                    Some(json!({"expression":expression,"returnByValue":true,"awaitPromise":true})),
                    Some(browser.active_session_id().unwrap()),
                ),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(result.get("exceptionDetails").is_none(), "{result}");
            results.push((origin, result["result"]["value"].clone()));
        }
        // The last page has the debugger's HTTP origin. A writable HTTP
        // endpoint would be another way to open an internal UI target, even
        // though this page's raw WebSocket was refused.
        let browser = state.browser.as_ref().unwrap();
        let expression="fetch('/json/new?chrome://version',{method:'PUT'}).then(async r=>({ok:r.ok,body:await r.json()})).catch(()=>({ok:false}))";
        browser.client.site_context().files.guard();
        browser
            .client
            .send_command(
                "Fetch.enable",
                Some(json!({"patterns":[{"urlPattern":"*","requestStage":"Request"}]})),
                Some(browser.active_session_id().unwrap()),
            )
            .await
            .unwrap();
        let created = browser
            .client
            .send_command(
                "Runtime.evaluate",
                Some(json!({"expression":expression,"awaitPromise":true,"returnByValue":true})),
                Some(browser.active_session_id().unwrap()),
            )
            .await
            .unwrap();
        let value = &created["result"]["value"];
        let created_ui =
            value["ok"] == true && value.pointer("/body/id").and_then(Value::as_str).is_some();
        println!("DEBUGGER_HTTP_UI {}", json!({"created":created_ui}));
        if let Some(target) = value.pointer("/body/id").and_then(Value::as_str) {
            browser
                .client
                .send_command("Target.closeTarget", Some(json!({"targetId":target})), None)
                .await
                .unwrap();
        }
        let closed = Box::pin(execute_command(&json!({"action":"close"}), &mut state)).await;
        assert_eq!(closed["success"], true, "{closed}");
        server.abort();
        let _ = server.await;
        println!("RAW_DEBUGGER {}", json!(results));
        for (origin, result) in results {
            assert_eq!(
                result["opened"], false,
                "{origin}: no page origin may attach to raw Chrome"
            );
            if origin != "debugger" {
                assert_eq!(
                    result["readable"], false,
                    "{origin}: discovery must not cross CORS"
                );
            }
        }
        assert!(!created_ui,"The model's page must not open browser-interface targets through the debugger HTTP surface");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore]
    async fn e2e_remote_file_request_interception_spike() {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("fixture.html");
        std::fs::write(
            &path,
            "<title>Public staging fixture</title><p>nosecret-file-gate</p>",
        )
        .unwrap();
        let url = url::Url::from_file_path(&path).unwrap().to_string();
        let mut state = DaemonState::new();
        let launched = Box::pin(execute_command(
            &json!({"action":"launch","headless":true}),
            &mut state,
        ))
        .await;
        assert_eq!(launched["success"], true, "{launched}");
        let browser = state.browser.as_ref().unwrap();
        // Use a private connection so the daemon's canonical resolver cannot
        // answer this probe's pause before the probe observes it.
        let client = Arc::new(
            crate::native::cdp::client::CdpClient::connect(browser.get_cdp_url())
                .await
                .unwrap(),
        );
        let target = browser.active_target_id().unwrap();
        let attached = client
            .send_command(
                "Target.attachToTarget",
                Some(json!({"targetId":target,"flatten":true})),
                None,
            )
            .await
            .unwrap();
        let session = attached["sessionId"].as_str().unwrap().to_owned();
        let mut events = client.subscribe();
        client
            .send_command(
                "Fetch.enable",
                Some(json!({"patterns":[{"urlPattern":"*","requestStage":"Request"}]})),
                Some(&session),
            )
            .await
            .unwrap();
        let connection = client.clone();
        let attached = session.clone();
        let target_url = url.clone();
        let navigation = tokio::spawn(async move {
            connection
                .send_command(
                    "Page.navigate",
                    Some(json!({"url":target_url})),
                    Some(&attached),
                )
                .await
        });
        let paused = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let event = events.recv().await.unwrap();
                if event.method == "Fetch.requestPaused" {
                    break event.params;
                }
            }
        })
        .await;
        if let Ok(event) = &paused {
            assert_eq!(
                event.pointer("/request/url").and_then(Value::as_str),
                Some(url.as_str())
            );
            client
                .send_command(
                    "Fetch.continueRequest",
                    Some(json!({"requestId":event["requestId"]})),
                    Some(&session),
                )
                .await
                .unwrap();
        }
        let result = tokio::time::timeout(Duration::from_secs(2), navigation)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        println!(
            "FILE_GATE {}",
            json!({"paused":paused.is_ok(),"navigationError":result.get("errorText")})
        );
        client.disconnect();
        let closed = Box::pin(execute_command(&json!({"action":"close"}), &mut state)).await;
        assert_eq!(closed["success"], true, "{closed}");
        assert!(paused.is_ok(),"Chrome must intercept local-file requests before a request-level staging policy can be trusted");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore]
    async fn e2e_remote_program_transport_lifecycle_and_owner_fences() {
        let mut state = DaemonState::new();
        for command in [
            json!({"action":"launch","headless":true}),
            json!({"action":"navigate","url":"data:text/html,<title>Remote</title><input>"}),
        ] {
            let response = Box::pin(execute_command(&command, &mut state)).await;
            assert_eq!(response["success"], true, "{response}");
        }
        let operations = state.playwright_operations.clone();
        let state = Arc::new(Mutex::new(state));
        let programs = Programs::default();
        let selected = program();
        let origin = channel();
        let opened = programs
            .request(
                &state,
                origin,
                owner(1),
                Request::Open {
                    program: selected,
                    timeout_ms: 10000,
                    target: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(opened["state"], "active");
        assert!(operations.active());
        let command = json!({"action":"get_title"});
        let rejected = Box::pin(execute_command(&command, &mut *state.lock().await)).await;
        assert_eq!(rejected["code"], "browser_operation_rejected", "{rejected}");
        let request = crate::native::feedback::FeedbackRequest {
            namespace: "fixture".into(),
            session: "default".into(),
            capture_directory: std::path::PathBuf::from("/unused-fixture"),
            timeout_ms: 1000,
            expected_observation: None,
            launch: None,
        };
        let rejected = Box::pin(crate::native::actions::run_host_command(
            &command,
            &request,
            &mut *state.lock().await,
            std::time::Instant::now(),
            &mut crate::native::feedback::ImageFence(&request),
        ))
        .await;
        assert_eq!(
            rejected["code"], "browser_operation_rejected",
            "host-bound dispatch must share the slot: {rejected}"
        );
        assert!(opened["remainingTimeoutMs"].as_u64().unwrap() <= 10000);
        assert_eq!(opened["runner"]["source"], super::super::RUNNER);
        assert_eq!(
            opened["runner"]["digest"],
            format!(
                "sha256:{:x}",
                Sha256::digest(super::super::RUNNER.as_bytes())
            )
        );
        let replay = programs
            .request(
                &state,
                origin,
                owner(1),
                Request::Open {
                    program: selected,
                    timeout_ms: 10000,
                    target: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(replay["endpoint"], opened["endpoint"]);
        assert!(programs
            .request(
                &state,
                origin,
                owner(1),
                Request::Open {
                    program: selected,
                    timeout_ms: 9999,
                    target: None
                }
            )
            .await
            .is_err());
        assert!(programs
            .request(
                &state,
                origin,
                owner(1),
                Request::Open {
                    program: program(),
                    timeout_ms: 1000,
                    target: None
                }
            )
            .await
            .is_err());
        let relay_channel = channel();
        let status = programs
            .request(
                &state,
                relay_channel,
                owner(1),
                Request::Status { program: selected },
            )
            .await
            .unwrap();
        assert_eq!(status["endpoint"], opened["endpoint"]);
        programs.end(relay_channel).await;
        assert!(
            operations.active(),
            "status reproof never adopts or cancels original ownership"
        );
        let foreign = Owner {
            action: ActionId::for_test(72),
            generation: 1,
        };
        assert!(programs
            .request(
                &state,
                relay_channel,
                foreign,
                Request::Status { program: selected }
            )
            .await
            .is_err());
        assert!(programs
            .request(
                &state,
                relay_channel,
                foreign,
                Request::Close {
                    program: selected,
                    reason: CloseReason::Cancel
                }
            )
            .await
            .is_err());
        let (mut socket, _) =
            tokio_tungstenite::connect_async(opened["endpoint"].as_str().unwrap())
                .await
                .unwrap();
        socket
            .send(Message::Text(
                json!({"id":1,"method":"Browser.getVersion","params":{}}).to_string(),
            ))
            .await
            .unwrap();
        loop {
            let message = tokio::time::timeout(Duration::from_secs(2), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if let Message::Text(text) = message {
                let reply: Value = serde_json::from_str(&text).unwrap();
                if reply["id"] == 1 {
                    assert!(reply.get("error").is_none(), "{reply}");
                    break;
                }
            }
        }
        programs.fence(owner(2)).await;
        let fenced = programs
            .request(
                &state,
                relay_channel,
                owner(2),
                Request::Status { program: selected },
            )
            .await
            .unwrap();
        assert_eq!(fenced["state"], "closed");
        assert_eq!(fenced["reason"], "shutdown");
        assert_eq!(fenced["nativeInputSettled"], true, "{fenced}");
        assert!(fenced.get("endpoint").is_none());
        assert!(!operations.active());
        let again = programs
            .request(
                &state,
                origin,
                owner(1),
                Request::Open {
                    program: selected,
                    timeout_ms: 10000,
                    target: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(again["state"], "closed", "same id never restarts");
        assert!(again.get("endpoint").is_none());

        let timed = program();
        programs
            .request(
                &state,
                origin,
                owner(2),
                Request::Open {
                    program: timed,
                    timeout_ms: 50,
                    target: None,
                },
            )
            .await
            .unwrap();
        let lease = programs
            .leases
            .lock()
            .await
            .iter()
            .find(|lease| lease.program == timed)
            .cloned()
            .unwrap();
        let timed = tokio::time::timeout(Duration::from_secs(2), lease.closed())
            .await
            .unwrap();
        assert_eq!(timed["reason"], "timeout");
        assert_eq!(timed["nativeInputSettled"], true);
        assert!(!operations.active());

        let human = program();
        programs
            .request(
                &state,
                origin,
                owner(2),
                Request::Open {
                    program: human,
                    timeout_ms: 10000,
                    target: None,
                },
            )
            .await
            .unwrap();
        let held = operations.interrupt(InterruptReason::HumanControl);
        let lease = programs
            .leases
            .lock()
            .await
            .iter()
            .find(|lease| lease.program == human)
            .cloned()
            .unwrap();
        let human = tokio::time::timeout(Duration::from_secs(2), lease.closed())
            .await
            .unwrap();
        assert_eq!(human["reason"], "human_control");
        assert_eq!(human["nativeInputSettled"], true);
        assert!(!operations.active());
        drop(held);
        let disconnected = program();
        programs
            .request(
                &state,
                origin,
                owner(2),
                Request::Open {
                    program: disconnected,
                    timeout_ms: 10000,
                    target: None,
                },
            )
            .await
            .unwrap();
        programs.end(origin).await;
        let status = programs
            .request(
                &state,
                relay_channel,
                owner(2),
                Request::Status {
                    program: disconnected,
                },
            )
            .await
            .unwrap();
        assert_eq!(status["reason"], "disconnected");
        assert_eq!(status["nativeInputSettled"], true);
        let response = Box::pin(execute_command(
            &json!({"action":"close"}),
            &mut *state.lock().await,
        ))
        .await;
        assert_eq!(response["success"], true, "{response}");
    }
}

impl std::fmt::Display for ProgramId {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(out)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CloseReason {
    Complete,
    Cancel,
    Timeout,
}

pub(crate) enum Request {
    Open {
        program: ProgramId,
        timeout_ms: u64,
        target: Option<String>,
    },
    Close {
        program: ProgramId,
        reason: CloseReason,
    },
    Status {
        program: ProgramId,
    },
    Files {
        program: ProgramId,
        files: Vec<super::files::Receipt>,
    },
}

pub(crate) fn is_kind(kind: &str) -> bool {
    matches!(
        kind,
        "program.open" | "program.close" | "program.status" | "program.files"
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OpenWire {
    #[allow(dead_code)]
    id: u64,
    program_id: String,
    timeout_ms: u64,
    target_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CloseWire {
    #[allow(dead_code)]
    id: u64,
    program_id: String,
    reason: CloseReason,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StatusWire {
    #[allow(dead_code)]
    id: u64,
    program_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FilesWire {
    #[allow(dead_code)]
    id: u64,
    program_id: String,
    files: Vec<super::files::Receipt>,
}

impl Request {
    pub(crate) fn independent(&self) -> bool {
        matches!(
            self,
            Self::Close { .. } | Self::Status { .. } | Self::Files { .. }
        )
    }
    pub(crate) fn read(kind: &str, value: Value) -> Result<Self, String> {
        Ok(match kind {
            "program.open" => {
                let frame: OpenWire = serde_json::from_value(value)
                    .map_err(|_| "The program.open frame is invalid.")?;
                if !(1..=120_000).contains(&frame.timeout_ms)
                    || frame
                        .target_id
                        .as_ref()
                        .is_some_and(|target| target.is_empty() || target.len() > 4096)
                {
                    return Err(
                        "timeoutMs must be between 1 and 120000; targetId must name a tab.".into(),
                    );
                }
                Self::Open {
                    program: ProgramId::parse(&frame.program_id)?,
                    timeout_ms: frame.timeout_ms,
                    target: frame.target_id,
                }
            }
            "program.close" => {
                let frame: CloseWire = serde_json::from_value(value)
                    .map_err(|_| "The program.close frame is invalid.")?;
                Self::Close {
                    program: ProgramId::parse(&frame.program_id)?,
                    reason: frame.reason,
                }
            }
            "program.status" => {
                let frame: StatusWire = serde_json::from_value(value)
                    .map_err(|_| "The program.status frame is invalid.")?;
                Self::Status {
                    program: ProgramId::parse(&frame.program_id)?,
                }
            }
            "program.files" => {
                let frame: FilesWire = serde_json::from_value(value)
                    .map_err(|_| "The program.files frame is invalid.")?;
                Self::Files {
                    program: ProgramId::parse(&frame.program_id)?,
                    files: frame.files,
                }
            }
            _ => return Err("Unknown remote program frame.".into()),
        })
    }
}

#[derive(Clone, Copy, Debug)]
enum StopReason {
    Complete,
    Cancel,
    Timeout,
    HumanControl,
    Shutdown,
    Disconnected,
}

impl StopReason {
    fn text(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Cancel => "cancel",
            Self::Timeout => "timeout",
            Self::HumanControl => "human_control",
            Self::Shutdown => "shutdown",
            Self::Disconnected => "disconnected",
        }
    }
}

impl From<CloseReason> for StopReason {
    fn from(reason: CloseReason) -> Self {
        match reason {
            CloseReason::Complete => Self::Complete,
            CloseReason::Cancel => Self::Cancel,
            CloseReason::Timeout => Self::Timeout,
        }
    }
}

struct Lease {
    program: ProgramId,
    channel: ChannelId,
    owner: Owner,
    timeout_ms: u64,
    selected: Option<String>,
    deadline: Instant,
    opened: Value,
    stop: watch::Sender<Option<StopReason>>,
    status: watch::Sender<Value>,
    files: super::files::StagedFiles,
}

impl Lease {
    fn stop(&self, reason: StopReason) {
        self.stop.send_if_modified(|current| {
            if current.is_some() {
                false
            } else {
                *current = Some(reason);
                true
            }
        });
    }

    fn data(&self) -> Value {
        let status = self.status.borrow().clone();
        if status["state"] != "active" {
            return status;
        }
        let mut data = self.opened.clone();
        data["remainingTimeoutMs"] = json!(self
            .deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(u128::from(u64::MAX)) as u64);
        data
    }

    async fn closed(&self) -> Value {
        let mut status = self.status.subscribe();
        loop {
            let value = status.borrow_and_update().clone();
            if value["state"] == "closed" {
                return value;
            }
            if status.changed().await.is_err() {
                return value;
            }
        }
    }
}

/// Transport receipts live no longer than this browser generation. This
/// bounded recovery history has the same size as the channel's op ledger;
/// it holds no code, Node result, credentials or durable custody state.
#[derive(Default)]
pub(crate) struct Programs {
    leases: Mutex<VecDeque<Arc<Lease>>>,
    seen: std::sync::Mutex<HashSet<ProgramId>>,
}

impl Programs {
    pub(crate) async fn fence(&self, owner: Owner) {
        let leases = self.leases.lock().await;
        let older: Vec<_> = leases
            .iter()
            .filter(|lease| {
                lease.owner.action == owner.action && lease.owner.generation < owner.generation
            })
            .cloned()
            .collect();
        drop(leases);
        for lease in older {
            lease.stop(StopReason::Shutdown);
            lease.closed().await;
        }
    }

    pub(crate) async fn end(&self, channel: ChannelId) {
        let leases = self.leases.lock().await;
        let owned: Vec<_> = leases
            .iter()
            .filter(|lease| lease.channel == channel)
            .cloned()
            .collect();
        drop(leases);
        for lease in owned {
            lease.stop(StopReason::Disconnected);
            lease.closed().await;
        }
    }

    #[cfg(test)]
    async fn request(
        &self,
        state: &Arc<Mutex<DaemonState>>,
        channel: ChannelId,
        owner: Owner,
        request: Request,
    ) -> Result<Value, String> {
        self.request_current(state, channel, owner, request, None)
            .await
    }

    pub(crate) async fn request_current(
        &self,
        state: &Arc<Mutex<DaemonState>>,
        channel: ChannelId,
        owner: Owner,
        request: Request,
        ledger: Option<&crate::native::agent_channel::ledger::Ledger>,
    ) -> Result<Value, String> {
        let mut leases = self.leases.lock().await;
        match request {
            Request::Open {
                program,
                timeout_ms,
                target,
            } => {
                if let Some(lease) = leases.iter().find(|lease| lease.program == program) {
                    if lease.owner != owner
                        || lease.timeout_ms != timeout_ms
                        || lease.selected != target
                    {
                        return Err("A program id already belongs to another owner or invocation; inspect its status and never replay it.".into());
                    }
                    return Ok(lease.data());
                }
                if self.seen.lock().unwrap().contains(&program) {
                    return Err("This browser generation no longer retains that program's receipt; the program was not restarted.".into());
                }
                while leases.len() >= 256 {
                    if leases
                        .front()
                        .is_some_and(|lease| lease.status.borrow()["state"] == "closed")
                    {
                        leases.pop_front();
                    } else {
                        return Err(
                            "The program recovery ledger is full; no new transport was admitted."
                                .into(),
                        );
                    }
                }
                // Waiting for an ordinary command never locks recovery
                // metadata. Reprove the canonical channel fence after that
                // wait and again before publishing an attachment.
                drop(leases);
                let mut state = state.lock().await;
                let mut leases = self.leases.lock().await;
                if ledger.is_some_and(|ledger| !ledger.may_continue(channel)) {
                    return Err("agent_channel_fenced: A newer Action owner reached the browser; the remote transport was not opened.".into());
                }
                if let Some(lease) = leases.iter().find(|lease| lease.program == program) {
                    if lease.owner != owner
                        || lease.timeout_ms != timeout_ms
                        || lease.selected != target
                    {
                        return Err("A program id already belongs to another invocation.".into());
                    }
                    return Ok(lease.data());
                }
                if self.seen.lock().unwrap().contains(&program) {
                    return Err("This browser generation no longer retains that program's receipt; the program was not restarted.".into());
                }
                state
                    .expire_browser_control()
                    .await
                    .map_err(|error| format!("{}: {}", error.code, error.message))?;
                let _ = state.drain_cdp_events_background().await;
                {
                    let control = state.browser_control.lock().await;
                    if let Some(error) = control.agent_error() {
                        return Err(format!("{}: {}", error.code, error.message));
                    }
                    if control.needs_observation() {
                        return Err(format!(
                            "browser_observation_required: {}",
                            crate::native::actions::OBSERVATION_REQUIRED
                        ));
                    }
                }
                if state.dialog_blocks_active_page() {
                    return Err("browser_dialog_open: Accept or dismiss the page's dialog before starting a browser program.".into());
                }
                if state.active_page_between_documents() {
                    return Err(format!("{}: The page is between documents; wait for navigation to settle before starting a browser program.",crate::native::documents::NAVIGATION_PENDING));
                }
                let mut operation = state.playwright_operations.begin()?;
                let deadline = Instant::now() + Duration::from_millis(timeout_ms);
                let attachment = Box::pin(super::attach(
                    &mut state,
                    target.as_deref(),
                    deadline,
                    &mut operation,
                ))
                .await?;
                if ledger.is_some_and(|ledger| !ledger.may_continue(channel)) {
                    return Err("agent_channel_fenced: A newer Action owner reached the browser; the remote transport was not published.".into());
                }
                let control = state.browser_control.clone();
                let files = attachment.owner.site_context().files;
                let scope =
                    super::files::Scope::parse(&program.to_string()).map_err(str::to_owned)?;
                files.activate(scope, owner).map_err(str::to_owned)?;
                files.downloads(attachment.artifacts.clone());
                drop(state);
                let opened = json!({"programId":program.to_string(), "state":"active", "nativeInputSettled":false,
                    "endpoint":attachment.tunnel.endpoint(), "targetId":attachment.target,
                    "isolatedContexts":attachment.isolated_contexts, "timeoutMs":timeout_ms,
                    "downloadRoot":attachment.artifacts,
                    "runner":{"source":super::RUNNER, "digest":format!("sha256:{:x}",Sha256::digest(super::RUNNER.as_bytes()))}});
                let (stop, mut stopping) = watch::channel(None);
                let (status, _) = watch::channel(
                    json!({"programId":program.to_string(),"state":"active","reason":null,"nativeInputSettled":false}),
                );
                let lease = Arc::new(Lease {
                    program,
                    channel,
                    owner,
                    timeout_ms,
                    selected: target,
                    deadline,
                    opened,
                    stop,
                    status,
                    files: files.clone(),
                });
                leases.push_back(lease.clone());
                self.seen.lock().unwrap().insert(program);
                let finishing = lease.clone();
                tokio::spawn(async move {
                    let mut tunnel = attachment.tunnel;
                    let (reason, ended) = tokio::select! {
                        biased;
                        _ = operation.canceled.changed() => (match *operation.canceled.borrow() {
                            Some(InterruptReason::HumanControl) => StopReason::HumanControl,
                            Some(InterruptReason::Shutdown) => StopReason::Shutdown,
                            _ => StopReason::Disconnected,
                        }, Ok(())),
                        _ = stopping.changed() => (stopping.borrow().unwrap_or(StopReason::Disconnected), Ok(())),
                        _ = tokio::time::sleep_until(deadline) => (StopReason::Timeout, Ok(())),
                        ended = tunnel.ended() => (StopReason::Disconnected, ended),
                    };
                    finishing.status.send_replace(json!({"programId":program.to_string(),"state":"closing","reason":reason.text(),"nativeInputSettled":false}));
                    tunnel.stop();
                    let transport = tunnel.finish().await;
                    let input = control
                        .lock()
                        .await
                        .finish_agent_program(&tunnel.client)
                        .await;
                    let settled = input.is_ok();
                    let clean = ended.and(transport).and(input).is_ok();
                    if !settled {
                        let _ = control.lock().await.cancel_native_input().await;
                    }
                    drop(tunnel);
                    files.clear(scope, owner);
                    // The shared slot stays occupied until all admitted input
                    // settled and this private connection has gone away.
                    drop(operation);
                    let mut outcome = json!({"programId":program.to_string(),"state":"closed","reason":reason.text(),"nativeInputSettled":settled});
                    if !clean {
                        outcome["error"]=json!("The remote program transport or its final native input did not settle; inspect a fresh observation before continuing.");
                    }
                    finishing.status.send_replace(outcome);
                });
                Ok(lease.data())
            }
            Request::Close { program, reason } => {
                let lease=leases.iter().find(|lease|lease.program==program).cloned().ok_or("This browser generation has no receipt for that program; do not replay it.")?;
                if lease.owner.action != owner.action || lease.owner.generation > owner.generation {
                    return Err("That program belongs to another Action or a newer owner.".into());
                }
                drop(leases);
                lease.stop(reason.into());
                Ok(lease.closed().await)
            }
            Request::Status { program } => {
                let lease = leases.iter().find(|lease| lease.program == program).ok_or(
                    "This browser generation has no receipt for that program; do not replay it.",
                )?;
                if lease.owner.action != owner.action || lease.owner.generation > owner.generation {
                    return Err("That program belongs to another Action or a newer owner.".into());
                }
                let mut status = lease.status.borrow().clone();
                if status["state"] == "active" && lease.owner == owner {
                    for field in ["endpoint", "targetId", "isolatedContexts", "downloadRoot"] {
                        status[field] = lease.opened[field].clone();
                    }
                    status["remainingTimeoutMs"] = json!(lease
                        .deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis()
                        .min(u128::from(u64::MAX))
                        as u64);
                }
                Ok(status)
            }
            Request::Files { program, files } => {
                let lease = leases
                    .iter()
                    .find(|lease| lease.program == program)
                    .cloned()
                    .ok_or("This browser generation has no active transport for that program.")?;
                if lease.owner != owner || lease.status.borrow()["state"] != "active" {
                    return Err(
                        "That program's staging custody is no longer active for this Action owner."
                            .into(),
                    );
                }
                drop(leases);
                let scope =
                    super::files::Scope::parse(&program.to_string()).map_err(str::to_owned)?;
                let registered = lease
                    .files
                    .register(scope, owner, files)
                    .await
                    .map_err(str::to_owned)?;
                if lease.status.borrow()["state"] != "active" {
                    return Err("That program's staging custody ended before publication.".into());
                }
                Ok(json!({"programId":program.to_string(),"registered":registered}))
            }
        }
    }
}
