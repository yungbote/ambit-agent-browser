//! Browser-session download observations, retained before CDP event broadcast.
//! Command waits share exact GUID state, including already-completed downloads.
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::watch;

const MAX_RETAINED_DOWNLOADS: usize = 256;
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DownloadStatus {
    InProgress,
    Completed,
    Canceled,
}
#[derive(Clone, Debug)]
pub(crate) struct Download {
    pub guid: String,
    pub frame_id: String,
    page_id: String,
    pub suggested_filename: String,
    pub status: DownloadStatus,
    pub received_bytes: u64,
    sequence: u64,
    reported: bool,
}
#[derive(Default)]
struct State {
    sequence: u64,
    records: VecDeque<Download>,
    frames: HashMap<String, (String, Option<String>)>,
    failure: Option<String>,
    evicted_through: u64,
}
pub(crate) struct Downloads {
    state: Mutex<State>,
    changed: watch::Sender<u64>,
}
impl Default for Downloads {
    fn default() -> Self {
        let (changed, _) = watch::channel(0);
        Self {
            state: Mutex::new(State::default()),
            changed,
        }
    }
}
impl Downloads {
    pub fn seed_frames(&self, tree: &Value) {
        fn seed(
            tree: &Value,
            root: &str,
            parent: Option<&str>,
            frames: &mut HashMap<String, (String, Option<String>)>,
        ) {
            if let Some(id) = tree["frame"]["id"].as_str() {
                frames.insert(id.to_owned(), (root.to_owned(), parent.map(str::to_owned)));
                if let Some(children) = tree["childFrames"].as_array() {
                    for child in children {
                        seed(child, root, Some(id), frames);
                    }
                }
            }
        }
        if let Some(root) = tree["frame"]["id"].as_str() {
            seed(
                tree,
                root,
                None,
                &mut self.state.lock().unwrap_or_else(|e| e.into_inner()).frames,
            );
        }
    }

