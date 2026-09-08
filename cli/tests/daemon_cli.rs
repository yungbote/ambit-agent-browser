#![cfg(unix)]

//! Real CLI processes and Unix sockets verify supervisor custody without Chrome.
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_agent-browser");
const SESSION: &str = "supervised";

struct Fixture(TempDir);

impl Fixture {
    fn new() -> Self {
        let fixture = Self(tempfile::tempdir().unwrap());
        fixture.config("{}");
        fixture
    }

    fn config(&self, value: &str) {
        fs::write(self.0.path().join("settings.json"), value).unwrap();
    }

    fn path(&self, extension: &str) -> std::path::PathBuf {
        self.0.path().join(format!("{SESSION}.{extension}"))
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(BIN);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("AGENT_BROWSER_") {
                command.env_remove(key);
            }
        }
        command
            .arg("--config")
            .arg(self.0.path().join("settings.json"))
            .args(["--session", SESSION])
            .args(args)
            .env("AGENT_BROWSER_SOCKET_DIR", self.0.path())
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        Process(Some(self.command(args).spawn().unwrap())).finish()
    }

    fn start(&self, args: &[&str]) -> Process {
        let mut command = self.command(args);
        command.arg("daemon");
        let mut process = Process(Some(command.spawn().unwrap()));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if UnixStream::connect(self.path("sock")).is_ok() {
                // A connectable socket must never precede published metadata.
                assert_eq!(self.pid(), process.pid());
                for extension in ["config", "version", "supervised"] {
                    assert!(self.path(extension).exists());
                }
                return process;
            }
            if process.0.as_mut().unwrap().try_wait().unwrap().is_some() {
                panic!("daemon exited: {:?}", process.finish());
            }
            assert!(Instant::now() < deadline, "daemon did not become ready");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn pid(&self) -> u32 {
        fs::read_to_string(self.path("pid"))
            .unwrap()
            .parse()
            .unwrap()
    }

    fn assert_clean(&self) {
        for extension in ["sock", "pid", "config", "version", "stream", "supervised"] {
            assert!(!self.path(extension).exists(), "leftover {extension}");
        }
    }
}

struct Process(Option<Child>);

impl Process {
    fn pid(&self) -> u32 {
        self.0.as_ref().unwrap().id()
    }

    fn signal(&self, signal: i32) {
        assert_eq!(unsafe { libc::kill(self.pid() as i32, signal) }, 0);
    }

    fn finish(mut self) -> Output {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.0.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "process {} did not exit",
                self.pid()
            );
            thread::sleep(Duration::from_millis(10));
        }
        self.0.take().unwrap().wait_with_output().unwrap()
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn response(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| panic!("{e}: {output:?}"))
}

