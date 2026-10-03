//! The host selects one browser session; tool arguments remain browser data.
//! Schemas derive from the normal MCP catalog and commands use its preparation
//! helpers and the canonical CLI parser, without reparsing data as global flags.
//!
//! Two transports carry host-bound calls, and both prepare a call here
//! (`HostFlags::prepare`), so what the host may send cannot drift between
//! them: the spawned MCP client, which then sends the command to the daemon
//! (`HostBinding::call`), and the daemon's agent channel, which dispatches it
//! in the daemon itself (`native::agent_channel`).

use super::*;
use crate::commands::parse_command_with_input;
use crate::connection::{ensure_daemon, send_command_detailed, send_command_if_running, Response};
use crate::flags::{apply_cli_flags, parse_flags_from_config, Config, Flags};
use crate::native::feedback::{ObservationId, MAX_CALL_MS, REQUEST_FIELD};
use crate::native::theme::Theme;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::OnceLock;

pub(crate) const PROFILE: &str = "ambit-host-bound-v1";

const HOST_ARGUMENTS: &[&str] = &[
    "session",
    "namespace",
    "restore",
    "restoreSave",
    "restoreCheckUrl",
    "restoreCheckText",
    "restoreCheckFn",
    "allowedDomains",
    "caCert",
    "clearCaCert",
    "idleTimeout",
    "requireDaemon",
    "requireSandbox",
    "extraArgs",
];

/// Tools the host calls itself and never offers to the model, marked
/// `"caller": "host"` in the descriptor. Who calls a tool is apart from how
/// it is dispatched: opening background tabs is an agent operation.
const HOST_CALLED: &[&str] = &[TOOL_SET_THEME, TOOL_OPEN_MANY];

/// Session operations: each acts on the running session as it is. It never
/// starts a daemon or sends launch settings, and it is neither agent input
/// nor an observation, so it captures no host feedback.
const SESSION_OPERATIONS: &[&str] = &[TOOL_SET_THEME];

/// Process discovery, supervisor configuration, dependency installation and
/// cross-session commands belong to the host. So do the person's sign-ins:
/// no host-bound call reads or sets cookies or site storage, saves or loads
/// browser state, keeps or types a password, or records a capture that
/// holds request headers and cookies (a HAR, a trace, a profile). The host
/// keeps sign-ins for the person, who signs in themselves. And the agent acts
/// on a page only as a person would, through visible input: rewriting the
/// page's history by script has no input to show (`open` navigates visibly).
pub(super) fn allows(name: &str) -> bool {
    !matches!(
        name,
        TOOL_COOKIES_GET
            | TOOL_COOKIES_SET
            | TOOL_COOKIES_SET_CURL
            | TOOL_COOKIES_CLEAR
            | TOOL_STORAGE_GET
            | TOOL_STORAGE_SET
            | TOOL_STORAGE_CLEAR
            | TOOL_STATE_SAVE
            | TOOL_STATE_LOAD
            | TOOL_STATE_SHOW
            | TOOL_STATE_LIST
            | TOOL_STATE_CLEAN
            | TOOL_STATE_CLEAR
            | TOOL_STATE_RENAME
            | TOOL_AUTH_SAVE
            | TOOL_AUTH_LOGIN
            | TOOL_AUTH_SHOW
            | TOOL_AUTH_LIST
            | TOOL_AUTH_DELETE
            | TOOL_SET_CREDENTIALS
            | TOOL_NETWORK_HAR_START
            | TOOL_NETWORK_HAR_STOP
            | TOOL_TRACE_START
            | TOOL_TRACE_STOP
            | TOOL_PROFILER_START
            | TOOL_PROFILER_STOP
            | TOOL_TOOLS_PROFILES
            | TOOL_SESSION
            | TOOL_SESSION_LIST
            | TOOL_SESSION_ID
            | TOOL_SESSION_INFO
            | TOOL_PROFILES
            | TOOL_SKILLS_LIST
            | TOOL_SKILLS_GET
            | TOOL_SKILLS_PATH
            | TOOL_PLUGIN_ADD
            | TOOL_PLUGIN_LIST
            | TOOL_PLUGIN_SHOW
            | TOOL_PLUGIN_RUN
            | TOOL_DOCTOR
            | TOOL_DASHBOARD_START
            | TOOL_DASHBOARD_STOP
            | TOOL_INSTALL
            | TOOL_UPGRADE
            | TOOL_CHAT
            | TOOL_CONNECT
            | TOOL_INSPECT
            | TOOL_GET_CDP_URL
            | TOOL_STREAM_ENABLE
            | TOOL_STREAM_DISABLE
            | TOOL_STREAM_STATUS
            | TOOL_DEVICE
            | TOOL_BATCH
            | TOOL_PUSHSTATE
    )
}

