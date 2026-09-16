//! The host selects one browser session; tool arguments remain browser data.
//! Schemas derive from the normal MCP catalog and commands use its preparation
//! helpers and the canonical CLI parser, without reparsing data as global flags.

use super::*;
use crate::commands::parse_command_with_input;
use crate::connection::{ensure_daemon, send_command_detailed, Response};
use crate::flags::{apply_cli_flags, parse_flags_from_config, Config, Flags};
use crate::native::feedback::{ObservationId, MAX_CALL_MS, REQUEST_FIELD};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

pub(super) const PROFILE: &str = "ambit-host-bound-v1";

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

/// Process discovery, supervisor configuration, dependency installation and
/// cross-session commands belong to the host. Browser auth and state stay in
/// the normal admitted filesystem and retain the canonical tool semantics.
pub(super) fn allows(name: &str) -> bool {
    !matches!(
        name,
        TOOL_TOOLS_PROFILES
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
    )
}

pub(super) fn tools() -> Vec<Value> {
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
            for key in ["headed", "webgpu", "webmcp"] {
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
    }
    tools.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    tools
}

pub(super) fn descriptor() -> Value {
    // serde_json's default object map sorts keys recursively. Tool names are
    // sorted too, matching the host's canonical-JSON catalog digest.
    let mut descriptor = json!({
        "profile": PROFILE, "capabilityVersion": 1,
        "driverVersion": env!("CARGO_PKG_VERSION"), "tools": tools(),
    });
    descriptor["schemaSha256"] = json!(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&descriptor).unwrap())
    ));
    descriptor
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HostConfig {
    version: u32,
    namespace: String,
    session: String,
    require_sandbox: bool,
    capture_directory: PathBuf,
    expected_observation: Option<ObservationId>,
}

#[derive(Debug, Clone)]
pub(super) struct HostBinding {
    config: HostConfig,
    flags: Flags,
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
        let mut flags = parse_flags_from_config(&[], Config::default());
        flags.namespace = Some(config.namespace.clone());
        flags.session = config.session.clone();
        flags.require_sandbox = true;
        flags.json = true;
        if flags.cdp.is_some() || flags.auto_connect || flags.provider.is_some() {
            return Err("Host-bound browser sessions require a locally owned browser.".into());
        }
        if let Some(path) = flags.ca_cert.as_ref() {
            crate::ca_bundle::load(path)?;
        }
        Ok(Self { config, flags })
    }

    pub(super) fn call(&self, name: &str, arguments: &Value) -> Result<Value, ProtocolError> {
        let schema = tools()
            .into_iter()
            .find(|tool| tool["name"] == name)
            .ok_or_else(|| {
                ProtocolError::invalid_params("Tool is not in the host-bound browser profile.")
            })?;
        let values = arguments
            .as_object()
            .ok_or_else(|| ProtocolError::invalid_params("arguments must be an object"))?;
        let properties = schema["inputSchema"]["properties"].as_object().unwrap();
        if values.keys().any(|key| !properties.contains_key(key)) {
            return Err(ProtocolError::invalid_params(
                "Tool arguments cannot override host browser settings.",
            ));
        }
        let invocation = prepare_tool(name, arguments)?;
        if invocation.timeout_ms > MAX_CALL_MS {
            return Err(ProtocolError::invalid_params(
                "timeoutMs must be at most 120000.",
            ));
        }
        let mut flags = self.flags.clone();
        apply_cli_flags(&invocation.global_args, &mut flags);
        let mut command = parse_command_with_input(
            &invocation.command_args,
            &flags,
            invocation.stdin_body.as_deref(),
        )
        .map_err(|error| ProtocolError::invalid_params(error.format()))?;
        crate::attach_plugins_to_command(&mut command, &flags.plugins);
        crate::attach_pin_tab_to_command(&mut command, &flags);
        crate::attach_restore_config_to_command(&mut command, &flags);
        let setup = crate::DaemonSetup::new(&flags);
        if let Err(error) = ensure_daemon(&flags.session, &setup.options(&flags)) {
            return Ok(result(Response {
                error: Some(error),
                code: Some("browser_runtime_unavailable".into()),
                ..Response::default()
            }));
        }
        let launch = crate::should_send_local_launch_config(&flags, &command)
            .then(|| crate::build_local_launch_command(&flags));
        command[REQUEST_FIELD] = json!({ "namespace": self.config.namespace,
            "session": self.config.session, "captureDirectory": self.config.capture_directory,
            "timeoutMs": invocation.timeout_ms, "expectedObservation": self.config.expected_observation,
            "launch": launch });
        Ok(result(send(command, &flags.session)))
    }
}

fn send(command: Value, session: &str) -> Response {
    send_command_detailed(command, session).unwrap_or_else(|error| Response {
        error: Some(error.message),
        code: Some(
            if error.outcome_unknown {
                "command_outcome_unknown"
            } else {
                "browser_runtime_unavailable"
            }
            .into(),
        ),
        ..Response::default()
    })
}

fn result(mut response: Response) -> Value {
    let browser = response.browser.take();
    let success = response.success;
    let response = serde_json::to_value(response).unwrap();
    json!({ "isError": !success,
        "content": [{ "type": "text", "text": response_text(&response).unwrap_or_else(|| response.to_string()) }],
        "structuredContent": { "response": response, "browser": browser },
    })
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
        for name in [
            TOOL_OPEN,
            TOOL_EVAL,
            TOOL_CLICK,
            TOOL_AUTH_SAVE,
            TOOL_AUTH_LOGIN,
            TOOL_STATE_SAVE,
            TOOL_STATE_LOAD,
            TOOL_SET_VIEWPORT,
        ] {
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
