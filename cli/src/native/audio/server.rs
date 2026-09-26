use super::{
    encoder::Encoder, pulse, AudioCodec, AudioError, AudioObservation, AudioQueue, AudioSource,
    AudioSubscription, FRAME_BYTES,
};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Condvar, Mutex, Weak,
};
use std::time::{Duration, Instant};

struct Subscriber {
    encoder: Encoder,
    queue: Arc<AudioQueue>,
}
struct State {
    subscribers: HashMap<uuid::Uuid, Subscriber>,
    retired: bool,
    failure: Option<AudioError>,
    capture_generation: u64,
}
impl State {
    fn end(&mut self, reason: AudioError) {
        if reason == AudioError::Unavailable && !self.retired {
            self.failure = Some(reason);
        }
        self.capture_generation = self.capture_generation.wrapping_add(1);
        for subscriber in self.subscribers.drain().map(|(_, v)| v) {
            subscriber.queue.end(reason);
        }
    }
}
pub(super) struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    startup_us: u64,
    child: Weak<Mutex<Child>>,
}
impl Shared {
    pub(super) fn observe(&self) -> AudioObservation {
        if !self.child.upgrade().is_some_and(|child| {
            matches!(
                child.lock().unwrap_or_else(|e| e.into_inner()).try_wait(),
                Ok(None)
            )
        }) {
            self.end(AudioError::Unavailable);
        }
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        AudioObservation {
            compiled: true,
            ready: !state.retired && state.failure.is_none(),
            subscribers: state.subscribers.len(),
            startup_us: self.startup_us,
            failure: state
                .retired
                .then_some(AudioError::Retired)
                .or(state.failure),
        }
    }
    pub(super) fn subscribe(
        self: &Arc<Self>,
        codec: AudioCodec,
    ) -> Result<AudioSubscription, AudioError> {
        let encoder = Encoder::new(codec)?;
        let queue = Arc::new(AudioQueue::default());
        let id = uuid::Uuid::new_v4();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.retired {
            return Err(AudioError::Retired);
        }
        if let Some(reason) = state.failure {
            return Err(reason);
        }
        if state.subscribers.is_empty() {
            // A reconnect cannot consume a previous capture's buffered samples.
            state.capture_generation = state.capture_generation.wrapping_add(1);
        }
        state.subscribers.insert(
            id,
            Subscriber {
                encoder,
                queue: queue.clone(),
            },
        );
        drop(state);
        self.wake.notify_one();
        Ok(AudioSubscription {
            queue,
            source: Arc::downgrade(self),
            id,
        })
    }
    pub(super) fn remove(&self, id: uuid::Uuid) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .subscribers
            .remove(&id);
        self.wake.notify_one();
    }
    fn stop_capture(&self, generation: u64) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.retired || state.subscribers.is_empty() || state.capture_generation != generation
    }
    fn end(&self, reason: AudioError) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .end(reason);
    }
    fn end_capture(&self, generation: u64, reason: AudioError) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.retired && state.capture_generation == generation {
            state.end(reason);
        }
    }
    fn publish(&self, generation: u64, bytes: &[u8], ts: u64) {
        let samples: Vec<i16> = bytes
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.capture_generation != generation || state.retired {
            return;
        }
        state.subscribers.retain(
            |_, subscriber| match subscriber.encoder.encode(&samples, ts) {
                Ok(packet) => subscriber.queue.push(packet),
                Err(reason) => {
                    subscriber.queue.end(reason);
                    false
                }
            },
        );
    }
}

