//! The daemon's own browser as the agent channel reaches it (agent-channel
//! contract §4 "Execution" rules 2, 3 and 7, §5 `landed` and `timing`). Each
//! step takes command custody for itself alone and is dispatched as its
//! host-bound call is dispatched today: an agent operation through the host
//! feedback path with the step's preconditions as its fence, a host operation
//! (the theme) as the theme is set. A step that may have acted then lands
//! without custody, and takes it again only for its final read. There is no
//! capture per step; the frame's capture comes after its last executed step.
//!
//! A person taking control never waits behind the channel: the landing's
//! wait holds no custody, and the landing's final read and the frame's
//! observation, resolutions and capture stop the moment a takeover is raised.
//! Nor does the channel wait on a page between documents (`documents`),
//! which answers nothing until its navigation commits: a step that reads the
//! page first is refused (`run_host_command`), and the landing and the
//! frame read nothing from it.

use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::frame::ChannelId;
use super::landed::{self, Landing, PersonWatch};
use super::observe;
use super::step::{self, PreparedStep};
use super::target::{self, Recorders};
use super::{Asks, Browser, Finish, FrameContext, StepRecord, StepTiming};
use crate::connection::Response;
use crate::mcp::host_bound::{step_result, Dispatch};
use crate::native::actions::{execute_command_received, run_host_command, DaemonState, HostFence};
use crate::native::browser_control::paced;
use crate::native::documents;
use crate::native::feedback::{self, FeedbackRequest, REQUEST_FIELD};

const DIALOG_OPEN: &str = "browser_dialog_open";
const CONTROLLED: &str = "browser_controlled_by_user";

/// The daemon's browser, reached under its command custody.
pub(crate) struct DaemonBrowser {
    custody: Arc<crate::native::site_sessions::custody::Custody>,
    programs: crate::native::playwright::remote::Programs,
    state: Arc<Mutex<DaemonState>>,
    recorders: Recorders,
    /// The channel whose step acted last: the input its steps left held is
    /// its to release when it ends. Read and written under command custody.
    actor: StdMutex<Option<ChannelId>>,
}

impl DaemonBrowser {
    pub(crate) fn new(state: Arc<Mutex<DaemonState>>) -> Self {
        Self {
            custody: crate::native::site_sessions::custody::Custody::new(),
            programs: crate::native::playwright::remote::Programs::default(),
            state,
            recorders: Recorders::default(),
            actor: StdMutex::new(None),
        }
    }

    fn actor(&self) -> std::sync::MutexGuard<'_, Option<ChannelId>> {
        self.actor.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The feedback request a step or frame of `frame` carries: the binding,
    /// the Action's `captures` directory and the deadline.
    fn request(frame: &FrameContext<'_>, timeout_ms: u64) -> FeedbackRequest {
        FeedbackRequest {
            namespace: frame.binding.namespace.clone(),
            session: frame.binding.session.clone(),
            capture_directory: frame.directory.join("captures"),
            timeout_ms,
            expected_observation: None,
            launch: None,
        }
    }

    async fn dispatch(
        &self,
        state: &mut DaemonState,
        frame: &FrameContext<'_>,
        step: &PreparedStep,
        fence: &mut StepFence<'_>,
    ) -> Value {
        let launch = match &step.call.dispatch {
            // The theme is session state: it passes no window, custody or
            // observation gate, in every browser state.
            Dispatch::Session => {
                return execute_command_received(&step.call.command, state, frame.received_at).await
            }
            Dispatch::Agent { launch } => launch,
        };
        let mut command = step.call.command.clone();
        command[REQUEST_FIELD] = json!({ "namespace": frame.binding.namespace,
            "session": frame.binding.session,
            "captureDirectory": frame.directory.join("captures"),
            "timeoutMs": step.call.timeout_ms, "expectedObservation": null, "launch": launch });
        if let Err((code, message)) = state
            .prepare_window_command(&command, frame.received_at)
            .await
        {
            return state.window_refusal(&command["id"], code, message);
        }
        let request = match FeedbackRequest::parse(&command[REQUEST_FIELD], state) {
            Ok(request) => request,
            Err(error) => {
                return json!({ "id": command["id"], "success": false,
                    "code": "browser_feedback_invalid", "error": error })
            }
        };
        run_host_command(&command, &request, state, frame.received_at, fence).await
    }

