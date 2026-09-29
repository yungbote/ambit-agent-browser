//! The temporary profiles a browser runs from: its own empty one, or a copy
//! of a person's Chrome profile. Each holds that browser's cookies. It is
//! created private and marked with the run of the machine that made it, so
//! that a later run can remove one a killed daemon left behind: a daemon
//! killed with its sandbox never removes its browser's profile.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// A browser's own empty profile.
pub(crate) const OWN: &str = "agent-browser-chrome-";
/// A copy of a person's Chrome profile.
pub(crate) const COPY: &str = "agent-browser-profile-";

/// The file in a temporary profile that names the run that made it.
const RUN_MARKER: &str = ".agent-browser-run";

/// Creates a private temporary profile named `prefix` and a fresh id in
/// `root`, marked with `run` when the run is known.
pub(crate) fn create(root: &Path, prefix: &str, run: Option<&str>) -> Result<PathBuf, String> {
    let directory = root.join(format!("{prefix}{}", uuid::Uuid::new_v4()));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&directory)
        .map_err(|error| format!("Failed to create temp profile dir: {error}"))?;
    // Unmarked, a profile is never judged left behind: it only stays.
    if let Some(run) = run {
        let _ = std::fs::write(directory.join(RUN_MARKER), run);
    }
    Ok(directory)
}

/// Creates a private temporary profile in the process's temporary directory,
/// marked with this run of the machine.
pub(crate) fn create_temporary(prefix: &str) -> Result<PathBuf, String> {
    create(&std::env::temp_dir(), prefix, machine_run())
}

/// This run of the machine: its kernel's boot, and the start of process 1,
/// which a sandbox's stop ends and its next start begins again. `None` where
/// the kernel does not say (no `/proc`).
pub(crate) fn machine_run() -> Option<&'static str> {
    static RUN: OnceLock<Option<String>> = OnceLock::new();
    RUN.get_or_init(|| {
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        let init = std::fs::read_to_string("/proc/1/stat").ok()?;
        Some(format!("{}/{}", boot.trim(), start_ticks(&init)?))
    })
    .as_deref()
}

/// When the process a `/proc/<pid>/stat` line describes started, in clock
/// ticks after boot: its 22nd field, counted after the command name, which
/// may itself hold spaces and parentheses.
fn start_ticks(stat: &str) -> Option<u64> {
    stat.get(stat.rfind(')')? + 1..)?
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

/// Removes, from `root`, the temporary profiles of this user that another
/// run than `run` made: nothing of this run can own them. Answers how many
/// were removed. A profile without a mark, a link and anything that is not
/// a directory are left as they are.
pub(crate) fn sweep_left(root: &Path, run: &str) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            [OWN, COPY].iter().any(|prefix| name.starts_with(prefix))
                && entry.file_type().is_ok_and(|kind| kind.is_dir())
                && owned(entry)
                && std::fs::read_to_string(entry.path().join(RUN_MARKER))
                    .is_ok_and(|made_by| made_by != run)
        })
        .filter(|entry| std::fs::remove_dir_all(entry.path()).is_ok())
        .count()
}

#[cfg(unix)]
fn owned(entry: &std::fs::DirEntry) -> bool {
    use std::os::unix::fs::MetadataExt;
    entry
        .metadata()
        .is_ok_and(|metadata| metadata.uid() == unsafe { libc::geteuid() })
}

#[cfg(not(unix))]
fn owned(_: &std::fs::DirEntry) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_start_is_read_past_a_command_name_with_spaces_and_parentheses() {
        let stat = "1 (a) b (c)) S 0 1 1 0 -1 4194560 100 200 3 4 5 6 7 8 20 0 1 0 987654 1000 200";
        assert_eq!(start_ticks(stat), Some(987654));
        assert_eq!(start_ticks("1 (init) S 0"), None);
        assert_eq!(start_ticks("no command name"), None);
    }

    /// Only this user's temporary profiles that another run made are
    /// removed: this run's stay, an unmarked one stays, and neither a link
    /// named like one nor a file is followed or removed.
    #[cfg(unix)]
    #[test]
    fn only_profiles_another_run_made_are_removed() {
        let root = tempfile::tempdir().unwrap();
        let left = create(root.path(), OWN, Some("boot-a/10")).unwrap();
        std::fs::write(left.join("Cookies"), "sid").unwrap();
        let copied = create(root.path(), COPY, Some("boot-a/10")).unwrap();
        let live = create(root.path(), OWN, Some("boot-b/20")).unwrap();
        let unmarked = create(root.path(), OWN, None).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let target = create(elsewhere.path(), OWN, Some("boot-a/10")).unwrap();
        let link = root.path().join(format!("{OWN}link"));
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let file = root.path().join(format!("{OWN}file"));
        std::fs::write(&file, "boot-a/10").unwrap();
        let other = root.path().join("other-profile");
        std::fs::create_dir(&other).unwrap();
        std::fs::write(other.join(RUN_MARKER), "boot-a/10").unwrap();

        assert_eq!(sweep_left(root.path(), "boot-b/20"), 2);
        assert!(!left.exists() && !copied.exists());
        for kept in [&live, &unmarked, &link, &file, &other, &target] {
            assert!(kept.exists(), "{}", kept.display());
        }
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&live).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