    fn observe_frame(&self, method: &str, params: &Value) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let frame = if method == "Page.frameNavigated" {
            &params["frame"]
        } else {
            params
        };
        let Some(id) = frame[if method == "Page.frameNavigated" {
            "id"
        } else {
            "frameId"
        }]
        .as_str() else {
            return;
        };
        if method == "Page.frameDetached" {
            if params["reason"] == "swap" {
                return;
            }
            let mut removed = HashSet::from([id.to_owned()]);
            loop {
                let children: Vec<_> = state
                    .frames
                    .iter()
                    .filter(|(_, (_, parent))| {
                        parent
                            .as_ref()
                            .is_some_and(|parent| removed.contains(parent))
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                let before = removed.len();
                removed.extend(children);
                if removed.len() == before {
                    break;
                }
            }
            state.frames.retain(|id, _| !removed.contains(id));
        } else if let Some(parent) = frame[if method == "Page.frameNavigated" {
            "parentId"
        } else {
            "parentFrameId"
        }]
        .as_str()
        {
            if let Some((root, _)) = state.frames.get(parent).cloned() {
                state
                    .frames
                    .insert(id.to_owned(), (root, Some(parent.to_owned())));
            }
        } else {
            // An OOPIF's renderer reports its root without a parent. Preserve
            // the outer page association already observed at frameAttached.
            state
                .frames
                .entry(id.to_owned())
                .or_insert((id.to_owned(), None));
        }
    }

    pub fn observe(&self, method: &str, params: &Value) {
        if matches!(
            method,
            "Page.frameAttached" | "Page.frameNavigated" | "Page.frameDetached"
        ) {
            self.observe_frame(method, params);
            return;
        }
        if !matches!(
            method,
            "Browser.downloadWillBegin" | "Browser.downloadProgress"
        ) {
            return;
        }
        let Some(guid) = params["guid"]
            .as_str()
            .filter(|id| uuid::Uuid::parse_str(id).is_ok())
        else {
            return;
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if method == "Browser.downloadWillBegin" {
            if state.records.iter().any(|item| item.guid == guid) {
                return;
            }
            let Some(frame) = params["frameId"]
                .as_str()
                .filter(|id| !id.is_empty() && id.len() <= 256)
            else {
                return;
            };
            let Some(name) = params["suggestedFilename"]
                .as_str()
                .filter(|name| name.len() <= 4096)
            else {
                return;
            };
            // Keep bounded recent history, preferring reported records. A
            // waiter whose exact history was evicted fails without reassignment.
            while state.records.len() >= MAX_RETAINED_DOWNLOADS {
                let index = state
                    .records
                    .iter()
                    .position(|item| item.reported)
                    .or_else(|| {
                        state
                            .records
                            .iter()
                            .position(|item| item.status != DownloadStatus::InProgress)
                    });
                if let Some(index) = index {
                    let removed = state.records.remove(index).unwrap();
                    state.evicted_through = state.evicted_through.max(removed.sequence);
                } else {
                    state.failure = Some(
                        "Too many active downloads to observe their completion reliably".into(),
                    );
                    self.changed.send_modify(|revision| *revision += 1);
                    return;
                }
            }
            state.sequence += 1;
            let sequence = state.sequence;
            let page_id = state
                .frames
                .get(frame)
                .map(|(root, _)| root.clone())
                .unwrap_or_else(|| frame.to_owned());
            state.records.push_back(Download {
                guid: guid.into(),
                frame_id: frame.into(),
                page_id,
                suggested_filename: name.into(),
                status: DownloadStatus::InProgress,
                received_bytes: 0,
                sequence,
                reported: false,
            });
        } else if let Some(item) = state.records.iter_mut().find(|item| item.guid == guid) {
            if item.status != DownloadStatus::InProgress {
                return;
            }
            let Some(bytes) = params["receivedBytes"].as_f64().filter(|n| {
                n.is_finite() && *n >= 0.0 && *n <= 9_007_199_254_740_991.0 && n.fract() == 0.0
            }) else {
                return;
            };
            // Chromium may reset receivedBytes when an interrupted transfer is
            // canceled. Its terminal disposition is independent of progress.
            if bytes < item.received_bytes as f64 && params["state"] != "canceled" {
                return;
            }
            item.received_bytes = item.received_bytes.max(bytes as u64);
            match params["state"].as_str() {
                Some("completed") => item.status = DownloadStatus::Completed,
                Some("canceled") => item.status = DownloadStatus::Canceled,
                Some("inProgress") => {}
                _ => return,
            }
        } else {
            return;
        }
        self.changed.send_modify(|revision| *revision += 1);
    }
    pub fn closed(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).failure =
            Some("Browser download connection closed".into());
        self.changed.send_modify(|revision| *revision += 1);
    }
    pub fn cursor(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sequence
    }
    /// A read-only Product snapshot shares the observer without consuming CLI waits.
    pub(crate) fn completed(
        &self,
        frames: Option<&HashSet<String>>,
        after: u64,
    ) -> Result<Vec<Download>, String> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(error) = &state.failure {
            return Err(error.clone());
        }
        Ok(state
            .records
            .iter()
            .filter(|item| {
                item.status == DownloadStatus::Completed
                    && item.sequence > after
                    && frames.is_none_or(|frames| frames.contains(&item.page_id))
            })
            .cloned()
            .collect())
    }

