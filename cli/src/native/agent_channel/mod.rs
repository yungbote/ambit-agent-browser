//! The daemon's agent channel (agent-channel contract r6.1): the host's one
//! connection to the daemon for host-bound browser operations, reached
//! through the toolbox's `…/agent/channel` route under the action
//! `ambit_browser_agent`. The channel changes the transport, never the
//! operation: every step is prepared as the spawned MCP client prepares a
//! call (`mcp::host_bound::HostFlags::prepare`) and dispatched through the
//! same host feedback path (`actions::run_host_command`).
//!
//! This module is the protocol: frames in receipt order, one reply line per
//! frame, the `hello`, fencing, the ledger and the reply bounds. Where a
//! step runs is the `Browser` it is given: the daemon's own browser
//! (`dispatch::DaemonBrowser`), or a script in tests.
//!
//! The endpoint always reads ahead, so it sees the connection end while a
//! step runs: that step completes and is recorded, and nothing after it
//! starts. At most one `sequence` is answered at a time; one that arrives
//! while another is unanswered is refused and recorded not started.

use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{stream::FuturesUnordered, FutureExt, StreamExt};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

pub(crate) mod ceiling;
pub(crate) mod dispatch;
pub(crate) mod frame;
pub(crate) mod landed;
pub(crate) mod ledger;
pub(crate) mod observe;
pub(crate) mod reply;
pub(crate) mod step;
pub(crate) mod target;

use frame::{Binding, ChannelId, Frame, FrameId, Hello, Invalid, OpStatus, Owner, Sequence};
use ledger::{Arrival, Ledger, OtherAction};
use reply::{Retained, Slot, MAX_REPLY_BYTES, MAX_REQUEST_BYTES, RETAINED_ABOVE};
use step::{PreparedStep, EXCLUDED_OPS};

use crate::mcp::host_bound::{catalog_identity, HostFlags};
use crate::native::site_sessions::protocol::{self as site_protocol, Request as SiteRequest};
use crate::native::stream::IdleActivity;

/// The daemon action the toolbox's agent route names on every line.
pub(crate) const ACTION: &str = "ambit_browser_agent";

/// What this daemon adds to the operations it runs: the motion layer's
/// glide, key cadence, per-sample interruption and end-of-travel hit test.
pub(crate) const FEATURES: &[&str] = &["motion"];

/// Frames written and unanswered that the toolbox allows.
const MAX_IN_FLIGHT: usize = 4;
/// Candidates an observation lists.
pub(crate) const CANDIDATES: usize = 60;

const PROTOCOL_REFUSED: &str = "agent_channel_protocol";
const BINDING_REFUSED: &str = "agent_channel_binding";
const FENCED: &str = "agent_channel_fenced";
const REJECTED: &str = "browser_operation_rejected";
const FENCED_MESSAGE: &str = "A newer owner of this Action reached the browser, so this frame did not start and this channel starts nothing more.";

/// What this daemon is, as a `hello`'s binding must state it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Identity {
    pub(crate) namespace: String,
    pub(crate) session: String,
    pub(crate) require_sandbox: bool,
    pub(crate) browser_host: bool,
}

impl Identity {
    /// This daemon's: the namespace of the environment it inherited from the
    /// client that started it, its session and its launch policy.
    pub(crate) fn of_daemon(session: &str) -> Self {
        Self {
            namespace: std::env::var("AGENT_BROWSER_NAMESPACE").unwrap_or_default(),
            session: session.to_string(),
            require_sandbox: crate::native::actions::require_sandbox_from_env(),
            browser_host: crate::native::workspace_role::current()
                .is_ok_and(|role| role.is_browser_host()),
        }
    }
}

/// A frame's context for the steps it runs.
pub(crate) struct FrameContext<'a> {
    pub(crate) channel: ChannelId,
    pub(crate) owner: Option<Owner>,
    pub(crate) binding: &'a Binding,
    /// The Action's private browser directory.
    pub(crate) directory: &'a Path,
    /// When the frame arrived: its steps were chosen before then.
    pub(crate) received_at: Instant,
}

/// What a frame asks for after its last executed step.
pub(crate) struct Asks<'a> {
    pub(crate) observe: bool,
    pub(crate) resolve: &'a [String],
    pub(crate) capture: bool,
}

