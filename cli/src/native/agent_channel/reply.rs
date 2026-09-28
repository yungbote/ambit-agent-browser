//! Reply lines and their bounds (agent-channel contract §5 "Bounds" and §6
//! "Large results"). A reply is one line of at most `MAX_REPLY_BYTES`, the
//! newline included, because the toolbox relays each line as one frame and
//! refuses a longer one. Nothing is truncated to fit: the largest step
//! results go to the Action's directory, and a retained-result reference the
//! host reads with `workspace_file` stands in their place.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::frame::{ChannelId, FrameId};

/// The longest reply line, its newline included.
pub(crate) const MAX_REPLY_BYTES: usize = 4 << 20;
/// The longest frame the toolbox forwards, and the daemon's read-ahead.
pub(crate) const MAX_REQUEST_BYTES: usize = 2 << 20;
/// A step result longer than this is kept by the ledger as a reference.
pub(crate) const RETAINED_ABOVE: usize = 64 << 10;

/// Where one step result goes when it leaves a line:
/// `<directory>/steps/<channel>-<id>-<step>.json`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Retained {
    pub(crate) directory: PathBuf,
    pub(crate) channel: ChannelId,
    pub(crate) id: FrameId,
    pub(crate) step: usize,
}

impl Retained {
    /// Writes `result`'s bytes and answers the reference that stands in for
    /// it. A failed write leaves the reference without `path`: the result is
    /// lost to the host, and the step's outcome stands.
    pub(crate) fn write(&self, result: &Value) -> Value {
        let bytes = serde_json::to_vec(result).unwrap_or_default();
        let mut reference = json!({
            "complete": false,
            "sizeBytes": bytes.len(),
            "contentDigest": format!("sha256:{:x}", Sha256::digest(&bytes)),
            "readWith": "workspace_file",
            "format": "json",
        });
        let name = format!("{}-{}-{}.json", self.channel, self.id, self.step);
        if let Ok(path) = write_private(&self.directory.join("steps"), &name, &bytes) {
            reference["path"] = json!(path);
        }
        reference
    }
}

/// Whether `value` is a retained-result reference rather than a result.
pub(crate) fn is_reference(value: &Value) -> bool {
    value["complete"] == false && value.get("contentDigest").is_some()
}

/// Creates `path` for this daemon's user alone (mode 0700) when it is
/// missing, and answers it only when its canonical form is the path asked
/// for: a symlinked component is refused.
pub(crate) fn private_directory(path: &Path) -> Result<PathBuf, ()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|_| ())?;
    let canonical = path.canonicalize().map_err(|_| ())?;
    (canonical == path && canonical.is_dir())
        .then_some(canonical)
        .ok_or(())
}