    pub fn reported(&self, guid: &str) {
        if let Some(item) = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .records
            .iter_mut()
            .find(|item| item.guid == guid)
        {
            item.reported = true;
        }
    }
    /// Select once by page/begin order, then await only that exact GUID.
    pub async fn wait(
        &self,
        frames: &HashSet<String>,
        after: Option<u64>,
        timeout: Duration,
    ) -> Result<Download, String> {
        let mut changes = self.changed.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        let mut selected: Option<String> = None;
        loop {
            {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(error) = &state.failure {
                    return Err(error.clone());
                }
                if selected.is_none() && after.is_some_and(|cursor| cursor < state.evicted_through)
                {
                    return Err("The requested download observation is no longer retained".into());
                }
                if selected
                    .as_ref()
                    .is_some_and(|guid| !state.records.iter().any(|item| &item.guid == guid))
                {
                    return Err("The selected download observation is no longer retained".into());
                }
                if selected.is_none() {
                    selected = state
                        .records
                        .iter()
                        .find(|item| {
                            !item.reported
                                && frames.contains(&item.page_id)
                                && after.is_none_or(|cursor| item.sequence > cursor)
                        })
                        .map(|item| item.guid.clone());
                }
                if let Some(item) = state
                    .records
                    .iter_mut()
                    .find(|item| Some(&item.guid) == selected.as_ref())
                {
                    match item.status {
                        DownloadStatus::Completed => return Ok(item.clone()),
                        DownloadStatus::Canceled => {
                            item.reported = true;
                            return Err(format!(
                                "Download {} was canceled ({})",
                                item.guid, item.suggested_filename
                            ));
                        }
                        DownloadStatus::InProgress => {}
                    }
                }
            }
            tokio::time::timeout_at(deadline, changes.changed()).await.map_err(|_| match selected.as_deref() {
                Some(guid) => format!("Timeout waiting for download {guid} to complete; the download may still be in progress"),
                None => "Timeout waiting for a download to start".into(),
            })?.map_err(|_| "Browser download observer closed".to_string())?;
        }
    }
}
/// Shared storage for admitted browser contexts; implicit storage ends with its browser.
pub(crate) struct DownloadDirectory {
    pub path: PathBuf,
    pub contexts: HashSet<Option<String>>,
    temporary: bool,
}
impl DownloadDirectory {
    /// Browser history is an observation, not a claim that a mutable source
    /// still exists. Capture/read owners reprove bytes when they are consumed.
    pub fn observation(&self, download: &Download) -> Value {
        json!({ "id":download.guid, "guid":download.guid, "frameId":download.frame_id,
          "path":self.path.join(&download.guid), "suggestedFilename":download.suggested_filename,
          "status":"completed", "receivedBytes":download.received_bytes })
    }

    pub fn new(path: Option<&str>) -> Result<Self, String> {
        let temporary = path.is_none();
        let path = path.map(PathBuf::from).unwrap_or_else(|| {
            std::env::temp_dir().join(format!("agent-browser-downloads-{}", uuid::Uuid::new_v4()))
        });
        if temporary {
            std::fs::create_dir(&path)
        } else {
            std::fs::create_dir_all(&path)
        }
        .map_err(|e| format!("Cannot create browser download directory: {e}"))?;
        let path = path
            .canonicalize()
            .map_err(|e| format!("Cannot resolve browser download directory: {e}"))?;
        Ok(Self {
            path,
            temporary,
            contexts: HashSet::new(),
        })
    }
    pub fn finish(&self, download: &Download, destination: Option<&str>) -> Result<Value, String> {
        let source = self.path.join(&download.guid);
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let mut file = options.open(&source).map_err(|error| {
            format!(
                "Completed download {} is unavailable: {error}",
                download.guid
            )
        })?;
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_file() || metadata.len() != download.received_bytes {
            return Err(format!(
                "Completed download {} does not match its observed file size",
                download.guid
            ));
        }
        let path = if let Some(destination) = destination {
            save_download(&mut file, &metadata, &source, Path::new(destination))?
        } else {
            source
        };
        Ok(
            json!({ "guid":download.guid, "frameId":download.frame_id, "path":path, "suggestedFilename":download.suggested_filename, "status":"completed", "receivedBytes":metadata.len() }),
        )
    }
}
impl Drop for DownloadDirectory {
    fn drop(&mut self) {
        if self.temporary {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}
fn save_download(
    input: &mut std::fs::File,
    expected: &std::fs::Metadata,
    source: &Path,
    requested: &Path,
) -> Result<PathBuf, String> {
    let requested = if requested.is_absolute() {
        requested.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(requested)
    };
    let parent = requested
        .parent()
        .ok_or("Download destination has no parent")?;
    let filename = requested
        .file_name()
        .ok_or("Download destination has no file name")?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("Cannot create download destination: {e}"))?;
    let parent = parent.canonicalize().map_err(|e| e.to_string())?;
    let destination = parent.join(filename);
    if destination == source {
        return Ok(destination);
    }
    // Destination-local scratch supports different filesystems. Existing files
    // never prove completion; preserve them unless the new copy is complete.
    let scratch = parent.join(format!(
        ".agent-browser-download-{}.part",
        uuid::Uuid::new_v4()
    ));
    let result = (|| {
        use std::io::Write;
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&scratch)?;
        let copied = std::io::copy(input, &mut output)?;
        let current = input.metadata()?;
        if copied != expected.len()
            || current.len() != expected.len()
            || current.modified()? != expected.modified()?
        {
            return Err(std::io::Error::other(
                "Completed download changed while being saved",
            ));
        }
        output.flush()?;
        output.sync_all()?;
        std::fs::rename(&scratch, &destination)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&scratch);
    }
    result.map_err(|e| format!("Cannot save completed download: {e}"))?;
    Ok(destination)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn removed_or_process_swapped_iframe_retains_its_download_page() {
        let downloads = Downloads::default();
        downloads.seed_frames(&json!({"frame":{"id":"root"}}));
        downloads.observe(
            "Page.frameAttached",
            &json!({"frameId":"child","parentFrameId":"root"}),
        );
        downloads.observe(
            "Page.frameDetached",
            &json!({"frameId":"child","reason":"swap"}),
        );
        downloads.observe("Page.frameNavigated", &json!({"frame":{"id":"child"}}));
        let guid = uuid::Uuid::new_v4().to_string();
        begin(&downloads, &guid, "child");
        downloads.observe(
            "Page.frameDetached",
            &json!({"frameId":"child","reason":"remove"}),
        );
        progress(&downloads, &guid, "completed", 0);
        assert_eq!(
            downloads
                .wait(
                    &HashSet::from(["root".into()]),
                    None,
                    Duration::from_millis(1)
                )
                .await
                .unwrap()
                .guid,
            guid
        );
        assert!(!downloads.state.lock().unwrap().frames.contains_key("child"));
    }