pub(crate) fn tools() -> Vec<Value> {
    catalog().to_vec()
}

/// The profile's tools, built once per process: they are fixed by this
/// binary, and every call on either transport is checked against them.
fn catalog() -> &'static [Value] {
    static CATALOG: OnceLock<Vec<Value>> = OnceLock::new();
    CATALOG.get_or_init(build_tools)
}

/// The profile's tool `name`, with its pinned schema and annotations.
pub(crate) fn tool(name: &str) -> Option<&'static Value> {
    catalog().iter().find(|tool| tool["name"] == name)
}

fn build_tools() -> Vec<Value> {
    let mut tools: Vec<_> = super::tools()
        .into_iter()
        .filter(|tool| tool["name"].as_str().is_some_and(allows))
        .collect();
    for tool in &mut tools {
        let name = tool["name"].as_str().unwrap().to_string();
        let properties = tool["inputSchema"]["properties"].as_object_mut().unwrap();
        for key in HOST_ARGUMENTS {
            properties.remove(*key);
        }
        if name == TOOL_OPEN {
            // The host decides these through its configuration.
            for key in ["headed", "webgpu", "webmcp", "theme"] {
                properties.remove(key);
            }
        }
        if name == TOOL_CLOSE {
            properties.remove("all");
        }
        properties.get_mut("timeoutMs").unwrap()["maximum"] = json!(MAX_CALL_MS);
        if name == TOOL_MOUSE_MOVE {
            tool["_meta"] = json!({ "io.ambit/browser": {
                "coordinateSpace": "viewport-css",
                "coordinateArguments": [{ "x": "/x", "y": "/y" }]
            }});
        }
        if HOST_CALLED.contains(&name.as_str()) {
            tool["_meta"] = json!({ "io.ambit/browser": { "caller": "host" } });
        }
    }
    tools.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    tools
}

pub(crate) fn descriptor() -> Value {
    static DESCRIPTOR: OnceLock<Value> = OnceLock::new();
    DESCRIPTOR
        .get_or_init(|| {
            // serde_json's default object map sorts keys recursively. Tool
            // names are sorted too, matching the host's canonical-JSON
            // catalog digest.
            let mut descriptor = json!({
                "profile": PROFILE, "capabilityVersion": 1,
                "driverVersion": env!("CARGO_PKG_VERSION"), "tools": tools(),
            });
            descriptor["schemaSha256"] = json!(format!(
                "sha256:{:x}",
                Sha256::digest(serde_json::to_vec(&descriptor).unwrap())
            ));
            descriptor
        })
        .clone()
}

