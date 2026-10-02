//! A converter run confined. A converter parses an untrusted document, so it
//! runs as a process of its own with:
//! - no network: a seccomp filter refuses every socket but a local (Unix)
//!   one, every `connect`, and io_uring, whose operations would otherwise
//!   pass around the filter; a foreign architecture's system calls kill it;
//! - bounded memory (`RLIMIT_DATA`, which counts the heap without the address
//!   space a runtime reserves), CPU time, file size and open files, and no
//!   core dump;
//! - a clean environment and its conversion's private directory as its
//!   working, home and temporary directory;
//! - a process group of its own, killed as a whole at the deadline, when its
//!   output passes its bound, or when its caller goes away.
//!
//! What it does not have: a filesystem view of its own. The workspace sandbox
//! grants neither mount nor Landlock, so a converter reads what the
//! workspace's user reads. It cannot send any of it anywhere but into its own
//! output, which is bounded and reaches the caller as the document's text.

use std::ffi::OsString;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::time::Instant;

use super::{code, DocumentError};

/// A converter's heap, whatever its runtime reserves.
const MEMORY: u64 = 1 << 30;
/// The largest file a converter writes: LibreOffice's intermediate output.
const FILE_SIZE: u64 = 256 << 20;
/// CPU seconds, a backstop behind the wall-clock deadline.
const CPU_SECONDS: u64 = 120;
const OPEN_FILES: u64 = 1024;
/// How much of a converter's diagnostics a failure quotes.
const DIAGNOSTICS: usize = 4096;

/// A program to run on a document: its name on `PATH`, and the words that
/// name it in errors.
#[derive(Debug, Clone, Copy)]
pub(super) struct Program {
    pub(super) command: &'static str,
    pub(super) what: &'static str,
}

/// A finished run: what it wrote to standard output, whether that stopped at
/// its bound, and its diagnostics.
pub(super) struct Ran {
    pub(super) stdout: Vec<u8>,
    pub(super) truncated: bool,
    pub(super) diagnostics: String,
}

impl Program {
    /// The program's path on this process's `PATH`.
    fn locate(&self) -> Result<PathBuf, DocumentError> {
        std::env::var_os("PATH")
            .iter()
            .flat_map(std::env::split_paths)
            .map(|directory| directory.join(self.command))
            .find(|candidate| is_executable(candidate))
            .ok_or_else(|| {
                DocumentError::new(
                    code::CONVERTER_UNAVAILABLE,
                    format!(
                        "Reading this document needs {}, which this browser's machine does not have.",
                        self.what
                    ),
                )
            })
    }

    /// Runs the program with `arguments` in `directory`, keeping at most
    /// `stdout_limit` bytes of its output, until `deadline`.
    pub(super) async fn run(
        &self,
        arguments: &[OsString],
        directory: &Path,
        stdout_limit: usize,
        deadline: Instant,
    ) -> Result<Ran, DocumentError> {
        let path = self.locate()?;
        let filter = network_filter();
        let mut command = tokio::process::Command::new(&path);
        command
            .args(arguments)
            .current_dir(directory)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", directory)
            .env("TMPDIR", directory)
            .env("LANG", "C.UTF-8")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // SAFETY: the closure runs in the child between fork and exec. It
        // only makes system calls, on memory prepared before the fork.
        unsafe {
            command.pre_exec(move || confine_self(&filter));
        }
        let mut child = command.spawn().map_err(|error| {
            DocumentError::new(
                code::CONVERSION_FAILED,
                format!("{} could not start: {error}", self.what),
            )
        })?;
        let group = Group::of(&child);
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let finished = tokio::select! {
            finished = async {
                let (output, diagnostics) = tokio::join!(
                    read_bounded(stdout, stdout_limit, &group),
                    read_bounded(stderr, DIAGNOSTICS, &Group::detached()),
                );
                (output, diagnostics, child.wait().await)
            } => Some(finished),
            _ = tokio::time::sleep_until(deadline) => None,
        };
        let Some(((stdout, truncated), (diagnostics, _), status)) = finished else {
            group.kill();
            let _ = child.wait().await;
            group.reaped();
            return Err(DocumentError::new(
                code::CONVERSION_TIMEOUT,
                format!(
                    "{} did not finish converting the document in time. It may be very large or damaged; take screenshots of the pages you need instead.",
                    self.what
                ),
            ));
        };
        group.reaped();
        let status = status.map_err(DocumentError::io)?;
        let diagnostics = String::from_utf8_lossy(&diagnostics).trim().to_string();
        if truncated || status.success() {
            return Ok(Ran {
                stdout,
                truncated,
                diagnostics,
            });
        }
        Err(match status.signal() {
            Some(libc::SIGXFSZ) => DocumentError::new(
                code::TOO_LARGE,
                format!(
                    "Converting the document made {} write more than {} MB, so it was stopped.",
                    self.what,
                    FILE_SIZE >> 20
                ),
            ),
            _ => DocumentError::new(
                code::CONVERSION_FAILED,
                format!(
                    "{} could not convert the document ({}). It may be damaged, or too complex to convert within {} GB of memory.{}",
                    self.what,
                    match status.signal() {
                        Some(signal) => format!("stopped by signal {signal}"),
                        None => format!("exit status {}", status.code().unwrap_or(-1)),
                    },
                    MEMORY >> 30,
                    quote(&diagnostics),
                ),
            ),
        })
    }
}