struct Server {
    child: Arc<Mutex<Child>>,
    directory: PathBuf,
    source: AudioSource,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.source
            .shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retired = true;
        self.source.shared.end(AudioError::Retired);
        self.source.shared.wake.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        let _ = child.kill();
        let _ = child.wait();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

/// Retained across the actual automation/sign-in Chrome relaunch, like the display.
#[derive(Clone)]
pub(crate) struct RetainedAudio {
    server: Arc<Server>,
}
impl RetainedAudio {
    pub(crate) fn start(canceled: &AtomicBool) -> Result<Self, AudioError> {
        Self::start_with(Path::new("/usr/bin/pulseaudio"), canceled)
    }
    fn start_with(executable: &Path, canceled: &AtomicBool) -> Result<Self, AudioError> {
        let started = Instant::now();
        if canceled.load(Ordering::Relaxed) {
            return Err(AudioError::Retired);
        }
        // Internal names are fixed ASCII and not influenced by a page or session id.
        let directory = Path::new("/tmp").join(format!("ambit-audio-{}", uuid::Uuid::new_v4()));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .map_err(|_| AudioError::Unavailable)?;
        let setup = (|| {
            let mut cookie = [0; 256];
            getrandom::getrandom(&mut cookie).map_err(|_| AudioError::Unavailable)?;
            write_private(&directory.join("cookie"), &cookie)?;
            write_private(&directory.join("client.conf"), format!("autospawn = no\ndefault-server = unix:{}/native\ndefault-sink = ambit\ncookie-file = {}/cookie\nenable-shm = no\n", directory.display(), directory.display()).as_bytes())?;
            let startup = format!("load-module module-null-sink sink_name=ambit rate=48000 channels=2 format=s16le norewinds=1\nset-default-sink ambit\nload-module module-native-protocol-unix socket={}/native auth-cookie={}/cookie auth-cookie-enabled=1 auth-anonymous=0\nload-module module-cli exit_on_eof=1\n", directory.display(), directory.display());
            write_private(&directory.join("default.pa"), startup.as_bytes())?;
            let mut command = Command::new(executable);
            command
                .args([
                    "--daemonize=no",
                    "--exit-idle-time=-1",
                    "--use-pid-file=no",
                    "--disable-shm=yes",
                    "--log-target=stderr",
                    "--log-level=error",
                    "-n",
                    "-F",
                ])
                .arg(directory.join("default.pa"))
                .env("PULSE_RUNTIME_PATH", &directory)
                .env("PULSE_STATE_PATH", &directory)
                .env("PULSE_CONFIG_PATH", &directory)
                .env_remove("PULSE_SERVER")
                .env_remove("PULSE_CLIENTCONFIG")
                .env_remove("PULSE_COOKIE")
                .env_remove("PULSE_SINK")
                .env_remove("PULSE_SOURCE")
                // Pulse's built-in EOF lifetime follows this owner even if the daemon dies.
                // No bytes are sent on this pipe and no child inherits its write end.
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            // module-cli must use its private stdin, never a caller's controlling terminal.
            // SAFETY: setsid is async-signal-safe; no allocation/locks run after fork.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            command.spawn().map_err(|_| AudioError::Unavailable)
        })();
        let mut child = match setup {
            Ok(child) => child,
            Err(reason) => {
                let _ = fs::remove_dir_all(&directory);
                return Err(reason);
            }
        };
        let ready = (|| {
            while !directory.join("native").exists() {
                if canceled.load(Ordering::Relaxed)
                    || started.elapsed() >= Duration::from_secs(1)
                    || child
                        .try_wait()
                        .map_err(|_| AudioError::Unavailable)?
                        .is_some()
                {
                    return Err(AudioError::Unavailable);
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            pulse::Connection::connect(
                &directory.join("native"),
                &directory.join("cookie"),
                || canceled.load(Ordering::Relaxed) || started.elapsed() >= Duration::from_secs(1),
            )?;
            Ok(())
        })();
        if let Err(reason) = ready {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_dir_all(&directory);
            return Err(reason);
        }
        let child = Arc::new(Mutex::new(child));
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                subscribers: HashMap::new(),
                retired: false,
                failure: None,
                capture_generation: 0,
            }),
            wake: Condvar::new(),
            startup_us: started.elapsed().as_micros() as u64,
            child: Arc::downgrade(&child),
        });
        let socket = directory.join("native");
        let cookie = directory.join("cookie");
        let state = shared.clone();
        let worker = match std::thread::Builder::new()
            .name("browser-audio".into())
            .spawn(move || capture_worker(state, socket, cookie))
        {
            Ok(worker) => worker,
            Err(_) => {
                let mut child = child.lock().unwrap_or_else(|e| e.into_inner());
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_dir_all(&directory);
                return Err(AudioError::Unavailable);
            }
        };
        Ok(Self {
            server: Arc::new(Server {
                child,
                directory,
                source: AudioSource { shared },
                worker: Some(worker),
            }),
        })
    }
    pub(crate) fn source(&self) -> AudioSource {
        self.server.source.clone()
    }
    /// The device survives a Chrome relaunch, but pre-transition audio never does.
    pub(crate) fn discontinue(&self) {
        self.server.source.shared.end(AudioError::Discontinuity);
    }
    pub(crate) fn apply_environment(&self, command: &mut Command) {
        command
            .env(
                "PULSE_SERVER",
                format!("unix:{}/native", self.server.directory.display()),
            )
            .env("PULSE_COOKIE", self.server.directory.join("cookie"))
            .env(
                "PULSE_CLIENTCONFIG",
                self.server.directory.join("client.conf"),
            )
            .env("PULSE_SINK", "ambit")
            .env_remove("PULSE_SOURCE");
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), AudioError> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut f| f.write_all(bytes))
        .map_err(|_| AudioError::Unavailable)
}

fn capture_worker(shared: Arc<Shared>, socket: PathBuf, cookie: PathBuf) {
    loop {
        let generation = {
            let state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            let state = shared
                .wake
                .wait_while(state, |s| !s.retired && s.subscribers.is_empty())
                .unwrap_or_else(|e| e.into_inner());
            if state.retired {
                break;
            }
            state.capture_generation
        };
        let result = capture_active(&shared, generation, &socket, &cookie);
        if let Err(reason) = result {
            shared.end_capture(generation, reason);
        }
    }
}

fn capture_active(
    shared: &Shared,
    generation: u64,
    socket: &Path,
    cookie: &Path,
) -> Result<(), AudioError> {
    let mut connection =
        pulse::Connection::connect(socket, cookie, || shared.stop_capture(generation))?;
    let mut stream = connection.record(|| shared.stop_capture(generation))?;
    let mut pending = Vec::with_capacity(FRAME_BYTES);
    let mut first_ts = None;
    while !shared.stop_capture(generation) {
        connection.tick()?;
        match pulse::read(&mut stream)? {
            pulse::Capture::Empty => continue,
            pulse::Capture::Gap if first_ts.is_none() => continue,
            pulse::Capture::Gap => return Err(AudioError::Discontinuity),
            pulse::Capture::Samples { bytes, ts } => {
                if first_ts.is_none() {
                    first_ts = Some(ts);
                }
                // A timing jump after queue overflow/restart is never stretched into old audio.
                let expected = first_ts.unwrap() + pending.len() as u64 * 1_000_000 / (48_000 * 4);
                if ts.abs_diff(expected) > 40_000 {
                    return Err(AudioError::Discontinuity);
                }
                pending.extend_from_slice(&bytes);
                while pending.len() >= FRAME_BYTES {
                    shared.publish(generation, &pending[..FRAME_BYTES], first_ts.unwrap());
                    pending.drain(..FRAME_BYTES);
                    first_ts = first_ts.map(|value| value + 10_000);
                }
            }
        }
    }
    let _ = stream.disconnect();
    Ok(())
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
