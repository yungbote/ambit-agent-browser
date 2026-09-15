//! Process-local custody for Product-mediated browser input.
//!
//! The daemon mutex serializes commands and maintenance. This narrower gate
//! also covers stream input, which deliberately does not wait one round trip
//! per event. A control acquisition drains its retained CDP acknowledgments
//! before reporting success. No controller identity is broadcast to viewers.

use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::activity::InputSource;
use serde::Deserialize;
use serde_json::{json, Value};

use super::cdp::client::{CdpClient, PendingCommand};
use super::input::{input_command, stream_event, HeldInputs};

pub(crate) const ACTION: &str = "ambit_browser_control";
const MAX_LEASE_MS: u64 = 30_000;
const MAX_REQUEST_BYTES: usize = 65_536;
const MAX_EVENTS: usize = 64;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_PENDING_STREAM_INPUTS: usize = 256;
const ACK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub(crate) struct ControlError {
    pub code: &'static str,
    pub message: String,
}

impl ControlError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new("browser_control_invalid", message)
    }

    pub(crate) fn unknown() -> Self {
        Self::new("browser_control_outcome_unknown", "The browser did not acknowledge all input. Inspect the browser before releasing control; do not replay the input.")
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ControlRequest {
    #[serde(default, rename = "id")]
    _id: Option<String>,
    action: String,
    op: Operation,
    #[serde(default)]
    controller_id: String,
    expires_at: Option<u64>,
    sequence: Option<u64>,
    events: Option<Vec<Value>>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Operation {
    Inspect,
    Acquire,
    Renew,
    Release,
    Input,
    Copy,
}

impl ControlRequest {
    pub(crate) fn parse(value: &Value) -> Result<Self, ControlError> {
        if value.to_string().len() > MAX_REQUEST_BYTES {
            return Err(ControlError::invalid(
                "Control requests must be at most 65536 bytes.",
            ));
        }
        let request: Self = serde_json::from_value(value.clone()).map_err(|_| {
            ControlError::invalid("Invalid browser control request fields or types.")
        })?;
        let allowed: &[&str] = match request.op {
            Operation::Inspect => &["id", "action", "op"],
            Operation::Acquire | Operation::Renew => {
                &["id", "action", "op", "controllerId", "expiresAt"]
            }
            Operation::Release | Operation::Copy => &["id", "action", "op", "controllerId"],
            Operation::Input => &["id", "action", "op", "controllerId", "sequence", "events"],
        };
        fields(value, allowed)?;
        if request.op == Operation::Inspect {
            if value.get("controllerId").is_some()
                || request.expires_at.is_some()
                || request.sequence.is_some()
                || request.events.is_some()
                || request.action != ACTION
            {
                return Err(ControlError::invalid(
                    "Inspect accepts no controller or input fields.",
                ));
            }
            return Ok(request);
        }
        let uuid = uuid::Uuid::parse_str(&request.controller_id).map_err(|_| {
            ControlError::invalid("controllerId must be a canonical lowercase UUID.")
        })?;
        if uuid.to_string() != request.controller_id || uuid.is_nil() || request.action != ACTION {
            return Err(ControlError::invalid(
                "controllerId must be a canonical nonzero lowercase UUID.",
            ));
        }
        match request.op {
            Operation::Inspect => unreachable!(),
            Operation::Acquire | Operation::Renew => {
                if request.expires_at.is_none()
                    || request.sequence.is_some()
                    || request.events.is_some()
                {
                    return Err(ControlError::invalid(
                        "Acquire and renew require only controllerId and expiresAt.",
                    ));
                }
            }
            Operation::Release | Operation::Copy => {
                if request.expires_at.is_some()
                    || request.sequence.is_some()
                    || request.events.is_some()
                {
                    return Err(ControlError::invalid(
                        "Release and copy require only controllerId.",
                    ));
                }
            }
            Operation::Input => {
                if request.expires_at.is_some()
                    || !request
                        .sequence
                        .is_some_and(|seq| (1..=MAX_SAFE_INTEGER).contains(&seq))
                {
                    return Err(ControlError::invalid(
                        "Input requires a safe integer sequence starting at 1 and no expiresAt.",
                    ));
                }
                let events = request
                    .events
                    .as_ref()
                    .filter(|events| (1..=MAX_EVENTS).contains(&events.len()))
                    .ok_or_else(|| {
                        ControlError::invalid("Input requires between 1 and 64 events.")
                    })?;
                for event in events {
                    validate_event(event)?;
                }
            }
        }
        Ok(request)
    }

    fn deadline(&self, now: Instant) -> Result<(u64, Instant), ControlError> {
        let expires_at = self
            .expires_at
            .ok_or_else(|| ControlError::invalid("expiresAt is required."))?;
        let wall_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ControlError::invalid("The daemon clock is before the Unix epoch."))?
            .as_millis() as u64;
        let remaining = expires_at
            .checked_sub(wall_ms)
            .filter(|ms| (1..=MAX_LEASE_MS).contains(ms))
            .ok_or_else(|| {
                ControlError::invalid(
                    "expiresAt must be in the future and at most 30000 milliseconds ahead.",
                )
            })?;
        Ok((expires_at, now + Duration::from_millis(remaining)))
    }
}

struct Lease {
    controller_id: String,
    expires_at: u64,
    deadline: Instant,
    last_sequence: u64,
    outcome_unknown: bool,
    held: HeldInputs,
}

impl Lease {
    fn holds_custody(&self, now: Instant) -> bool {
        self.deadline > now
    }

    fn response(&self, status: &str) -> Value {
        json!({ "controllerId": self.controller_id, "expiresAt": self.expires_at,
            "lastSequence": self.last_sequence, "status": status })
    }
}

#[derive(Default)]
pub(crate) struct BrowserControl {
    lease: Option<Lease>,
    needs_observation: bool,
    last_released: Option<Lease>,
    pending_stream: VecDeque<PendingStreamInput>,
    stream_held: HeldInputs,
    /// Set before enqueueing so cancellation cannot lose an uncertain send.
    stream_outcome_unknown: bool,
}

struct PendingStreamInput {
    command: PendingCommand,
    event: Value,
    session_id: String,
}

impl PendingStreamInput {
    fn record(&self, response: &super::cdp::types::CdpMessage, held: &mut HeldInputs) {
        if response.error.is_none() {
            held.acknowledged(&self.session_id, &self.event);
        }
    }
}

async fn neutralize_held(client: &CdpClient, held: &mut HeldInputs) -> Result<(), ControlError> {
    let result = tokio::time::timeout(ACK_TIMEOUT, async {
        while let Some((session_id, event)) = held.next_release() {
            held.before_send(&session_id, &event);
            let (method, params) = input_command(event["type"].as_str().unwrap(), &event).unwrap();
            client
                .send_command_from(method, Some(params), Some(&session_id), InputSource::Human)
                .await?;
        }
        Ok::<_, String>(())
    })
    .await;
    if matches!(result, Ok(Ok(()))) {
        Ok(())
    } else {
        Err(ControlError::unknown())
    }
}

impl BrowserControl {
    pub(crate) fn needs_observation(&self) -> bool {
        self.needs_observation
    }
    pub(crate) fn observed(&mut self) {
        self.needs_observation = false;
    }

    /// Stateful low-level agent input shares custody and held-input tracking
    /// with dashboard input. Complete gestures keep their interaction helpers.
    pub(crate) async fn agent_input(
        &mut self,
        kind: &str,
        params: Value,
        client: &CdpClient,
        session_id: &str,
    ) -> Result<(), String> {
        self.drain_stream()
            .await
            .map_err(|error| format!("{}: {}", error.code, error.message))?;
        if let Some(error) = self.agent_error() {
            return Err(format!("{}: {}", error.code, error.message));
        }
        let event = stream_event(kind, &params);
        if !self
            .stream_held
            .accepts(std::iter::once((session_id, &event)))
        {
            return Err("At most 256 inputs may be held at once.".to_string());
        }
        let (method, params) = self
            .stream_held
            .command(session_id, kind, &event, None)
            .ok_or("Unsupported input type")?;
        self.stream_held.before_send(session_id, &event);
        self.stream_outcome_unknown = true;
        let mut pending = client
            .enqueue_command(method, Some(params), Some(session_id))
            .await?;
        let response = tokio::time::timeout(Duration::from_secs(30), pending.acknowledgment())
            .await
            .map_err(|_| format!("CDP command timed out: {}", method))??;
        self.stream_outcome_unknown = false;
        if let Some(error) = response.error {
            return Err(format!("CDP error ({}): {}", method, error));
        }
        self.stream_held.acknowledged(session_id, &event);
        Ok(())
    }

    /// Called under existing command/maintenance custody. No timer or task is
    /// added: expiry releases only inputs whose held state was acknowledged.
    pub(crate) async fn expire(
        &mut self,
        browser: Option<(&CdpClient, &str)>,
    ) -> Result<(), ControlError> {
        if self
            .lease
            .as_ref()
            .is_some_and(|lease| lease.deadline <= Instant::now())
        {
            self.release_held(browser).await?;
        }
        Ok(())
    }

    async fn release_held(
        &mut self,
        browser: Option<(&CdpClient, &str)>,
    ) -> Result<(), ControlError> {
        let Some(lease) = self.lease.as_mut() else {
            return Ok(());
        };
        if lease.held.next_release().is_none() {
            return Ok(());
        }
        let (client, _) = browser.ok_or_else(|| {
            ControlError::new(
                "browser_control_unavailable",
                "The controlled browser is no longer connected.",
            )
        })?;
        let was_unknown = lease.outcome_unknown;
        lease.outcome_unknown = true;
        neutralize_held(client, &mut lease.held).await?;
        lease.outcome_unknown = was_unknown;
        Ok(())
    }
    pub(crate) fn agent_error(&self) -> Option<ControlError> {
        self.lease.as_ref().filter(|lease| lease.holds_custody(Instant::now())).map(|lease| {
            if lease.outcome_unknown { ControlError::unknown() } else {
                ControlError::new("browser_controlled_by_user", "The user is controlling this browser. Wait until they release control before continuing.")
            }
        })
    }

    pub(crate) fn reset_browser(&mut self) {
        self.needs_observation = false;
        self.pending_stream.clear();
        self.stream_held = HeldInputs::default();
        self.stream_outcome_unknown = false;
        if let Some(lease) = self.lease.take() {
            self.last_released = Some(lease);
        }
    }

    /// Enqueue in wire order without waiting for each event's acknowledgment.
    /// The gate is held by the caller, and retained receipts bound takeover.
    pub(crate) async fn stream_input(
        &mut self,
        kind: &str,
        event: &Value,
        client: &CdpClient,
        session_id: Option<&str>,
    ) -> Result<(), ControlError> {
        self.expire(Some((client, session_id.unwrap_or_default())))
            .await?;
        if let Some(error) = self.agent_error() {
            return Err(error);
        }
        if self.stream_outcome_unknown {
            return Err(ControlError::unknown());
        }

        while let Some(pending) = self.pending_stream.front_mut() {
            match pending.command.try_acknowledgment() {
                Ok(Some(response)) => {
                    pending.record(&response, &mut self.stream_held);
                    self.pending_stream.pop_front();
                }
                Ok(None) => break,
                Err(_) => {
                    self.stream_outcome_unknown = true;
                    return Err(ControlError::unknown());
                }
            }
        }
        if self.pending_stream.len() >= MAX_PENDING_STREAM_INPUTS {
            // Backpressure preserves a press/release following a long mouse
            // sweep. Dropping it merely because the ledger is full would
            // leave the pointer held or make the click disappear.
            match tokio::time::timeout(ACK_TIMEOUT, self.pending_stream.front_mut().unwrap().command.acknowledgment()).await {
                Ok(Ok(response)) => { self.pending_stream.front().unwrap().record(&response, &mut self.stream_held); self.pending_stream.pop_front(); }
                Ok(Err(_)) => { self.stream_outcome_unknown = true; return Err(ControlError::unknown()); }
                Err(_) => return Err(ControlError::new("browser_control_input_busy", "The browser has not acknowledged earlier input. Wait for it before sending more.")),
            }
        }
        let sid = session_id.unwrap_or_default();
        let pending_buttons = self
            .pending_stream
            .iter()
            .rev()
            .find(|pending| pending.session_id == sid && pending.event["type"] == "input_mouse")
            .and_then(|pending| pending.event["buttons"].as_i64())
            .map(|buttons| buttons as i32);
        let Some((method, params)) = self.stream_held.command(sid, kind, event, pending_buttons)
        else {
            return Ok(());
        };
        let event = if method == "Input.insertText" {
            json!({ "type": "input_keyboard", "eventType": "insertText", "text": params["text"] })
        } else {
            stream_event(kind, &params)
        };
        if !self.stream_held.accepts(
            self.pending_stream
                .iter()
                .map(|pending| (pending.session_id.as_str(), &pending.event))
                .chain(std::iter::once((sid, &event))),
        ) {
            return Err(ControlError::invalid(
                "At most 256 inputs may be held at once.",
            ));
        }
        self.stream_held.before_send(sid, &event);
        self.stream_outcome_unknown = true;
        let pending = tokio::time::timeout(
            ACK_TIMEOUT,
            client.enqueue_command_from(method, Some(params), session_id, InputSource::Human),
        )
        .await
        .map_err(|_| ControlError::unknown())?
        .map_err(|_| ControlError::unknown())?;
        self.pending_stream.push_back(PendingStreamInput {
            command: pending,
            event,
            session_id: sid.to_string(),
        });
        self.stream_outcome_unknown = false;
        Ok(())
    }

    async fn drain_stream(&mut self) -> Result<(), ControlError> {
        if self.stream_outcome_unknown {
            return Err(ControlError::unknown());
        }
        tokio::time::timeout(ACK_TIMEOUT, async {
            while let Some(pending) = self.pending_stream.front_mut() {
                match pending.command.acknowledgment().await {
                    Ok(response) => pending.record(&response, &mut self.stream_held),
                    Err(_) => {
                        self.stream_outcome_unknown = true;
                        return Err(ControlError::unknown());
                    }
                }
                self.pending_stream.pop_front();
            }
            Ok(())
        })
        .await
        .map_err(|_| ControlError::unknown())?
    }

    #[cfg(test)]
    pub(crate) async fn execute(
        &mut self,
        request: ControlRequest,
        browser: Option<(&CdpClient, &str)>,
    ) -> Result<Value, ControlError> {
        self.execute_with_page(request, browser, None).await
    }

    pub(crate) async fn execute_with_page(
        &mut self,
        request: ControlRequest,
        browser: Option<(&CdpClient, &str)>,
        mut page: Option<super::actions::ControlPage<'_>>,
    ) -> Result<Value, ControlError> {
        let now = Instant::now();
        match request.op {
            Operation::Inspect => {
                Ok(json!({ "supported": true, "controlled": self.agent_error().is_some() }))
            }
            Operation::Acquire => {
                self.expire(browser).await?;
                request.deadline(now)?;
                if let Some(lease) = self.lease.as_ref() {
                    if lease.controller_id == request.controller_id {
                        self.require_owner(&request.controller_id)?;
                        return Ok(lease.response("controlled"));
                    }
                    if lease.holds_custody(now) {
                        return Err(ControlError::new(
                            "browser_control_conflict",
                            "Another user already controls this browser.",
                        ));
                    }
                }
                if self
                    .last_released
                    .as_ref()
                    .is_some_and(|lease| lease.controller_id == request.controller_id)
                {
                    return Err(ControlError::new(
                        "browser_control_stale",
                        "This controller was released. Acquire with a new controllerId.",
                    ));
                }
                if browser.is_none() {
                    return Err(ControlError::new(
                        "browser_control_unavailable",
                        "This session has no active CDP browser to control.",
                    ));
                }
                self.drain_stream().await?;
                self.stream_outcome_unknown = true;
                neutralize_held(browser.unwrap().0, &mut self.stream_held).await?;
                self.stream_outcome_unknown = false;
                let (expires_at, deadline) = request.deadline(Instant::now())?;
                self.lease = Some(Lease {
                    controller_id: request.controller_id,
                    expires_at,
                    deadline,
                    last_sequence: 0,
                    outcome_unknown: false,
                    held: HeldInputs::default(),
                });
                self.needs_observation = true;
                if let Some((client, session)) = browser {
                    client.rotate_page_generation(session);
                }
                Ok(self.lease.as_ref().unwrap().response("controlled"))
            }
            Operation::Renew => {
                self.require_owner(&request.controller_id)?;
                let (expires_at, deadline) = request.deadline(now)?;
                let lease = self.lease.as_mut().unwrap();
                lease.expires_at = expires_at;
                lease.deadline = deadline;
                Ok(lease.response("controlled"))
            }
            Operation::Release => {
                if self
                    .lease
                    .as_ref()
                    .is_some_and(|lease| lease.controller_id == request.controller_id)
                {
                    self.release_held(browser).await?;
                    let lease = self.lease.take().unwrap();
                    let response = lease.response("released");
                    self.last_released = Some(lease);
                    return Ok(response);
                }
                self.last_released
                    .as_ref()
                    .filter(|lease| lease.controller_id == request.controller_id)
                    .map(|lease| lease.response("released"))
                    .ok_or_else(|| {
                        ControlError::new(
                            "browser_control_stale",
                            "This controller does not own the browser.",
                        )
                    })
            }
            Operation::Copy => {
                let deadline = self
                    .require_owner(&request.controller_id)?
                    .deadline
                    .min(Instant::now() + ACK_TIMEOUT);
                let selected =
                    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), async {
                        page.as_mut()
                            .ok_or(super::selection::SelectionError::Unavailable)?
                            .selected_text()
                            .await
                    })
                    .await;
                let lease = self.require_owner(&request.controller_id)?;
                let text = match selected {
                    Ok(Ok(text)) => text,
                    Ok(Err(super::selection::SelectionError::TooLarge)) => {
                        return Err(ControlError::new(
                            "browser_control_copy_too_large",
                            "Select at most 1 MiB of text to copy.",
                        ))
                    }
                    _ => return Err(ControlError::new(
                        "browser_control_unavailable",
                        "The browser selection could not be read. Select the text again and retry.",
                    )),
                };
                let mut response = lease.response("copied");
                response["clipboard"] =
                    json!({ "bytes": text.len(), "text": text, "complete": true });
                Ok(response)
            }
            Operation::Input => {
                self.require_owner(&request.controller_id)?;
                let sequence = request.sequence.unwrap();
                let lease = self.lease.as_mut().unwrap();
                if sequence <= lease.last_sequence {
                    return Ok(lease.response("duplicate"));
                }
                if sequence != lease.last_sequence + 1 {
                    return Err(ControlError::new(
                        "browser_control_sequence_gap",
                        format!("Expected input sequence {}.", lease.last_sequence + 1),
                    ));
                }
                let (client, session_id) = browser.ok_or_else(|| {
                    ControlError::new(
                        "browser_control_unavailable",
                        "The controlled browser is no longer connected.",
                    )
                })?;
                if !lease.held.accepts(
                    request
                        .events
                        .as_ref()
                        .unwrap()
                        .iter()
                        .map(|event| (session_id, event)),
                ) {
                    return Err(ControlError::invalid(
                        "At most 256 inputs may be held at once.",
                    ));
                }
                if let Some(page) = page.as_ref() {
                    page.validate_events(request.events.as_ref().unwrap())
                        .await
                        .map_err(ControlError::invalid)?;
                }
                // Reserve before the first await. A cancelled command or a lost
                // reply must never leave the sequence available for replay.
                lease.outcome_unknown = true;
                let deadline = lease.deadline.min(Instant::now() + ACK_TIMEOUT);
                let result =
                    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), async {
                        for event in request.events.unwrap() {
                            if Instant::now() >= deadline {
                                return Err("Input deadline elapsed".to_string());
                            }
                            if matches!(event["type"].as_str(), Some("viewport" | "navigation")) {
                                page.as_mut()
                                    .ok_or("Browser page control is unavailable")?
                                    .apply(&event)
                                    .await?;
                                continue;
                            }
                            lease.held.before_send(session_id, &event);
                            let (method, params) = lease
                                .held
                                .command(session_id, event["type"].as_str().unwrap(), &event, None)
                                .unwrap();
                            client
                                .send_command_from(
                                    method,
                                    Some(params),
                                    Some(session_id),
                                    InputSource::Human,
                                )
                                .await?;
                            lease.held.acknowledged(session_id, &event);
                        }
                        Ok::<_, String>(())
                    })
                    .await;
                if !matches!(result, Ok(Ok(()))) {
                    return Err(ControlError::unknown());
                }
                lease.last_sequence = sequence;
                lease.outcome_unknown = false;
                Ok(lease.response("applied"))
            }
        }
    }

    fn require_owner(&self, controller_id: &str) -> Result<&Lease, ControlError> {
        let lease = self
            .lease
            .as_ref()
            .filter(|lease| lease.controller_id == controller_id)
            .ok_or_else(|| {
                ControlError::new(
                    "browser_control_stale",
                    "This controller does not own the browser.",
                )
            })?;
        if lease.deadline <= Instant::now() {
            return Err(ControlError::new(
                "browser_control_expired",
                "Control expired. Acquire a new controller before sending input.",
            ));
        }
        if lease.outcome_unknown {
            return Err(ControlError::unknown());
        }
        Ok(lease)
    }
}