/// Writes a new file `name` in the private directory `directory` (mode
/// 0600, never following a link, never replacing a file), as captures are
/// written.
fn write_private(directory: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf, ()> {
    let directory = private_directory(directory)?;
    let path = directory.join(name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&path).map_err(|_| ())?;
    file.write_all(bytes).map_err(|_| ())?;
    file.sync_data().map_err(|_| ())?;
    Ok(path)
}

/// One step result a line carries, and where it goes if it must leave.
pub(crate) struct Slot {
    /// A JSON pointer to the result in the reply.
    pub(crate) pointer: String,
    pub(crate) retained: Retained,
    /// The reference of a result already written to its file.
    pub(crate) reference: Option<Value>,
}

/// A result written while fitting a line: the ledger keeps its reference.
pub(crate) struct Written {
    pub(crate) retained: Retained,
    pub(crate) reference: Value,
}

/// Fits `reply` within `limit` bytes as one line: while it is longer, the
/// largest inline result among `slots` is written away (unless its file
/// already holds it) and its reference put in its place. Serialization is
/// compact, so replacing a value changes the line's length by exactly the
/// difference of their lengths. Only results written here are returned.
pub(crate) fn fit(reply: &mut Value, slots: Vec<Slot>, limit: usize) -> Vec<Written> {
    let mut length = line_length(reply);
    let mut sized: Vec<(usize, Slot)> = slots
        .into_iter()
        .filter_map(|slot| {
            let result = reply.pointer(&slot.pointer)?;
            (!is_reference(result)).then(|| (result.to_string().len(), slot))
        })
        .collect();
    sized.sort_by(|left, right| right.0.cmp(&left.0));
    let mut written = Vec::new();
    for (size, slot) in sized {
        if length <= limit {
            break;
        }
        let Some(result) = reply.pointer_mut(&slot.pointer) else {
            continue;
        };
        let (reference, fresh) = match slot.reference {
            Some(reference) => (reference, false),
            None => (slot.retained.write(result), true),
        };
        length = length - size + reference.to_string().len();
        *result = reference.clone();
        if fresh {
            written.push(Written {
                retained: slot.retained,
                reference,
            });
        }
    }
    written
}

/// The bytes `value` takes as one reply line, its newline included.
pub(crate) fn line_length(value: &Value) -> usize {
    value.to_string().len() + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel() -> ChannelId {
        ChannelId::parse("5b0c0b1e-6f0a-4c1c-9d59-0e3f2b7a9c11").unwrap()
    }

    fn result(text: &str) -> Value {
        json!({ "isError": false, "content": [{ "type": "text", "text": text }],
            "structuredContent": { "response": { "success": true, "data": { "text": text } } } })
    }

    fn retained(directory: &Path, step: usize) -> Retained {
        Retained {
            directory: directory.to_path_buf(),
            channel: channel(),
            id: 7,
            step,
        }
    }

    #[test]
    fn a_retained_result_is_written_once_privately_with_its_digest() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path().canonicalize().unwrap();
        let big = result(&"x".repeat(RETAINED_ABOVE));
        let reference = retained(&directory, 2).write(&big);
        let bytes = serde_json::to_vec(&big).unwrap();
        let path = directory
            .join("steps")
            .join("5b0c0b1e-6f0a-4c1c-9d59-0e3f2b7a9c11-7-2.json");
        assert_eq!(
            reference,
            json!({ "complete": false, "sizeBytes": bytes.len(),
                "contentDigest": format!("sha256:{:x}", Sha256::digest(&bytes)),
                "path": path, "readWith": "workspace_file", "format": "json" })
        );
        assert!(is_reference(&reference) && !is_reference(&big));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(&directory.join("steps")), 0o700);
        }
        // Never replaced: a second write keeps the first file and answers
        // a reference without a path.
        let again = retained(&directory, 2).write(&result("other"));
        assert!(again.get("path").is_none());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_behind_a_link_is_refused_and_the_reference_keeps_no_path() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("real")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        assert_eq!(private_directory(&root.join("link").join("steps")), Err(()));
        let reference = retained(&root.join("link"), 0).write(&result("x"));
        assert!(reference.get("path").is_none());
        assert!(!root.join("real").join("steps").join("x").exists());
        assert_eq!(
            private_directory(&root.join("real").join("captures")),
            Ok(root.join("real").join("captures"))
        );
    }

    #[test]
    fn a_line_over_its_bound_sends_its_largest_results_away_until_it_fits() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path().canonicalize().unwrap();
        let mut reply = json!({ "id": 7, "success": true, "steps": [
            { "op": "a", "result": result(&"s".repeat(1_000)) },
            { "op": "b", "result": result(&"l".repeat(300_000)) },
            { "op": "c", "result": result(&"m".repeat(200_000)) },
        ] });
        let slots = |count: usize| {
            (0..count)
                .map(|step| Slot {
                    pointer: format!("/steps/{step}/result"),
                    retained: retained(&directory, step),
                    reference: None,
                })
                .collect::<Vec<_>>()
        };
        let original = reply.clone();
        // A line that already fits is unchanged.
        assert!(fit(&mut reply, slots(3), MAX_REPLY_BYTES).is_empty());
        assert_eq!(reply, original);
        // Each result carries its text twice. Over a bound of 450 kB, only
        // the largest goes.
        let written = fit(&mut reply, slots(3), 450_000);
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].retained.step, 1);
        assert!(is_reference(&reply["steps"][1]["result"]));
        assert_eq!(reply["steps"][2]["result"], original["steps"][2]["result"]);
        assert!(line_length(&reply) <= 450_000);
        // Nothing was cut: the file holds the result exactly.
        let path = reply["steps"][1]["result"]["path"].as_str().unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&std::fs::read(path).unwrap()).unwrap(),
            original["steps"][1]["result"]
        );
        // A tighter bound sends the next largest; a reference never moves.
        let written = fit(&mut reply, slots(3), 10_000);
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].retained.step, 2);
        assert!(line_length(&reply) <= 10_000);
        assert_eq!(reply["steps"][0]["result"], original["steps"][0]["result"]);
    }

    #[test]
    fn a_result_already_in_its_file_leaves_by_its_reference_without_a_second_write() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path().canonicalize().unwrap();
        let big = result(&"b".repeat(200_000));
        let reference = retained(&directory, 0).write(&big);
        let mut reply = json!({ "steps": [{ "result": big }] });
        let written = fit(
            &mut reply,
            vec![Slot {
                pointer: "/steps/0/result".into(),
                retained: retained(&directory, 0),
                reference: Some(reference.clone()),
            }],
            1_000,
        );
        assert!(written.is_empty());
        assert_eq!(reply["steps"][0]["result"], reference);
        assert!(reference.get("path").is_some());
    }
}
