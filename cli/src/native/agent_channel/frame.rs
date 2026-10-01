//! The agent channel's frames as the daemon reads them (agent-channel
//! contract r6.1 §2 to §4 and §6): `hello`, `sequence` and `op_status`, each
//! one line the toolbox forwarded with the agent action added. A frame that
//! is not one of them, or carries a member that is not its own, is refused
//! as a protocol violation before anything runs; what a step's `op` and
//! `arguments` mean is judged by the host-bound preparation, not here.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use super::ceiling::Ceiling;
use crate::native::theme::Theme;

/// The one version of the channel this daemon serves.
pub(crate) const PROTOCOL: u64 = 1;
/// Steps in one `sequence`.
pub(crate) const MAX_STEPS: usize = 30;
/// Selectors one `sequence` resolves.
pub(crate) const MAX_RESOLVE: usize = 30;
/// The longest an `op_status` holds a `running` answer.
pub(crate) const MAX_WAIT_MS: u64 = 10_000;
/// The largest frame id: the largest integer a JSON reader keeps exactly.
const MAX_FRAME_ID: u64 = (1 << 53) - 1;
/// The largest owner generation: the journal's positive 64-bit decimal.
const MAX_OWNER_GENERATION: u64 = i64::MAX as u64;
/// The longest directory path a frame may name.
const MAX_DIRECTORY_BYTES: usize = 4096;

/// A frame's id: an integer in 1..=2^53−1, strictly increasing per channel.
pub(crate) type FrameId = u64;

/// A channel's ledger key: a canonical, lower-case, non-nil UUID that the
/// host makes fresh for every open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ChannelId(uuid::Uuid);

impl ChannelId {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        canonical_uuid(value).map(Self)
    }
}

impl std::fmt::Display for ChannelId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// An Action's key, which the daemon fences and files ledger entries under.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ActionId(uuid::Uuid);
impl std::fmt::Display for ActionId {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(out)
    }
}

#[cfg(test)]
impl ActionId {
    pub(crate) fn for_test(n: u16) -> Self {
        Self(uuid::Uuid::from_u128(u128::from(n) + 1))
    }
}

/// Who sends a frame: an Action and the dispatcher's ownership generation
/// for it. A newer generation fences every older one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Owner {
    pub(crate) action: ActionId,
    pub(crate) generation: u64,
}

pub(crate) enum Frame {
    Hello(Hello),
    Sequence(Sequence),
    OpStatus(OpStatus),
    Site(crate::native::site_sessions::protocol::Request),
    Program(Owner, crate::native::playwright::remote::Request),
    Files(
        Owner,
        Option<Vec<crate::native::playwright::files::Receipt>>,
    ),
}

/// The channel-scoped host configuration a `hello` carries: the members of
/// `HostConfig` that bind a session, under the same names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Binding {
    pub(crate) namespace: String,
    pub(crate) session: String,
    pub(crate) require_sandbox: bool,
    pub(crate) browser_host: bool,
    pub(crate) theme: Option<Theme>,
}

pub(crate) struct Hello {
    pub(crate) channel: ChannelId,
    pub(crate) binding: Binding,
    pub(crate) owner: Option<Owner>,
}

pub(crate) struct Sequence {
    /// Absent only on the host's theme push, which no Action owns.
    pub(crate) owner: Option<Owner>,
    /// The Action's private browser directory.
    pub(crate) directory: PathBuf,
    pub(crate) steps: Vec<Step>,
    pub(crate) observe: bool,
    pub(crate) resolve: Vec<String>,
    pub(crate) capture: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct Step {
    pub(crate) op: String,
    pub(crate) arguments: Value,
    pub(crate) preconditions: Preconditions,
}

/// What a judged step requires of the page before its first input (§4).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Preconditions {
    pub(crate) page_generation: Option<String>,
    pub(crate) geometry_sha256: Option<String>,
    pub(crate) backend_node_id: Option<i64>,
    #[serde(rename = "box")]
    pub(crate) bounds: Option<Bounds>,
    pub(crate) effects: Option<Ceiling>,
}

/// A border box in viewport CSS pixels.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Bounds {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) width: f64,
    pub(crate) height: f64,
}

