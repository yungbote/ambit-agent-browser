//! Real MCP processes prove the catalog and host boundary; opt-in Chromium
//! exercises command execution, native pixels and stale image coordinates.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_agent-browser");

struct Host {
    directory: TempDir,
}

impl Host {
    fn new() -> Self {
        let host = Self {
            directory: tempfile::tempdir().unwrap(),
        };
        fs::create_dir(host.path("captures")).unwrap();
        fs::create_dir(host.path("sockets")).unwrap();
        host.configure(None);
        host
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.directory.path().join(name)
    }

    fn configure(&self, expected: Option<Value>) {
        fs::write(
            self.path("config.json"),
            serde_json::to_vec(&json!({
                "version": 1, "namespace": "host-mcp-test", "session": "browser",
                "requireSandbox": true, "captureDirectory": self.path("captures"),
                "expectedObservation": expected,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("AGENT_BROWSER_") {
                command.env_remove(key);
            }
        }
        command
            .args(["mcp", "--host-bound-config"])
            .arg(self.path("config.json"))
            .env("AGENT_BROWSER_SOCKET_DIR", self.path("sockets"))
            .env("AGENT_BROWSER_NO_WEBMCP", "1")
            .env("AGENT_BROWSER_HEADED", "false")
            .env("AGENT_BROWSER_IDLE_TIMEOUT", "20s")
            .env("NO_COLOR", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Ok(chrome) = std::env::var("AMBIT_TEST_CHROME_EXECUTABLE") {
            command.env("AGENT_BROWSER_EXECUTABLE_PATH", chrome);
        }
        command
    }

    fn request(&self, method: &str, params: Value) -> Value {
        let mut child = self.command().spawn().unwrap();
        writeln!(
            child.stdin.take().unwrap(),
            "{}",
            json!({
                "jsonrpc": "2.0", "id": 1, "method": method, "params": params,
            })
        )
        .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn call(&self, name: &str, arguments: Value) -> Value {
        let reply = self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        );
        assert!(reply.get("error").is_none(), "{reply}");
        reply["result"].clone()
    }

    fn capture(&self, result: &Value) -> Value {
        let browser = &result["structuredContent"]["browser"];
        assert_eq!(browser["namespace"], "host-mcp-test");
        assert_eq!(browser["session"], "browser");
        let capture = &browser["capture"];
        let path = Path::new(capture["path"].as_str().unwrap());
        assert!(path.starts_with(self.path("captures")));
        let bytes = fs::read(path).unwrap();
        assert_eq!(capture["sizeBytes"], bytes.len());
        assert_eq!(
            capture["sha256"],
            format!("sha256:{:x}", Sha256::digest(&bytes))
        );
        let image = image::load_from_memory(&bytes).unwrap();
        assert_eq!(capture["width"], image.width());
        assert_eq!(capture["height"], image.height());
        assert_eq!(capture["coordinateSpace"]["name"], "viewport-css");
        browser.clone()
    }
}

#[test]
fn host_descriptor_matches_paginated_protocol_catalog() {
    let descriptor: Value = serde_json::from_slice(
        &Command::new(BIN)
            .args(["mcp", "--describe-host-bound"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let host = Host::new();
    let initialize = host.request("initialize", json!({}));
    let capability = &initialize["result"]["capabilities"]["experimental"]["io.ambit/browser"];
    for key in [
        "profile",
        "capabilityVersion",
        "driverVersion",
        "schemaSha256",
    ] {
        assert_eq!(capability[key], descriptor[key]);
    }
    let mut tools = Vec::new();
    let mut params = json!({});
    loop {
        let response = host.request("tools/list", params);
        tools.extend(response["result"]["tools"].as_array().unwrap().clone());
        let Some(cursor) = response["result"]["nextCursor"].as_str() else {
            break;
        };
        params = json!({ "cursor": cursor });
    }
    assert_eq!(Value::Array(tools), descriptor["tools"]);
    assert_eq!(fs::read_dir(host.path("sockets")).unwrap().count(), 0);
}

#[test]
fn host_overrides_are_rejected_before_daemon_start() {
    let host = Host::new();
    for arguments in [
        json!({ "namespace": "outside" }),
        json!({ "session": "outside" }),
        json!({ "extraArgs": ["--no-sandbox"] }),
        json!({ "headed": true }),
        json!({ "timeoutMs": 120001 }),
    ] {
        let response = host.request(
            "tools/call",
            json!({ "name": "agent_browser_open", "arguments": arguments }),
        );
        assert_eq!(response["error"]["code"], -32602, "{response}");
    }
    assert_eq!(fs::read_dir(host.path("sockets")).unwrap().count(), 0);
}

#[test]
#[ignore = "requires AMBIT_TEST_CHROME_EXECUTABLE with working Chrome sandbox"]
fn host_real_chromium_outcomes_pixels_and_coordinate_identity() {
    let host = Host::new();
    let opened = host.call("agent_browser_open", json!({ "url": "data:text/html,<title>Host browser proof</title><input id=field><button id=button>Continue</button>" }));
    assert_eq!(opened["isError"], false, "{opened}");
    host.capture(&opened);
    let filled = host.call(
        "agent_browser_fill",
        json!({ "selector": "#field", "text": "--session outside \"typed\"" }),
    );
    assert_eq!(filled["isError"], false, "{filled}");
    host.capture(&filled);
    let read = host.call("agent_browser_get_value", json!({ "selector": "#field" }));
    assert_eq!(
        read["structuredContent"]["response"]["data"]["value"],
        "--session outside \"typed\""
    );
    let resized = host.call(
        "agent_browser_set_viewport",
        json!({ "width": 640, "height": 480 }),
    );
    let browser = host.capture(&resized);
    assert_eq!(browser["capture"]["coordinateSpace"]["cssWidth"], 640.0);
    assert_eq!(browser["capture"]["coordinateSpace"]["cssHeight"], 480.0);
    let failed = host.call(
        "agent_browser_get_value",
        json!({ "selector": "#absent", "timeoutMs": 50 }),
    );
    assert_eq!(failed["isError"], true);
    host.capture(&failed);
    let observation = json!({ "targetId": browser["page"]["targetId"],
        "loaderId": browser["page"]["loaderId"],
        "geometrySha256": browser["capture"]["coordinateSpace"]["geometrySha256"] });
    host.configure(Some(observation));
    let current = host.call("agent_browser_mouse_move", json!({ "x": 10, "y": 10 }));
    assert_eq!(current["isError"], false, "{current}");
    let changed = host.call(
        "agent_browser_set_viewport",
        json!({ "width": 700, "height": 500 }),
    );
    assert_eq!(changed["isError"], false, "{changed}");
    let stale = host.call("agent_browser_mouse_move", json!({ "x": 10, "y": 10 }));
    assert_eq!(
        stale["structuredContent"]["response"]["code"],
        "browser_observation_stale"
    );
    host.capture(&stale);
    host.configure(None);
    let closed = host.call("agent_browser_close", json!({}));
    assert_eq!(closed["isError"], false, "{closed}");
    assert_eq!(
        closed["structuredContent"]["browser"]["capture"]["status"],
        "unavailable"
    );
}