    #[tokio::test]
    async fn bounded_terminal_history_does_not_poison_future_downloads() {
        let downloads = Downloads::default();
        for _ in 0..MAX_RETAINED_DOWNLOADS + 2 {
            let guid = uuid::Uuid::new_v4().to_string();
            begin(&downloads, &guid, "page");
            progress(&downloads, &guid, "completed", 0);
        }
        assert_eq!(
            downloads.state.lock().unwrap().records.len(),
            MAX_RETAINED_DOWNLOADS
        );
        let frames = HashSet::from(["page".into()]);
        assert!(downloads
            .wait(&frames, Some(0), Duration::from_millis(1))
            .await
            .unwrap_err()
            .contains("no longer retained"));
        let cursor = downloads.cursor();
        let guid = uuid::Uuid::new_v4().to_string();
        begin(&downloads, &guid, "page");
        progress(&downloads, &guid, "completed", 0);
        assert_eq!(
            downloads
                .wait(&frames, Some(cursor), Duration::from_millis(1))
                .await
                .unwrap()
                .guid,
            guid
        );
    }

    #[cfg(unix)]
    #[test]
    fn completed_download_does_not_follow_a_replaced_symlink() {
        let root = tempfile::tempdir().unwrap();
        let directory = DownloadDirectory::new(root.path().to_str()).unwrap();
        let target = root.path().join("unrelated.txt");
        std::fs::write(&target, b"secret").unwrap();
        let guid = uuid::Uuid::new_v4().to_string();
        std::os::unix::fs::symlink(&target, root.path().join(&guid)).unwrap();
        let item = Download {
            guid,
            frame_id: "page".into(),
            page_id: "page".into(),
            suggested_filename: "test.bin".into(),
            status: DownloadStatus::Completed,
            received_bytes: 6,
            sequence: 1,
            reported: false,
        };
        assert!(directory.finish(&item, None).is_err());
    }

