//! The agent channel against a real browser: a host-bound MCP client starts
//! the daemon over the file protocol, as the host's first step does, then a
//! channel speaks to the daemon's socket exactly as the toolbox's agent route
//! writes: one line per frame, `{"action":"ambit_browser_agent", …}`, and one
//! reply line per frame, in order.
//!
//! Needs Chrome (`AMBIT_TEST_CHROME_EXECUTABLE`) with a working sandbox, Xvfb,
//! and the display helper (`AGENT_BROWSER_DISPLAY_HELPER`) for the owned
//! window the motion layer drives. `AMBIT_AGENT_CHANNEL_EVIDENCE` names a
//! file the measurements are written to.

use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_agent-browser");
const NAMESPACE: &str = "3f2a9c1e5b7d4e6f8a0b1c2d3e4f5a6b";
const ACTION: &str = "8c1f0e2a-3b4c-4d5e-8f60-718293a4b5c6";

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
    let mut parts = request.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let path = target.split('?').next().unwrap_or("/");
    let _ = (method, body);
    let (status, page) = page(path);
    if path == "/slow" {
        thread::sleep(Duration::from_millis(1500));
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
             <form id=signup action=\"/submit\" method=post><input id=name name=name aria-label=Name>\
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
        "/cover" => format!(
            "<!doctype html><title>Cover</title>{STYLE}\
             <button id=target style=\"position:absolute;left:700px;top:500px;width:160px;height:48px\">Target</button>\
             <div id=banner style=\"display:none;position:fixed;left:0;right:0;top:460px;height:140px;background:#333;color:#fff\">Accept cookies</div>\
             <script>let shown=false;addEventListener('pointermove',()=>{{if(!shown){{shown=true;setTimeout(()=>{{banner.style.display='block'}},40)}}}},true)</script>"
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
        host
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.path().canonicalize().unwrap().join(name)
    }

    /// The Action's private directory, as the host names it.
    fn action_directory(&self) -> PathBuf {
        self.path("actions/one")
    }

    /// A host-bound MCP call over the file protocol: a fresh client per
    /// call, which starts the daemon when none runs.
    fn mcp(&self, name: &str, arguments: Value) -> Value {
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
            .env("AGENT_BROWSER_IDLE_TIMEOUT", "60s")
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
        let mut child = command.spawn().unwrap();
        writeln!(
            child.stdin.take().unwrap(),
            "{}",
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": name, "arguments": arguments } })
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
    fn channel(&self) -> Channel {
        let stream = UnixStream::connect(self.socket()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        Channel {
            reader: BufReader::new(stream.try_clone().unwrap()),
            stream,
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = Command::new(BIN)
            .args(["--session", "browser", "close"])
            .env("AGENT_BROWSER_SOCKET_DIR", self.path("sockets"))
            .env("AGENT_BROWSER_NAMESPACE", NAMESPACE)
            .env("AGENT_BROWSER_REQUIRE_DAEMON", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// One agent channel on its own daemon connection.
struct Channel {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
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

    fn call(&mut self, frame: Value) -> (Value, Duration) {
        let started = Instant::now();
        self.send(&frame);
        let reply = self.reply(frame["id"].as_u64().unwrap());
        (reply, started.elapsed())
    }
}

fn hello(id: u64, channel: &str) -> Value {
    json!({ "type": "hello", "id": id, "protocol": 1, "channel": channel,
        "binding": { "version": 1, "namespace": NAMESPACE, "session": "browser",
            "requireSandbox": true } })
}

fn sequence(id: u64, generation: &str, directory: &Path, steps: Value) -> Value {
    json!({ "type": "sequence", "id": id, "actionId": ACTION, "ownerGeneration": generation,
        "directory": directory, "steps": steps })
}

fn step(op: &str, arguments: Value) -> Value {
    json!({ "op": op, "arguments": arguments })
}

fn uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[test]
#[ignore = "requires AMBIT_TEST_CHROME_EXECUTABLE, Xvfb and AGENT_BROWSER_DISPLAY_HELPER"]
fn e2e_agent_channel_smoke() {
    let host = Host::new();
    let opened = host.mcp("agent_browser_open", json!({ "url": host.site.url("/") }));
    assert_eq!(opened["isError"], false, "{opened}");
    let mut channel = host.channel();
    let (greeting, _) = channel.call(hello(1, &uuid()));
    assert_eq!(greeting["success"], true, "{greeting}");
    let (reply, elapsed) = channel.call(sequence(
        2,
        "41",
        &host.action_directory(),
        json!([
            step(
                "agent_browser_open",
                json!({ "url": host.site.url("/pricing") })
            ),
            step("agent_browser_get_text", json!({ "selector": "main" }))
        ]),
    ));
    println!("SMOKE {elapsed:?} {reply}");
    assert_eq!(reply["success"], true, "{reply}");
    let mut frame = sequence(
        3,
        "41",
        &host.action_directory(),
        json!([step(
            "agent_browser_open",
            json!({ "url": host.site.url("/form") })
        )]),
    );
    frame["observe"] = json!(true);
    frame["resolve"] = json!(["#password", "#q", "#toggled", "#nothing", "@e1"]);
    let (reply, elapsed) = channel.call(frame);
    println!(
        "OBSERVE {elapsed:?} {}",
        serde_json::to_string_pretty(&reply).unwrap()
    );
}