/// Durations in microseconds on the sandbox's monotonic clock.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct StepTiming {
    /// Waiting for command custody, including retaking it for a landed read.
    pub(crate) queue_us: u64,
    /// From first holding custody to the step's result.
    pub(crate) op_us: u64,
    /// The part of `op_us` spent in paced input.
    pub(crate) motion_us: u64,
}

/// One step as it ran.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StepRecord {
    pub(crate) op: String,
    /// The host-bound `CallToolResult`, as the spawned client answers it,
    /// its feedback moved to the frame.
    pub(crate) result: Value,
    pub(crate) succeeded: bool,
    pub(crate) landed: Option<Value>,
    pub(crate) timing: StepTiming,
}

impl StepRecord {
    fn to_json(&self) -> Value {
        let mut step = json!({ "op": self.op, "result": self.result, "timing": {
            "queueUs": self.timing.queue_us, "opUs": self.timing.op_us,
            "motionUs": self.timing.motion_us } });
        if let Some(landed) = &self.landed {
            step["landed"] = landed.clone();
        }
        step
    }
}

/// What a frame ends with: the page it ended on and what it asked for.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Finish {
    /// The feedback object: `{namespace, session, page, capture}`.
    pub(crate) browser: Value,
    pub(crate) observation: Option<Value>,
    pub(crate) resolved: Option<Value>,
    pub(crate) queue_us: u64,
    pub(crate) observe_us: u64,
}

/// Where a channel's steps run.
pub(crate) trait Browser: Sync {
    fn action_files(
        &self,
        _channel: ChannelId,
        _owner: Owner,
        _files: Option<Vec<crate::native::playwright::files::Receipt>>,
        _ledger: &Ledger,
    ) -> impl Future<Output = Result<Value, String>> + Send {
        async { Err("This browser does not serve native Action staging.".into()) }
    }
    fn program_request(
        &self,
        _channel: ChannelId,
        _owner: Owner,
        _request: crate::native::playwright::remote::Request,
        _ledger: &Ledger,
    ) -> impl Future<Output = Result<Value, String>> + Send {
        async { Err("This browser does not serve remote programs.".into()) }
    }
    fn fence_program(&self, _owner: Owner) -> impl Future<Output = ()> + Send {
        async {}
    }
    fn site_request(
        &self,
        _channel: ChannelId,
        _request: SiteRequest,
        _ledger: Arc<Ledger>,
    ) -> impl Future<Output = Result<Value, &'static str>> + Send {
        async { Err("This browser does not serve site custody.") }
    }

    fn custody_events(&self) -> Option<tokio::sync::broadcast::Receiver<(ChannelId, Value)>> {
        None
    }
    fn redact(&self, _value: &mut Value) {}
    fn redact_step(&self, _op: &str, value: &mut Value) {
        self.redact(value);
    }
    /// Runs one prepared step under command custody taken for it alone.
    fn step(
        &self,
        frame: &FrameContext<'_>,
        step: &PreparedStep,
    ) -> impl Future<Output = StepRecord> + Send;

    /// After the last executed step: the page identity, then the
    /// observation, the resolutions and the capture the frame asked for.
    fn finish(
        &self,
        frame: &FrameContext<'_>,
        asks: &Asks<'_>,
    ) -> impl Future<Output = Finish> + Send;

    /// Immediate authority fence when the connection ends. Ordinary input
    /// settlement remains in end after its currently executing step finishes.
    fn channel_closed(&self, _channel: ChannelId) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// `channel` ended and starts nothing more: the input its steps left
    /// held is released, unless another channel's step acted since.
    fn end(&self, channel: ChannelId) -> impl Future<Output = ()> + Send;
}

/// The daemon's agent channels and the browser their steps reach.
pub(crate) struct AgentChannels {
    endpoint: Endpoint,
    browser: dispatch::DaemonBrowser,
}

impl AgentChannels {
    /// The channels of the daemon for `session`, whose command custody is
    /// `state`.
    pub(crate) fn new(
        session: &str,
        state: Arc<tokio::sync::Mutex<crate::native::actions::DaemonState>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            endpoint: Endpoint::new(Identity::of_daemon(session)),
            browser: dispatch::DaemonBrowser::new(state),
        })
    }

    /// Serves a connection whose line `first` named the agent action.
    pub(crate) async fn serve<R, W>(
        &self,
        idle: &IdleActivity,
        first: String,
        reader: &mut BufReader<R>,
        writer: &mut W,
        pending: &mut VecDeque<String>,
        partial: &mut Vec<u8>,
    ) where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        self.endpoint
            .serve(&self.browser, idle, first, reader, writer, pending, partial)
            .await
    }
}

