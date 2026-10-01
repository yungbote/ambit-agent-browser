//! Maintained desktop services for one SystemTheme-owned private display.
//! The session theme is authoritative; maintained dconf owns GSettings writes.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::system_theme::{read_owned_output, write_private_configuration};
use crate::native::theme::Theme;

const FRONTEND: &str = "/usr/libexec/xdg-desktop-portal";
const BACKEND: &str = "/usr/libexec/xdg-desktop-portal-gtk";
const OBSERVER: &str = "/usr/bin/dbus-send";
const SETTER: &str = "/usr/bin/gsettings";
const WRITER: &str = "/usr/libexec/dconf-service";
const READINESS: Duration = Duration::from_secs(5);

pub(super) struct DesktopPortal {
    children: Vec<Child>,
    address: String,
    configuration: PathBuf,
    directory: PathBuf,
    display: String,
    authority: PathBuf,
}

impl DesktopPortal {
    pub(super) fn installed() -> bool {
        Path::new(FRONTEND).is_file() || Path::new(BACKEND).is_file()
    }

    pub(super) fn start(
        directory: &Path,
        display: &str,
        authority: &Path,
        theme: Theme,
        canceled: &AtomicBool,
    ) -> Result<Self, String> {
        let directory = directory.join("desktop");
        for child in [
            "",
            "config",
            "config/xdg-desktop-portal",
            "data",
            "cache",
            "runtime",
        ] {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(directory.join(child))
                .map_err(|_| "Could not create the private desktop directory")?;
        }
        let mut owner = Self {
            children: Vec::new(),
            address: bus_address(&directory.join("bus")),
            configuration: directory.join("bus.conf"),
            directory,
            display: display.into(),
            authority: authority.into(),
        };
        write_private_configuration(&owner.configuration, &bus_configuration(&owner.address))?;
        write_private_configuration(&owner.directory.join("config/xdg-desktop-portal/portals.conf"),
            "[preferred]\ndefault=none\norg.freedesktop.impl.portal.Settings=gtk\norg.freedesktop.impl.portal.FileChooser=gtk\n")?;
        write_private_configuration(&owner.directory.join("dconf-profile"), "user-db:user\n")?;
        let deadline = Instant::now() + READINESS;
        let configuration = owner
            .configuration
            .to_str()
            .ok_or("Private bus path is not UTF-8")?
            .to_owned();
        owner.spawn(
            "/usr/bin/dbus-daemon",
            &["--nofork", "--config-file", &configuration],
            canceled,
        )?;
        if !owner.await_condition(deadline, canceled, |owner, deadline| {
            owner.name_owned("org.freedesktop.DBus", deadline, canceled)
        }) {
            return Err("Private desktop bus did not become ready".into());
        }
        owner.spawn(WRITER, &[], canceled)?;
        if !owner.await_condition(deadline, canceled, |owner, deadline| {
            owner.name_owned("ca.desrt.dconf", deadline, canceled)
        }) || !owner.project(theme, deadline, canceled)
        {
            return Err("Private GSettings writer did not become ready".into());
        }
        owner.spawn(BACKEND, &[], canceled)?;
        if !owner.await_condition(deadline, canceled, |owner, deadline| {
            owner.name_owned(
                "org.freedesktop.impl.portal.desktop.gtk",
                deadline,
                canceled,
            )
        }) {
            return Err("Private GTK desktop service did not become ready".into());
        }
        owner.spawn(FRONTEND, &[], canceled)?;
        if !owner.await_condition(deadline, canceled, |owner, deadline| {
            owner.file_chooser_ready(deadline, canceled)
                && owner.theme_acknowledged(theme, deadline, canceled)
        }) {
            return Err("Private Settings and FileChooser services did not become ready".into());
        }
        Ok(owner)
    }

    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .env("DISPLAY", &self.display)
            .env("XAUTHORITY", &self.authority)
            .env("DBUS_SESSION_BUS_ADDRESS", &self.address)
            .env(
                "DBUS_SYSTEM_BUS_ADDRESS",
                "unix:path=/nonexistent/ambit-no-system-bus",
            )
            .env("XDG_CONFIG_HOME", self.directory.join("config"))
            .env("XDG_DATA_HOME", self.directory.join("data"))
            .env("XDG_CACHE_HOME", self.directory.join("cache"))
            .env("XDG_RUNTIME_DIR", self.directory.join("runtime"))
            .env("GSETTINGS_BACKEND", "dconf")
            .env("DCONF_PROFILE", self.directory.join("dconf-profile"))
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET");
        command
    }

    fn spawn(
        &mut self,
        program: &str,
        arguments: &[&str],
        canceled: &AtomicBool,
    ) -> Result<(), String> {
        if canceled.load(Ordering::Relaxed) {
            return Err("Private desktop startup canceled".into());
        }
        let child = self
            .command(program)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("Private desktop service is unavailable: {error}"))?;
        self.children.push(child);
        Ok(())
    }

    pub(super) fn running(&mut self) -> bool {
        self.children
            .iter_mut()
            .all(|child| matches!(child.try_wait(), Ok(None)))
    }

    fn await_condition(
        &mut self,
        deadline: Instant,
        canceled: &AtomicBool,
        ready: impl Fn(&Self, Instant) -> bool,
    ) -> bool {
        while !canceled.load(Ordering::Relaxed) && Instant::now() < deadline && self.running() {
            if ready(self, deadline) {
                return true;
            }
            // This retries an absent DBus property, never delays an acknowledged value.
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    fn observe(
        &self,
        destination: &str,
        path: &str,
        method: &str,
        arguments: &[&str],
        deadline: Instant,
        canceled: &AtomicBool,
    ) -> Option<Vec<u8>> {
        let mut command = self.command(OBSERVER);
        command
            .args(["--session", "--print-reply=literal", "--reply-timeout=500"])
            .arg(format!("--dest={destination}"))
            .arg(path)
            .arg(method)
            .args(arguments);
        read_owned_output(command, deadline, canceled)
    }

    fn name_owned(&self, name: &str, deadline: Instant, canceled: &AtomicBool) -> bool {
        self.observe(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus.NameHasOwner",
            &[&format!("string:{name}")],
            deadline,
            canceled,
        )
        .is_some_and(|bytes| tokens(&bytes) == Some(vec!["boolean", "true"]))
    }

    fn file_chooser_ready(&self, deadline: Instant, canceled: &AtomicBool) -> bool {
        self.observe("org.freedesktop.portal.Desktop", "/org/freedesktop/portal/desktop", "org.freedesktop.DBus.Properties.Get",
            &["string:org.freedesktop.portal.FileChooser", "string:version"], deadline, canceled)
            .is_some_and(|bytes| matches!(tokens(&bytes).as_deref(), Some(["variant", "uint32", value]) if value.parse::<u32>().is_ok_and(|version| version >= 3)))
    }

    fn theme_acknowledged(&self, theme: Theme, deadline: Instant, canceled: &AtomicBool) -> bool {
        self.observe(
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Settings.Read",
            &["string:org.freedesktop.appearance", "string:color-scheme"],
            deadline,
            canceled,
        )
        .is_some_and(|bytes| appearance_acknowledged(&bytes, theme))
    }

    fn project(&self, theme: Theme, deadline: Instant, canceled: &AtomicBool) -> bool {
        self.project_with_program(SETTER, theme, deadline, canceled)
    }

    fn project_with_program(
        &self,
        setter: &str,
        theme: Theme,
        deadline: Instant,
        canceled: &AtomicBool,
    ) -> bool {
        let mut command = self.command(setter);
        // The maintained CLI changes only this key and flushes its GSettings
        // write before exit; dconf serializes it with the chooser's own keys.
        command.args([
            "set",
            "org.gnome.desktop.interface",
            "color-scheme",
            appearance_value(theme),
        ]);
        read_owned_output(command, deadline, canceled).is_some()
    }

    pub(super) fn apply(&mut self, theme: Theme, canceled: &AtomicBool) -> bool {
        let deadline = Instant::now() + READINESS;
        if canceled.load(Ordering::Relaxed)
            || !self.running()
            || !self.project(theme, deadline, canceled)
        {
            return false;
        }
        self.await_condition(deadline, canceled, |owner, deadline| {
            owner.theme_acknowledged(theme, deadline, canceled)
        })
    }

    pub(super) fn apply_chrome_environment(&self, command: &mut Command) {
        command.env("DBUS_SESSION_BUS_ADDRESS", &self.address);
    }
}

impl Drop for DesktopPortal {
    fn drop(&mut self) {
        // Frontend/backend retire before their bus. Each child remains owned
        // and unreaped until this owner kills or waits it, so PID reuse is impossible.
        for child in self.children.iter_mut().rev() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn bus_address(path: &Path) -> String {
    let mut address = "unix:path=".to_owned();
    for byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"_-/.".contains(byte) {
            address.push(*byte as char);
        } else {
            address.push_str(&format!("%{byte:02X}"));
        }
    }
    address
}

fn bus_configuration(address: &str) -> String {
    format!("<busconfig>\n<type>session</type>\n<listen>{address}</listen>\n<auth>EXTERNAL</auth>\n<policy context=\"default\">\n<deny user=\"*\"/>\n<allow user=\"{}\"/>\n<allow own=\"*\"/>\n<allow send_destination=\"*\"/>\n<allow receive_sender=\"*\"/>\n</policy>\n</busconfig>\n", unsafe { libc::geteuid() })
}

fn appearance_value(theme: Theme) -> &'static str {
    match theme {
        Theme::Dark => "prefer-dark",
        Theme::Light => "prefer-light",
    }
}

fn tokens(bytes: &[u8]) -> Option<Vec<&str>> {
    Some(
        std::str::from_utf8(bytes)
            .ok()?
            .split_whitespace()
            .collect(),
    )
}

fn appearance_acknowledged(bytes: &[u8], theme: Theme) -> bool {
    tokens(bytes)
        == Some(vec![
            "variant",
            "variant",
            "uint32",
            match theme {
                Theme::Dark => "1",
                Theme::Light => "2",
            },
        ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn the_same_session_enum_projects_to_standard_settings_values() {
        assert_eq!(appearance_value(Theme::Dark), "prefer-dark");
        assert_eq!(appearance_value(Theme::Light), "prefer-light");
        assert!(appearance_acknowledged(
            b" variant variant uint32 1\n",
            Theme::Dark
        ));
        assert!(appearance_acknowledged(
            b" variant variant uint32 2\n",
            Theme::Light
        ));
        for bytes in [
            b"variant variant uint32 0".as_slice(),
            b"variant uint32 1",
            b"variant variant string 1",
            b"variant variant uint32 1 trailing",
            &[0xff],
        ] {
            assert!(!appearance_acknowledged(bytes, Theme::Dark));
        }
        assert!(!appearance_acknowledged(
            b"variant variant uint32 2",
            Theme::Dark
        ));
    }

    #[test]
    fn private_bus_has_only_unix_external_authentication_and_no_activation() {
        let address = bus_address(Path::new("/private/a b,%&<c>/bus"));
        assert_eq!(address, "unix:path=/private/a%20b%2C%25%26%3Cc%3E/bus");
        let config = bus_configuration(&address);
        assert!(config.contains("<auth>EXTERNAL</auth>"));
        assert!(config.contains("<deny user=\"*\"/>"));
        assert!(!config.contains("servicedir"));
        assert!(!config.contains("tcp:"));
        assert!(!config.contains("ANONYMOUS"));
    }

    #[test]
    fn canceled_startup_removes_its_private_projection_without_starting_services() {
        let root = tempfile::tempdir().unwrap();
        let result = DesktopPortal::start(
            root.path(),
            ":994",
            &root.path().join("authority"),
            Theme::Dark,
            &AtomicBool::new(true),
        );
        assert!(result.is_err());
        assert!(!root.path().join("desktop").exists());
    }

    #[test]
    fn maintained_setter_changes_only_the_theme_key_with_private_environment_and_bounded_failure() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("desktop");
        fs::create_dir_all(directory.join("config/dconf")).unwrap();
        let database = directory.join("config/dconf/user");
        fs::write(&database, "chooser preferences must remain").unwrap();
        let record = root.path().join("setter-call.json");
        let setter = root.path().join("gsettings");
        let owner = DesktopPortal {
            children: Vec::new(),
            address: bus_address(&directory.join("bus")),
            configuration: directory.join("bus.conf"),
            directory: directory.clone(),
            display: ":994".into(),
            authority: root.path().join("authority"),
        };
        for theme in Theme::ALL {
            fs::write(&setter, format!("#!/usr/bin/python3\nimport json,os,sys\nfrom pathlib import Path\nPath({record:?}).write_text(json.dumps(dict(args=sys.argv[1:],config=os.environ['XDG_CONFIG_HOME'],backend=os.environ['GSETTINGS_BACKEND'],profile=os.environ['DCONF_PROFILE'])))\n")).unwrap();
            fs::set_permissions(&setter, fs::Permissions::from_mode(0o700)).unwrap();
            assert!(owner.project_with_program(
                setter.to_str().unwrap(),
                theme,
                Instant::now() + Duration::from_secs(2),
                &AtomicBool::new(false)
            ));
            let observed: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&record).unwrap()).unwrap();
            assert_eq!(
                observed["args"],
                serde_json::json!([
                    "set",
                    "org.gnome.desktop.interface",
                    "color-scheme",
                    appearance_value(theme)
                ])
            );
            assert_eq!(
                observed["config"],
                directory.join("config").to_str().unwrap()
            );
            assert_eq!(observed["backend"], "dconf");
            assert_eq!(
                observed["profile"],
                directory.join("dconf-profile").to_str().unwrap()
            );
            assert_eq!(
                fs::read_to_string(&database).unwrap(),
                "chooser preferences must remain"
            );
        }
        fs::remove_file(&record).unwrap();
        assert!(!owner.project_with_program(
            setter.to_str().unwrap(),
            Theme::Dark,
            Instant::now() + Duration::from_secs(2),
            &AtomicBool::new(true)
        ));
        assert!(!record.exists());
        fs::write(&setter, "#!/usr/bin/python3\nimport sys\nsys.exit(1)\n").unwrap();
        assert!(!owner.project_with_program(
            setter.to_str().unwrap(),
            Theme::Dark,
            Instant::now() + Duration::from_secs(2),
            &AtomicBool::new(false)
        ));
        assert_eq!(
            fs::read_to_string(database).unwrap(),
            "chooser preferences must remain"
        );
    }

    #[test]
    fn dropping_the_owner_reaps_its_children_and_removes_only_its_directory() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("desktop");
        fs::create_dir(&directory).unwrap();
        let preserved = root.path().join("profile");
        fs::write(&preserved, "retained profile").unwrap();
        let child = Command::new("/usr/bin/python3")
            .args(["-c", "import signal; signal.pause()"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let owner = DesktopPortal {
            children: vec![child],
            address: bus_address(&directory.join("bus")),
            configuration: directory.join("bus.conf"),
            directory: directory.clone(),
            display: ":994".into(),
            authority: root.path().join("authority"),
        };
        let configuration = directory.join("bus.conf");
        write_private_configuration(&configuration, &bus_configuration(&owner.address)).unwrap();
        assert_eq!(
            fs::metadata(&configuration).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let mut chrome = Command::new("unexecuted-chrome");
        chrome.env("XDG_CONFIG_HOME", &preserved);
        owner.apply_chrome_environment(&mut chrome);
        assert_eq!(
            chrome
                .get_envs()
                .find(|(key, _)| *key == "XDG_CONFIG_HOME")
                .unwrap()
                .1
                .unwrap(),
            preserved.as_os_str()
        );
        drop(owner);
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert!(!directory.exists());
        assert_eq!(fs::read_to_string(preserved).unwrap(), "retained profile");
    }
}