/// The first line of a converter's diagnostics, quoted after a failure.
fn quote(diagnostics: &str) -> String {
    diagnostics
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| format!(" It said: {}", line.chars().take(300).collect::<String>()))
        .unwrap_or_default()
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Reads `stream` to its end, keeping at most `limit` bytes. Past the limit
/// the stream's writers are stopped: their group is killed.
async fn read_bounded(
    stream: Option<impl AsyncRead + Unpin>,
    limit: usize,
    group: &Group,
) -> (Vec<u8>, bool) {
    let Some(mut stream) = stream else {
        return (Vec::new(), false);
    };
    let mut kept = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return (kept, false),
            Ok(read) => {
                let room = limit - kept.len();
                if read > room {
                    kept.extend_from_slice(&chunk[..room]);
                    group.kill();
                    return (kept, true);
                }
                kept.extend_from_slice(&chunk[..read]);
            }
        }
    }
}

/// A converter's process group, killed as a whole while its leader is not yet
/// reaped (so its id cannot name another group), and when dropped.
struct Group {
    leader: Option<i32>,
    reaped: std::cell::Cell<bool>,
}

impl Group {
    fn of(child: &tokio::process::Child) -> Self {
        Self {
            leader: child.id().map(|id| id as i32),
            reaped: std::cell::Cell::new(false),
        }
    }

    /// A group nothing kills: diagnostics past their bound are dropped, not
    /// stopped.
    fn detached() -> Self {
        Self {
            leader: None,
            reaped: std::cell::Cell::new(true),
        }
    }

    fn kill(&self) {
        if let Some(leader) = self.leader.filter(|_| !self.reaped.get()) {
            // SAFETY: killpg(2) on the group this run created; its leader is
            // unreaped, so the id still names this group.
            unsafe {
                libc::killpg(leader, libc::SIGKILL);
            }
        }
    }

    fn reaped(&self) {
        self.reaped.set(true);
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
    }
}

/// In the child, before exec: a process group of its own, the limits, and the
/// network filter. Only system calls, on memory prepared before the fork.
fn confine_self(filter: &[libc::sock_filter]) -> std::io::Result<()> {
    let check = |result: libc::c_int| {
        if result == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    };
    // SAFETY: plain system calls on this process; the filter outlives them.
    unsafe {
        check(libc::setpgid(0, 0))?;
        for (resource, value) in [
            (libc::RLIMIT_DATA, MEMORY),
            (libc::RLIMIT_FSIZE, FILE_SIZE),
            (libc::RLIMIT_CPU, CPU_SECONDS),
            (libc::RLIMIT_NOFILE, OPEN_FILES),
            (libc::RLIMIT_CORE, 0),
        ] {
            let limit = libc::rlimit {
                rlim_cur: value,
                rlim_max: value,
            };
            check(libc::setrlimit(resource, &limit))?;
        }
        check(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0))?;
        let program = libc::sock_fprog {
            len: filter.len() as u16,
            filter: filter.as_ptr() as *mut libc::sock_filter,
        };
        check(libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER,
            &program as *const libc::sock_fprog,
        ))
    }
}

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;

/// Where `struct seccomp_data` keeps the call's number, its architecture and
/// the low half of its first argument (little-endian).
const NR: u32 = 0;
const ARCH: u32 = 4;
const FIRST_ARGUMENT: u32 = 16;

/// The seccomp program: a foreign architecture is killed; `connect` and
/// io_uring are refused; `socket` is refused for every family but a local
/// one; everything else is allowed.
fn network_filter() -> Vec<libc::sock_filter> {
    let refuse = libc::SECCOMP_RET_ERRNO | libc::EACCES as u32;
    let mut program = vec![
        load(ARCH),
        jump_if_equal(AUDIT_ARCH, 1, 0),
        ret(libc::SECCOMP_RET_KILL_PROCESS),
        load(NR),
    ];
    // x32 calls are x86_64's numbers with this bit set.
    #[cfg(target_arch = "x86_64")]
    program.extend([jump_if_at_least(0x4000_0000, 0, 1), ret(refuse)]);
    for call in [
        libc::SYS_connect,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ] {
        program.extend([jump_if_equal(call as u32, 0, 1), ret(refuse)]);
    }
    program.extend([
        jump_if_equal(libc::SYS_socket as u32, 0, 3),
        load(FIRST_ARGUMENT),
        jump_if_equal(libc::AF_UNIX as u32, 1, 0),
        ret(refuse),
        ret(libc::SECCOMP_RET_ALLOW),
    ]);
    program
}

fn load(offset: u32) -> libc::sock_filter {
    statement((libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16, offset)
}

fn ret(value: u32) -> libc::sock_filter {
    statement((libc::BPF_RET | libc::BPF_K) as u16, value)
}

fn jump_if_equal(value: u32, if_true: u8, if_false: u8) -> libc::sock_filter {
    jump((libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16, value, if_true, if_false)
}

#[cfg(target_arch = "x86_64")]
fn jump_if_at_least(value: u32, if_true: u8, if_false: u8) -> libc::sock_filter {
    jump((libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K) as u16, value, if_true, if_false)
}

fn statement(code: u16, k: u32) -> libc::sock_filter {
    jump(code, k, 0, 0)
}

fn jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}