impl Bounds {
    pub(crate) fn center(&self) -> (f64, f64) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    /// Whether `point` lies in the box, edges included.
    pub(crate) fn contains(&self, (x, y): (f64, f64)) -> bool {
        x >= self.x && x <= self.x + self.width && y >= self.y && y <= self.y + self.height
    }
}

pub(crate) struct OpStatus {
    pub(crate) owner: Owner,
    /// The frame asked about; without it, every retained entry of the Action.
    pub(crate) of: Option<(ChannelId, FrameId)>,
    pub(crate) wait_ms: u64,
}

/// A line the channel cannot serve. `id` is the frame's own id when it had a
/// usable one, so the refusal answers in step; without one the connection
/// cannot stay in step and ends.
#[derive(Debug, PartialEq)]
pub(crate) struct Invalid {
    pub(crate) id: Option<FrameId>,
    pub(crate) message: String,
}

/// A line's frame id, whatever else it carries.
pub(crate) fn id_of(line: &Value) -> Option<FrameId> {
    frame_id(&line["id"])
}

/// The id and type of a line, read before anything else in it.
pub(crate) fn envelope(line: &Value) -> Result<(FrameId, &str), Invalid> {
    let id = id_of(line).ok_or_else(|| Invalid {
        id: None,
        message: "An agent frame needs an integer id from 1 to 2^53-1.".into(),
    })?;
    let kind = line["type"].as_str().ok_or_else(|| Invalid {
        id: Some(id),
        message: "An agent frame needs a type: hello, sequence or op_status.".into(),
    })?;
    Ok((id, kind))
}

impl Frame {
    /// Reads one forwarded line: its `action` (the toolbox's) and `type`
    /// select the frame, and every other member must be the frame's own.
    pub(crate) fn parse(line: &Value) -> Result<(FrameId, Frame), Invalid> {
        let (id, kind) = envelope(line)?;
        let invalid = |message: String| Invalid {
            id: Some(id),
            message,
        };
        let mut members = line.as_object().cloned().unwrap_or_default();
        members.remove("action");
        members.remove("type");
        let members = Value::Object(members);
        let frame = match kind {
            "hello" => Frame::Hello(
                serde_json::from_value::<HelloWire>(members)
                    .map_err(|error| invalid(format!("The hello is not valid: {error}.")))?
                    .check()
                    .map_err(invalid)?,
            ),
            "sequence" => Frame::Sequence(
                serde_json::from_value::<SequenceWire>(members)
                    .map_err(|error| invalid(format!("The sequence is not valid: {error}.")))?
                    .check()
                    .map_err(invalid)?,
            ),
            "op_status" => Frame::OpStatus(
                serde_json::from_value::<OpStatusWire>(members)
                    .map_err(|error| invalid(format!("The op_status is not valid: {error}.")))?
                    .check()
                    .map_err(invalid)?,
            ),
            kind if crate::native::site_sessions::protocol::is_kind(kind) => Frame::Site(
                crate::native::site_sessions::protocol::Request::read(kind, members)
                    .map_err(|message| invalid(message.into()))?,
            ),
            kind if crate::native::playwright::remote::is_kind(kind) => {
                let text = |key: &str| line[key].as_str().map(str::to_owned);
                let owned = owner(text("actionId"), text("ownerGeneration"))
                    .map_err(invalid)?.ok_or_else(|| invalid("A remote program frame needs its Action and owner generation.".into()))?;
                let mut members = members;
                members.as_object_mut().unwrap().remove("actionId");
                members.as_object_mut().unwrap().remove("ownerGeneration");
                Frame::Program(owned, crate::native::playwright::remote::Request::read(kind, members).map_err(invalid)?)
            }
            "action.files"|"action.files.release"=>{
                let has_files=members.get("files").is_some();
                let wire:ActionFilesWire=serde_json::from_value(members).map_err(|_|invalid("The native Action file frame is invalid.".into()))?;
                let owner=owner(Some(wire.action_id),Some(wire.owner_generation)).map_err(invalid)?.ok_or_else(||invalid("A native file frame needs its Action owner.".into()))?;
                if (kind=="action.files")!=has_files || (kind=="action.files" && wire.files.is_none()) {return Err(invalid("Native file registration needs a roster; release carries no roster.".into()));}
                Frame::Files(owner,wire.files)
            },
            other => {
                return Err(invalid(format!(
                    "Unknown agent frame type `{other}`: this daemon serves hello, sequence and op_status."
                )))
            }
        };
        Ok((id, frame))
    }
}

