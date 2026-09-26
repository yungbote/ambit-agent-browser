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
        json!({ "theme": "dark" }),
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

/// The theme operation acts on a running session only: with none it starts
/// no daemon and captures nothing. The configuration's theme is validated
/// when the host binding loads.
#[test]
fn host_theme_operation_starts_nothing_and_its_configuration_is_validated() {
    let host = Host::new();
    host.configure_with(json!({ "theme": "dark" }));
    let result = host.call("agent_browser_set_theme", json!({ "theme": "light" }));
    assert_eq!(result["isError"], true, "{result}");
    assert_eq!(
        result["structuredContent"]["response"]["code"],
        "browser_runtime_unavailable"
    );
    assert!(result["structuredContent"]["browser"].is_null());
    assert_eq!(fs::read_dir(host.path("sockets")).unwrap().count(), 0);

    host.configure_with(json!({ "theme": "system" }));
    let output = host.command().output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Invalid host browser configuration"));
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

/// After a person hands the browser back, a command that names what it acts
/// on runs and returns a fresh observation; only input addressed to the
/// focused element or the pointer waits for one, and a point from an image
/// taken before the hand-back is stale.
#[test]
#[cfg(unix)]
#[ignore = "requires AMBIT_TEST_CHROME_EXECUTABLE with working Chrome sandbox"]
fn host_human_handoff_holds_only_input_without_a_named_target() {
    let host = Host::new();
    let opened = host.call("agent_browser_open", json!({ "url": "data:text/html,<title>Form</title><style>input,button{display:block;height:40px;width:200px}</style><input id=field><button onclick='window.clicks++'>Count</button><script>window.clicks=0;window.enters=0;addEventListener('keydown',e=>{if(e.key==='Enter')window.enters++})</script>" }));
    let before = host.capture(&opened);
    let hand_over = |events: Value| {
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
        if events.as_array().is_some_and(|events| !events.is_empty()) {
            host.control(
                json!({ "op": "input", "controllerId": owner, "sequence": 1, "events": events }),
            );
        }
        host.control(json!({ "op": "release", "controllerId": owner }));
    };
    let eval = |script: &str| {
        let result = host.call("agent_browser_eval", json!({ "script": script }));
        assert_eq!(result["isError"], false, "{result}");
        result["structuredContent"]["response"]["data"]["result"].clone()
    };

    // The person types into the field; the agent's next command names its
    // target and runs, with an observation of the page the person left.
    hand_over(json!([
        { "type": "input_mouse", "eventType": "mousePressed", "x": 20, "y": 20, "button": "left", "buttons": 1, "clickCount": 1 },
        { "type": "input_mouse", "eventType": "mouseReleased", "x": 20, "y": 20, "button": "left", "buttons": 0, "clickCount": 1 },
        { "type": "input_keyboard", "eventType": "insertText", "text": "Human changed this" }
    ]));
    let url = host.call("agent_browser_get_url", json!({}));
    assert_eq!(url["isError"], false, "{url}");
    let current = host.capture(&url);
    assert_ne!(
        current["page"]["pageGeneration"],
        before["page"]["pageGeneration"]
    );
    let value = host.call("agent_browser_get_value", json!({ "selector": "#field" }));
    assert_eq!(
        value["structuredContent"]["response"]["data"]["value"],
        "Human changed this"
    );
    assert_eq!(
        host.call("agent_browser_click", json!({ "selector": "button" }))["isError"],
        false
    );
    assert_eq!(eval("window.clicks"), 1);

    // A key goes to whatever has focus, which the person may have moved:
    // it waits for one observation, which its refusal carries.
    hand_over(json!([]));
    let held = host.call("agent_browser_press", json!({ "key": "Enter" }));
    assert_eq!(
        held["structuredContent"]["response"]["code"], "browser_observation_required",
        "{held}"
    );
    host.capture(&held);
    assert_eq!(eval("window.enters"), 0);
    let pressed = host.call("agent_browser_press", json!({ "key": "Enter" }));
    assert_eq!(pressed["isError"], false, "{pressed}");
    assert_eq!(eval("window.enters"), 1);

    // A point read from the image taken before the hand-back is stale.
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

    // Opening a page is the first command after a hand-back most often.
    hand_over(json!([]));
    let reopened = host.call(
        "agent_browser_open",
        json!({ "url": "data:text/html,<title>Next page</title><p>next</p>" }),
    );
    assert_eq!(reopened["isError"], false, "{reopened}");
    assert_eq!(
        host.capture(&reopened)["page"]["url"],
        "data:text/html,<title>Next page</title><p>next</p>"
    );
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

/// The configured theme reaches pages at launch; while the person holds the
/// browser the theme operation is admitted where agent operations are
/// refused, switches pages at once, and leaves the observation the agent
/// owes after the handoff pending.
#[test]
#[cfg(unix)]
#[ignore = "requires AMBIT_TEST_CHROME_EXECUTABLE with working Chrome sandbox"]
fn host_theme_launches_from_configuration_and_switches_under_person_control() {
    let host = Host::new();
    host.configure_with(json!({ "theme": "dark" }));
    let scheme = |host: &Host| {
        let result = host.call(
            "agent_browser_eval",
            json!({ "script": "matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light'" }),
        );
        assert_eq!(result["isError"], false, "{result}");
        result["structuredContent"]["response"]["data"]["result"].clone()
    };
    let opened = host.call(
        "agent_browser_open",
        json!({ "url": "data:text/html,<button>Count</button>" }),
    );
    assert_eq!(opened["isError"], false, "{opened}");
    assert_eq!(scheme(&host), "dark");

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
    let switched = host.call("agent_browser_set_theme", json!({ "theme": "light" }));
    assert_eq!(switched["isError"], false, "{switched}");
    assert_eq!(
        switched["structuredContent"]["response"]["data"],
        json!({ "theme": "light", "pages": "live", "ui": "none" })
    );
    assert!(switched["structuredContent"]["browser"].is_null());
    host.control(json!({ "op": "release", "controllerId": owner }));

    let again = host.call("agent_browser_set_theme", json!({ "theme": "light" }));
    assert_eq!(again["isError"], false, "{again}");
    let owed = host.call("agent_browser_press", json!({ "key": "Enter" }));
    assert_eq!(
        owed["structuredContent"]["response"]["code"],
        "browser_observation_required"
    );
    assert_eq!(scheme(&host), "light");
    assert_eq!(
        host.call("agent_browser_close", json!({}))["isError"],
        false
    );
}

/// The production failure through the host-bound catalog: a program that
/// reads `document` in Node before any Playwright call reaches the model as a
/// program error it can fix, with the error's place in the program, and the
/// page is untouched.
#[test]
#[cfg(unix)]
#[ignore = "requires AMBIT_TEST_CHROME_EXECUTABLE, AMBIT_TEST_NODE and AMBIT_TEST_PLAYWRIGHT_MODULE"]
fn host_playwright_program_that_fails_before_any_call_is_a_program_error() {
    let host = Host::new();
    let opened = host.call(
        "agent_browser_open",
        json!({ "url": "data:text/html,<title>Untouched</title><table><tr><td>1</td></tr></table>" }),
    );
    assert_eq!(opened["isError"], false, "{opened}");
    let refused = host.call(
        "agent_browser_run_playwright",
        json!({ "code": "const rows = document.querySelectorAll('tr');\nreturn rows.length;" }),
    );
    assert_eq!(refused["isError"], true, "{refused}");
    let response = &refused["structuredContent"]["response"];
    assert_eq!(response["code"], "browser_program_error", "{refused}");
    assert_eq!(
        response["data"],
        json!({"error":{"name":"ReferenceError","message":"document is not defined","line":1,"column":14},"pageCallsIssued":0})
    );
    let text = refused["content"][0]["text"].as_str().unwrap();
    assert!(
        text.starts_with("browser_program_error: The program failed before it issued any Playwright call, so nothing was done in the browser.")
            && text.ends_with("Line 1, column 14: ReferenceError: document is not defined"),
        "{text}"
    );
    let fixed = host.call(
        "agent_browser_run_playwright",
        json!({ "code": "return await page.evaluate(() => document.querySelectorAll('tr').length);" }),
    );
    assert_eq!(fixed["isError"], false, "{fixed}");
    assert_eq!(fixed["structuredContent"]["response"]["data"]["result"], 1);
    let title = host.call("agent_browser_get_title", json!({}));
    assert_eq!(
        title["structuredContent"]["response"]["data"]["title"], "Untouched",
        "{title}"
    );
    assert_eq!(
        host.call("agent_browser_close", json!({}))["isError"],
        false
    );
}