/// sha256 of the daemon's own executable (`/proc/self/exe`), the digest the
/// host pins for this driver generation. It is computed once per process,
/// from the first agent connection, on a thread of its own: a daemon that
/// stops never waits for it, as a runtime waits for its blocking tasks.
struct ArtifactDigest {
    value: Arc<std::sync::OnceLock<Result<String, String>>>,
    ready: Arc<tokio::sync::Notify>,
    started: std::sync::Once,
}

impl ArtifactDigest {
    fn of_executable() -> Self {
        Self {
            value: Arc::default(),
            ready: Arc::default(),
            started: std::sync::Once::new(),
        }
    }

    fn start(&self) {
        self.started.call_once(|| {
            let (value, ready) = (self.value.clone(), self.ready.clone());
            let hashing = std::thread::Builder::new()
                .name("agent-channel-digest".into())
                .spawn(move || {
                    let _ = value.set(executable_digest());
                    ready.notify_waiters();
                });
            if let Err(error) = hashing {
                let _ = self.value.set(Err(error.to_string()));
            }
        });
    }

    async fn get(&self) -> Result<String, String> {
        self.start();
        loop {
            let notified = self.ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(value) = self.value.get() {
                return value.clone();
            }
            notified.await;
        }
    }
}

/// The daemon's agent channels: what outlives any one connection.
pub(crate) struct Endpoint {
    ledger: Arc<Ledger>,
    identity: Identity,
    artifact: ArtifactDigest,
}

impl Endpoint {
    pub(crate) fn new(identity: Identity) -> Self {
        Self {
            ledger: Arc::new(Ledger::default()),
            identity,
            artifact: ArtifactDigest::of_executable(),
        }
    }

    /// An endpoint that states `digest` as its executable's: hashing a test
    /// binary of a gigabyte is not what a protocol test measures.
    #[cfg(test)]
    pub(crate) fn with_digest(identity: Identity, digest: &str) -> Self {
        let endpoint = Self::new(identity);
        let _ = endpoint.artifact.value.set(Ok(digest.to_string()));
        endpoint
    }