#[test]
fn foreground_keeps_pid_process_group_and_configured_client_reuses_it() {
    let fixture = Fixture::new();
    fixture.config(r#"{"idleTimeout":"0","noAutoDialog":true,"requireDaemon":true}"#);
    let daemon = fixture.start(&[]);
    assert_eq!(unsafe { libc::getpgid(daemon.pid() as i32) }, unsafe {
        libc::getpgrp()
    });
    assert_eq!(unsafe { libc::getsid(daemon.pid() as i32) }, unsafe {
        libc::getsid(0)
    });
    let inspection = fixture.run(&["--json", "inspect"]);
    assert!(inspection.status.success(), "{inspection:?}");
    assert_eq!(response(&inspection)["success"], true);
    assert_eq!(fixture.pid(), daemon.pid());
    let close = fixture.run(&["--json", "close"]);
    assert!(close.status.success(), "{close:?}");
    assert!(daemon.finish().status.success());
    fixture.assert_clean();
}

#[test]
fn absent_required_daemon_never_creates_session_files() {
    let fixture = Fixture::new();
    for use_env in [false, true] {
        let mut command = fixture.command(&["--json", "inspect"]);
        if use_env {
            command.env("AGENT_BROWSER_REQUIRE_DAEMON", "1");
        } else {
            command.arg("--require-daemon");
        }
        let output = Process(Some(command.spawn().unwrap())).finish();
        assert!(!output.status.success());
        assert!(response(&output)["error"]
            .as_str()
            .unwrap()
            .contains("is unavailable"));
        fixture.assert_clean();
        assert!(!fixture.path("lock").exists());
    }
}

#[test]
fn mcp_server_forwards_required_daemon_policy_to_real_tool_processes() {
    let fixture = Fixture::new();
    let mut command = fixture.command(&["--require-daemon", "mcp", "--tools", "all"]);
    command.stdin(Stdio::piped());
    let mut process = Process(Some(command.spawn().unwrap()));
    let mut stdin = process.0.as_mut().unwrap().stdin.take().unwrap();
    for request in [
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"supervision-test","version":"1"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"agent_browser_inspect","arguments":{"session":SESSION}}}),
    ] {
        writeln!(stdin, "{request}").unwrap();
    }
    drop(stdin);
    let output = process.finish();
    assert!(output.status.success(), "{output:?}");
    let responses: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let tool = responses.iter().find(|value| value["id"] == 2).unwrap();
    assert_eq!(tool["result"]["isError"], true, "{tool}");
    assert!(tool.to_string().contains("Required daemon"), "{tool}");
    fixture.assert_clean();
    assert!(!fixture.path("lock").exists());
}

#[test]
fn required_daemon_doctor_skips_scratch_browser_probes() {
    let fixture = Fixture::new();
    let output = fixture.run(&[
        "--json",
        "--require-daemon",
        "doctor",
        "--offline",
        "--webgpu",
    ]);
    let result = response(&output);
    assert!(
        result["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check["id"] == "launch.skipped.required_daemon"),
        "{result}"
    );
    assert_eq!(fs::read_dir(fixture.0.path()).unwrap().count(), 1);
    fixture.assert_clean();
}

#[test]
fn mismatch_never_restarts_supervised_daemon_even_for_ordinary_clients() {
    let fixture = Fixture::new();
    let daemon = fixture.start(&["--idle-timeout", "0"]);
    let config = fs::read(fixture.path("config")).unwrap();
    let inode = fs::metadata(fixture.path("sock")).unwrap().ino();
    for require in ["true", "false"] {
        let output = fixture.run(&["--json", "--require-daemon", require, "inspect"]);
        assert!(!output.status.success());
        assert!(response(&output)["error"]
            .as_str()
            .unwrap()
            .contains("configuration"));
        assert_eq!(fixture.pid(), daemon.pid());
        assert_eq!(fs::read(fixture.path("config")).unwrap(), config);
        assert_eq!(fs::metadata(fixture.path("sock")).unwrap().ino(), inode);
    }
    fs::write(fixture.path("version"), "different-version").unwrap();
    let output = fixture.run(&[
        "--json",
        "--require-daemon",
        "--idle-timeout",
        "0",
        "inspect",
    ]);
    assert!(!output.status.success());
    assert!(response(&output)["error"]
        .as_str()
        .unwrap()
        .contains("version"));
    assert_eq!(fixture.pid(), daemon.pid());
    daemon.signal(libc::SIGTERM);
    assert!(daemon.finish().status.success());
    fixture.assert_clean();
}

#[test]
fn second_foreground_daemon_cannot_change_live_session() {
    let fixture = Fixture::new();
    let daemon = fixture.start(&[]);
    assert!(fixture.run(&["inspect"]).status.success());
    assert_eq!(fixture.pid(), daemon.pid());
    let config = fs::read(fixture.path("config")).unwrap();
    let inode = fs::metadata(fixture.path("sock")).unwrap().ino();
    let contender = fixture.run(&["--debug", "--idle-timeout", "5s", "daemon"]);
    assert!(!contender.status.success());
    assert!(String::from_utf8_lossy(&contender.stderr).contains("Cannot own session"));
    assert_eq!(fixture.pid(), daemon.pid());
    assert_eq!(fs::read(fixture.path("config")).unwrap(), config);
    assert_eq!(fs::metadata(fixture.path("sock")).unwrap().ino(), inode);
    assert!(!fixture.path("log").exists());
    assert!(fixture.run(&["--require-daemon", "close"]).status.success());
    assert!(daemon.finish().status.success());
    fixture.assert_clean();
}

#[test]
fn concurrent_foreground_starts_leave_exactly_one_owner() {
    for _ in 0..5 {
        let fixture = Fixture::new();
        let first = Process(Some(fixture.command(&["daemon"]).spawn().unwrap()));
        let second = Process(Some(fixture.command(&["daemon"]).spawn().unwrap()));
        let deadline = Instant::now() + Duration::from_secs(10);
        while UnixStream::connect(fixture.path("sock")).is_err() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        let (winner, loser) = if fixture.pid() == first.pid() {
            (first, second)
        } else {
            assert_eq!(fixture.pid(), second.pid());
            (second, first)
        };
        assert!(!loser.finish().status.success());
        assert!(fixture
            .run(&["--require-daemon", "inspect"])
            .status
            .success());
        assert_eq!(fixture.pid(), winner.pid());
        winner.signal(libc::SIGTERM);
        assert!(winner.finish().status.success());
        fixture.assert_clean();
    }
}

#[test]
fn ordinary_clients_still_start_and_restart_background_daemons() {
    let fixture = Fixture::new();
    // The fixture's socket directory is the sole authority for this process.
    struct Cleanup<'a>(&'a Fixture);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            if let Ok(pid) = fs::read_to_string(self.0.path("pid")) {
                if let Ok(pid) = pid.parse::<i32>() {
                    unsafe {
                        libc::kill(pid, libc::SIGTERM);
                    }
                }
            }
        }
    }
    let _cleanup = Cleanup(&fixture);
    let first = fixture.run(&["--json", "inspect"]);
    assert!(first.status.success(), "{first:?}");
    let first_pid = fixture.pid();
    assert_ne!(unsafe { libc::getsid(first_pid as i32) }, unsafe {
        libc::getsid(0)
    });
    assert!(!fixture.path("supervised").exists());
    let second = fixture.run(&["--json", "--idle-timeout", "0", "inspect"]);
    assert!(second.status.success(), "{second:?}");
    assert_ne!(fixture.pid(), first_pid);
    assert!(fixture
        .run(&["--idle-timeout", "0", "close"])
        .status
        .success());
    let deadline = Instant::now() + Duration::from_secs(10);
    while fixture.path("pid").exists() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    fixture.assert_clean();
}