/// The owner a line names, if it names a valid one, whatever else it
/// carries: a frame refused as invalid is still recorded under its Action.
pub(crate) fn lenient_owner(line: &Value) -> Option<Owner> {
    let text = |key: &str| line[key].as_str().map(str::to_string);
    owner(text("actionId"), text("ownerGeneration"))
        .ok()
        .flatten()
}

fn frame_id(value: &Value) -> Option<FrameId> {
    value.as_u64().filter(|id| (1..=MAX_FRAME_ID).contains(id))
}

fn canonical_uuid(value: &str) -> Option<uuid::Uuid> {
    let uuid = uuid::Uuid::parse_str(value).ok()?;
    (!uuid.is_nil() && uuid.hyphenated().to_string() == value).then_some(uuid)
}

fn owner(action: Option<String>, generation: Option<String>) -> Result<Option<Owner>, String> {
    match (action, generation) {
        (None, None) => Ok(None),
        (Some(action), Some(generation)) => {
            let action = canonical_uuid(&action)
                .map(ActionId)
                .ok_or("actionId must be a canonical lower-case UUID.")?;
            let digits = generation.as_bytes();
            let generation = (!digits.is_empty()
                && digits.len() <= 19
                && digits[0] != b'0'
                && digits.iter().all(u8::is_ascii_digit))
            .then(|| generation.parse::<u64>().ok())
            .flatten()
            .filter(|generation| *generation <= MAX_OWNER_GENERATION)
            .ok_or("ownerGeneration must be a positive decimal no greater than 2^63-1.")?;
            Ok(Some(Owner { action, generation }))
        }
        _ => Err("actionId and ownerGeneration go together.".into()),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HelloWire {
    #[allow(dead_code)]
    id: u64,
    protocol: u64,
    channel: String,
    binding: BindingWire,
    action_id: Option<String>,
    owner_generation: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BindingWire {
    version: u32,
    namespace: String,
    session: String,
    require_sandbox: bool,
    #[serde(default)]
    browser_host: bool,
    theme: Option<Theme>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ActionFilesWire {
    #[allow(dead_code)]
    id: u64,
    action_id: String,
    owner_generation: String,
    files: Option<Vec<crate::native::playwright::files::Receipt>>,
}

impl HelloWire {
    fn check(self) -> Result<Hello, String> {
        if self.protocol != PROTOCOL || self.binding.version != 1 {
            return Err(format!(
                "This daemon serves agent channel protocol {PROTOCOL} with binding version 1."
            ));
        }
        Ok(Hello {
            channel: ChannelId::parse(&self.channel)
                .ok_or("channel must be a canonical lower-case UUID.")?,
            binding: Binding {
                namespace: self.binding.namespace,
                session: self.binding.session,
                require_sandbox: self.binding.require_sandbox,
                browser_host: self.binding.browser_host,
                theme: self.binding.theme,
            },
            owner: owner(self.action_id, self.owner_generation)?,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SequenceWire {
    #[allow(dead_code)]
    id: u64,
    action_id: Option<String>,
    owner_generation: Option<String>,
    directory: String,
    steps: Vec<StepWire>,
    #[serde(default)]
    observe: bool,
    #[serde(default)]
    resolve: Vec<String>,
    #[serde(default)]
    capture: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StepWire {
    op: String,
    arguments: Value,
    #[serde(default)]
    preconditions: Preconditions,
}

impl SequenceWire {
    fn check(self) -> Result<Sequence, String> {
        if self.steps.len() > MAX_STEPS {
            return Err(format!("A sequence carries at most {MAX_STEPS} steps."));
        }
        if self.resolve.len() > MAX_RESOLVE {
            return Err(format!(
                "A sequence resolves at most {MAX_RESOLVE} selectors."
            ));
        }
        if self.steps.is_empty() && !self.observe && self.resolve.is_empty() {
            return Err("A sequence without steps must observe or resolve.".into());
        }
        for step in &self.steps {
            if let Some(bounds) = step.preconditions.bounds {
                if ![bounds.x, bounds.y, bounds.width, bounds.height]
                    .iter()
                    .all(|value| value.is_finite())
                    || bounds.width < 0.0
                    || bounds.height < 0.0
                {
                    return Err(
                        "A precondition box has finite coordinates and a size of at least zero."
                            .into(),
                    );
                }
            }
        }
        Ok(Sequence {
            owner: owner(self.action_id, self.owner_generation)?,
            directory: private_directory(&self.directory)?,
            steps: self
                .steps
                .into_iter()
                .map(|step| Step {
                    op: step.op,
                    arguments: step.arguments,
                    preconditions: step.preconditions,
                })
                .collect(),
            observe: self.observe,
            resolve: self.resolve,
            capture: self.capture,
        })
    }
}

/// The Action's directory: an absolute path written as the daemon will use
/// it, with no `.` or `..` component to resolve.
fn private_directory(path: &str) -> Result<PathBuf, String> {
    let directory = Path::new(path);
    // Checked on the text: `Path::components` drops an interior `.`.
    if path.len() > MAX_DIRECTORY_BYTES
        || path.contains('\0')
        || !directory.is_absolute()
        || path
            .split('/')
            .any(|segment| segment == "." || segment == "..")
    {
        return Err("directory must be an absolute path without . or .. components.".into());
    }
    Ok(directory.to_path_buf())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OpStatusWire {
    #[allow(dead_code)]
    id: u64,
    action_id: String,
    owner_generation: String,
    of: Option<OfWire>,
    #[serde(default)]
    wait_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OfWire {
    channel: String,
    id: Value,
}

impl OpStatusWire {
    fn check(self) -> Result<OpStatus, String> {
        if self.wait_ms > MAX_WAIT_MS {
            return Err(format!("waitMs is at most {MAX_WAIT_MS}."));
        }
        let of = match self.of {
            None => None,
            Some(of) => Some((
                ChannelId::parse(&of.channel).ok_or("of.channel must be a canonical UUID.")?,
                frame_id(&of.id).ok_or("of.id must be an integer from 1 to 2^53-1.")?,
            )),
        };
        Ok(OpStatus {
            owner: owner(Some(self.action_id), Some(self.owner_generation))?
                .expect("both members are present"),
            of,
            wait_ms: self.wait_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CHANNEL: &str = "5b0c0b1e-6f0a-4c1c-9d59-0e3f2b7a9c11";
    const ACTION: &str = "8c1f0e2a-3b4c-4d5e-8f60-718293a4b5c6";

    fn line(mut frame: Value) -> Value {
        frame["action"] = json!("ambit_browser_agent");
        frame
    }

    fn hello(extra: Value) -> Value {
        let mut frame = json!({ "type": "hello", "id": 1, "protocol": 1, "channel": CHANNEL,
            "binding": { "version": 1, "namespace": "3f2a", "session": "browser",
                "requireSandbox": true } });
        frame
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        line(frame)
    }

    fn sequence(extra: Value) -> Value {
        let mut frame = json!({ "type": "sequence", "id": 7, "actionId": ACTION,
            "ownerGeneration": "41", "directory": "/workspace/.ambit/browser/actions/5d41",
            "steps": [{ "op": "agent_browser_open", "arguments": { "url": "https://example.com/" } }] });
        frame
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        line(frame)
    }

    fn refused(value: Value) -> Invalid {
        match Frame::parse(&value) {
            Ok(_) => panic!("accepted {value}"),
            Err(invalid) => invalid,
        }
    }

    #[test]
    fn a_hello_binds_a_fresh_channel_and_may_name_its_owner() {
        let (id, Frame::Hello(greeting)) = Frame::parse(&hello(json!({}))).unwrap() else {
            panic!("not a hello");
        };
        assert_eq!(id, 1);
        assert_eq!(greeting.channel.to_string(), CHANNEL);
        assert_eq!(
            greeting.binding,
            Binding {
                namespace: "3f2a".into(),
                session: "browser".into(),
                require_sandbox: true,
                browser_host: false,
                theme: None,
            }
        );
        assert!(greeting.owner.is_none());
        let (_, Frame::Hello(owned)) = Frame::parse(&hello(json!({ "actionId": ACTION,
            "ownerGeneration": "9223372036854775807", "binding": { "version": 1,
            "namespace": "n", "session": "browser", "requireSandbox": true, "theme": "dark" } })))
        .unwrap() else {
            panic!("not a hello");
        };
        assert_eq!(owned.owner.unwrap().generation, i64::MAX as u64);
        assert_eq!(owned.binding.theme, Some(Theme::Dark));
    }

    #[test]
    fn a_hello_that_is_not_one_this_daemon_serves_is_refused_with_its_id() {
        for extra in [
            json!({ "protocol": 2 }),
            json!({ "protocol": "1" }),
            json!({ "channel": CHANNEL.to_uppercase() }),
            json!({ "channel": "00000000-0000-0000-0000-000000000000" }),
            json!({ "channel": "not-a-uuid" }),
            json!({ "binding": { "version": 2, "namespace": "n", "session": "browser", "requireSandbox": true } }),
            json!({ "binding": { "version": 1, "namespace": "n", "session": "browser" } }),
            json!({ "binding": { "version": 1, "namespace": "n", "session": "browser", "requireSandbox": true, "captureDirectory": "/x" } }),
            json!({ "binding": { "version": 1, "namespace": "n", "session": "browser", "requireSandbox": true, "theme": "system" } }),
            json!({ "actionId": ACTION }),
            json!({ "ownerGeneration": "41" }),
            json!({ "actionId": ACTION, "ownerGeneration": "041" }),
            json!({ "actionId": ACTION, "ownerGeneration": "0" }),
            json!({ "actionId": ACTION, "ownerGeneration": "-4" }),
            json!({ "actionId": ACTION, "ownerGeneration": "9223372036854775808" }),
            json!({ "actionId": ACTION, "ownerGeneration": 41 }),
            json!({ "extra": true }),
        ] {
            let invalid = refused(hello(extra.clone()));
            assert_eq!(invalid.id, Some(1), "{extra}");
        }
    }

    #[test]
    fn a_line_without_a_usable_id_cannot_be_answered_in_step() {
        for id in [
            json!(0),
            json!(-1),
            json!(1.5),
            json!("1"),
            json!(1u64 << 53),
            json!(null),
        ] {
            let mut frame = hello(json!({}));
            frame["id"] = id.clone();
            assert_eq!(refused(frame).id, None, "{id}");
        }
        let mut untyped = hello(json!({}));
        untyped.as_object_mut().unwrap().remove("type");
        assert_eq!(refused(untyped).id, Some(1));
        assert!(refused(line(json!({ "type": "op", "id": 3 })))
            .message
            .contains("Unknown agent frame type"));
    }

    #[test]
    fn a_sequence_carries_its_steps_owner_directory_and_asks() {
        let (id, Frame::Sequence(frame)) = Frame::parse(&sequence(json!({
            "steps": [
                { "op": "agent_browser_open", "arguments": { "url": "https://example.com/" } },
                { "op": "agent_browser_click", "arguments": { "selector": "@e21" },
                  "preconditions": { "pageGeneration": "5b0e", "backendNodeId": 530,
                    "box": { "x": 24, "y": 610, "width": 180, "height": 40 }, "effects": "read",
                    "geometrySha256": "sha256:00" } }],
            "observe": true, "resolve": ["#q"], "capture": true })))
        .unwrap() else {
            panic!("not a sequence");
        };
        assert_eq!(id, 7);
        assert_eq!(frame.owner.unwrap().generation, 41);
        assert_eq!(
            frame.directory,
            PathBuf::from("/workspace/.ambit/browser/actions/5d41")
        );
        assert_eq!(frame.steps.len(), 2);
        assert_eq!(frame.steps[0].preconditions, Preconditions::default());
        let judged = &frame.steps[1].preconditions;
        assert_eq!(judged.page_generation.as_deref(), Some("5b0e"));
        assert_eq!(judged.backend_node_id, Some(530));
        assert_eq!(judged.effects, Some(Ceiling::Read));
        assert_eq!(judged.bounds.unwrap().center(), (114.0, 630.0));
        assert!(frame.observe && frame.capture);
        assert_eq!(frame.resolve, ["#q"]);
        // The theme push carries no owner.
        let mut push = sequence(json!({ "steps": [{ "op": "agent_browser_set_theme",
            "arguments": { "theme": "dark" } }] }));
        push.as_object_mut().unwrap().remove("actionId");
        push.as_object_mut().unwrap().remove("ownerGeneration");
        let (_, Frame::Sequence(push)) = Frame::parse(&push).unwrap() else {
            panic!("not a sequence");
        };
        assert!(push.owner.is_none());
        // The host derives the Action's id as a name-based (v5) UUID: any
        // canonical UUID is an Action's.
        let mut derived = sequence(json!({ "steps": [], "observe": true }));
        derived["actionId"] = json!("9b0c5e3a-6d1f-5a2b-8c4d-0e1f2a3b4c5d");
        let (_, Frame::Sequence(derived)) = Frame::parse(&derived).unwrap() else {
            panic!("not a sequence");
        };
        assert_eq!(
            derived.owner.unwrap().action.0.to_string(),
            "9b0c5e3a-6d1f-5a2b-8c4d-0e1f2a3b4c5d"
        );
    }

    #[test]
    fn a_sequence_with_members_that_are_not_its_own_is_refused() {
        let step = json!({ "op": "agent_browser_get_title", "arguments": {} });
        let many = |count: usize| json!(vec![step.clone(); count]);
        for extra in [
            json!({ "steps": [] }),
            json!({ "steps": [], "capture": true }),
            json!({ "steps": many(31) }),
            json!({ "resolve": vec!["#a"; 31] }),
            json!({ "directory": "relative/path" }),
            json!({ "directory": "/workspace/../etc" }),
            json!({ "directory": "/workspace/./captures" }),
            json!({ "directory": "/workspace/a\u{0}b" }),
            json!({ "actionId": "8C1F0E2A-3B4C-4D5E-8F60-718293A4B5C6" }),
            json!({ "steps": [{ "op": "agent_browser_click", "arguments": {}, "note": 1 }] }),
            json!({ "steps": [{ "op": "agent_browser_click" }] }),
            json!({ "steps": [{ "op": "agent_browser_click", "arguments": {},
                "preconditions": { "effects": "delete" } }] }),
            json!({ "steps": [{ "op": "agent_browser_click", "arguments": {},
                "preconditions": { "expectedObservation": {} } }] }),
            json!({ "steps": [{ "op": "agent_browser_click", "arguments": {},
                "preconditions": { "box": { "x": 0, "y": 0, "width": -1, "height": 4 } } }] }),
            json!({ "steps": [{ "op": "agent_browser_click", "arguments": {},
                "preconditions": { "backendNodeId": "530" } }] }),
            json!({ "captureDirectory": "/workspace" }),
            json!({ "observe": "yes" }),
        ] {
            assert_eq!(refused(sequence(extra.clone())).id, Some(7), "{extra}");
        }
        // Only observing or resolving makes an empty sequence meaningful.
        assert!(Frame::parse(&sequence(json!({ "steps": [], "observe": true }))).is_ok());
        assert!(Frame::parse(&sequence(json!({ "steps": [], "resolve": ["#q"] }))).is_ok());
        assert!(Frame::parse(&sequence(json!({ "steps": many(30) }))).is_ok());
    }

    #[test]
    fn an_op_status_names_its_owner_and_may_name_one_frame_and_a_wait() {
        let (id, Frame::OpStatus(query)) = Frame::parse(&line(json!({ "type": "op_status",
            "id": 9, "actionId": ACTION, "ownerGeneration": "42",
            "of": { "channel": CHANNEL, "id": 7 }, "waitMs": 10000 })))
        .unwrap() else {
            panic!("not an op_status");
        };
        assert_eq!(id, 9);
        assert_eq!(query.owner.generation, 42);
        assert_eq!(query.of, Some((ChannelId::parse(CHANNEL).unwrap(), 7)));
        assert_eq!(query.wait_ms, 10_000);
        for extra in [
            json!({ "waitMs": 10001 }),
            json!({ "waitMs": -1 }),
            json!({ "of": { "channel": CHANNEL } }),
            json!({ "of": { "channel": CHANNEL, "id": 0 } }),
            json!({ "of": { "channel": "x", "id": 1 } }),
            json!({ "ownerGeneration": null }),
        ] {
            let mut frame = json!({ "type": "op_status", "id": 9, "actionId": ACTION,
                "ownerGeneration": "42" });
            frame
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert_eq!(refused(line(frame)).id, Some(9), "{extra}");
        }
        let mut ownerless = json!({ "type": "op_status", "id": 9 });
        assert_eq!(refused(line(ownerless.take())).id, Some(9));
    }

    #[test]
    fn remote_program_frames_accept_only_the_host_transport_contract() {
        let program = "6b87fd14-4712-45e1-829e-95ee008fd783";
        for frame in [
            json!({"type":"program.open","id":2,"programId":program,"timeoutMs":1}),
            json!({"type":"program.open","id":3,"programId":program,"timeoutMs":120000,"targetId":"tab"}),
            json!({"type":"program.close","id":4,"programId":program,"reason":"complete"}),
            json!({"type":"program.close","id":5,"programId":program,"reason":"cancel"}),
            json!({"type":"program.close","id":6,"programId":program,"reason":"timeout"}),
            json!({"type":"program.status","id":7,"programId":program}),
            json!({"type":"program.files","id":12,"programId":program,"files":[]}),
            json!({"type":"program.files","id":13,"programId":program,"files":[{"path":"/workspace/.ambit/browser/staged/fixture.txt","byteSize":0,"contentRef":format!("sha256:{}","0".repeat(64))}]}),
        ] {
            let mut frame = frame;
            frame["actionId"] = json!(ACTION);
            frame["ownerGeneration"] = json!("1");
            assert!(Frame::parse(&line(frame.clone())).is_ok(), "{frame}");
        }
        for extra in [
            json!({"timeoutMs":0}),
            json!({"timeoutMs":120001}),
            json!({"timeoutMs":1.5}),
            json!({"programId":"00000000-0000-0000-0000-000000000000"}),
            json!({"programId":"6B87FD14-4712-45E1-829E-95EE008FD783"}),
            json!({"targetId":""}),
            json!({"targetId":42}),
            json!({"code":"return 1"}),
            json!({"environment":{}}),
            json!({"artifactsDir":"/host"}),
        ] {
            let mut frame = json!({"type":"program.open","id":8,"programId":program,"timeoutMs":1000,"actionId":ACTION,"ownerGeneration":"1"});
            frame
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert!(Frame::parse(&line(frame.clone())).is_err(), "{frame}");
        }
        for frame in [
            json!({"type":"program.close","id":9,"programId":program,"reason":"retry"}),
            json!({"type":"program.status","id":10,"programId":program,"endpoint":"ws://foreign"}),
        ] {
            assert!(Frame::parse(&line(frame.clone())).is_err(), "{frame}");
        }
        for extra in [
            json!({}),
            json!({"actionId":ACTION}),
            json!({"actionId":ACTION,"ownerGeneration":"0"}),
            json!({"actionId":"x","ownerGeneration":"1"}),
        ] {
            let mut frame = json!({"type":"program.status","id":11,"programId":program});
            frame
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert!(Frame::parse(&line(frame.clone())).is_err(), "{frame}");
        }
    }

    #[test]
    fn native_action_staging_has_no_model_selected_scope_or_program_slot() {
        for value in [
            json!({"type":"action.files","id":2,"actionId":ACTION,"ownerGeneration":"1","files":[]}),
            json!({"type":"action.files.release","id":3,"actionId":ACTION,"ownerGeneration":"2"}),
        ] {
            assert!(matches!(
                Frame::parse(&line(value)),
                Ok((_, Frame::Files(_, _)))
            ));
        }
        for extra in [
            json!({"scope":"6b87fd14-4712-45e1-829e-95ee008fd783"}),
            json!({"programId":"6b87fd14-4712-45e1-829e-95ee008fd783"}),
            json!({"ownerGeneration":"0"}),
            json!({"ownerGeneration":1}),
            json!({"files":null}),
        ] {
            let mut value = json!({"type":"action.files","id":4,"actionId":ACTION,"ownerGeneration":"1","files":[]});
            value
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert!(Frame::parse(&line(value)).is_err());
        }
        assert!(Frame::parse(&line(json!({"type":"action.files.release","id":5,"actionId":ACTION,"ownerGeneration":"1","files":null}))).is_err());
    }

    #[test]
    fn a_box_contains_its_edges_and_centre() {
        let bounds = Bounds {
            x: 24.0,
            y: 610.0,
            width: 180.0,
            height: 40.0,
        };
        assert!(bounds.contains(bounds.center()));
        assert!(bounds.contains((24.0, 650.0)));
        assert!(!bounds.contains((23.9, 630.0)));
        assert!(!bounds.contains((114.0, 650.1)));
    }
}