    /// Serves one connection whose first line, `first`, carried the agent
    /// action, until the connection ends. The lines the connection had
    /// already read ahead (`pending`, `partial`) are its next frames.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn serve<R, W, B>(
        &self,
        browser: &B,
        idle: &IdleActivity,
        first: String,
        reader: &mut BufReader<R>,
        writer: &mut W,
        pending: &mut VecDeque<String>,
        partial: &mut Vec<u8>,
    ) where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
        B: Browser,
    {
        self.artifact.start();
        let mut inbox = Inbox {
            reader,
            partial,
            frames: VecDeque::new(),
            channel: None,
            unanswered_sequences: 0,
            queued_bytes: 0,
            closed: false,
        };
        inbox.receive(first, &self.ledger);
        for line in pending.drain(..) {
            inbox.receive(line, &self.ledger);
        }
        let mut session: Option<Session> = None;
        let mut events = browser.custody_events();
        loop {
            let received = loop {
                tokio::select! {
                    received=inbox.next(&self.ledger)=>break received,
                    event=async { events.as_mut().unwrap().recv().await }, if events.is_some()=>{
                        if let Ok((channel,event))=event {
                            if session.as_ref().is_some_and(|session|session.channel==channel) && write_line(writer,&event).await.is_err() {inbox.close(&self.ledger);}
                        }
                    }
                }
            };
            let Some(received) = received else {
                if let Some(session) = &session {
                    browser.channel_closed(session.channel).await;
                }
                break;
            };
            idle.mark();
            let sequence = received.sequence;
            let outcome = {
                let work = self.process(browser, session.as_ref(), received);
                tokio::pin!(work);
                let mut completed = None;
                let mut independent = FuturesUnordered::new();
                loop {
                    if independent.is_empty() {
                        if let Some(output) = completed.take() {
                            break output;
                        }
                    }
                    tokio::select! {
                        biased;
                        output=&mut work, if completed.is_none()=> { completed=Some(output); }
                        output=independent.next(), if !independent.is_empty()=> {
                            if let Some(Some((reply,_)))=output {
                                if write_line(writer,&reply).await.is_err() { inbox.close(&self.ledger); }
                            }
                        }
                        event=async { events.as_mut().unwrap().recv().await }, if events.is_some()=>{
                            if let Ok((channel,event))=event {
                                if session.as_ref().is_some_and(|session|session.channel==channel) && write_line(writer,&event).await.is_err() {inbox.close(&self.ledger);}
                            }
                        }
                        line=inbox.read_line(), if !inbox.closed=>{
                            if let Some(line)=line {
                                inbox.receive(line,&self.ledger);
                                if inbox.frames.back().is_some_and(|received| match &received.frame {
                                    Ok((_,Frame::Site(_)))=>true,
                                    Ok((_,Frame::Files(_,_)))=>true,
                                    Ok((_,Frame::Program(_,request)))=>request.independent(),
                                    _=>false,
                                }) && independent.len()<MAX_IN_FLIGHT {
                                    let received=inbox.frames.pop_back().unwrap();
                                    inbox.queued_bytes=inbox.queued_bytes.saturating_sub(received.bytes);
                                    independent.push(self.process(browser,session.as_ref(),received).boxed());
                                }
                            } else { inbox.close(&self.ledger); }
                        }
                    }
                    if inbox.closed {
                        if let Some(session) = &session {
                            browser.channel_closed(session.channel).await;
                        }
                    }
                }
            };
            // A line without a usable id cannot be answered in step.
            let Some((reply, established)) = outcome else {
                break;
            };
            if let Some(established) = established {
                inbox.channel = Some(established.channel);
                session = Some(established);
            }
            let mut line = reply.to_string();
            line.push('\n');
            if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
                break;
            }
            if sequence {
                inbox.unanswered_sequences = inbox.unanswered_sequences.saturating_sub(1);
            }
            if inbox.closed {
                break;
            }
        }
        if let Some(session) = session {
            self.ledger.end(session.channel);
            browser.end(session.channel).await;
        }
    }

    async fn process<B: Browser>(
        &self,
        browser: &B,
        session: Option<&Session>,
        received: Received,
    ) -> Option<(Value, Option<Session>)> {
        let (id, frame) = match received.frame {
            Err(Invalid { id: None, .. }) => return None,
            Err(Invalid {
                id: Some(id),
                message,
            }) => {
                if let (Some(session), true) = (session, received.sequence) {
                    self.ledger.not_started(session.channel, id, received.owner);
                }
                return Some((
                    refusal(id, PROTOCOL_REFUSED, message, received.sequence),
                    None,
                ));
            }
            Ok(frame) => frame,
        };
        Some(match (frame, session) {
            (Frame::Hello(hello), None) => {
                let owner = hello.owner;
                let result = self.hello(id, hello).await;
                if result.1.is_some() {
                    if let Some(owner) = owner {
                        browser.fence_program(owner).await;
                    }
                }
                result
            }
            (Frame::Hello(_), Some(_)) => (
                refusal(
                    id,
                    PROTOCOL_REFUSED,
                    "This channel already said hello.",
                    false,
                ),
                None,
            ),
            (Frame::Sequence(sequence), Some(session)) => (
                self.sequence(
                    browser,
                    session,
                    id,
                    sequence,
                    received.refused,
                    received.at,
                )
                .await,
                None,
            ),
            (Frame::OpStatus(query), Some(session)) => {
                if self.ledger.register(session.channel, query.owner).is_ok() {
                    browser.fence_program(query.owner).await;
                }
                (self.op_status(session, id, query).await, None)
            }
            (Frame::Site(request), Some(session)) => {
                let result =
                    if session.binding.browser_host && self.ledger.may_continue(session.channel) {
                        browser
                            .site_request(session.channel, request, self.ledger.clone())
                            .await
                    } else {
                        Err("Site custody requires this daemon's immutable browser-host role.")
                    };
                let reply = match result {
                    Ok(data) => json!({"id":id,"success":true,"data":data}),
                    Err(error) => refusal(id, "browser_site_custody_refused", error, false),
                };
                (reply, None)
            }
            (Frame::Program(owner, request), Some(session)) => {
                let reply = if !session.binding.browser_host {
                    refusal(
                        id,
                        BINDING_REFUSED,
                        "Remote programs require this daemon's immutable browser-host role.",
                        false,
                    )
                } else if self.ledger.register(session.channel, owner).is_err() {
                    refusal(id, FENCED, FENCED_MESSAGE, false)
                } else {
                    browser.fence_program(owner).await;
                    match browser
                        .program_request(session.channel, owner, request, &self.ledger)
                        .await
                    {
                        Ok(data) => json!({"id":id,"success":true,"data":data}),
                        Err(error) => {
                            let code = crate::native::browser::error_code(&error)
                                .unwrap_or(REJECTED)
                                .to_owned();
                            refusal(id, &code, error, false)
                        }
                    }
                };
                (reply, None)
            }
            (Frame::Files(owner, files), Some(session)) => {
                let reply = if !session.binding.browser_host {
                    refusal(
                        id,
                        BINDING_REFUSED,
                        "Native Action staging requires the immutable browser-host role.",
                        false,
                    )
                } else if self.ledger.register(session.channel, owner).is_err() {
                    refusal(id, FENCED, FENCED_MESSAGE, false)
                } else {
                    browser.fence_program(owner).await;
                    match browser
                        .action_files(session.channel, owner, files, &self.ledger)
                        .await
                    {
                        Ok(data) => json!({"id":id,"success":true,"data":data}),
                        Err(error) => refusal(id, REJECTED, error, false),
                    }
                };
                (reply, None)
            }
            (Frame::Sequence(_), None)
            | (Frame::OpStatus(_), None)
            | (Frame::Site(_), None)
            | (Frame::Program(_, _), None)
            | (Frame::Files(_, _), None) => (
                refusal(
                    id,
                    PROTOCOL_REFUSED,
                    "A channel's first frame is its hello.",
                    received.sequence,
                ),
                None,
            ),
        })
    }

    /// The handshake: the binding is compared with this daemon, never loaded
    /// into its environment; then the channel's ledger key is opened and the
    /// Action's owner, if any, registered.
    async fn hello(&self, id: FrameId, hello: Hello) -> (Value, Option<Session>) {
        let binding = &hello.binding;
        if binding.namespace != self.identity.namespace
            || binding.session != self.identity.session
            || binding.require_sandbox != self.identity.require_sandbox
            || binding.browser_host != self.identity.browser_host
        {
            return (
                refusal(
                    id,
                    BINDING_REFUSED,
                    "The binding is not this browser's: its namespace, session, sandbox policy or workspace role differ.",
                    false,
                ),
                None,
            );
        }
        let flags = match HostFlags::new(&binding.namespace, &binding.session, binding.theme) {
            Ok(flags) => flags,
            Err(error) => return (refusal(id, BINDING_REFUSED, error, false), None),
        };
        let digest = match self.artifact.get().await {
            Ok(digest) => digest,
            Err(error) => {
                return (
                    refusal(
                        id,
                        PROTOCOL_REFUSED,
                        format!("The browser driver cannot state its own digest ({error}), so it serves no agent channel."),
                        false,
                    ),
                    None,
                )
            }
        };
        if !self.ledger.open(hello.channel) {
            return (
                refusal(
                    id,
                    PROTOCOL_REFUSED,
                    "This channel id was used before; open every channel with a fresh one.",
                    false,
                ),
                None,
            );
        }
        self.ledger.receive(hello.channel, id, Arrival::Other);
        if let Some(owner) = hello.owner {
            if self.ledger.register(hello.channel, owner).is_err() {
                self.ledger.end(hello.channel);
                return (refusal(id, FENCED, FENCED_MESSAGE, false), None);
            }
        }
        let reply = json!({ "id": id, "success": true, "data": {
            "protocol": frame::PROTOCOL,
            "catalog": catalog_identity(),
            "driverArtifactDigest": digest,
            "features": FEATURES,
            "sessionCustody": site_protocol::VERSION,
            "excludedOps": EXCLUDED_OPS,
            "limits": {
                "maxInFlight": MAX_IN_FLIGHT, "maxRequestBytes": MAX_REQUEST_BYTES,
                "maxReplyBytes": MAX_REPLY_BYTES, "ledgerEntries": ledger::ENTRIES,
                "maxCustodyBytes":site_protocol::MAX_FRAME_BYTES,
                "maxCustodyChunkBytes":crate::native::site_sessions::bytes::CHUNK_BYTES,
                "ledgerBytes": ledger::BYTES, "candidates": CANDIDATES,
                "resolve": frame::MAX_RESOLVE,
            },
        } });
        (
            reply,
            Some(Session {
                channel: hello.channel,
                flags,
                binding: hello.binding,
            }),
        )
    }

    /// A `sequence`: every step validated before any runs, then run one at
    /// a time until one does not succeed or the channel stops, then what the
    /// frame asked for after its last executed step.
    async fn sequence<B: Browser>(
        &self,
        browser: &B,
        session: &Session,
        id: FrameId,
        frame: Sequence,
        refused_on_arrival: bool,
        received_at: Instant,
    ) -> Value {
        let channel = session.channel;
        let owner = frame.owner;
        self.ledger.receive(channel, id, Arrival::Other);
        if refused_on_arrival {
            self.ledger.not_started(channel, id, owner);
            return refusal(
                id,
                PROTOCOL_REFUSED,
                "This sequence arrived while another was unanswered; send one sequence at a time.",
                true,
            );
        }
        if let Some(owner) = owner {
            if self.ledger.register(channel, owner).is_err() {
                self.ledger.not_started(channel, id, Some(owner));
                return refusal(id, FENCED, FENCED_MESSAGE, true);
            }
            browser.fence_program(owner).await;
        }
        let steps = match step::prepare(&session.flags, &frame.steps) {
            Ok(steps) => steps,
            Err(rejected) => {
                self.ledger.not_started(channel, id, owner);
                let mut reply = refusal(id, REJECTED, rejected.message, true);
                reply["data"] = json!({ "step": rejected.step });
                return reply;
            }
        };
        if !self
            .ledger
            .start(channel, id, owner, Some(frame.directory.clone()))
        {
            return refusal(id, FENCED, FENCED_MESSAGE, true);
        }
        let context = FrameContext {
            channel,
            owner,
            binding: &session.binding,
            directory: &frame.directory,
            received_at,
        };
        let mut records: Vec<StepRecord> = Vec::new();
        for step in &steps {
            if !self.ledger.may_continue(channel) {
                break;
            }
            let mut record = browser.step(&context, step).await;
            // Redact before either the ledger or a retained result can keep it.
            browser.redact_step(&record.op, &mut record.result);
            if let Some(landed) = &mut record.landed {
                browser.redact(landed);
                landed::retain_bounded_changed_text(landed);
            }
            let succeeded = record.succeeded;
            records.push(record);
            if !succeeded {
                break;
            }
        }
        // A channel that stopped asks for nothing more than the page it
        // left: nobody will act on an observation.
        let live = self.ledger.may_continue(channel);
        let mut finish = browser
            .finish(
                &context,
                &Asks {
                    observe: frame.observe && live,
                    resolve: if live { &frame.resolve } else { &[] },
                    capture: frame.capture && live,
                },
            )
            .await;
        browser.redact(&mut finish.browser);
        if let Some(observation) = &mut finish.observation {
            browser.redact(observation);
        }
        if let Some(resolved) = &mut finish.resolved {
            browser.redact(resolved);
        }
        let success = records.iter().all(|record| record.succeeded);
        let retained = |step: usize| Retained {
            directory: frame.directory.clone(),
            channel,
            id,
            step,
        };
        // The ledger keeps every result over its bound as a reference to
        // the file it is written to now, and each step's arguments, so a
        // recovering host can tell its steps apart.
        let steps: Vec<Value> = records.iter().map(StepRecord::to_json).collect();
        let references: Vec<Option<Value>> = steps
            .iter()
            .enumerate()
            .map(|(index, step)| {
                (step["result"].to_string().len() > RETAINED_ABOVE)
                    .then(|| retained(index).write(&step["result"]))
            })
            .collect();
        let kept: Vec<Value> = steps
            .iter()
            .zip(&references)
            .zip(&frame.steps)
            .map(|((step, reference), sent)| {
                let mut step = step.clone();
                step["arguments"] = sent.arguments.clone();
                if let Some(reference) = reference {
                    step["result"] = reference.clone();
                }
                step
            })
            .collect();
        self.ledger.settle(
            channel,
            id,
            json!({ "success": success, "steps": kept, "browser": finish.browser }),
        );
        let timing = records
            .iter()
            .fold(StepTiming::default(), |total, record| StepTiming {
                queue_us: total.queue_us + record.timing.queue_us,
                op_us: total.op_us + record.timing.op_us,
                motion_us: total.motion_us + record.timing.motion_us,
            });
        // The reply carries every result it can, and in place of the
        // largest, when it must, the references the ledger keeps.
        let mut reply = json!({ "id": id, "success": success, "steps": steps,
            "browser": finish.browser,
            "timing": { "queueUs": timing.queue_us + finish.queue_us, "opUs": timing.op_us,
                "motionUs": timing.motion_us, "observeUs": finish.observe_us } });
        if let Some(observation) = finish.observation {
            reply["observation"] = observation;
        }
        if let Some(resolved) = finish.resolved {
            reply["resolved"] = resolved;
        }
        let slots = references
            .into_iter()
            .enumerate()
            .map(|(index, reference)| Slot {
                pointer: format!("/steps/{index}/result"),
                retained: retained(index),
                reference,
            })
            .collect();
        reply::fit(&mut reply, slots, MAX_REPLY_BYTES);
        reply
    }

    /// `op_status`: registers the asker's generation, then answers from the
    /// ledger, holding a `running` answer up to `waitMs`.
    async fn op_status(&self, session: &Session, id: FrameId, query: OpStatus) -> Value {
        self.ledger.receive(session.channel, id, Arrival::Other);
        if self.ledger.register(session.channel, query.owner).is_err() {
            return refusal(id, FENCED, FENCED_MESSAGE, false);
        }
        let wait = Duration::from_millis(query.wait_ms);
        let (mut reply, held) = match query.of {
            Some(of) => {
                match self
                    .ledger
                    .status_within(query.owner.action, of, wait)
                    .await
                {
                    Err(OtherAction) => {
                        return refusal(
                            id,
                            PROTOCOL_REFUSED,
                            "That frame belongs to another Action.",
                            false,
                        )
                    }
                    Ok(held) => (
                        json!({ "id": id, "success": true, "data": held.status.to_json() }),
                        vec![("/data/result".to_string(), held)],
                    ),
                }
            }
            None => {
                let entries = self.ledger.entries_within(query.owner.action, wait).await;
                let listed: Vec<Value> = entries
                    .iter()
                    .map(|held| {
                        let mut entry = held.status.to_json();
                        entry["channel"] = json!(held.channel.to_string());
                        entry["id"] = json!(held.id);
                        entry
                    })
                    .collect();
                (
                    json!({ "id": id, "success": true, "data": { "entries": listed } }),
                    entries
                        .into_iter()
                        .enumerate()
                        .map(|(index, held)| (format!("/data/entries/{index}/result"), held))
                        .collect(),
                )
            }
        };
        // Results the ledger holds inline go to their files when the answer
        // would not fit one line; the ledger keeps their references.
        let slots: Vec<Slot> = held
            .iter()
            .filter_map(|(pointer, held)| Some((pointer, held, held.directory.as_ref()?)))
            .flat_map(|(pointer, held, directory)| {
                let steps = reply
                    .pointer(&format!("{pointer}/steps"))
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                (0..steps).map(move |step| Slot {
                    pointer: format!("{pointer}/steps/{step}/result"),
                    retained: Retained {
                        directory: directory.clone(),
                        channel: held.channel,
                        id: held.id,
                        step,
                    },
                    reference: None,
                })
            })
            .collect();
        for written in reply::fit(&mut reply, slots, MAX_REPLY_BYTES) {
            self.ledger.retain(
                written.retained.channel,
                written.retained.id,
                written.retained.step,
                written.reference,
            );
        }
        reply
    }
}