/// The descriptor without its tools: what MCP `initialize` advertises under
/// `experimental["io.ambit/browser"]` and the agent channel's `hello` answers
/// as its catalog.
pub(crate) fn catalog_identity() -> Value {
    let mut descriptor = descriptor();
    descriptor.as_object_mut().unwrap().remove("tools");
    descriptor
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HostConfig {
    version: u32,
    namespace: String,
    session: String,
    require_sandbox: bool,
    #[serde(default)]
    browser_host: bool,
    capture_directory: PathBuf,
    expected_observation: Option<ObservationId>,
    semantic_judgement_config_path: Option<PathBuf>,
    semantic_judgement_client_module_path: Option<PathBuf>,
    node_network_bootstrap_path: Option<PathBuf>,
    /// The person's theme for every launch this configuration makes. Admitted
    /// exactly by the driver generations whose descriptor lists the theme
    /// operation.
    theme: Option<Theme>,
}

/// The session and launch settings a binding fixes for every call it makes.
/// A spawned client derives them from its configuration file
/// (`HostBinding::load`); the daemon's agent channel from a `hello`'s binding,
/// in the daemon's own environment, which the daemon inherited from the
/// client that started it. No tool argument changes them.
#[derive(Debug, Clone)]
pub(crate) struct HostFlags {
    flags: Flags,
}

/// How a prepared call reaches the browser.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Dispatch {
    /// A host operation: it acts on the running session as it is. It never
    /// starts a daemon or a browser, is neither agent input nor an
    /// observation, and captures no host feedback.
    Session,
    /// An agent operation, dispatched through the daemon's host feedback path
    /// (custody, observation gates, host deadline). `launch` starts the
    /// browser the binding describes when none runs.
    Agent { launch: Option<Value> },
}

/// A host-bound call as both transports dispatch it: checked against the
/// pinned profile and parsed by the canonical CLI parser. Nothing about it has
/// been sent anywhere.
#[derive(Debug, Clone)]
pub(crate) struct HostCall {
    pub(crate) command: Value,
    /// The call's host deadline.
    pub(crate) timeout_ms: u64,
    pub(crate) dispatch: Dispatch,
    /// The flags the call was parsed with, for a transport that must start
    /// the daemon they describe.
    flags: Flags,
}

impl HostFlags {
    pub(crate) fn new(
        namespace: &str,
        session: &str,
        theme: Option<Theme>,
    ) -> Result<Self, String> {
        let mut flags = parse_flags_from_config(&[], Config::default());
        flags.namespace = Some(namespace.to_string());
        flags.session = session.to_string();
        flags.require_sandbox = true;
        flags.json = true;
        // The binding is the only theme source here, env included.
        flags.theme = theme.map(|theme| theme.as_str().to_string());
        if flags.cdp.is_some() || flags.auto_connect || flags.provider.is_some() {
            return Err("Host-bound browser sessions require a locally owned browser.".into());
        }
        if let Some(path) = flags.ca_cert.as_ref() {
            crate::ca_bundle::load(path)?;
        }
        Ok(Self { flags })
    }

    /// Prepares `name` with `arguments`, in the order the host-bound profile
    /// checks a call: membership in the profile, no argument that overrides a
    /// host setting, the tool's own preparation and deadline bound, the
    /// canonical parser, then (for an agent operation) the binding's plugins,
    /// tab pinning, restore settings and launch configuration. A refusal is
    /// the message a host shows for invalid arguments; nothing was sent.
    pub(crate) fn prepare(&self, name: &str, arguments: &Value) -> Result<HostCall, String> {
        let schema = tool(name).ok_or("Tool is not in the host-bound browser profile.")?;
        let values = arguments.as_object().ok_or("arguments must be an object")?;
        let properties = schema["inputSchema"]["properties"].as_object().unwrap();
        if values.keys().any(|key| !properties.contains_key(key)) {
            return Err("Tool arguments cannot override host browser settings.".into());
        }
        let invocation = prepare_tool(name, arguments).map_err(|error| error.message)?;
        if invocation.timeout_ms > MAX_CALL_MS {
            return Err("timeoutMs must be at most 120000.".into());
        }
        let mut flags = self.flags.clone();
        apply_cli_flags(&invocation.global_args, &mut flags);
        let mut command = parse_command_with_input(
            &invocation.command_args,
            &flags,
            invocation.stdin_body.as_deref(),
        )
        .map_err(|error| error.format())?;
        let dispatch = if SESSION_OPERATIONS.contains(&name) {
            Dispatch::Session
        } else {
            crate::attach_plugins_to_command(&mut command, &flags.plugins);
            crate::attach_pin_tab_to_command(&mut command, &flags);
            crate::attach_restore_config_to_command(&mut command, &flags);
            Dispatch::Agent {
                launch: crate::should_send_local_launch_config(&flags, &command)
                    .then(|| crate::build_local_launch_command(&flags)),
            }
        };
        Ok(HostCall {
            command,
            timeout_ms: invocation.timeout_ms,
            dispatch,
            flags,
        })
    }
}

