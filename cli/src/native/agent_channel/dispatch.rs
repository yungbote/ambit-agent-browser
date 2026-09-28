//! The daemon's own browser as the agent channel reaches it (agent-channel
//! contract §4 "Execution" rules 2, 3 and 7). Each step takes command custody
//! for itself alone and is dispatched as its host-bound call is dispatched
//! today: an agent operation through the host feedback path with the step's
//! preconditions as its fence, a host operation (the theme) as the theme is
//! set. There is no capture per step; the frame's capture comes after its
//! last executed step.

use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::step::PreparedStep;
use super::target::{self, Recorders};
use super::{Asks, Browser, Finish, FrameContext, StepRecord, StepTiming};
use crate::connection::Response;
use crate::mcp::host_bound::{step_result, Dispatch};
use crate::native::actions::{execute_command_received, run_host_command, DaemonState, HostFence};
use crate::native::feedback::{self, FeedbackRequest, REQUEST_FIELD};

/// The daemon's browser, reached under its command custody.
pub(crate) struct DaemonBrowser {
    state: Arc<Mutex<DaemonState>>,
    recorders: Recorders,
}

impl DaemonBrowser {
    pub(crate) fn new(state: Arc<Mutex<DaemonState>>) -> Self {
        Self {
            state,
            recorders: Recorders::default(),
        }
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
        let mut fence = StepFence {
            step,
            recorders: &self.recorders,
        };
        run_host_command(&command, &request, state, frame.received_at, &mut fence).await
    }
}

/// A channel step's fence: its preconditions, checked where the file
/// protocol checks the image a point came from.
struct StepFence<'a> {
    step: &'a PreparedStep,
    recorders: &'a Recorders,
}

impl HostFence for StepFence<'_> {
    fn fences_point(&self) -> bool {
        let preconditions = &self.step.preconditions;
        preconditions.page_generation.is_some() && preconditions.geometry_sha256.is_some()
    }

    async fn admit(&mut self, command: &Value, state: &mut DaemonState) -> Result<(), Value> {
        target::check(self.step, command, state, self.recorders).await?;
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

fn micros(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

impl Browser for DaemonBrowser {
    async fn step(&self, frame: &FrameContext<'_>, step: &PreparedStep) -> StepRecord {
        let asked = Instant::now();
        let mut state = self.state.lock().await;
        let held = Instant::now();
        let response = self.dispatch(&mut state, frame, step).await;
        drop(state);
        let (result, succeeded) = tool_result(response);
        StepRecord {
            op: step.op.clone(),
            result,
            succeeded,
            landed: None,
            timing: StepTiming {
                queue_us: micros(held - asked),
                op_us: micros(held.elapsed()),
                motion_us: 0,
            },
        }
    }

    async fn finish(&self, frame: &FrameContext<'_>, asks: &Asks<'_>) -> Finish {
        let asked = Instant::now();
        let state = self.state.lock().await;
        let held = Instant::now();
        let request = Self::request(frame, feedback::MAX_CALL_MS);
        let controlled = state.browser_control.lock().await.agent_error();
        let unavailable = |code: &str| json!({ "status": "unavailable", "code": code });
        let (browser, observation, resolved) = if let Some(error) = controlled {
            // While a person holds the browser, nothing is read from it.
            (
                request.unavailable(error.code),
                asks.observe.then(|| unavailable(error.code)),
                (!asks.resolve.is_empty()).then(|| unavailable(error.code)),
            )
        } else {
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
            (
                browser,
                asks.observe
                    .then(|| unavailable("browser_observation_unavailable")),
                (!asks.resolve.is_empty()).then(|| unavailable("browser_observation_unavailable")),
            )
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
}
