//! Where a picture's time went, for attributing an input's time to the
//! viewer. Behind the daemon's diagnostics switch (`AGENT_BROWSER_DEBUG`),
//! each picture carries the media-clock time (`monotonic_us`) at which it
//! passed each producer stage, and each viewer that writes it logs one line
//! (`[video] stages {…}`; a detached daemon's stderr is its session log).
//! Without the switch nothing is recorded or logged.
//!
//! These are the sandbox's clock only. A viewer's own clock is never mixed
//! in: its arrival times are its own measurements.

use serde_json::json;

/// One picture's stages, in media-clock microseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Stages {
    /// The newest input the controller's lease had applied when the capture
    /// read the screen, and when the driver recorded it as applied: the
    /// display helper had injected it into the browser by then.
    pub input: Option<(u64, u64)>,
    /// The capture asked the display helper for a picture.
    pub requested: u64,
    /// The helper read the screen (the picture's `ts`). It waited
    /// `waited_us` of that for damage; a capture asked before an input and
    /// held for damage reads it about one millisecond after the browser's
    /// first damage (the helper's settle).
    pub read: u64,
    pub waited_us: u64,
    /// The helper's reply, with the changed rows in shared memory, was in
    /// hand.
    pub received: u64,
    /// Rows the picture changed; none for a capture that saw no damage.
    pub rows: u32,
    /// The picture was converted for its encoding and handed to its encoder.
    pub converted: u64,
    /// The encoder began and finished this unit.
    pub encode_started: u64,
    pub encoded: u64,
}

/// Whether pictures record their stages: the diagnostics switch, read at
/// each capture.
pub(crate) fn enabled() -> bool {
    std::env::var_os("AGENT_BROWSER_DEBUG").is_some()
}

impl Stages {
    /// A viewer finished writing the unit at `written`: one diagnostics
    /// line with every stage. `ts` and `quality` name the unit.
    pub(crate) fn written(&self, ts: u64, quality: &str, key: bool, bytes: usize, written: u64) {
        let line = json!({
            "ts": ts, "quality": quality, "key": key, "bytes": bytes,
            "inputSeq": self.input.map(|(sequence, _)| sequence),
            "applied": self.input.map(|(_, at)| at),
            "requested": self.requested, "read": self.read, "waitedUs": self.waited_us,
            "received": self.received, "rows": self.rows, "converted": self.converted,
            "encodeStarted": self.encode_started, "encoded": self.encoded, "written": written,
        });
        #[cfg(test)]
        logged::push(line.clone());
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), "[video] stages {line}");
    }
}

/// The lines logged, for the proofs that run the producer in process.
#[cfg(test)]
pub(crate) mod logged {
    use std::sync::Mutex;

    static LINES: Mutex<Vec<serde_json::Value>> = Mutex::new(Vec::new());

    pub(super) fn push(line: serde_json::Value) {
        LINES
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(line);
    }

    /// Every line logged since the last call.
    pub(crate) fn take() -> Vec<serde_json::Value> {
        std::mem::take(&mut *LINES.lock().unwrap_or_else(|error| error.into_inner()))
    }
}