    /// The step's landing, from the end of its operation: the wait without
    /// custody, then the final read under custody taken again, which a
    /// person taking control forgoes. Answers the block and the time spent
    /// waiting for custody again.
    async fn land(
        &self,
        mut landing: Landing,
        state: tokio::sync::MutexGuard<'_, DaemonState>,
        deadline: Instant,
    ) -> (Value, Duration) {
        let mut person = PersonWatch::of(&state).await;
        drop(state);
        let pending = landing.wait(deadline, &mut person).await;
        landing.stop_watching().await;
        if person.holds() {
            return (landing.without_read(), Duration::ZERO);
        }
        let asked = Instant::now();
        let mut state = self.state.lock().await;
        let queued = asked.elapsed();
        let read = tokio::select! {
            biased;
            _ = person.taken() => None,
            read = landing.read(&mut state, pending, deadline) => Some(read),
        };
        drop(state);
        (read.unwrap_or_else(|| landing.without_read()), queued)
    }
}

/// A channel step's fence: its preconditions, checked where the file
/// protocol checks the image a point came from. Once they admit a step that
/// is not read-only, its landing starts recording.
struct StepFence<'a> {
    channel: ChannelId,
    owner: Option<super::frame::Owner>,
    custody: &'a Arc<crate::native::site_sessions::custody::Custody>,
    step: &'a PreparedStep,
    recorders: &'a Recorders,
    landing: Option<Landing>,
}

/// Clear the projection even if a command future is canceled before it
/// returns. The surrounding command mutex prevents another writer.
struct ActionProjection(Arc<StdMutex<Option<super::frame::Owner>>>);
impl Drop for ActionProjection {
    fn drop(&mut self) {
        *self.0.lock().unwrap() = None;
    }
}