#[derive(Debug, Clone)]
pub(super) struct HostBinding {
    config: HostConfig,
    flags: HostFlags,
}

impl HostBinding {
    pub(super) fn load(path: &str) -> Result<Self, String> {
        let file = fs::File::open(path).map_err(|_| "Cannot read host browser configuration.")?;
        let mut bytes = Vec::new();
        file.take(65_537)
            .read_to_end(&mut bytes)
            .map_err(|_| "Cannot read host browser configuration.")?;
        if bytes.len() > 65_536 {
            return Err("Host browser configuration is too large.".into());
        }
        let mut config: HostConfig =
            serde_json::from_slice(&bytes).map_err(|_| "Invalid host browser configuration.")?;
        if config.version != 1
            || !config.require_sandbox
            || !crate::validation::is_valid_session_name(&config.session)
            || !crate::validation::is_valid_session_name(&config.namespace)
            || !config.capture_directory.is_absolute()
        {
            return Err(
                "Invalid host browser session, sandbox policy or capture directory.".into(),
            );
        }
        if crate::native::workspace_role::current()
            .map_err(str::to_owned)?
            .is_browser_host()
            != config.browser_host
        {
            return Err(
                "Host browser configuration differs from the immutable workspace role.".into(),
            );
        }
        config.capture_directory = config
            .capture_directory
            .canonicalize()
            .map_err(|_| "Host capture directory is unavailable.")?;
        if !config.capture_directory.is_dir() {
            return Err("Host capture directory is unavailable.".into());
        }
        // Set once before any daemon or runtime starts. Per-call tool data
        // cannot change socket resolution or the host's launch snapshot.
        env::set_var("AGENT_BROWSER_NAMESPACE", &config.namespace);
        let flags = HostFlags::new(&config.namespace, &config.session, config.theme)?;
        Ok(Self { config, flags })
    }

    /// The spawned client's transport: the shared preparation, then the
    /// daemon started if needed and the command sent to it with the
    /// configuration's host feedback request.
    pub(super) fn call(&self, name: &str, arguments: &Value) -> Result<Value, ProtocolError> {
        let HostCall {
            mut command,
            timeout_ms,
            dispatch,
            flags,
        } = self
            .flags
            .prepare(name, arguments)
            .map_err(ProtocolError::invalid_params)?;
        let launch = match dispatch {
            Dispatch::Session => {
                return Ok(result(send_command_if_running(command, &flags.session)))
            }
            Dispatch::Agent { launch } => launch,
        };
        let setup = crate::DaemonSetup::new(&flags);
        if let Err(error) = ensure_daemon(&flags.session, &setup.options(&flags)) {
            return Ok(result(Response {
                error: Some(error),
                code: Some("browser_runtime_unavailable".into()),
                ..Response::default()
            }));
        }
        if command["action"] == "run_playwright" {
            command[crate::native::playwright::ENVIRONMENT_FIELD] = json!({
                "semanticJudgementConfigPath": self.config.semantic_judgement_config_path,
                "semanticJudgementClientModulePath": self.config.semantic_judgement_client_module_path,
                "nodeNetworkBootstrapPath": self.config.node_network_bootstrap_path,
            });
        }
        command[REQUEST_FIELD] = json!({ "namespace": self.config.namespace,
            "session": self.config.session, "captureDirectory": self.config.capture_directory,
            "timeoutMs": timeout_ms, "expectedObservation": self.config.expected_observation,
            "launch": launch });
        Ok(result(
            send_command_detailed(command, &flags.session).unwrap_or_else(Response::from),
        ))
    }
}

