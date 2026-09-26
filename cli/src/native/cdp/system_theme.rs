//! The standard XSettings service for one driver-owned private display.
//! Theme is still session state; this private file is only its OS projection.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::native::theme::Theme;

const ACKNOWLEDGMENT: Duration = Duration::from_secs(2);
const OBSERVER_BYTES: u64 = 64 * 1024;

pub(super) struct SystemTheme {
    child: Child,
    directory: PathBuf,
    configuration: PathBuf,
    display: String,
    authority: PathBuf,
    observer: PathBuf,
    acknowledged: Option<Theme>,
}

impl SystemTheme {
    pub(super) fn enabled() -> bool {
        std::env::var("AGENT_BROWSER_SYSTEM_THEME").as_deref() == Ok("xsettingsd")
            && std::env::var_os("GTK_THEME").is_none()
    }

    pub(super) fn start(
        display: &str,
        authority: &Path,
        theme: Theme,
        canceled: &AtomicBool,
    ) -> Result<Self, String> {
        Self::start_with_programs(
            display,
            authority,
            theme,
            canceled,
            Path::new("/usr/bin/xsettingsd"),
            Path::new("/usr/bin/dump_xsettings"),
        )
    }

    fn start_with_programs(
        display: &str,
        authority: &Path,
        theme: Theme,
        canceled: &AtomicBool,
        daemon: &Path,
        observer: &Path,
    ) -> Result<Self, String> {
        if canceled.load(Ordering::Relaxed) {
            return Err("Private system theme startup canceled".into());
        }
        let directory =
            std::env::temp_dir().join(format!("agent-browser-settings-{}", uuid::Uuid::new_v4()));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .map_err(|_| "Could not create private system theme directory")?;
        let configuration = directory.join("xsettings.conf");
        let result = (|| {
            write_configuration(&configuration, theme)?;
            Command::new(daemon)
                .arg("-c")
                .arg(&configuration)
                .env("DISPLAY", display)
                .env("XAUTHORITY", authority)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|error| format!("Private XSettings service is unavailable: {error}"))
        })();
        let child = match result {
            Ok(child) => child,
            Err(error) => {
                let _ = fs::remove_dir_all(&directory);
                return Err(error);
            }
        };
        let mut owner = Self {
            child,
            directory,
            configuration,
            display: display.into(),
            authority: authority.into(),
            observer: observer.into(),
            acknowledged: None,
        };
        if !owner.await_acknowledgment(theme, canceled) {
            return Err("Private XSettings service did not acknowledge the theme".into());
        }
        owner.acknowledged = Some(theme);
        Ok(owner)
    }

    pub(super) fn apply(&mut self, theme: Theme, canceled: &AtomicBool) -> bool {
        if canceled.load(Ordering::Relaxed) || !self.running() {
            return false;
        }
        if self.acknowledged == Some(theme) {
            return true;
        }
        self.acknowledged = None;
        if write_configuration(&self.configuration, theme).is_err() {
            return false;
        }
        // The child is owned and unreaped. A concurrent exit leaves a zombie,
        // never a reusable PID; this owner is the only code that can reap it.
        if unsafe { libc::kill(self.child.id() as i32, libc::SIGHUP) } != 0 {
            return false;
        }
        if !self.await_acknowledgment(theme, canceled) {
            return false;
        }
        self.acknowledged = Some(theme);
        true
    }

    fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn await_acknowledgment(&mut self, theme: Theme, canceled: &AtomicBool) -> bool {
        let deadline = Instant::now() + ACKNOWLEDGMENT;
        while !canceled.load(Ordering::Relaxed) && Instant::now() < deadline && self.running() {
            if self.read_acknowledgment(theme, deadline, canceled) {
                return true;
            }
            // Retry an actual absent/mismatched property. No delay is applied
            // after success, and no fixed wait substitutes for readiness.
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    fn read_acknowledgment(&self, theme: Theme, deadline: Instant, canceled: &AtomicBool) -> bool {
        let Ok(child) = Command::new(&self.observer)
            .env("DISPLAY", &self.display)
            .env("XAUTHORITY", &self.authority)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        else {
            return false;
        };
        let mut observer = Observer(child);
        let Some(mut stdout) = observer.0.stdout.take() else {
            return false;
        };
        // The deadline also bounds a wedged observer or inherited pipe writer.
        // Never call blocking read_to_end merely because the child exited.
        let descriptor = stdout.as_raw_fd();
        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
        {
            return false;
        }
        let mut bytes = Vec::new();
        let mut eof = false;
        loop {
            if canceled.load(Ordering::Relaxed) || Instant::now() >= deadline {
                return false;
            }
            loop {
                let mut chunk = [0u8; 4096];
                match stdout.read(&mut chunk) {
                    Ok(0) => {
                        eof = true;
                        break;
                    }
                    Ok(size) => {
                        bytes.extend_from_slice(&chunk[..size]);
                        if bytes.len() as u64 > OBSERVER_BYTES {
                            return false;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => return false,
                }
            }
            match observer.0.try_wait() {
                Ok(Some(status)) => {
                    if !status.success() {
                        return false;
                    }
                    if eof {
                        return acknowledged_theme(&bytes, theme);
                    }
                }
                Ok(None) => {}
                Err(_) => return false,
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

struct Observer(Child);
impl Drop for Observer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for SystemTheme {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn configuration(theme: Theme) -> String {
    format!(
        "Net/ThemeName \"{}\"\n",
        match theme {
            Theme::Dark => "Adwaita-dark",
            Theme::Light => "Adwaita",
        }
    )
}

fn write_configuration(path: &Path, theme: Theme) -> Result<(), String> {
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(configuration(theme).as_bytes())?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|_| "Could not project the private system theme".into())
}

fn acknowledged_theme(bytes: &[u8], theme: Theme) -> bool {
    std::str::from_utf8(bytes).is_ok_and(|text| {
        text.lines()
            .any(|line| line.trim() == configuration(theme).trim())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn only_the_exact_standard_theme_property_acknowledges_an_update() {
        for theme in Theme::ALL {
            assert!(acknowledged_theme(configuration(theme).as_bytes(), theme));
        }
        assert!(!acknowledged_theme(
            b"Net/ThemeName \"Adwaita-dark\"",
            Theme::Light
        ));
        assert!(!acknowledged_theme(
            b"Other/ThemeName \"Adwaita\"",
            Theme::Light
        ));
        assert!(!acknowledged_theme(
            b"Net/ThemeName \"Adwaita\" trailing",
            Theme::Light
        ));
        assert!(!acknowledged_theme(&[0xff], Theme::Light));
    }

    #[test]
    fn atomic_projection_is_private_and_leaves_no_intermediate_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.conf");
        write_configuration(&path, Theme::Dark).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            configuration(Theme::Dark)
        );
        write_configuration(&path, Theme::Light).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            configuration(Theme::Light)
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    fn program(path: &Path, source: &str) {
        fs::write(path, source).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn owned_service_acknowledges_updates_and_cleans_up_its_private_lifetime() {
        let _environment = crate::test_utils::EnvGuard::new(&[]);
        let fixture = tempfile::tempdir().unwrap();
        let acknowledgment = fixture.path().join("ack");
        let observed_environment = fixture.path().join("environment");
        let observer_calls = fixture.path().join("observer-calls");
        let daemon = fixture.path().join("daemon");
        let observer = fixture.path().join("observer");
        let authority = fixture.path().join("private-authority");
        fs::write(&authority, "test-cookie").unwrap();
        program(
            &daemon,
            &format!(
                r#"#!/usr/bin/python3
import json,os,signal,sys
from pathlib import Path
source=Path(sys.argv[2])
def apply(*ignored): Path({acknowledgment:?}).write_text(source.read_text())
signal.signal(signal.SIGHUP,apply)
Path({observed_environment:?}).write_text(json.dumps(dict(display=os.environ['DISPLAY'],authority=os.environ['XAUTHORITY'])))
apply()
while True: signal.pause()
"#
            ),
        );
        program(
            &observer,
            &format!(
                r#"#!/usr/bin/python3
from pathlib import Path
with Path({observer_calls:?}).open('a') as output: output.write('called\n')
print(Path({acknowledgment:?}).read_text(),end='')
"#
            ),
        );
        let canceled = AtomicBool::new(false);
        let mut owner = SystemTheme::start_with_programs(
            ":997",
            &authority,
            Theme::Dark,
            &canceled,
            &daemon,
            &observer,
        )
        .unwrap();
        let pid = owner.child.id();
        let private_directory = owner.directory.clone();
        assert_eq!(
            fs::metadata(&private_directory)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let environment: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&observed_environment).unwrap()).unwrap();
        assert_eq!(environment["display"], ":997");
        assert_eq!(
            environment["authority"],
            authority.to_string_lossy().as_ref()
        );
        let calls = fs::read_to_string(&observer_calls).unwrap();
        assert!(owner.apply(Theme::Dark, &canceled));
        assert_eq!(
            fs::read_to_string(&observer_calls).unwrap(),
            calls,
            "an unchanged acknowledged theme needs no subprocess"
        );
        assert!(owner.apply(Theme::Light, &canceled));
        assert_eq!(
            fs::read_to_string(&acknowledgment).unwrap(),
            configuration(Theme::Light)
        );
        canceled.store(true, Ordering::Relaxed);
        assert!(!owner.apply(Theme::Dark, &canceled));
        assert_eq!(
            fs::read_to_string(&acknowledgment).unwrap(),
            configuration(Theme::Light)
        );
        canceled.store(false, Ordering::Relaxed);
        owner.child.kill().unwrap();
        owner.child.wait().unwrap();
        assert!(
            !owner.apply(Theme::Light, &canceled),
            "a cached value cannot hide service exit"
        );
        drop(owner);
        assert!(!private_directory.exists());
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    }

    #[test]
    fn canceled_or_unavailable_startup_never_claims_a_system_theme() {
        let authority = Path::new("/unused-private-authority");
        assert!(SystemTheme::start_with_programs(
            ":998",
            authority,
            Theme::Light,
            &AtomicBool::new(true),
            Path::new("/missing-daemon"),
            Path::new("/missing-observer")
        )
        .is_err());
        assert!(SystemTheme::start_with_programs(
            ":998",
            authority,
            Theme::Light,
            &AtomicBool::new(false),
            Path::new("/missing-daemon"),
            Path::new("/missing-observer")
        )
        .is_err());
    }

    #[test]
    fn a_wedged_observer_is_canceled_and_reaped_without_waiting_for_the_deadline() {
        let _environment = crate::test_utils::EnvGuard::new(&[]);
        let fixture = tempfile::tempdir().unwrap();
        let daemon = fixture.path().join("daemon");
        let observer = fixture.path().join("observer");
        let owner_record = fixture.path().join("owner.json");
        let observer_record = fixture.path().join("observer.pid");
        program(
            &daemon,
            &format!(
                r#"#!/usr/bin/python3
import json,os,signal,sys
from pathlib import Path
Path({owner_record:?}).write_text(json.dumps(dict(pid=os.getpid(),configuration=sys.argv[2])))
while True: signal.pause()
"#
            ),
        );
        program(
            &observer,
            &format!(
                r#"#!/usr/bin/python3
import os,signal
from pathlib import Path
Path({observer_record:?}).write_text(str(os.getpid()))
while True: signal.pause()
"#
            ),
        );
        let canceled = AtomicBool::new(false);
        let started = Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let deadline = Instant::now() + Duration::from_secs(1);
                while !observer_record.exists() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(2));
                }
                canceled.store(true, Ordering::Relaxed);
            });
            assert!(SystemTheme::start_with_programs(
                ":996",
                Path::new("/private-authority"),
                Theme::Light,
                &canceled,
                &daemon,
                &observer,
            )
            .is_err());
        });
        assert!(started.elapsed() < ACKNOWLEDGMENT);
        let owner: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(owner_record).unwrap()).unwrap();
        let observer_pid: i32 = fs::read_to_string(observer_record)
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            unsafe { libc::kill(owner["pid"].as_i64().unwrap() as i32, 0) },
            -1
        );
        assert_eq!(unsafe { libc::kill(observer_pid, 0) }, -1);
        assert!(!Path::new(owner["configuration"].as_str().unwrap())
            .parent()
            .unwrap()
            .exists());
    }

    #[test]
    fn bounded_observer_refuses_overflow_and_expiry_and_reaps_the_child() {
        let _environment = crate::test_utils::EnvGuard::new(&[]);
        let fixture = tempfile::tempdir().unwrap();
        let observer = fixture.path().join("observer");
        let observer_record = fixture.path().join("observer.pid");
        let daemon = Command::new("/usr/bin/python3")
            .args(["-c", "import signal; signal.pause()"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let owner = SystemTheme {
            child: daemon,
            directory: fixture.path().join("unused"),
            configuration: fixture.path().join("settings.conf"),
            display: ":995".into(),
            authority: fixture.path().join("authority"),
            observer: observer.clone(),
            acknowledged: None,
        };
        for body in [
            "import sys; sys.stdout.buffer.write(b'x' * 65537); sys.stdout.flush()",
            "import signal; signal.pause()",
        ] {
            program(&observer, &format!("#!/usr/bin/python3\nimport os\nfrom pathlib import Path\nPath({observer_record:?}).write_text(str(os.getpid()))\n{body}\n"));
            let started = Instant::now();
            assert!(!owner.read_acknowledgment(
                Theme::Light,
                started + Duration::from_millis(150),
                &AtomicBool::new(false)
            ));
            assert!(started.elapsed() < Duration::from_secs(1));
            let pid: i32 = fs::read_to_string(&observer_record)
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        }
    }
}