fn fields(event: &Value, allowed: &[&str]) -> Result<(), ControlError> {
    let object = event
        .as_object()
        .ok_or_else(|| ControlError::invalid("Every input event must be an object."))?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(ControlError::invalid(
            "Input event contains an unsupported field.",
        ));
    }
    Ok(())
}

fn one_of(event: &Value, key: &str, choices: &[&str], required: bool) -> Result<(), ControlError> {
    match event.get(key) {
        None if !required => Ok(()),
        Some(value) if value.as_str().is_some_and(|value| choices.contains(&value)) => Ok(()),
        _ => Err(ControlError::invalid(format!("Invalid input {}.", key))),
    }
}

fn number(
    event: &Value,
    key: &str,
    min: f64,
    max: f64,
    integer: bool,
    required: bool,
) -> Result<(), ControlError> {
    match event.get(key) {
        None if !required => Ok(()),
        Some(value)
            if value.as_f64().is_some_and(|n| {
                n.is_finite() && n >= min && n <= max && (!integer || value.as_u64().is_some())
            }) =>
        {
            Ok(())
        }
        _ => Err(ControlError::invalid(format!("Invalid input {}.", key))),
    }
}

fn validate_event(event: &Value) -> Result<(), ControlError> {
    match event.get("type").and_then(Value::as_str) {
        Some("navigation") => {
            fields(event, &["type", "action", "url"])?;
            one_of(
                event,
                "action",
                &["navigate", "back", "forward", "reload"],
                true,
            )?;
            if event["action"] == "navigate" {
                let url = event["url"]
                    .as_str()
                    .filter(|url| !url.trim().is_empty())
                    .ok_or_else(|| ControlError::invalid("Navigation requires a URL."))?;
                url::Url::parse(&crate::commands::normalize_navigation_url(url))
                    .map_err(|_| ControlError::invalid("The navigation URL is invalid."))?;
            } else if event.get("url").is_some() {
                return Err(ControlError::invalid("Only navigate accepts a URL."));
            }
            return Ok(());
        }
        Some("viewport") => {
            fields(event, &["type", "width", "height"])?;
            number(event, "width", 1.0, 32768.0, true, true)?;
            number(event, "height", 1.0, 32768.0, true, true)?;
            return Ok(());
        }
        Some("input_mouse") => {
            fields(
                event,
                &[
                    "type",
                    "eventType",
                    "x",
                    "y",
                    "button",
                    "buttons",
                    "clickCount",
                    "deltaX",
                    "deltaY",
                    "modifiers",
                ],
            )?;
            one_of(
                event,
                "eventType",
                &["mouseMoved", "mousePressed", "mouseReleased", "mouseWheel"],
                true,
            )?;
            one_of(
                event,
                "button",
                &["none", "left", "middle", "right", "back", "forward"],
                false,
            )?;
            for key in ["x", "y"] {
                number(event, key, 0.0, 32768.0, false, true)?;
            }
            number(event, "buttons", 0.0, 31.0, true, false)?;
            number(event, "clickCount", 0.0, 3.0, true, false)?;
            for key in ["deltaX", "deltaY"] {
                number(event, key, -32768.0, 32768.0, false, false)?;
            }
        }
        Some("input_keyboard") => {
            if event["eventType"] == "insertText" {
                fields(event, &["type", "eventType", "text"])?;
                return if event
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.is_empty() && text.len() <= 4096)
                {
                    Ok(())
                } else {
                    Err(ControlError::invalid(
                        "Text insertion requires between 1 and 4096 UTF-8 bytes.",
                    ))
                };
            }
            fields(
                event,
                &[
                    "type",
                    "eventType",
                    "key",
                    "code",
                    "text",
                    "windowsVirtualKeyCode",
                    "modifiers",
                ],
            )?;
            one_of(
                event,
                "eventType",
                &["keyDown", "keyUp", "rawKeyDown", "char"],
                true,
            )?;
            for (key, limit) in [("key", 256), ("code", 256), ("text", 12)] {
                if event
                    .get(key)
                    .is_some_and(|value| value.as_str().is_none_or(|s| s.len() > limit))
                {
                    return Err(ControlError::invalid(format!(
                        "Input {} must be a string of at most {} bytes.",
                        key, limit
                    )));
                }
            }
            // Chromium reserves the fourth UTF-16 slot for the terminating
            // NUL. Pasted or composed text uses explicit insertText instead.
            if event
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| text.encode_utf16().count() > 3)
            {
                return Err(ControlError::invalid(
                    "Key event text must fit 3 UTF-16 units; use insertText for longer text.",
                ));
            }
            if !["key", "text"].iter().any(|key| {
                event
                    .get(*key)
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty())
            }) {
                return Err(ControlError::invalid(
                    "Keyboard input requires key or text.",
                ));
            }
            number(event, "windowsVirtualKeyCode", 0.0, 255.0, true, false)?;
        }
        Some("input_touch") => {
            fields(event, &["type", "eventType", "touchPoints", "modifiers"])?;
            one_of(
                event,
                "eventType",
                &["touchStart", "touchMove", "touchEnd", "touchCancel"],
                true,
            )?;
            let points = event
                .get("touchPoints")
                .and_then(Value::as_array)
                .filter(|points| points.len() <= 10)
                .ok_or_else(|| {
                    ControlError::invalid("touchPoints must contain at most 10 points.")
                })?;
            let end = matches!(
                event["eventType"].as_str(),
                Some("touchEnd" | "touchCancel")
            );
            if points.is_empty() != end {
                return Err(ControlError::invalid(
                    "Touch start and move require points; touch end and cancel require no points.",
                ));
            }
            let mut ids = std::collections::HashSet::new();
            for point in points {
                fields(
                    point,
                    &[
                        "x",
                        "y",
                        "id",
                        "radiusX",
                        "radiusY",
                        "rotationAngle",
                        "force",
                    ],
                )?;
                for key in ["x", "y"] {
                    number(point, key, 0.0, 32768.0, false, true)?;
                }
                number(point, "id", 0.0, i32::MAX as f64, true, true)?;
                if !ids.insert(point["id"].as_u64().unwrap()) {
                    return Err(ControlError::invalid("Touch point ids must be unique."));
                }
                for key in ["radiusX", "radiusY"] {
                    number(point, key, 0.0, 32768.0, false, false)?;
                }
                number(point, "rotationAngle", 0.0, 360.0, false, false)?;
                number(point, "force", 0.0, 1.0, false, false)?;
            }
        }
        _ => return Err(ControlError::invalid("Unsupported input event type.")),
    }
    number(event, "modifiers", 0.0, 15.0, true, false)
}

#[cfg(test)]
#[path = "browser_control_tests.rs"]
mod tests;