/// A daemon response as the host-bound `CallToolResult`: the text the model
/// reads and the response itself. The spawned client adds the feedback it
/// captured (`result`); the agent channel reports feedback once per frame.
pub(crate) fn step_result(response: Response) -> Value {
    let success = response.success;
    let response = serde_json::to_value(response).unwrap();
    json!({ "isError": !success,
        "content": [{ "type": "text", "text": response_text(&response).unwrap_or_else(|| response.to_string()) }],
        "structuredContent": { "response": response },
    })
}

fn result(mut response: Response) -> Value {
    let browser = response.browser.take();
    let mut result = step_result(response);
    result["structuredContent"]["browser"] = json!(browser);
    result
}

#[cfg(test)]
pub(super) fn native_error_result_for_test(error: &str) -> Value {
    result(
        serde_json::from_value(crate::native::actions::native_error_response_for_test(
            error,
        ))
        .unwrap(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_catalog_preserves_browser_tools_and_removes_host_authority() {
        let tools = tools();
        for tool in &tools {
            let properties = tool["inputSchema"]["properties"].as_object().unwrap();
            for name in HOST_ARGUMENTS {
                assert!(!properties.contains_key(*name));
            }
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        }
        for name in [TOOL_OPEN, TOOL_EVAL, TOOL_CLICK, TOOL_SET_VIEWPORT] {
            assert!(tools.iter().any(|tool| tool["name"] == name));
        }
        for name in [
            TOOL_CONNECT,
            TOOL_INSTALL,
            TOOL_SESSION_LIST,
            TOOL_BATCH,
            TOOL_PLUGIN_RUN,
        ] {
            assert!(!tools.iter().any(|tool| tool["name"] == name));
        }
        let descriptor = descriptor();
        let mut basis = descriptor.clone();
        basis.as_object_mut().unwrap().remove("schemaSha256");
        assert_eq!(
            descriptor["schemaSha256"],
            format!(
                "sha256:{:x}",
                Sha256::digest(serde_json::to_vec(&basis).unwrap())
            )
        );
    }

    /// An effect with no visible input is not offered: the canonical catalog
    /// keeps `pushstate`, the host-bound one withholds it, and neither
    /// transport prepares a call to it.
    #[test]
    fn an_effect_without_visible_input_is_withheld() {
        assert!(super::super::tools()
            .iter()
            .any(|tool| tool["name"] == TOOL_PUSHSTATE));
        assert!(tool(TOOL_PUSHSTATE).is_none());
        let flags = HostFlags::new("host", "browser", None).unwrap();
        assert_eq!(
            flags
                .prepare(TOOL_PUSHSTATE, &json!({"url": "https://example.com/next"}))
                .unwrap_err(),
            "Tool is not in the host-bound browser profile."
        );
    }

    /// The person's sign-ins stay with the host. No host-bound tool reads or
    /// sets cookies or site storage, saves or loads browser state, keeps or
    /// types a password, or records a capture that holds request headers and
    /// cookies, though the canonical catalog keeps them all, and neither
    /// transport prepares a call to one.
    #[test]
    fn sign_ins_stay_with_the_host() {
        let withheld = [
            TOOL_COOKIES_GET,
            TOOL_COOKIES_SET,
            TOOL_COOKIES_SET_CURL,
            TOOL_COOKIES_CLEAR,
            TOOL_STORAGE_GET,
            TOOL_STORAGE_SET,
            TOOL_STORAGE_CLEAR,
            TOOL_STATE_SAVE,
            TOOL_STATE_LOAD,
            TOOL_STATE_SHOW,
            TOOL_STATE_LIST,
            TOOL_STATE_CLEAN,
            TOOL_STATE_CLEAR,
            TOOL_STATE_RENAME,
            TOOL_AUTH_SAVE,
            TOOL_AUTH_LOGIN,
            TOOL_AUTH_SHOW,
            TOOL_AUTH_LIST,
            TOOL_AUTH_DELETE,
            TOOL_SET_CREDENTIALS,
            TOOL_NETWORK_HAR_START,
            TOOL_NETWORK_HAR_STOP,
            TOOL_TRACE_START,
            TOOL_TRACE_STOP,
            TOOL_PROFILER_START,
            TOOL_PROFILER_STOP,
        ];
        let canonical = super::super::tools();
        let flags = HostFlags::new("host", "browser", None).unwrap();
        for name in withheld {
            assert!(canonical.iter().any(|tool| tool["name"] == name), "{name}");
            assert!(tool(name).is_none(), "{name}");
            assert_eq!(
                flags.prepare(name, &json!({})).unwrap_err(),
                "Tool is not in the host-bound browser profile.",
                "{name}"
            );
        }
    }

    /// Arguments that satisfy a tool's schema: each required property with a
    /// value of its type (the first of an enumeration).
    fn schema_arguments(schema: &Value) -> Value {
        let mut arguments = serde_json::Map::new();
        for name in schema["required"].as_array().into_iter().flatten() {
            let name = name.as_str().unwrap();
            let property = &schema["properties"][name];
            let value = if let Some(first) = property["enum"].get(0) {
                first.clone()
            } else {
                match property["type"].as_str() {
                    Some("number" | "integer") => json!(10),
                    Some("boolean") => json!(false),
                    Some("array") => json!(["value"]),
                    Some("object") => json!({ "X-Test": "value" }),
                    _ => json!(match name {
                        "url" | "url1" | "url2" => "https://example.test/",
                        "selector" | "source" | "target" | "frame" => "#element",
                        "path" => "/workspace/file",
                        "key" => "Enter",
                        "tab" => "t1",
                        _ => "value",
                    }),
                }
            };
            arguments.insert(name.to_string(), value);
        }
        Value::Object(arguments)
    }

    /// Every tool in the host catalog, as the daemon receives it: only input
    /// that goes to the focused element (a dialog answer included), the
    /// pointer or a point no image fences, and a program, which can send any
    /// of these, waits for a fresh observation after a person used the
    /// browser. Every other tool names what it acts on and runs.
    #[test]
    fn only_input_without_a_named_target_waits_after_a_handback() {
        use crate::native::actions::observation_required_after_handback_for_test as held;
        let flags = parse_flags_from_config(&[], Config::default());
        let implicit = [
            TOOL_PRESS,
            TOOL_KEYDOWN,
            TOOL_KEYUP,
            TOOL_KEYBOARD_TYPE,
            TOOL_KEYBOARD_INSERT_TEXT,
            TOOL_CLIPBOARD_COPY,
            TOOL_CLIPBOARD_PASTE,
            TOOL_MOUSE_DOWN,
            TOOL_MOUSE_UP,
            TOOL_MOUSE_MOVE,
            TOOL_MOUSE_WHEEL,
            TOOL_SWIPE,
            TOOL_DIALOG_ACCEPT,
            TOOL_DIALOG_DISMISS,
            TOOL_RUN_PLAYWRIGHT,
        ];
        let tools = tools();
        for name in implicit {
            assert!(tools.iter().any(|tool| tool["name"] == name), "{name}");
        }
        for tool in &tools {
            let name = tool["name"].as_str().unwrap();
            let mut arguments = schema_arguments(&tool["inputSchema"]);
            // The parser needs more than these schemas require.
            match name {
                TOOL_DIFF_SCREENSHOT => arguments["baseline"] = json!("/workspace/base.png"),
                TOOL_RECORD_START | TOOL_RECORD_RESTART => {
                    arguments["path"] = json!("/workspace/a.webm")
                }
                _ => {}
            }
            let invocation = prepare_tool(name, &arguments)
                .unwrap_or_else(|error| panic!("{name} {arguments}: {error:?}"));
            let command = parse_command_with_input(
                &invocation.command_args,
                &flags,
                invocation.stdin_body.as_deref(),
            )
            .unwrap_or_else(|error| panic!("{name} {arguments}: {}", error.format()));
            assert_eq!(
                held(&command),
                implicit.contains(&name),
                "{name}: {command}"
            );
        }
        // A point the host read from an image is fenced by that image.
        let invocation = prepare_tool(TOOL_MOUSE_MOVE, &json!({ "x": 10, "y": 20 })).unwrap();
        let mut command = parse_command_with_input(&invocation.command_args, &flags, None).unwrap();
        command[REQUEST_FIELD] = json!({ "expectedObservation": {
            "targetId": "T", "loaderId": "L", "pageGeneration": "G", "geometrySha256": "sha256:0",
        } });
        assert!(!held(&command), "{command}");
    }

    /// The theme belongs to the host: no model-visible schema carries it, the
    /// theme operation is marked for the host alone, and the configuration
    /// admits a theme exactly when the descriptor lists that operation.
    #[test]
    fn theme_is_a_host_operation_and_host_configuration() {
        let tools = tools();
        assert_eq!(tools.len(), 105);
        let set_theme = tools
            .iter()
            .find(|tool| tool["name"] == TOOL_SET_THEME)
            .unwrap();
        assert_eq!(set_theme["title"], "Set theme");
        assert_eq!(
            set_theme["description"],
            "Set the browser theme: Chrome's own window UI and every page's prefers-color-scheme. Pages switch now; supported private windows switch live; other managed windows follow at the next launch unless custom UI settings are pinned."
        );
        let schema = &set_theme["inputSchema"];
        assert_eq!(
            schema["properties"]["theme"],
            json!({ "type": "string", "enum": ["dark", "light"] })
        );
        assert_eq!(schema["required"], json!(["theme"]));
        assert_eq!(
            schema["properties"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["theme", "timeoutMs"]
        );
        for tool in &tools {
            let host_only = tool["_meta"]["io.ambit/browser"]["caller"] == "host";
            assert_eq!(
                host_only,
                HOST_CALLED.contains(&tool["name"].as_str().unwrap()),
                "{}",
                tool["name"]
            );
            if !host_only {
                assert!(
                    tool["inputSchema"]["properties"].get("theme").is_none(),
                    "{}",
                    tool["name"]
                );
            }
        }
        assert_eq!(
            set_theme["_meta"],
            json!({ "io.ambit/browser": { "caller": "host" } })
        );

        let config = |extra: Value| {
            let mut config = json!({ "version": 1, "namespace": "host", "session": "browser",
                "requireSandbox": true, "captureDirectory": "/" });
            config
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            serde_json::from_value::<HostConfig>(config)
        };
        assert_eq!(config(json!({})).unwrap().theme, None);
        assert_eq!(
            config(json!({ "theme": "dark" })).unwrap().theme,
            Some(Theme::Dark)
        );
        assert_eq!(
            config(json!({ "theme": "light" })).unwrap().theme,
            Some(Theme::Light)
        );
        assert!(config(json!({ "theme": "system" })).is_err());
        assert!(config(json!({ "colorScheme": "dark" })).is_err());
        assert_eq!(
            tools.iter().any(|tool| tool["name"] == TOOL_SET_THEME),
            config(json!({ "theme": "dark" })).is_ok()
        );
    }

    fn binding(theme: Option<Theme>) -> HostBinding {
        HostBinding {
            config: serde_json::from_value(json!({ "version": 1, "namespace": "host",
                "session": "browser", "requireSandbox": true, "captureDirectory": "/" }))
            .unwrap(),
            flags: HostFlags::new("host", "browser", theme).unwrap(),
        }
    }

    /// Both transports prepare a call here, so they refuse the same calls
    /// with the same words before anything is sent: the spawned client as
    /// invalid parameters, the agent channel as a rejected operation.
    #[test]
    fn both_transports_refuse_a_call_the_profile_refuses_with_the_same_words() {
        let binding = binding(None);
        for (name, arguments, message) in [
            (
                TOOL_BATCH,
                json!({ "commands": ["open https://example.test/"] }),
                "Tool is not in the host-bound browser profile.",
            ),
            (
                "agent_browser_nonexistent",
                json!({}),
                "Tool is not in the host-bound browser profile.",
            ),
            (
                TOOL_CLICK,
                json!({ "selector": "#go", "session": "elsewhere" }),
                "Tool arguments cannot override host browser settings.",
            ),
            (TOOL_CLICK, json!(["#go"]), "arguments must be an object"),
            (
                TOOL_GET_TEXT,
                json!({ "selector": "main", "timeoutMs": MAX_CALL_MS + 1 }),
                "timeoutMs must be at most 120000.",
            ),
        ] {
            let refused = binding.flags.prepare(name, &arguments).unwrap_err();
            assert_eq!(refused, message, "{name} {arguments}");
            let error = binding.call(name, &arguments).unwrap_err();
            assert_eq!(error.code, -32602);
            assert_eq!(error.message, refused, "{name} {arguments}");
        }
        // A tool's own preparation and the canonical parser refuse too.
        let refused = binding
            .flags
            .prepare(TOOL_CLICK, &json!({ "selector": 7 }))
            .unwrap_err();
        assert_eq!(
            binding
                .call(TOOL_CLICK, &json!({ "selector": 7 }))
                .unwrap_err()
                .message,
            refused
        );
    }

    /// A host operation acts on the running session without launch settings;
    /// an agent operation carries the binding's plugins, pinning, restore
    /// settings and launch configuration, whichever transport sends it.
    #[test]
    fn a_prepared_call_says_how_it_reaches_the_browser() {
        let themed = binding(Some(Theme::Dark));
        let theme = themed
            .flags
            .prepare(TOOL_SET_THEME, &json!({ "theme": "light" }))
            .unwrap();
        assert_eq!(theme.dispatch, Dispatch::Session);
        assert_eq!(theme.command["action"], crate::native::theme::ACTION);
        assert!(theme.command.get("plugins").is_none());
        assert_eq!(theme.timeout_ms, DEFAULT_TIMEOUT_MS);

        let click = themed
            .flags
            .prepare(TOOL_CLICK, &json!({ "selector": "#go", "timeoutMs": 900 }))
            .unwrap();
        assert_eq!(click.command["action"], "click");
        assert_eq!(click.command["selector"], "#go");
        assert!(click.command["plugins"].is_array());
        assert_eq!(click.timeout_ms, 900);
        let Dispatch::Agent {
            launch: Some(launch),
        } = click.dispatch
        else {
            panic!(
                "a themed binding launches with its theme: {:?}",
                click.dispatch
            );
        };
        assert_eq!(launch["action"], "launch");
        assert_eq!(launch["theme"], "dark");

        // Opening background tabs is the host's to call and never the
        // model's, yet it is an agent operation: custody, gates, launch.
        let open_many = themed
            .flags
            .prepare(
                TOOL_OPEN_MANY,
                &json!({ "urls": ["https://a.example/", "https://b.example/"] }),
            )
            .unwrap();
        assert_eq!(
            tool(TOOL_OPEN_MANY).unwrap()["_meta"],
            json!({ "io.ambit/browser": { "caller": "host" } })
        );
        assert_eq!(open_many.command["action"], "tab_new");
        assert_eq!(open_many.command["background"], true);
        assert_eq!(
            open_many.command["urls"],
            json!(["https://a.example/", "https://b.example/"])
        );
        assert!(matches!(
            open_many.dispatch,
            Dispatch::Agent { launch: Some(_) }
        ));
    }

    #[test]
    fn data_is_not_reparsed_as_host_options() {
        let flags = parse_flags_from_config(&[], Config::default());
        let invocation = prepare_tool(
            TOOL_FILL,
            &json!({ "selector": "#input", "text": "--session outside --no-sandbox" }),
        )
        .unwrap();
        let command = parse_command_with_input(&invocation.command_args, &flags, None).unwrap();
        assert_eq!(command["value"], "--session outside --no-sandbox");
    }
}
