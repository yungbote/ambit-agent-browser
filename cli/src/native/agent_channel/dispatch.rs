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
    state: Arc<Mutex<DaemonState>>,
    recorders: Recorders,
    /// The channel whose step acted last: the input its steps left held is
    /// its to release when it ends. Read and written under command custody.
    actor: StdMutex<Option<ChannelId>>,
}

impl DaemonBrowser {
    pub(crate) fn new(state: Arc<Mutex<DaemonState>>) -> Self {
        Self {
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
    step: &'a PreparedStep,
    recorders: &'a Recorders,
    landing: Option<Landing>,
}

impl HostFence for StepFence<'_> {
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
    async fn step(&self, frame: &FrameContext<'_>, step: &PreparedStep) -> StepRecord {
        let asked = Instant::now();
        let mut state = self.state.lock().await;
        let held = Instant::now();
        *self.actor() = Some(frame.channel);
        let mut fence = StepFence {
            step,
            recorders: &self.recorders,
            landing: None,
        };
        let (response, paced) =
            paced::measured(self.dispatch(&mut state, frame, step, &mut fence)).await;
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

    async fn end(&self, channel: ChannelId) {
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