impl HostFence for StepFence<'_> {
    async fn browser_ready(&mut self, state: &mut DaemonState) -> Result<(), Value> {
        if !self.custody.admits_channel(self.channel).await {
            return Err(
                json!({"success":false,"code":"browser_site_custody_refused","error":"This channel needs its host's site custody offer before using the browser."}),
            );
        }
        if let Some(browser) = &state.browser {
            let sessions = browser
                .tab_list()
                .iter()
                .filter_map(|tab| {
                    tab["targetId"]
                        .as_str()
                        .and_then(|target| browser.client.session_for_target(target))
                })
                .collect();
            self.custody.browser_ready(browser.client.clone(),sessions).await.map_err(|error|json!({"success":false,"code":"browser_site_custody_refused","error":error}))?;
        }
        Ok(())
    }
    fn fences_point(&self) -> bool {
        let preconditions = &self.step.preconditions;
        preconditions.page_generation.is_some() && preconditions.geometry_sha256.is_some()
    }

    /// A judged step, and one carrying page identity, reads the page before
    /// its operation does.
    fn reads_page(&self) -> bool {
        let preconditions = &self.step.preconditions;
        self.step.judged.is_some()
            || preconditions.page_generation.is_some()
            || preconditions.geometry_sha256.is_some()
    }

    async fn admit(&mut self, command: &Value, state: &mut DaemonState) -> Result<(), Value> {
        let _ = state.drain_cdp_events_background().await;
        let blocked = state.dialog_blocks_active_page();
        if blocked && self.reads_page() {
            return Err(target::refusal(
                command,
                DIALOG_OPEN,
                "A JavaScript dialog is blocking the page, so the step's target could not be checked. Nothing was done; accept or dismiss the dialog first.",
                json!({}),
            ));
        }
        let node = target::check(self.step, command, state, self.recorders).await?;
        if command["action"] == "upload" && self.custody.files().guarded() {
            let owner=self.owner.ok_or_else(||json!({"success":false,"code":"browser_operation_rejected","error":"Native uploads require their Action staging owner."}))?;
            let scope=crate::native::playwright::files::Scope::parse(&owner.action.to_string()).map_err(|_|json!({"success":false,"code":"browser_operation_rejected","error":"The Action staging owner is invalid."}))?;
            let paths = command["files"]
                .as_array()
                .map(|files| {
                    files
                        .iter()
                        .filter_map(|file| file.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_else(|| {
                    command["file"]
                        .as_str()
                        .map(|file| vec![file.to_owned()])
                        .unwrap_or_default()
                });
            self.custody.files().paths_for(scope,owner,paths).await.map_err(|_|json!({"success":false,"code":"browser_operation_rejected","error":"The upload has no current exact Action staging receipt."}))?;
        }
        if !step::read_only(&self.step.op) {
            self.landing = Some(Landing::arm(state, self.recorders, node, !blocked).await);
        }
        Ok(())
    }
}

/// A native response as the host-bound result the host reads.
fn tool_result(response: Value) -> (Value, bool) {
    let response = serde_json::from_value::<Response>(response).unwrap_or_else(|error| Response {
        error: Some(format!("The browser answered a step unreadably ({error}).")),
        code: Some("browser_operation_outcome_unknown".into()),
        ..Response::default()
    });
    let succeeded = response.success;
    (step_result(response), succeeded)
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

/// What a frame reads after its last step, when it cannot read it: why.
fn unavailable(
    request: &FeedbackRequest,
    asks: &Asks<'_>,
    code: &str,
) -> (Value, Option<Value>, Option<Value>) {
    let unavailable = json!({ "status": "unavailable", "code": code });
    (
        request.unavailable(code),
        asks.observe.then(|| unavailable.clone()),
        (!asks.resolve.is_empty()).then_some(unavailable),
    )
}

impl Browser for DaemonBrowser {
    async fn action_files(
        &self,
        channel: ChannelId,
        owner: super::frame::Owner,
        files: Option<Vec<crate::native::playwright::files::Receipt>>,
        ledger: &super::ledger::Ledger,
    ) -> Result<Value, String> {
        if !ledger.may_continue(channel) {
            return Err("The Action staging owner was fenced before admission.".into());
        }
        let scope = crate::native::playwright::files::Scope::parse(&owner.action.to_string())
            .map_err(str::to_owned)?;
        let registry = self.custody.files();
        if let Some(files) = files {
            registry.admit(scope, owner).map_err(str::to_owned)?;
            let registered = registry
                .register(scope, owner, files)
                .await
                .map_err(str::to_owned)?;
            Ok(json!({"actionId":owner.action.to_string(),"registered":registered}))
        } else {
            registry.release(scope, owner).map_err(str::to_owned)?;
            Ok(json!({"actionId":owner.action.to_string(),"released":true}))
        }
    }
    async fn program_request(
        &self,
        channel: ChannelId,
        owner: super::frame::Owner,
        request: crate::native::playwright::remote::Request,
        ledger: &super::ledger::Ledger,
    ) -> Result<Value, String> {
        if matches!(
            &request,
            crate::native::playwright::remote::Request::Open { .. }
        ) && !self.custody.admits_channel(channel).await
        {
            return Err(
                "This channel needs its host's site custody offer before using the browser.".into(),
            );
        }
        if matches!(
            &request,
            crate::native::playwright::remote::Request::Open { .. }
        ) {
            let state = self.state.lock().await;
            if let Some(browser) = &state.browser {
                let sessions = browser
                    .tab_list()
                    .iter()
                    .filter_map(|tab| {
                        tab["targetId"]
                            .as_str()
                            .and_then(|target| browser.client.session_for_target(target))
                    })
                    .collect();
                self.custody
                    .browser_ready(browser.client.clone(), sessions)
                    .await
                    .map_err(str::to_owned)?;
            }
        }
        self.programs
            .request_current(&self.state, channel, owner, request, Some(ledger))
            .await
    }
    async fn fence_program(&self, owner: super::frame::Owner) {
        self.custody.files().fence(owner);
        self.programs.fence(owner).await;
    }
    async fn site_request(
        &self,
        channel: ChannelId,
        request: crate::native::site_sessions::protocol::Request,
        ledger: Arc<super::ledger::Ledger>,
    ) -> Result<Value, &'static str> {
        // Attach must never wait behind the navigation it is releasing.
        // Other requests can refresh a new native connection without taking
        // command custody away from an operation already using it.
        if !matches!(
            request,
            crate::native::site_sessions::protocol::Request::Attach { .. }
                | crate::native::site_sessions::protocol::Request::Refuse { .. }
        ) {
            if let Ok(state) = self.state.try_lock() {
                if let Some(browser) = &state.browser {
                    let sessions = browser
                        .tab_list()
                        .iter()
                        .filter_map(|tab| {
                            tab["targetId"]
                                .as_str()
                                .and_then(|target| browser.client.session_for_target(target))
                        })
                        .collect();
                    self.custody
                        .browser_ready(browser.client.clone(), sessions)
                        .await?;
                }
            }
        }
        let response = self
            .custody
            .request_current(channel, request, ledger)
            .await?;
        // The first offer may arrive before the browser was launched.
        if let Ok(state) = self.state.try_lock() {
            if let Some(browser) = &state.browser {
                let sessions = browser
                    .tab_list()
                    .iter()
                    .filter_map(|tab| {
                        tab["targetId"]
                            .as_str()
                            .and_then(|target| browser.client.session_for_target(target))
                    })
                    .collect();
                self.custody
                    .browser_ready(browser.client.clone(), sessions)
                    .await?;
            }
        }
        Ok(response)
    }

    fn custody_events(&self) -> Option<tokio::sync::broadcast::Receiver<(ChannelId, Value)>> {
        Some(self.custody.subscribe())
    }
    fn redact(&self, value: &mut Value) {
        self.custody.scrub(value);
    }
    fn redact_step(&self, op: &str, value: &mut Value) {
        self.custody.scrub_tool(op, value);
    }
    async fn step(&self, frame: &FrameContext<'_>, step: &PreparedStep) -> StepRecord {
        let asked = Instant::now();
        let mut state = self.state.lock().await;
        let held = Instant::now();
        *self.actor() = Some(frame.channel);
        let mut fence = StepFence {
            channel: frame.channel,
            owner: frame.owner,
            custody: &self.custody,
            step,
            recorders: &self.recorders,
            landing: None,
        };
        *state.native_action_owner.lock().unwrap() = frame.owner;
        let projection = ActionProjection(state.native_action_owner.clone());
        let (response, paced) =
            paced::measured(self.dispatch(&mut state, frame, step, &mut fence)).await;
        drop(projection);
        let (landed, queued_again) = match fence.landing.filter(|_| landed::lands(&response)) {
            Some(landing) => {
                let deadline = held + Duration::from_millis(step.call.timeout_ms);
                let (landed, queued) = self.land(landing, state, deadline).await;
                (Some(landed), queued)
            }
            None => {
                drop(state);
                (None, Duration::ZERO)
            }
        };
        let (result, succeeded) = tool_result(response);
        StepRecord {
            op: step.op.clone(),
            result,
            succeeded,
            landed,
            timing: StepTiming {
                queue_us: micros(held - asked + queued_again),
                op_us: micros(held.elapsed().saturating_sub(queued_again)),
                motion_us: micros(paced),
            },
        }
    }

    async fn finish(&self, frame: &FrameContext<'_>, asks: &Asks<'_>) -> Finish {
        let asked = Instant::now();
        let mut state = self.state.lock().await;
        let held = Instant::now();
        let request = Self::request(frame, feedback::MAX_CALL_MS);
        let _ = state.drain_cdp_events_background().await;
        let controlled = state.browser_control.lock().await.agent_error();
        let (browser, observation, resolved) = if let Some(error) = controlled {
            // While a person holds the browser, nothing is read from it: the
            // capture, the observation and the resolutions say why.
            unavailable(&request, asks, error.code)
        } else if state.dialog_blocks_active_page() {
            unavailable(&request, asks, DIALOG_OPEN)
        } else if state.active_page_between_documents() {
            unavailable(&request, asks, documents::NAVIGATION_PENDING)
        } else {
            let mut person = PersonWatch::of(&state).await;
            let read = async {
                let observation = if asks.observe {
                    Some(observe::observation(&mut state, &self.recorders).await)
                } else {
                    None
                };
                let resolved = if asks.resolve.is_empty() {
                    None
                } else {
                    Some(observe::resolutions(&mut state, &self.recorders, asks.resolve).await)
                };
                let browser = if asks.capture {
                    match super::reply::private_directory(&request.capture_directory) {
                        Ok(_) => feedback::capture_for(&request, &state).await,
                        Err(()) => request.unavailable("capture_directory_unavailable"),
                    }
                } else {
                    match feedback::page_identity(&state).await {
                        Ok((_, page)) => json!({ "namespace": request.namespace,
                            "session": request.session, "page": page,
                            "capture": { "status": "not_requested" } }),
                        Err(code) => request.unavailable(code),
                    }
                };
                (browser, observation, resolved)
            };
            tokio::select! {
                biased;
                _ = person.taken() => unavailable(&request, asks, CONTROLLED),
                read = read => read,
            }
        };
        drop(state);
        Finish {
            browser,
            observation,
            resolved,
            queue_us: micros(held - asked),
            observe_us: micros(held.elapsed()),
        }
    }

    async fn channel_closed(&self, channel: ChannelId) {
        self.custody.channel_closed(channel).await;
    }

    async fn end(&self, channel: ChannelId) {
        self.programs.end(channel).await;
        self.custody.end(channel).await;
        // Custody is held throughout: no step starts while the input its
        // channel left held is settled.
        let state = self.state.lock().await;
        if *self.actor() != Some(channel) {
            return;
        }
        *self.actor() = None;
        let Some(client) = state.browser.as_ref().map(|browser| browser.client.clone()) else {
            return;
        };
        let _ = state
            .browser_control
            .lock()
            .await
            .finish_agent_channel(&client)
            .await;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::native::agent_channel::{
        frame::{ActionId, Binding, Owner, Step},
        ledger::Ledger,
    };
    use crate::native::playwright::files::{Receipt, Scope, StagedFiles};
    use crate::native::site_sessions::{custody::Custody, protocol::Request};
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn canceling_command_clears_its_native_action_projection() {
        let owner = Owner {
            action: ActionId::for_test(230),
            generation: 1,
        };
        let projected = Arc::new(StdMutex::new(None));
        let started = Arc::new(tokio::sync::Notify::new());
        let (field, signal) = (projected.clone(), started.clone());
        let task = tokio::spawn(async move {
            *field.lock().unwrap() = Some(owner);
            let _projection = ActionProjection(field);
            signal.notify_one();
            std::future::pending::<()>().await;
        });
        started.notified().await;
        assert_eq!(*projected.lock().unwrap(), Some(owner));
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(projected.lock().unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore]
    async fn e2e_native_action_upload_keeps_scope_across_disconnect_then_releases() {
        let env =
            crate::test_utils::EnvGuard::new(&["AGENT_BROWSER_NAMESPACE", "AGENT_BROWSER_SESSION"]);
        env.set("AGENT_BROWSER_NAMESPACE", "native-scope-fixture");
        env.set("AGENT_BROWSER_SESSION", "default");
        let temporary = tempfile::tempdir().unwrap();
        let files = StagedFiles::with_root(temporary.path().join("staged"));
        let owner = Owner {
            action: ActionId::for_test(231),
            generation: 1,
        };
        let scope = Scope::parse(&owner.action.to_string()).unwrap();
        let directory = temporary
            .path()
            .join("staged")
            .join(scope.to_string())
            .join("a".repeat(64));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("input.txt");
        std::fs::write(&path, b"nosecret-native").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o555)).unwrap();
        let mut state = DaemonState::new();
        for command in [
            json!({"action":"launch","headless":true}),
            json!({"action":"navigate","url":"data:text/html,<title>Native scope</title><input id=files type=file>"}),
        ] {
            let reply = Box::pin(crate::native::actions::execute_command(
                &command, &mut state,
            ))
            .await;
            assert_eq!(reply["success"], true, "{reply}");
        }
        let state = Arc::new(Mutex::new(state));
        let browser = DaemonBrowser {
            custody: Custody::with_files(files.clone()),
            state: state.clone(),
            programs: Default::default(),
            recorders: Default::default(),
            actor: Default::default(),
        };
        let channel = ChannelId::parse("11111111-1111-4111-8111-111111111111").unwrap();
        let later = ChannelId::parse("22222222-2222-4222-8222-222222222222").unwrap();
        let ledger = Arc::new(Ledger::default());
        assert!(ledger.open(channel));
        ledger.register(channel, owner).unwrap();
        let receipt = Receipt {
            path: path.to_str().unwrap().into(),
            byte_size: b"nosecret-native".len() as u64,
            content_ref: format!("sha256:{:x}", Sha256::digest(b"nosecret-native")),
        };
        let registration = browser
            .action_files(channel, owner, Some(vec![receipt.clone()]), &ledger)
            .await
            .unwrap();
        assert_eq!(registration["registered"], 1);
        browser.end(channel).await;
        ledger.end(channel);
        files
            .paths_for(scope, owner, vec![receipt.path.clone()])
            .await
            .unwrap();
        assert!(ledger.open(later));
        ledger.register(later, owner).unwrap();
        browser
            .site_request(
                later,
                Request::read("site_sessions.offer", json!({"sites":[]})).unwrap(),
                ledger.clone(),
            )
            .await
            .unwrap();
        let binding = Binding {
            namespace: "native-scope-fixture".into(),
            session: "default".into(),
            require_sandbox: false,
            browser_host: true,
            theme: None,
        };
        let context = FrameContext {
            channel: later,
            owner: Some(owner),
            binding: &binding,
            directory: temporary.path(),
            received_at: Instant::now(),
        };
        let (_, observed) = crate::native::feedback::page_identity(&*state.lock().await)
            .await
            .unwrap();
        let (target_client, target_session) = {
            let state = state.lock().await;
            let browser = state.browser.as_ref().unwrap();
            (
                browser.client.clone(),
                browser.active_session_id().unwrap().to_owned(),
            )
        };
        let document = target_client
            .send_command("DOM.getDocument", Some(json!({})), Some(&target_session))
            .await
            .unwrap();
        let selected = target_client
            .send_command(
                "DOM.querySelector",
                Some(json!({"nodeId":document["root"]["nodeId"],"selector":"#files"})),
                Some(&target_session),
            )
            .await
            .unwrap();
        let described = target_client
            .send_command(
                "DOM.describeNode",
                Some(json!({"nodeId":selected["nodeId"]})),
                Some(&target_session),
            )
            .await
            .unwrap();
        let steps = step::prepare(
            &crate::mcp::host_bound::HostFlags::new(&binding.namespace, &binding.session, None)
                .unwrap(),
            &[Step {
                op: "agent_browser_upload".into(),
                arguments: json!({"selector":"#files","files":[receipt.path]}),
                preconditions: super::super::frame::Preconditions {
                    page_generation: observed["pageGeneration"].as_str().map(str::to_owned),
                    backend_node_id: described["node"]["backendNodeId"].as_i64(),
                    effects: Some(super::super::ceiling::Ceiling::Commit),
                    ..Default::default()
                },
            }],
        )
        .unwrap();
        let uploaded = browser.step(&context, &steps[0]).await;
        assert!(uploaded.succeeded, "{:?}", uploaded.result);
        assert!(
            state
                .lock()
                .await
                .native_action_owner
                .lock()
                .unwrap()
                .is_none(),
            "projection cannot escape its command"
        );
        let held = state.lock().await;
        let client = held.browser.as_ref().unwrap().client.clone();
        let session = held
            .browser
            .as_ref()
            .unwrap()
            .active_session_id()
            .unwrap()
            .to_owned();
        drop(held);
        let value=client.send_command("Runtime.evaluate",Some(json!({"expression":"document.querySelector('#files').files[0].text()","awaitPromise":true,"returnByValue":true})),Some(&session)).await.unwrap();
        assert_eq!(value["result"]["value"], "nosecret-native");
        browser
            .action_files(later, owner, None, &ledger)
            .await
            .unwrap();
        assert!(
            browser
                .action_files(later, owner, Some(vec![receipt]), &ledger)
                .await
                .is_err(),
            "late registration cannot resurrect terminal authority"
        );
        let refused = browser.step(&context, &steps[0]).await;
        assert!(!refused.succeeded);
        browser.end(later).await;
        let closed = Box::pin(crate::native::actions::execute_command(
            &json!({"action":"close"}),
            &mut *state.lock().await,
        ))
        .await;
        assert_eq!(closed["success"], true, "{closed}");
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
