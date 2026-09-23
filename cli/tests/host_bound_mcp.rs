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
        self.configure_with(json!({ "expectedObservation": expected }));
    }

    /// Host configuration for the next call; `extra` adds host-only fields.
    fn configure_with(&self, extra: Value) {
        let mut config = json!({
            "version": 1, "namespace": "host-mcp-test", "session": "browser",
            "requireSandbox": true, "captureDirectory": self.path("captures"),
        });
        config
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        fs::write(
            self.path("config.json"),
            serde_json::to_vec(&config).unwrap(),
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
        for (test, runtime) in [
            ("AMBIT_TEST_NODE", "AGENT_BROWSER_NODE_PATH"),
            (
                "AMBIT_TEST_PLAYWRIGHT_MODULE",
                "AGENT_BROWSER_PLAYWRIGHT_MODULE",
            ),
        ] {
            if let Ok(value) = std::env::var(test) {
                command.env(runtime, value);
            }
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

    #[cfg(unix)]
    fn control(&self, mut request: Value) -> Value {
        use std::io::{BufRead, BufReader};
        use std::os::unix::net::UnixStream;
        request["action"] = json!("ambit_browser_control");
        let mut socket =
            UnixStream::connect(self.path("sockets/namespaces/host-mcp-test/run/browser.sock"))
                .unwrap();
        writeln!(socket, "{request}").unwrap();
        let mut line = String::new();
        BufReader::new(socket).read_line(&mut line).unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["success"], true, "{response}");
        response
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
        "loaderId": browser["page"]["loaderId"], "pageGeneration": browser["page"]["pageGeneration"],
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

#[test]
#[cfg(unix)]
#[ignore = "requires AMBIT_TEST_CHROME_EXECUTABLE with working Chrome sandbox"]
fn host_human_handoff_requires_fresh_observation_and_rejects_old_image_coordinates() {
    let host = Host::new();
    let opened = host.call("agent_browser_open", json!({ "url": "data:text/html,<style>input,button{display:block;height:40px;width:200px}</style><input id=field><button onclick='window.clicks++'>Count</button><script>window.clicks=0</script>" }));
    let before = host.capture(&opened);
    let owner = uuid::Uuid::new_v4().to_string();
    let expires = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 25000;
    host.control(json!({ "op": "acquire", "controllerId": owner, "expiresAt": expires }));
    let blocked = host.call("agent_browser_click", json!({ "selector": "button" }));
    assert_eq!(
        blocked["structuredContent"]["response"]["code"],
        "browser_controlled_by_user"
    );
    host.control(json!({ "op": "input", "controllerId": owner, "sequence": 1, "events": [
        { "type": "input_mouse", "eventType": "mousePressed", "x": 20, "y": 20, "button": "left", "buttons": 1, "clickCount": 1 },
        { "type": "input_mouse", "eventType": "mouseReleased", "x": 20, "y": 20, "button": "left", "buttons": 0, "clickCount": 1 },
        { "type": "input_keyboard", "eventType": "insertText", "text": "Human changed this" }
    ] }));
    host.control(json!({ "op": "release", "controllerId": owner }));
    let resumed = host.call("agent_browser_click", json!({ "selector": "button" }));
    assert_eq!(
        resumed["structuredContent"]["response"]["code"],
        "browser_observation_required"
    );
    let current = host.capture(&resumed);
    assert_ne!(
        current["page"]["pageGeneration"],
        before["page"]["pageGeneration"]
    );
    let count = host.call("agent_browser_eval", json!({ "script": "window.clicks" }));
    assert_eq!(count["structuredContent"]["response"]["data"]["result"], 0);
    host.configure(Some(
        json!({ "targetId": before["page"]["targetId"], "loaderId": before["page"]["loaderId"],
        "pageGeneration": before["page"]["pageGeneration"],
        "geometrySha256": before["capture"]["coordinateSpace"]["geometrySha256"] }),
    ));
    let stale = host.call("agent_browser_mouse_move", json!({ "x": 20, "y": 20 }));
    assert_eq!(
        stale["structuredContent"]["response"]["code"],
        "browser_observation_stale"
    );
    host.configure(None);
    let value = host.call("agent_browser_get_value", json!({ "selector": "#field" }));
    assert_eq!(
        value["structuredContent"]["response"]["data"]["value"],
        "Human changed this"
    );
    assert_eq!(
        host.call("agent_browser_click", json!({ "selector": "button" }))["isError"],
        false
    );
    let count = host.call("agent_browser_eval", json!({ "script": "window.clicks" }));
    assert_eq!(count["structuredContent"]["response"]["data"]["result"], 1);
    assert_eq!(
        host.call("agent_browser_close", json!({}))["isError"],
        false
    );
}

/// The per-Action semantic binding reaches only its own Playwright program:
/// page text chooses a later click in the same browser, successive Actions
/// get their own bindings, a call without one gets none, and credentials
/// never appear in tool results, even when the relay map is corrupt.
#[test]
#[cfg(unix)]
#[ignore = "requires AMBIT_TEST_CHROME_EXECUTABLE, AMBIT_TEST_NODE and AMBIT_TEST_PLAYWRIGHT_MODULE"]
fn host_playwright_program_uses_only_its_own_semantic_binding() {
    use std::os::unix::fs::PermissionsExt;
    let host = Host::new();
    // Stands in for the host SDK factory: it reports which binding it was
    // created with and, like the real client, never echoes its credential.
    fs::write(
        host.path("client.cjs"),
        r#"exports.createAmbitSemanticJudgementClient = (environment) => {
  if (typeof environment.AMBIT_TEST_BEARER !== 'string') throw new Error('unavailable');
  return async ({ state, questions }) => ({
    binding: environment.AMBIT_TEST_BINDING,
    answers: questions.map((question) => state.text.includes(question.contains)),
  });
};
"#,
    )
    .unwrap();
    let secrets = ["relay-secret-a-7f3a91", "relay-secret-b-c04e22"];
    let relay = |name: &str, content: String| {
        let path = host.path(name);
        fs::write(&path, content).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    };
    let bind = |path: &Path| {
        host.configure_with(json!({
            "semanticJudgementConfigPath": path,
            "semanticJudgementClientModulePath": host.path("client.cjs"),
        }));
    };
    let assert_private = |reply: &Value| {
        let text = reply.to_string();
        for secret in secrets {
            assert!(!text.contains(secret), "credential leaked: {text}");
        }
        assert!(!text.contains("relay-"), "relay path leaked: {text}");
    };
    let opened = host.call(
        "agent_browser_open",
        json!({ "url": "data:text/html,<h1>Invoice 255 is overdue</h1><button onclick=\"document.title='Escalated'\">Escalate</button>" }),
    );
    assert_eq!(opened["isError"], false, "{opened}");
    let program = "const text = await page.locator('h1').innerText(); const judged = await semanticJudgement({ state: { text }, questions: [{ contains: 'overdue' }] }); if (judged.answers[0]) await page.getByRole('button', { name: 'Escalate' }).click(); return { judged, title: await page.title(), inherited: ['AMBIT_TEST_BINDING', 'AMBIT_TEST_BEARER'].filter((key) => key in process.env) };";

    bind(&relay(
        "relay-a.json",
        json!({ "AMBIT_TEST_BINDING": "action-a", "AMBIT_TEST_BEARER": secrets[0] }).to_string(),
    ));
    let first = host.call("agent_browser_run_playwright", json!({ "code": program }));
    assert_eq!(first["isError"], false, "{first}");
    let result = &first["structuredContent"]["response"]["data"]["result"];
    assert_eq!(
        result["judged"],
        json!({ "binding": "action-a", "answers": [true] })
    );
    assert_eq!(result["title"], "Escalated");
    assert_eq!(result["inherited"], json!([]));
    assert_private(&first);

    bind(&relay(
        "relay-b.json",
        json!({ "AMBIT_TEST_BINDING": "action-b", "AMBIT_TEST_BEARER": secrets[1] }).to_string(),
    ));
    let second = host.call(
        "agent_browser_run_playwright",
        json!({ "code": "return (await semanticJudgement({ state: { text: 'none' }, questions: [] })).binding;" }),
    );
    assert_eq!(
        second["structuredContent"]["response"]["data"]["result"], "action-b",
        "{second}"
    );
    assert_private(&second);

    host.configure(None);
    let unbound = host.call(
        "agent_browser_run_playwright",
        json!({ "code": "return typeof semanticJudgement;" }),
    );
    assert_eq!(
        unbound["structuredContent"]["response"]["data"]["result"], "undefined",
        "{unbound}"
    );

    // A corrupt map: JSON errors would quote these bytes.
    bind(&relay("relay-corrupt.json", format!("{} {{", secrets[0])));
    let corrupt = host.call(
        "agent_browser_run_playwright",
        json!({ "code": "await page.evaluate(() => document.title = 'Program ran');" }),
    );
    assert_eq!(corrupt["isError"], true, "{corrupt}");
    assert_eq!(
        corrupt["structuredContent"]["response"]["code"], "browser_operation_rejected",
        "{corrupt}"
    );
    assert_private(&corrupt);
    host.configure(None);
    let title = host.call("agent_browser_get_title", json!({}));
    assert_eq!(
        title["structuredContent"]["response"]["data"]["title"], "Escalated",
        "{title}"
    );
    assert_eq!(
        host.call("agent_browser_close", json!({}))["isError"],
        false
    );
}
