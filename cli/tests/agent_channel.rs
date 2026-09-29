//! The agent channel against a real browser: a host-bound MCP client starts
//! the daemon over the file protocol, as the host's first step does, then a
//! channel speaks to the daemon's socket exactly as the toolbox's agent route
//! writes: one line per frame, `{"action":"ambit_browser_agent", …}`, and one
//! reply line per frame, in order.
//!
//! Needs Chrome (`AMBIT_TEST_CHROME_EXECUTABLE`) with a working sandbox, Xvfb,
//! and the display helper (`AGENT_BROWSER_DISPLAY_HELPER`) for the owned
//! window the motion layer drives. `AMBIT_AGENT_CHANNEL_EVIDENCE` names a
//! directory each test writes its measurements to. Run them one at a time
//! (`--test-threads=1`), with `--release` for timings an image would show.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_agent-browser");
const NAMESPACE: &str = "3f2a9c1e5b7d4e6f8a0b1c2d3e4f5a6b";
const ACTION: &str = "8c1f0e2a-3b4c-4d5e-8f60-718293a4b5c6";
const REQUIRES: &str =
    "requires AMBIT_TEST_CHROME_EXECUTABLE, Xvfb and AGENT_BROWSER_DISPLAY_HELPER";

/// Pages the browser loads, served on the loopback.
struct Site {
    origin: String,
}

impl Site {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                thread::spawn(move || serve(stream));
            }
        });
        Self { origin }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.origin, path)
    }
}

fn serve(mut stream: std::net::TcpStream) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request = String::new();
    if reader.read_line(&mut request).is_err() {
        return;
    }
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).is_err() || header == "\r\n" || header.is_empty() {
            break;
        }
        if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    let _ = reader.read_exact(&mut body);
    let target = request.split_whitespace().nth(1).unwrap_or("/").to_string();
    let path = target.split('?').next().unwrap_or("/");
    let (status, page) = page(path);
    match path {
        "/slow" => thread::sleep(Duration::from_millis(1500)),
        "/slower" => thread::sleep(Duration::from_millis(3000)),
        _ => {}
    }
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
        page.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

const STYLE: &str =
    "<style>body{margin:0;font:16px sans-serif}button,input,a{font:16px sans-serif}</style>";

fn page(path: &str) -> (&'static str, String) {
    let html = match path {
        "/" => format!("<!doctype html><title>Home</title>{STYLE}<main><h1>Home</h1><a href=\"/pricing\">Pricing</a></main>"),
        "/pricing" => format!(
            "<!doctype html><title>Pricing</title>{STYLE}<main style=\"padding:24px\"><h1>Pricing</h1>\
             <p>Starter $9 a month. Team $29 a month.</p>\
             <button id=compare aria-expanded=false aria-controls=plans style=\"position:absolute;left:24px;top:610px;width:180px;height:40px\" \
             onclick=\"this.setAttribute('aria-expanded','true');plans.hidden=false\">Compare plans</button>\
             <section id=plans hidden><p>Starter vs Team</p></section></main>"
        ),
        "/form" => format!(
            "<!doctype html><title>Forms</title>{STYLE}<main style=\"padding:16px\">\
             <form id=search action=\"/results\" method=get><input id=q name=q aria-label=Search><button id=go>Search</button></form>\
             <form id=signup action=\"/submit\" method=post><input id=name name=name aria-label=Name style=\"width:420px\">\
             <input id=password type=password name=password aria-label=Password autocomplete=current-password>\
             <input id=code name=code aria-label=Code autocomplete=one-time-code>\
             <input id=toggled name=toggled aria-label=Toggled type=password>\
             <button id=send>Send</button></form>\
             <button id=plain type=button>Plain</button>\
             <a id=file href=\"/report.csv\" download>Report</a>\
             <input id=upload type=file aria-label=Upload>\
             <script>setTimeout(()=>{{document.getElementById('toggled').type='text'}},50)</script></main>"
        ),
        "/results" => format!("<!doctype html><title>Results</title>{STYLE}<main><h1>Results</h1></main>"),
        "/submit" => format!("<!doctype html><title>Submitted</title>{STYLE}<main><h1>Submitted</h1></main>"),
        "/slow" => format!("<!doctype html><title>Slow</title>{STYLE}<main><h1>Slow</h1></main>"),
        "/slow-link" => format!(
            "<!doctype html><title>Slow link</title>{STYLE}<main style=\"padding:24px\"><a id=slow href=\"/slow\">Go slowly</a></main>"
        ),
        "/slower" => format!("<!doctype html><title>Slower</title>{STYLE}<main><h1>Slower</h1></main>"),
        "/slower-link" => format!(
            "<!doctype html><title>Slower link</title>{STYLE}<main style=\"padding:24px\"><a id=slow href=\"/slower\">Go slower</a></main>"
        ),
        "/cover" => format!(
            "<!doctype html><title>Cover</title>{STYLE}\
             <button id=target style=\"position:absolute;left:700px;top:500px;width:160px;height:48px\">Target</button>\
             <div id=banner style=\"display:none;position:fixed;left:0;right:0;top:460px;height:140px;background:#333;color:#fff\">Accept cookies</div>\
             <script>let shown=false;addEventListener('pointermove',()=>{{if(!shown){{shown=true;setTimeout(()=>{{banner.style.display='block'}},40)}}}},true)</script>"
        ),
        "/keys" => format!(
            "<!doctype html><title>Keys</title>{STYLE}<main style=\"padding:24px\"><input id=k aria-label=Keys></main>\
             <script>window.__keys=[];addEventListener('keydown',e=>__keys.push('down '+e.key),true);addEventListener('keyup',e=>__keys.push('up '+e.key),true)</script>"
        ),
        "/large" => {
            let mut body = String::new();
            for index in 0..3000 {
                body.push_str(&format!(
                    "<div class=row><a href=\"/item/{index}\">Item {index}</a> <button>Add {index}</button> <input aria-label=\"Qty {index}\" value=1></div>"
                ));
            }
            format!("<!doctype html><title>Large</title>{STYLE}<main>{body}</main>")
        }
        "/tabs/a" => format!("<!doctype html><title>Tab A</title>{STYLE}<main><h1>Tab A</h1></main>"),
        "/tabs/b" => format!("<!doctype html><title>Tab B</title>{STYLE}<main><h1>Tab B</h1></main>"),
        "/missing" => return ("404 Not Found", format!("<!doctype html><title>Missing</title>{STYLE}<h1>Missing</h1>")),
        _ => format!("<!doctype html><title>Page</title>{STYLE}<main>{path}</main>"),
    };
    ("200 OK", html)
}