#[test]
fn signals_cleanup_and_keep_debug_output_on_stderr() {
    for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
        let fixture = Fixture::new();
        let daemon = fixture.start(&["--debug"]);
        daemon.signal(signal);
        let output = daemon.finish();
        assert!(output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("[daemon] Started"));
        assert!(!fixture.path("log").exists());
        fixture.assert_clean();
        let restarted = fixture.start(&[]);
        restarted.signal(libc::SIGTERM);
        assert!(restarted.finish().status.success());
    }
}

#[test]
fn old_live_socket_without_lock_is_never_unlinked() {
    let fixture = Fixture::new();
    let listener = UnixListener::bind(fixture.path("sock")).unwrap();
    fs::write(fixture.path("pid"), std::process::id().to_string()).unwrap();
    let inode = fs::metadata(fixture.path("sock")).unwrap().ino();
    let output = fixture.run(&["daemon"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already owns"));
    assert_eq!(fixture.pid(), std::process::id());
    assert_eq!(fs::metadata(fixture.path("sock")).unwrap().ino(), inode);
    drop(listener);
}

#[test]
fn stale_pid_reused_by_another_live_process_does_not_block_startup() {
    let fixture = Fixture::new();
    fs::write(fixture.path("pid"), std::process::id().to_string()).unwrap();
    let daemon = fixture.start(&[]);
    assert_eq!(fixture.pid(), daemon.pid());
    assert_ne!(fixture.pid(), std::process::id());
    daemon.signal(libc::SIGTERM);
    assert!(daemon.finish().status.success());
    fixture.assert_clean();
}

#[test]
fn required_client_does_not_spawn_after_socket_disappears_during_request() {
    let fixture = Fixture::new();
    let daemon = fixture.start(&[]);
    let config = fs::read(fixture.path("config")).unwrap();
    daemon.signal(libc::SIGTERM);
    assert!(daemon.finish().status.success());
    fs::write(fixture.path("config"), config).unwrap();
    fs::write(fixture.path("version"), env!("CARGO_PKG_VERSION")).unwrap();
    let listener = UnixListener::bind(fixture.path("sock")).unwrap();
    let socket = fixture.path("sock");
    let server = thread::spawn(move || {
        let (ready, _) = listener.accept().unwrap();
        let mut line = String::new();
        assert_eq!(BufReader::new(ready).read_line(&mut line).unwrap(), 0);
        let (request, _) = listener.accept().unwrap();
        fs::remove_file(socket).unwrap();
        drop(listener);
        BufReader::new(request).read_line(&mut line).unwrap();
        assert!(line.contains("inspect"));
    });
    let output = fixture.run(&["--json", "--require-daemon", "inspect"]);
    server.join().unwrap();
    assert!(!output.status.success(), "{output:?}");
    assert!(!fixture.path("pid").exists());
    assert!(!fixture.path("sock").exists());
}

#[test]
fn foreground_parser_rejects_subcommands_and_unsafe_session_names() {
    let fixture = Fixture::new();
    for args in [
        &["daemon", "start"][..],
        &["--session", "../outside", "daemon"][..],
    ] {
        let output = fixture.run(args);
        assert!(!output.status.success());
        fixture.assert_clean();
        assert!(!fixture.path("lock").exists());
    }
}

#[test]
fn batch_cannot_dispatch_the_host_daemon_command() {
    let fixture = Fixture::new();
    let daemon = fixture.start(&[]);
    let output = fixture.run(&["--json", "--require-daemon", "batch", "daemon"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(response(&output)[0]["error"]
        .as_str()
        .unwrap()
        .contains("standalone foreground process"));
    assert_eq!(fixture.pid(), daemon.pid());
    daemon.signal(libc::SIGTERM);
    assert!(daemon.finish().status.success());
    fixture.assert_clean();
}

#[test]
fn required_sandbox_rejects_unsafe_launch_without_replacing_supervisor() {
    let fixture = Fixture::new();
    fixture.config(r#"{"requireSandbox":true,"executablePath":"/does-not-exist/chrome","args":"--no-sandbox"}"#);
    let daemon = fixture.start(&[]);
    let output = fixture.run(&["--json", "--require-daemon", "open"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(response(&output)["error"]
        .as_str()
        .unwrap()
        .contains("--require-sandbox cannot be combined"));
    assert_eq!(fixture.pid(), daemon.pid());
    let relaxed = fixture.run(&[
        "--json",
        "--require-daemon",
        "--require-sandbox",
        "false",
        "inspect",
    ]);
    assert!(!relaxed.status.success());
    assert!(response(&relaxed)["error"]
        .as_str()
        .unwrap()
        .contains("configuration"));
    assert_eq!(fixture.pid(), daemon.pid());
    daemon.signal(libc::SIGTERM);
    assert!(daemon.finish().status.success());
    fixture.assert_clean();
}

/// Run in an ephemeral container with Chrome and a sandbox-capable seccomp
/// policy. The HTTP server is loopback-only and intentionally never responds.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires isolated Chrome runtime; set AGENT_BROWSER_TEST_CHROME"]
fn foreground_signal_stops_blocked_navigation_and_owned_chrome() {
    use std::net::TcpListener;
    use std::sync::mpsc;

    let chrome = std::env::var("AGENT_BROWSER_TEST_CHROME")
        .expect("set AGENT_BROWSER_TEST_CHROME inside an isolated runtime");

    struct BrowserCleanup(i32);
    impl Drop for BrowserCleanup {
        fn drop(&mut self) {
            // This PID is the owned Chrome process-group leader in the test
            // container. Even a failed assertion must not leak its children.
            unsafe {
                libc::kill(-self.0, libc::SIGKILL);
            }
        }
    }

    for (signal, restore) in [
        (libc::SIGTERM, false),
        (libc::SIGHUP, false),
        (libc::SIGTERM, true),
    ] {
        let fixture = Fixture::new();
        let profile = fixture.0.path().join("profile");
        let mut config = serde_json::json!({
            "executablePath": chrome,
            "profile": profile,
            "idleTimeout": "0",
            "requireSandbox": true,
        });
        if restore {
            config["restore"] = serde_json::json!(format!("shutdown-proof-{}", std::process::id()));
        }
        fixture.config(&config.to_string());
        let daemon = fixture.start(&[]);
        let opened = fixture.run(&[
            "--json",
            "--require-daemon",
            "open",
            "data:text/html,<title>Ready</title>",
        ]);
        assert!(opened.status.success(), "Chrome launch failed: {opened:?}");

        let profile_arg = format!("--user-data-dir={}", profile.display());
        let browser_pid = fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .find_map(|entry| {
                let pid = entry.file_name().to_str()?.parse::<i32>().ok()?;
                let command = fs::read(entry.path().join("cmdline")).ok()?;
                let args: Vec<_> = command.split(|byte| *byte == 0).collect();
                (args.contains(&profile_arg.as_bytes())
                    && !args.iter().any(|arg| arg.starts_with(b"--type=")))
                .then_some(pid)
            })
            .expect("owned Chrome process was not found");
        let _browser_cleanup = BrowserCleanup(browser_pid);

        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/blocked", server.local_addr().unwrap());
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let server_thread = thread::spawn(move || {
            server.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match server.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("loopback accept failed: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut request = String::new();
            BufReader::new(&mut stream).read_line(&mut request).unwrap();
            assert!(request.starts_with("GET /blocked "), "{request}");
            started_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(10));
        });
        let mut navigation = Process(Some(
            fixture
                .command(&["--json", "--require-daemon", "open", &url])
                .spawn()
                .unwrap(),
        ));
        started_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("Chrome did not begin the blocked request");
        // Let the maintenance tick try to acquire the command-held state lock.
        thread::sleep(Duration::from_millis(250));
        assert!(navigation.0.as_mut().unwrap().try_wait().unwrap().is_none());
        let stopped = Instant::now();
        daemon.signal(signal);
        let output = daemon.finish();
        let elapsed = stopped.elapsed();
        assert!(output.status.success(), "{output:?}");
        assert!(
            elapsed < Duration::from_secs(5),
            "shutdown took {elapsed:?}"
        );
        assert!(!navigation.finish().status.success());
        fixture.assert_clean();
        // Child.wait in the driver must reap Chrome before daemon exit.
        assert_eq!(
            unsafe { libc::kill(browser_pid, 0) },
            -1,
            "Chrome survived its daemon"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        let surviving_helpers: Vec<_> = fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .filter_map(|entry| {
                let pid = entry.file_name().to_str()?.parse::<i32>().ok()?;
                let stat = fs::read_to_string(entry.path().join("stat")).ok()?;
                let fields: Vec<_> = stat.rsplit_once(')')?.1.split_whitespace().collect();
                (fields.get(2)?.parse::<i32>().ok()? == browser_pid
                    && !matches!(*fields.first()?, "Z" | "X"))
                .then_some(pid)
            })
            .collect();
        assert!(
            surviving_helpers.is_empty(),
            "Chrome helpers survived: {surviving_helpers:?}"
        );
        if restore {
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("1s termination grace period"),
                "{output:?}"
            );
        }
        drop(release_tx);
        server_thread.join().unwrap();
        eprintln!(
            "signal={signal} restore={restore} shutdown_ms={} owned_chrome_reaped=true",
            elapsed.as_millis()
        );
    }
}