/// A connection's channel once its hello was accepted.
struct Session {
    channel: ChannelId,
    flags: HostFlags,
    binding: Binding,
}

/// A frame as it arrived.
struct Received {
    frame: Result<(FrameId, Frame), Invalid>,
    /// Its `type` named a sequence.
    sequence: bool,
    /// Its owner, read leniently so a frame refused as invalid is still
    /// recorded under its Action.
    owner: Option<Owner>,
    /// A sequence that arrived while another was unanswered.
    refused: bool,
    at: Instant,
    bytes: usize,
}

/// The connection's read side: the frames that arrived and wait their turn.
struct Inbox<'a, R> {
    reader: &'a mut BufReader<R>,
    partial: &'a mut Vec<u8>,
    frames: VecDeque<Received>,
    channel: Option<ChannelId>,
    unanswered_sequences: usize,
    queued_bytes: usize,
    /// The connection ended, or went past the read-ahead bound: nothing
    /// more arrives, and nothing more starts.
    closed: bool,
}

impl<R: AsyncRead + Unpin> Inbox<'_, R> {
    /// Takes in one line: what it is, and, once the channel exists, its
    /// arrival in the ledger. A second unanswered `sequence` is refused now.
    fn receive(&mut self, line: String, ledger: &Ledger) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        let parsed: Result<Value, _> = serde_json::from_str(trimmed);
        let (frame, sequence, owner) = match parsed {
            Err(_) => (
                Err(Invalid {
                    id: None,
                    message: "An agent frame is one JSON object.".into(),
                }),
                false,
                None,
            ),
            Ok(value) => {
                let sequence = value["type"] == "sequence";
                let owner = frame::lenient_owner(&value);
                let kind = value["type"].as_str().unwrap_or_default();
                if !site_protocol::is_kind(kind)
                    && (line.len() > MAX_REQUEST_BYTES + 1
                        || self
                            .frames
                            .iter()
                            .filter(|received| !matches!(received.frame, Ok((_, Frame::Site(_)))))
                            .map(|received| received.bytes)
                            .sum::<usize>()
                            + line.len()
                            > MAX_REQUEST_BYTES + 1)
                {
                    self.close(ledger);
                    return;
                }
                let limit = if matches!(kind, "site_session.attach" | "site_session.export") {
                    site_protocol::MAX_FRAME_BYTES
                } else {
                    MAX_REQUEST_BYTES
                };
                let frame = if line.len() > limit + 1 {
                    Err(Invalid {
                        id: frame::id_of(&value),
                        message: "The agent frame exceeds its byte limit.".into(),
                    })
                } else if value["action"] != ACTION {
                    Err(Invalid {
                        id: frame::id_of(&value),
                        message: "An agent channel carries agent frames only.".into(),
                    })
                } else {
                    Frame::parse(&value)
                };
                (frame, sequence, owner)
            }
        };
        let refused = sequence && self.unanswered_sequences > 0;
        if sequence {
            self.unanswered_sequences += 1;
        }
        if let (Some(channel), Some(id)) = (
            self.channel,
            match &frame {
                Ok((id, _)) => Some(*id),
                Err(invalid) => invalid.id,
            },
        ) {
            let arrival = if sequence {
                Arrival::Sequence { owner, refused }
            } else {
                Arrival::Other
            };
            ledger.receive(channel, id, arrival);
        }
        self.queued_bytes += line.len();
        self.frames.push_back(Received {
            frame,
            sequence,
            owner,
            refused,
            at: Instant::now(),
            bytes: line.len(),
        });
    }

    /// The next frame in receipt order, reading one when none waits.
    async fn next(&mut self, ledger: &Ledger) -> Option<Received> {
        while self.frames.is_empty() && !self.closed {
            match self.read_line().await {
                Some(line) => self.receive(line, ledger),
                None => self.close(ledger),
            }
        }
        let received = self.frames.pop_front()?;
        self.queued_bytes -= received.bytes;
        Some(received)
    }

    fn close(&mut self, ledger: &Ledger) {
        self.closed = true;
        if let Some(channel) = self.channel {
            ledger.end(channel);
        }
    }

    /// One whole line, or `None` when the connection ended, a line was not
    /// text, or the frames waiting would pass the read-ahead bound.
    async fn read_line(&mut self) -> Option<String> {
        loop {
            let available = match self.reader.fill_buf().await {
                Ok([]) | Err(_) => return None,
                Ok(bytes) => bytes,
            };
            let count = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |index| index + 1);
            if self.queued_bytes + self.partial.len() + count
                > MAX_REQUEST_BYTES + site_protocol::MAX_FRAME_BYTES + 1
            {
                return None;
            }
            self.partial.extend_from_slice(&available[..count]);
            self.reader.consume(count);
            if self.partial.last() == Some(&b'\n') {
                return String::from_utf8(std::mem::take(self.partial)).ok();
            }
        }
    }
}

async fn write_line<W: AsyncWrite + Unpin>(writer: &mut W, value: &Value) -> std::io::Result<()> {
    let mut line = value.to_string();
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await
}

/// A frame refused before any step ran. A refused `sequence` answers with
/// no steps.
fn refusal(id: FrameId, code: &str, error: impl Into<String>, sequence: bool) -> Value {
    let mut reply = json!({ "id": id, "success": false, "code": code, "error": error.into() });
    if sequence {
        reply["steps"] = json!([]);
    }
    reply
}

/// sha256 of the running executable.
fn executable_digest() -> Result<String, String> {
    let path: PathBuf = if cfg!(target_os = "linux") {
        PathBuf::from("/proc/self/exe")
    } else {
        std::env::current_exe().map_err(|error| error.to_string())?
    };
    let mut file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read =
            std::io::Read::read(&mut file, &mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests;