    #[test]
    fn source_changed_before_copy_preserves_existing_destination() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        std::fs::write(&source, b"original").unwrap();
        std::fs::write(&destination, b"prior").unwrap();
        let mut file = std::fs::File::open(&source).unwrap();
        let expected = file.metadata().unwrap();
        std::fs::write(&source, b"changed length").unwrap();
        assert!(save_download(&mut file, &expected, &source, &destination).is_err());
        assert_eq!(std::fs::read(destination).unwrap(), b"prior");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
    }
    fn begin(d: &Downloads, guid: &str, frame: &str) {
        d.observe(
            "Browser.downloadWillBegin",
            &json!({"guid":guid,"frameId":frame,"suggestedFilename":"test.bin"}),
        );
    }
    fn progress(d: &Downloads, guid: &str, state: &str, bytes: u64) {
        d.observe(
            "Browser.downloadProgress",
            &json!({"guid":guid,"state":state,"receivedBytes":bytes}),
        );
    }
    #[tokio::test]
    async fn exact_guid_ignores_another_completion_or_cancellation() {
        let d = Downloads::default();
        let selected = uuid::Uuid::new_v4().to_string();
        let other = uuid::Uuid::new_v4().to_string();
        begin(&d, &selected, "page");
        begin(&d, &other, "page");
        progress(&d, &other, "completed", 17);
        let frames = HashSet::from(["page".to_string()]);
        assert!(d
            .wait(&frames, None, Duration::from_millis(1))
            .await
            .unwrap_err()
            .contains(&selected));
        progress(&d, &selected, "inProgress", 5);
        progress(&d, &selected, "canceled", 0);
        assert!(d
            .wait(&frames, None, Duration::from_millis(1))
            .await
            .unwrap_err()
            .contains("canceled"));
        assert_eq!(
            d.wait(&frames, None, Duration::from_millis(1))
                .await
                .unwrap()
                .guid,
            other
        );
    }
    #[tokio::test]
    async fn cursor_and_frame_scope_exclude_old_or_unrelated_downloads() {
        let d = Downloads::default();
        let old = uuid::Uuid::new_v4().to_string();
        begin(&d, &old, "page");
        let cursor = d.cursor();
        progress(&d, &old, "completed", 0);
        let other = uuid::Uuid::new_v4().to_string();
        begin(&d, &other, "other");
        progress(&d, &other, "completed", 0);
        let selected = uuid::Uuid::new_v4().to_string();
        begin(&d, &selected, "page");
        progress(&d, &selected, "completed", 0);
        assert_eq!(
            d.wait(
                &HashSet::from(["page".into()]),
                Some(cursor),
                Duration::from_millis(1)
            )
            .await
            .unwrap()
            .guid,
            selected
        );
    }
    #[tokio::test]
    async fn orphan_progress_and_regressive_terminal_events_never_invent_a_download() {
        let d = Downloads::default();
        let guid = uuid::Uuid::new_v4().to_string();
        progress(&d, &guid, "completed", 4);
        assert_eq!(d.cursor(), 0);
        begin(&d, &guid, "page");
        progress(&d, &guid, "completed", 4);
        progress(&d, &guid, "canceled", 4);
        assert_eq!(
            d.wait(
                &HashSet::from(["page".into()]),
                None,
                Duration::from_millis(1)
            )
            .await
            .unwrap()
            .status,
            DownloadStatus::Completed
        );
    }
    #[tokio::test]
    async fn connection_close_is_observable_instead_of_wait_timeout() {
        let d = Downloads::default();
        d.closed();
        assert!(d
            .wait(
                &HashSet::from(["page".into()]),
                None,
                Duration::from_secs(30)
            )
            .await
            .unwrap_err()
            .contains("connection closed"));
    }
    #[test]
    fn existing_destination_is_not_evidence_of_download_completion() {
        let root = tempfile::tempdir().unwrap();
        let dir = DownloadDirectory::new(root.path().to_str()).unwrap();
        let dest = root.path().join("old.txt");
        std::fs::write(&dest, b"old").unwrap();
        let item = Download {
            guid: uuid::Uuid::new_v4().to_string(),
            frame_id: "page".into(),
            page_id: "page".into(),
            suggested_filename: "test.bin".into(),
            status: DownloadStatus::Completed,
            received_bytes: 3,
            sequence: 1,
            reported: false,
        };
        assert!(dir.finish(&item, dest.to_str()).is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), b"old");
    }
}