/// The host side: its configuration, its spawned MCP client, and the daemon
/// socket its channels connect to.
struct Host {
    directory: TempDir,
    site: Site,
}

impl Host {
    /// A host whose first step, over the file protocol, starts the daemon
    /// and opens the site's home page.
    fn new() -> Self {
        let host = Self {
            directory: tempfile::tempdir().unwrap(),
            site: Site::start(),
        };
        fs::create_dir(host.path("captures")).unwrap();
        fs::create_dir(host.path("sockets")).unwrap();
        fs::create_dir_all(host.path("actions/one")).unwrap();
        let config = json!({ "version": 1, "namespace": NAMESPACE, "session": "browser",
            "requireSandbox": true, "captureDirectory": host.path("captures") });
        fs::write(host.path("config.json"), config.to_string()).unwrap();
        let opened = host.mcp("agent_browser_open", json!({ "url": host.site.url("/") }));
        assert_eq!(opened["isError"], false, "{opened}");
        host
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.path().canonicalize().unwrap().join(name)
    }

    /// The Action's private directory, as the host names it.
    fn action_directory(&self) -> PathBuf {
        self.path("actions/one")
    }

    /// The host-bound MCP client as the host spawns it: the binding's
    /// environment and none of the test's own.
    fn client(&self) -> Command {
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
            .env("AGENT_BROWSER_IDLE_TIMEOUT", "120s")
            .env("AGENT_BROWSER_WINDOW_STREAM", "1")
            .env("DISPLAY", "")
            .env("NO_COLOR", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Ok(chrome) = std::env::var("AMBIT_TEST_CHROME_EXECUTABLE") {
            command.env("AGENT_BROWSER_EXECUTABLE_PATH", chrome);
        }
        if let Ok(helper) = std::env::var("AGENT_BROWSER_DISPLAY_HELPER") {
            command.env("AGENT_BROWSER_DISPLAY_HELPER", helper);
        }
        command
    }

    /// A host-bound MCP call over the file protocol: a fresh client per
    /// call, which starts the daemon when none runs.
    fn mcp(&self, name: &str, arguments: Value) -> Value {
        let mut child = self.client().spawn().unwrap();
        writeln!(
            child.stdin.take().unwrap(),
            "{}",
            tool_call(name, arguments)
        )
        .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(reply.get("error").is_none(), "{reply}");
        reply["result"].clone()
    }

    fn socket(&self) -> PathBuf {
        self.path(&format!("sockets/namespaces/{NAMESPACE}/run/browser.sock"))
    }

    /// A new connection to the daemon, as the toolbox dials it.
    fn connect(&self) -> Channel {
        let stream = UnixStream::connect(self.socket()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        Channel {
            reader: BufReader::new(stream.try_clone().unwrap()),
            stream,
            channel: uuid(),
            next: 1,
        }
    }

    /// A channel whose hello was accepted.
    fn channel(&self) -> Channel {
        let mut channel = self.connect();
        let greeting = channel.hello();
        assert_eq!(greeting["success"], true, "{greeting}");
        channel
    }

    /// A person's control operation, as the toolbox's control route sends it
    /// on a connection of its own. Answers the reply and how long it took.
    fn control(&self, op: &str, controller: &str) -> (Value, Duration) {
        let mut request = json!({ "action": "ambit_browser_control", "op": op,
            "controllerId": controller });
        if op == "acquire" {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
            request["expiresAt"] = json!(now.as_millis() as u64 + 20_000);
        }
        let started = Instant::now();
        let mut stream = UnixStream::connect(self.socket()).unwrap();
        stream.write_all(format!("{request}\n").as_bytes()).unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        (serde_json::from_str(&line).unwrap(), started.elapsed())
    }
}

impl Drop for Host {
    /// The host ends its session through its binding, as it began it: the
    /// daemon closes its browser and exits. A CLI with other launch settings
    /// would restart that daemon instead of closing it.
    fn drop(&mut self) {
        let Ok(mut child) = self.client().spawn() else {
            return;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = writeln!(stdin, "{}", tool_call("agent_browser_close", json!({})));
        }
        let _ = child.wait_with_output();
    }
}

fn tool_call(name: &str, arguments: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": arguments } })
}

/// One agent channel on its own daemon connection.
struct Channel {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    channel: String,
    next: u64,
}

impl Channel {
    /// Writes `frame` as the toolbox forwards it: the agent action first,
    /// then the frame's members, compact, on one line.
    fn send(&mut self, frame: &Value) {
        let members = frame.to_string();
        let line = format!("{{\"action\":\"ambit_browser_agent\",{}\n", &members[1..]);
        self.stream.write_all(line.as_bytes()).unwrap();
    }

    fn reply(&mut self, id: u64) -> Value {
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        assert!(line.len() <= 4 << 20, "a reply line over 4 MiB");
        let reply: Value =
            serde_json::from_str(&line).unwrap_or_else(|error| panic!("{error}: {line:?}"));
        assert_eq!(reply["id"], id, "{reply}");
        reply
    }

    /// The next frame id on this channel.
    fn id(&mut self) -> u64 {
        let id = self.next;
        self.next += 1;
        id
    }

    /// Sends `frame` under the next id and answers its reply and round trip.
    fn call(&mut self, mut frame: Value) -> (Value, Duration) {
        let id = self.id();
        frame["id"] = json!(id);
        let started = Instant::now();
        self.send(&frame);
        let reply = self.reply(id);
        (reply, started.elapsed())
    }

    fn hello(&mut self) -> Value {
        let frame = json!({ "type": "hello", "protocol": 1, "channel": self.channel,
            "binding": { "version": 1, "namespace": NAMESPACE, "session": "browser",
                "requireSandbox": true },
            "actionId": ACTION, "ownerGeneration": "41" });
        self.call(frame).0
    }

    /// Runs `steps` in one sequence frame of the Action's directory.
    fn run(&mut self, host: &Host, steps: Value) -> Value {
        self.call(sequence(host, steps)).0
    }

    /// Resolves `selectors` on the page as it is: its generation and each
    /// node's `backendNodeId`.
    fn resolve(&mut self, host: &Host, selectors: &[&str]) -> (Value, Vec<Value>) {
        let mut frame = sequence(host, json!([]));
        frame["resolve"] = json!(selectors);
        let (reply, _) = self.call(frame);
        assert_eq!(reply["success"], true, "{reply}");
        let nodes = reply["resolved"]
            .as_array()
            .unwrap()
            .iter()
            .map(|resolved| resolved["backendNodeId"].clone())
            .collect();
        (reply["browser"]["page"]["pageGeneration"].clone(), nodes)
    }
}

fn sequence(host: &Host, steps: Value) -> Value {
    json!({ "type": "sequence", "actionId": ACTION, "ownerGeneration": "41",
        "directory": host.action_directory(), "steps": steps })
}

fn step(op: &str, arguments: Value) -> Value {
    json!({ "op": op, "arguments": arguments })
}

/// A judged step: what the plan chose it on, and the plan's ceiling.
fn judged(op: &str, arguments: Value, generation: &Value, node: &Value, effects: &str) -> Value {
    json!({ "op": op, "arguments": arguments, "preconditions": {
        "pageGeneration": generation, "backendNodeId": node, "effects": effects } })
}

fn uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The native response a step answered.
fn response(reply: &Value, index: usize) -> &Value {
    &reply["steps"][index]["result"]["structuredContent"]["response"]
}

fn micros(value: &Value) -> u64 {
    value.as_u64().unwrap_or_default()
}

/// p50 and p95 of `values`.
fn quantiles(values: &[u64]) -> Value {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let at = |q: f64| sorted[((sorted.len() - 1) as f64 * q).round() as usize];
    json!({ "n": sorted.len(), "p50": at(0.5), "p95": at(0.95), "min": sorted[0],
        "max": sorted[sorted.len() - 1] })
}

/// Writes `value` as `<evidence>/<name>.json` when an evidence directory is
/// named.
fn evidence(name: &str, value: &Value) {
    if let Ok(directory) = std::env::var("AMBIT_AGENT_CHANNEL_EVIDENCE") {
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            PathBuf::from(directory).join(format!("{name}.json")),
            serde_json::to_string_pretty(value).unwrap(),
        )
        .unwrap();
    }
}

/// Stage times of one-step frames running `steps`, `count` times.
fn stage_times(channel: &mut Channel, host: &Host, steps: Value, count: usize) -> Value {
    let (mut queue, mut op, mut observe, mut round) = (vec![], vec![], vec![], vec![]);
    for _ in 0..count {
        let (reply, elapsed) = channel.call(sequence(host, steps.clone()));
        assert_eq!(reply["success"], true, "{reply}");
        queue.push(micros(&reply["timing"]["queueUs"]));
        op.push(micros(&reply["timing"]["opUs"]));
        observe.push(micros(&reply["timing"]["observeUs"]));
        round.push(elapsed.as_micros() as u64);
    }
    json!({ "queueUs": quantiles(&queue), "opUs": quantiles(&op),
        "observeUs": quantiles(&observe), "roundTripUs": quantiles(&round) })
}

#[test]
#[ignore = "requires AMBIT_TEST_CHROME_EXECUTABLE, Xvfb and AGENT_BROWSER_DISPLAY_HELPER"]
fn e2e_agent_channel_frames_and_what_each_stage_costs() {
    let host = Host::new();
    // The first hello waits for the driver's digest, hashed once per daemon.
    let mut channel = host.connect();
    let started = Instant::now();
    let greeting = channel.hello();
    let first_hello = started.elapsed();
    assert_eq!(greeting["success"], true, "{greeting}");
    let binary = fs::read(fs::canonicalize(BIN).unwrap()).unwrap();
    assert_eq!(
        greeting["data"]["driverArtifactDigest"],
        format!("sha256:{:x}", Sha256::digest(&binary)),
        "the hello states the digest of the daemon's own executable"
    );
    let mut hellos = vec![];
    for _ in 0..10 {
        let mut other = host.connect();
        let started = Instant::now();
        assert_eq!(other.hello()["success"], true);
        hellos.push(started.elapsed().as_micros() as u64);
    }

    // A two-step named sequence with an observation.
    let mut frame = sequence(
        &host,
        json!([
            step(
                "agent_browser_open",
                json!({ "url": host.site.url("/pricing") })
            ),
            step("agent_browser_get_text", json!({ "selector": "main" }))
        ]),
    );
    frame["observe"] = json!(true);
    let (named, _) = channel.call(frame);
    assert_eq!(named["success"], true, "{named}");
    assert_eq!(named["steps"].as_array().unwrap().len(), 2);
    assert!(response(&named, 1)["data"]["text"]
        .as_str()
        .unwrap()
        .contains("Starter $9"));
    assert_eq!(
        named["steps"][0]["landed"]["navigation"]["httpStatus"], 200,
        "{named}"
    );
    assert!(
        named["steps"][1].get("landed").is_none(),
        "a read lands nothing"
    );
    let candidates = named["observation"]["candidates"].as_array().unwrap();
    let compare = candidates
        .iter()
        .find(|candidate| candidate["name"] == "Compare plans")
        .unwrap_or_else(|| panic!("{named}"));
    assert_eq!(compare["effect"], json!({ "controls": true }));

    let get_title = stage_times(
        &mut channel,
        &host,
        json!([step("agent_browser_get_title", json!({}))]),
        30,
    );
    let get_text = stage_times(
        &mut channel,
        &host,
        json!([step(
            "agent_browser_get_text",
            json!({ "selector": "main" })
        )]),
        30,
    );

    // A judged click on the candidate the observation offered, under read:
    // a disclosure is admitted under every ceiling, and it lands expanded.
    let generation = named["browser"]["page"]["pageGeneration"].clone();
    let (clicked, _) = channel.call(sequence(
        &host,
        json!([judged(
            "agent_browser_click",
            json!({ "selector": compare["ref"] }),
            &generation,
            &compare["backendNodeId"],
            "read"
        )]),
    ));
    assert_eq!(clicked["success"], true, "{clicked}");
    assert_eq!(
        clicked["steps"][0]["landed"]["target"]["expanded"], true,
        "{clicked}"
    );
    let mut clicks = vec![clicked["steps"][0]["timing"].clone()];

    // A 20-character fill under fill.
    let mut fills = vec![];
    let mut observations = vec![];
    for _ in 0..3 {
        let (opened, _) = channel.call(sequence(
            &host,
            json!([step(
                "agent_browser_open",
                json!({ "url": host.site.url("/form") })
            )]),
        ));
        assert_eq!(opened["success"], true, "{opened}");
        let (generation, nodes) = channel.resolve(&host, &["#name", "#go"]);
        let (filled, _) = channel.call(sequence(
            &host,
            json!([judged(
                "agent_browser_fill",
                json!({ "selector": "#name", "text": "Ada Lovelace, 1843." }),
                &generation,
                &nodes[0],
                "fill"
            )]),
        ));
        assert_eq!(filled["success"], true, "{filled}");
        assert_eq!(
            filled["steps"][0]["landed"]["target"]["value"],
            "Ada Lovelace, 1843."
        );
        fills.push(filled["steps"][0]["timing"].clone());
        // An observation of the simple page.
        let mut observe = sequence(&host, json!([]));
        observe["observe"] = json!(true);
        let (observed, _) = channel.call(observe);
        assert!(
            observed["observation"]["candidates"]
                .as_array()
                .unwrap()
                .len()
                >= 9
        );
        observations.push(micros(&observed["timing"]["observeUs"]));
        let (clicked, _) = channel.call(sequence(
            &host,
            json!([judged(
                "agent_browser_click",
                json!({ "selector": "#go" }),
                &generation,
                &nodes[1],
                "read"
            )]),
        ));
        assert_eq!(clicked["success"], true, "{clicked}");
        clicks.push(clicked["steps"][0]["timing"].clone());
    }

    // An observation of a large page lists its first 60 candidates.
    let (opened, _) = channel.call(sequence(
        &host,
        json!([step(
            "agent_browser_open",
            json!({ "url": host.site.url("/large") })
        )]),
    ));
    assert_eq!(opened["success"], true, "{opened}");
    let mut large = vec![];
    let mut omitted = 0;
    for _ in 0..5 {
        let mut observe = sequence(&host, json!([]));
        observe["observe"] = json!(true);
        let (observed, _) = channel.call(observe);
        let observation = &observed["observation"];
        assert_eq!(observation["candidates"].as_array().unwrap().len(), 60);
        omitted = micros(&observation["omitted"]);
        assert!(omitted > 0, "{observation}");
        large.push(micros(&observed["timing"]["observeUs"]));
    }

    let measured = json!({
        "firstHelloUs": first_hello.as_micros() as u64,
        "helloRoundTripUs": quantiles(&hellos),
        "getTitle": get_title,
        "getText": get_text,
        "judgedClick": clicks,
        "fill20Characters": fills,
        "observeFormUs": quantiles(&observations),
        "observeLargeUs": quantiles(&large),
        "largeOmitted": omitted,
        "binary": BIN,
    });
    println!("{}", serde_json::to_string_pretty(&measured).unwrap());
    evidence("e2e-frames-and-costs", &measured);
}

#[test]
#[ignore = "requires AMBIT_TEST_CHROME_EXECUTABLE, Xvfb and AGENT_BROWSER_DISPLAY_HELPER"]
fn e2e_agent_channel_refusals_by_hit_test_ceiling_and_secret_field() {
    let host = Host::new();
    let mut channel = host.channel();
    let mut seen = serde_json::Map::new();

    // The banner covers the target once the pointer moves: the hit test at
    // the end of travel refuses the press, and nothing lands.
    channel.run(
        &host,
        json!([step(
            "agent_browser_open",
            json!({ "url": host.site.url("/cover") })
        )]),
    );
    let (generation, nodes) = channel.resolve(&host, &["#target"]);
    let (covered, _) = channel.call(sequence(
        &host,
        json!([judged(
            "agent_browser_click",
            json!({ "selector": "#target" }),
            &generation,
            &nodes[0],
            "commit"
        )]),
    ));
    assert_eq!(covered["success"], false, "{covered}");
    assert_eq!(response(&covered, 0)["code"], "browser_observation_stale");
    assert_eq!(response(&covered, 0)["data"]["precondition"], "hitTest");
    assert!(covered["steps"][0].get("landed").is_none());
    seen.insert("hitTest".into(), response(&covered, 0).clone());

    // The form: its secret fields are marked and carry no value.
    let mut opened = sequence(
        &host,
        json!([step(
            "agent_browser_open",
            json!({ "url": host.site.url("/form") })
        )]),
    );
    opened["observe"] = json!(true);
    // The toggled field turns from a password into text 50 ms after load.
    thread::sleep(Duration::from_millis(100));
    let (form, _) = channel.call(opened);
    assert_eq!(form["success"], true, "{form}");
    thread::sleep(Duration::from_millis(100));
    let mut observe = sequence(&host, json!([]));
    observe["observe"] = json!(true);
    let (observed, _) = channel.call(observe);
    let candidates = observed["observation"]["candidates"].as_array().unwrap();
    for name in ["Password", "Code", "Toggled"] {
        let candidate = candidates
            .iter()
            .find(|candidate| candidate["name"] == name)
            .unwrap_or_else(|| panic!("{name}: {observed}"));
        assert_eq!(candidate["secret"], true, "{candidate}");
        assert!(candidate.get("value").is_none(), "{candidate}");
    }
    let (generation, nodes) = channel.resolve(
        &host,
        &[
            "#name",
            "#q",
            "#send",
            "#plain",
            "#password",
            "#toggled",
            "#code",
        ],
    );
    let refused =
        |channel: &mut Channel, op: &str, arguments: Value, node: &Value, effects: &str| {
            let (reply, _) = channel.call(sequence(
                &host,
                json!([judged(op, arguments, &generation, node, effects)]),
            ));
            assert_eq!(reply["success"], false, "{reply}");
            assert!(reply["steps"][0].get("landed").is_none(), "{reply}");
            response(&reply, 0).clone()
        };

    // A fill under read is refused; the same fill under fill is typed.
    let ceiling = refused(
        &mut channel,
        "agent_browser_fill",
        json!({ "selector": "#name", "text": "Ada" }),
        &nodes[0],
        "read",
    );
    assert_eq!(ceiling["code"], "browser_effect_refused", "{ceiling}");
    assert_eq!(ceiling["data"]["precondition"], "effects");
    seen.insert("fillUnderRead".into(), ceiling);
    let (filled, _) = channel.call(sequence(
        &host,
        json!([judged(
            "agent_browser_fill",
            json!({ "selector": "#name", "text": "Ada" }),
            &generation,
            &nodes[0],
            "fill"
        )]),
    ));
    assert_eq!(filled["success"], true, "{filled}");
    // A GET form's field is typed under read.
    let (searched, _) = channel.call(sequence(
        &host,
        json!([judged(
            "agent_browser_fill",
            json!({ "selector": "#q", "text": "plans" }),
            &generation,
            &nodes[1],
            "read"
        )]),
    ));
    assert_eq!(searched["success"], true, "{searched}");

    // Submitting a POST form, or pressing a plain button, needs commit.
    for (selector, node, name) in [
        ("#send", &nodes[2], "postSubmitUnderFill"),
        ("#plain", &nodes[3], "plainButtonUnderFill"),
    ] {
        let commit = refused(
            &mut channel,
            "agent_browser_click",
            json!({ "selector": selector }),
            node,
            "fill",
        );
        assert_eq!(commit["code"], "browser_effect_refused", "{commit}");
        seen.insert(name.into(), commit);
    }

    // Nothing types into a secret field, under any ceiling; the field that
    // was a password when its document started stays secret.
    for (selector, node, name) in [
        ("#password", &nodes[4], "password"),
        ("#toggled", &nodes[5], "toggledPassword"),
        ("#code", &nodes[6], "oneTimeCode"),
    ] {
        let secret = refused(
            &mut channel,
            "agent_browser_fill",
            json!({ "selector": selector, "text": "hunter2" }),
            node,
            "commit",
        );
        assert_eq!(secret["code"], "browser_effect_refused", "{secret}");
        assert_eq!(secret["data"]["observed"]["secret"], true, "{secret}");
        seen.insert(name.into(), secret);
    }
    let (values, _) = channel.call(sequence(
        &host,
        json!([step("agent_browser_eval", json!({ "script":
            "JSON.stringify([...document.querySelectorAll('#password,#toggled,#code')].map(e => e.value))" }))]),
    ));
    assert_eq!(
        response(&values, 0)["data"]["result"],
        "[\"\",\"\",\"\"]",
        "{values}"
    );

    // A judged step without its preconditions refuses the whole frame.
    let (rejected, _) = channel.call(sequence(
        &host,
        json!([
            step("agent_browser_get_title", json!({})),
            step("agent_browser_click", json!({ "selector": "#plain" }))
        ]),
    ));
    assert_eq!(rejected["code"], "browser_operation_rejected", "{rejected}");
    assert_eq!(rejected["data"], json!({ "step": 1 }));
    assert_eq!(rejected["steps"], json!([]));
    evidence("e2e-refusals", &Value::Object(seen));
}

#[test]
#[ignore = "requires AMBIT_TEST_CHROME_EXECUTABLE, Xvfb and AGENT_BROWSER_DISPLAY_HELPER"]
fn e2e_agent_channel_open_many_pages_and_the_theme() {
    let host = Host::new();
    let mut channel = host.channel();
    let (home, _) = channel.call(sequence(
        &host,
        json!([step("agent_browser_get_url", json!({}))]),
    ));
    let home_target = home["browser"]["page"]["targetId"].clone();

    let urls = [
        host.site.url("/tabs/a"),
        host.site.url("/tabs/b"),
        host.site.url("/missing"),
    ];
    let started = Instant::now();
    let (opened, _) = channel.call(sequence(
        &host,
        json!([step("agent_browser_open_many", json!({ "urls": urls }))]),
    ));
    let elapsed = started.elapsed();
    assert_eq!(opened["success"], true, "{opened}");
    let tabs = response(&opened, 0)["data"]["tabs"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(tabs.len(), 3, "{opened}");
    assert_eq!(tabs[0]["page"]["title"], "Tab A");
    assert_eq!(tabs[1]["httpStatus"], 200);
    assert_eq!(tabs[2]["httpStatus"], 404);
    let landed = &opened["steps"][0]["landed"];
    let opened_ids: Vec<&Value> = landed["openedTabs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tab| &tab["tabId"])
        .collect();
    let answered_ids: Vec<&Value> = tabs.iter().map(|tab| &tab["tabId"]).collect();
    assert_eq!(opened_ids, answered_ids, "{landed}");
    // The active tab did not change.
    assert_eq!(
        opened["browser"]["page"]["targetId"], home_target,
        "{opened}"
    );

    // A page is read by switching to it by its target id, then back home.
    let (read, _) = channel.call(sequence(
        &host,
        json!([
            step(
                "agent_browser_tab_switch",
                json!({ "tab": tabs[0]["page"]["targetId"] })
            ),
            step("agent_browser_read", json!({})),
            step("agent_browser_tab_switch", json!({ "tab": home_target }))
        ]),
    ));
    assert_eq!(read["success"], true, "{read}");
    assert!(
        read["steps"][1]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Tab A"),
        "{read}"
    );
    assert_eq!(read["browser"]["page"]["targetId"], home_target);

    // The theme is a session operation: it lands nothing.
    let (themed, _) = channel.call(sequence(
        &host,
        json!([step("agent_browser_set_theme", json!({ "theme": "dark" }))]),
    ));
    assert_eq!(themed["success"], true, "{themed}");
    assert_eq!(response(&themed, 0)["data"]["theme"], "dark", "{themed}");
    assert!(themed["steps"][0].get("landed").is_none(), "{themed}");
    evidence(
        "e2e-open-many-and-theme",
        &json!({ "openManyRoundTripUs": elapsed.as_micros() as u64, "tabs": tabs,
            "landed": landed, "theme": response(&themed, 0)["data"] }),
    );
}

#[test]
#[ignore = "requires AMBIT_TEST_CHROME_EXECUTABLE, Xvfb and AGENT_BROWSER_DISPLAY_HELPER"]
fn e2e_agent_channel_ledger_answers_and_a_dropped_connection() {
    let host = Host::new();
    let mut runner = host.channel();
    let mut asker = host.channel();
    let status = |asker: &mut Channel, channel: &str, id: u64, wait: u64| {
        let (reply, _) = asker.call(json!({ "type": "op_status", "actionId": ACTION,
            "ownerGeneration": "41", "of": { "channel": channel, "id": id }, "waitMs": wait }));
        assert_eq!(reply["success"], true, "{reply}");
        reply["data"].clone()
    };
    let wait = step("agent_browser_wait_ms", json!({ "ms": 1500 }));
    let title = step("agent_browser_get_title", json!({}));

    // Running while it runs; a second sequence sent meanwhile never starts.
    let first = runner.id();
    let mut frame = sequence(&host, json!([wait, title]));
    frame["id"] = json!(first);
    runner.send(&frame);
    let second = runner.id();
    let mut frame = sequence(&host, json!([title]));
    frame["id"] = json!(second);
    runner.send(&frame);
    thread::sleep(Duration::from_millis(300));
    assert_eq!(
        status(&mut asker, &runner.channel, first, 0)["state"],
        "running"
    );
    assert_eq!(status(&mut asker, &uuid(), 1, 0)["state"], "unknown");
    let settled = runner.reply(first);
    assert_eq!(settled["success"], true, "{settled}");
    let refused = runner.reply(second);
    assert_eq!(refused["code"], "agent_channel_protocol", "{refused}");
    assert_eq!(
        status(&mut asker, &runner.channel, second, 0)["state"],
        "not_started"
    );
    let answered = status(&mut asker, &runner.channel, first, 0);
    assert_eq!(answered["state"], "settled");
    assert_eq!(answered["result"]["steps"].as_array().unwrap().len(), 2);
    assert_eq!(
        answered["result"]["steps"][0]["arguments"],
        json!({ "ms": 1500 })
    );

    // The connection drops while the first of two steps runs: that step
    // completes and is recorded, and the second never starts.
    let dropped = runner.id();
    let mut frame = sequence(&host, json!([wait, title]));
    frame["id"] = json!(dropped);
    runner.send(&frame);
    thread::sleep(Duration::from_millis(300));
    let dropped_channel = runner.channel.clone();
    drop(runner);
    let prefix = status(&mut asker, &dropped_channel, dropped, 5000);
    assert_eq!(prefix["state"], "settled", "{prefix}");
    let steps = prefix["result"]["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 1, "{prefix}");
    assert_eq!(steps[0]["op"], "agent_browser_wait_ms");
    // Nothing more of that channel will ever start.
    assert_eq!(
        status(&mut asker, &dropped_channel, dropped + 1, 0)["state"],
        "not_started"
    );
    evidence("e2e-ledger", &json!({ "dropped": prefix }));
}

#[test]
#[ignore = "requires AMBIT_TEST_CHROME_EXECUTABLE, Xvfb and AGENT_BROWSER_DISPLAY_HELPER"]
fn e2e_agent_channel_take_control_during_a_step_and_during_the_landed_wait() {
    let host = Host::new();
    let mut channel = host.channel();
    channel.run(
        &host,
        json!([step(
            "agent_browser_open",
            json!({ "url": host.site.url("/form") })
        )]),
    );
    let (generation, nodes) = channel.resolve(&host, &["#name"]);

    // During a step: the typing stops at the next key.
    let id = channel.id();
    let mut frame = sequence(
        &host,
        json!([judged(
            "agent_browser_fill",
            json!({ "selector": "#name", "text": "abcdefghij".repeat(6) }),
            &generation,
            &nodes[0],
            "fill"
        )]),
    );
    frame["id"] = json!(id);
    channel.send(&frame);
    thread::sleep(Duration::from_millis(600));
    let controller = uuid();
    let (acquired, during_step) = host.control("acquire", &controller);
    assert_eq!(acquired["success"], true, "{acquired}");
    let interrupted = channel.reply(id);
    let answer = response(&interrupted, 0);
    assert_eq!(
        answer["code"], "browser_operation_interrupted",
        "{interrupted}"
    );
    assert!(answer["data"]["charactersTyped"].as_u64().unwrap() > 0);
    assert_eq!(
        interrupted["steps"][0]["landed"]["pending"], true,
        "{interrupted}"
    );
    // While a person holds the browser, a step is refused and nothing is read.
    let mut held = sequence(&host, json!([step("agent_browser_get_title", json!({}))]));
    held["observe"] = json!(true);
    let (refused, _) = channel.call(held);
    assert_eq!(
        response(&refused, 0)["code"],
        "browser_controlled_by_user",
        "{refused}"
    );
    assert_eq!(refused["observation"]["status"], "unavailable");
    let (released, _) = host.control("release", &controller);
    assert_eq!(released["success"], true, "{released}");

    // During the landed wait: a link to a page that takes 1.5 s to answer
    // is clicked; its landing waits for the commit, and a person taking
    // control ends that wait at once, without waiting for it.
    channel.run(
        &host,
        json!([step(
            "agent_browser_open",
            json!({ "url": host.site.url("/slow-link") })
        )]),
    );
    let (generation, nodes) = channel.resolve(&host, &["#slow"]);
    let id = channel.id();
    let mut frame = sequence(
        &host,
        json!([judged(
            "agent_browser_click",
            json!({ "selector": "#slow" }),
            &generation,
            &nodes[0],
            "read"
        )]),
    );
    frame["id"] = json!(id);
    let sent = Instant::now();
    channel.send(&frame);
    thread::sleep(Duration::from_millis(500));
    let controller = uuid();
    let asked = sent.elapsed();
    let (acquired, during_wait) = host.control("acquire", &controller);
    assert_eq!(acquired["success"], true, "{acquired}");
    let clicked = channel.reply(id);
    let answered = sent.elapsed();
    assert_eq!(clicked["success"], true, "{clicked}");
    let landed = &clicked["steps"][0]["landed"];
    assert_eq!(landed["pending"], true, "{clicked}");
    assert!(landed.get("navigation").is_none(), "{landed}");
    // The step held custody from about when it was sent: its landing ended
    // shortly after the takeover was asked, not at the commit 1.5 s later.
    // The frame's reply waits for the person's acquire to finish, since the
    // frame reads its page under custody after the step.
    let op = Duration::from_micros(micros(&clicked["steps"][0]["timing"]["opUs"]));
    let after_takeover = op.saturating_sub(asked);
    assert!(
        after_takeover < Duration::from_millis(400),
        "the landing ended {after_takeover:?} after the takeover was asked: {clicked}"
    );
    assert_eq!(
        clicked["browser"]["capture"]["code"], "browser_controlled_by_user",
        "{clicked}"
    );
    let (released, _) = host.control("release", &controller);
    assert_eq!(released["success"], true, "{released}");
    evidence(
        "e2e-take-control",
        &json!({ "acquireDuringStepUs": during_step.as_micros() as u64,
            "acquireDuringLandedWaitUs": during_wait.as_micros() as u64,
            "landingEndedAfterTakeoverUs": after_takeover.as_micros() as u64,
            "clickAnsweredAfterUs": answered.as_micros() as u64,
            "interruptedStep": interrupted["steps"][0], "pendingLanding": landed }),
    );
}

/// A page between documents answers nothing until its navigation commits,
/// so nothing waits on it: a click whose page takes 3 s to answer ends at
/// its 1 s deadline with `pending`, and the frame's reads say why; a judged
/// step sent next is refused at once, which also shows the first reply came
/// before the commit; and the host's wait for the load then observes the
/// page the click went to.
#[test]
#[ignore = "requires AMBIT_TEST_CHROME_EXECUTABLE, Xvfb and AGENT_BROWSER_DISPLAY_HELPER"]
fn e2e_agent_channel_a_page_between_documents_is_not_waited_on() {
    let host = Host::new();
    let mut channel = host.channel();
    channel.run(
        &host,
        json!([step(
            "agent_browser_open",
            json!({ "url": host.site.url("/slower-link") })
        )]),
    );
    let (generation, nodes) = channel.resolve(&host, &["#slow"]);
    let click = |timeout: u64| {
        json!({ "op": "agent_browser_click",
            "arguments": { "selector": "#slow", "timeoutMs": timeout },
            "preconditions": { "pageGeneration": generation,
                "backendNodeId": nodes[0], "effects": "read" } })
    };
    let pending = json!({ "status": "unavailable", "code": "browser_navigation_pending" });

    let mut frame = sequence(&host, json!([click(1000)]));
    frame["observe"] = json!(true);
    frame["resolve"] = json!(["#slow"]);
    let (clicked, clicked_round) = channel.call(frame);
    assert_eq!(clicked["success"], true, "{clicked}");
    let landed = &clicked["steps"][0]["landed"];
    assert_eq!(landed["pending"], true, "{clicked}");
    assert!(landed.get("navigation").is_none(), "{landed}");
    assert_eq!(
        landed["target"],
        json!({ "backendNodeId": nodes[0] }),
        "{landed}"
    );
    assert_eq!(clicked["observation"], pending, "{clicked}");
    assert_eq!(clicked["resolved"], pending, "{clicked}");
    assert_eq!(
        clicked["browser"]["capture"]["code"], "browser_navigation_pending",
        "{clicked}"
    );

    let (refused, refused_round) = channel.call(sequence(&host, json!([click(1000)])));
    assert_eq!(refused["success"], false, "{refused}");
    assert_eq!(
        response(&refused, 0)["code"],
        "browser_navigation_pending",
        "{refused}"
    );
    assert_eq!(response(&refused, 0)["data"], json!({}), "{refused}");
    assert!(
        refused["steps"][0].get("landed").is_none(),
        "a refused step has no landing: {refused}"
    );

    let mut waited = sequence(
        &host,
        json!([step(
            "agent_browser_wait_for_load",
            json!({ "state": "domcontentloaded", "timeoutMs": 10000 })
        )]),
    );
    waited["observe"] = json!(true);
    let (loaded, loaded_round) = channel.call(waited);
    assert_eq!(loaded["success"], true, "{loaded}");
    assert!(loaded["observation"]["candidates"].is_array(), "{loaded}");
    assert!(
        loaded["browser"]["page"]["url"]
            .as_str()
            .is_some_and(|url| url.ends_with("/slower")),
        "{loaded}"
    );
    evidence(
        "e2e-navigation-pending",
        &json!({ "clickRoundTripUs": clicked_round.as_micros() as u64,
            "clickTiming": clicked["timing"], "clickLanded": landed,
            "refusedRoundTripUs": refused_round.as_micros() as u64,
            "refusal": response(&refused, 0),
            "waitRoundTripUs": loaded_round.as_micros() as u64,
            "waitTiming": loaded["timing"] }),
    );
}

#[test]
#[ignore = "requires AMBIT_TEST_CHROME_EXECUTABLE, Xvfb and AGENT_BROWSER_DISPLAY_HELPER"]
fn e2e_agent_channel_end_releases_the_keys_its_steps_held() {
    let host = Host::new();
    let mut channel = host.channel();
    channel.run(
        &host,
        json!([step(
            "agent_browser_open",
            json!({ "url": host.site.url("/keys") })
        )]),
    );
    let (generation, nodes) = channel.resolve(&host, &["#k"]);
    let keys = |channel: &mut Channel| {
        let (read, _) = channel.call(sequence(
            &host,
            json!([step(
                "agent_browser_eval",
                json!({ "script": "JSON.stringify(window.__keys)" })
            )]),
        ));
        serde_json::from_str::<Vec<String>>(response(&read, 0)["data"]["result"].as_str().unwrap())
            .unwrap()
    };
    let hold_shift = |channel: &mut Channel| {
        let (held, _) = channel.call(sequence(
            &host,
            json!([judged(
                "agent_browser_keydown",
                json!({ "key": "Shift" }),
                &generation,
                &nodes[0],
                "read"
            )]),
        ));
        assert_eq!(held["success"], true, "{held}");
    };
    // The page shows nothing contentful, so Chrome holds its first paint,
    // and drops a press sent before it. The open's landing makes the page
    // paint, so this click, sent at once, focuses the field.
    let (focused, _) = channel.call(sequence(
        &host,
        json!([judged(
            "agent_browser_click",
            json!({ "selector": "#k" }),
            &generation,
            &nodes[0],
            "read"
        )]),
    ));
    assert_eq!(focused["success"], true, "{focused}");
    let (active, _) = channel.call(sequence(
        &host,
        json!([step(
            "agent_browser_eval",
            json!({ "script": "document.activeElement.id" })
        )]),
    ));
    assert_eq!(response(&active, 0)["data"]["result"], "k", "{active}");

    // A channel that ends with Shift held releases it.
    hold_shift(&mut channel);
    drop(channel);
    thread::sleep(Duration::from_millis(500));
    let mut next = host.channel();
    let released = keys(&mut next);
    assert!(released.contains(&"down Shift".to_string()), "{released:?}");
    assert!(released.contains(&"up Shift".to_string()), "{released:?}");

    // What a channel held is released by the last channel that acted: an
    // older channel that ends after another acted leaves it held.
    hold_shift(&mut next);
    let mut later = host.channel();
    let before = keys(&mut later).len();
    drop(next);
    thread::sleep(Duration::from_millis(500));
    let still = keys(&mut later);
    assert_eq!(
        still.iter().filter(|key| *key == "up Shift").count(),
        1,
        "released while another channel acts: {still:?}"
    );
    assert_eq!(still.len(), before);
    drop(later);
    thread::sleep(Duration::from_millis(500));
    let mut last = host.channel();
    let finally = keys(&mut last);
    assert_eq!(
        finally.iter().filter(|key| *key == "up Shift").count(),
        2,
        "{finally:?}"
    );
    evidence("e2e-held-keys", &json!({ "keys": finally }));
}

/// Keeps `REQUIRES` in one place for the ignore reasons above.
#[test]
fn the_real_browser_suite_names_what_it_requires() {
    assert!(REQUIRES.contains("AMBIT_TEST_CHROME_EXECUTABLE"));
}
