//! Host-staged file receipts. The model's process cannot read this filesystem;
//! every local file Chrome consumes is rechecked against its live receipt.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::native::agent_channel::frame::Owner;

const ROOT: &str = "/workspace/.ambit/browser/staged";
const REFUSED: &str = "The browser file has no current, exact host staging receipt.";

/// One host-admitted operation's staging identity, whether that operation is
/// a native upload or a remote program. It confers no authority by itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Scope(uuid::Uuid);
impl Scope {
    pub(crate) fn parse(value: &str) -> Result<Self, &'static str> {
        let id = uuid::Uuid::parse_str(value).map_err(|_| REFUSED)?;
        if id.is_nil() || id.hyphenated().to_string() != value {
            return Err(REFUSED);
        }
        Ok(Self(id))
    }
}
impl std::fmt::Display for Scope {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(out)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Receipt {
    pub(crate) path: String,
    pub(crate) byte_size: u64,
    pub(crate) content_ref: String,
}

#[derive(Clone)]
struct State {
    root: PathBuf,
    guarded: bool,
    owner: Option<(Scope, Owner)>,
    revision: u64,
    download_root: Option<PathBuf>,
    receipts: BTreeMap<PathBuf, Receipt>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            root: ROOT.into(),
            guarded: false,
            owner: None,
            revision: 0,
            download_root: None,
            receipts: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct StagedFiles(Arc<RwLock<State>>);

impl StagedFiles {
    pub(crate) fn downloads(&self, root: Option<PathBuf>) {
        self.0.write().unwrap().download_root = root;
    }
    #[cfg(test)]
    pub(crate) fn with_root(root: PathBuf) -> Self {
        Self(Arc::new(RwLock::new(State {
            root,
            ..State::default()
        })))
    }
    pub(crate) fn guarded(&self) -> bool {
        self.0.read().unwrap().guarded
    }
    pub(crate) fn guard(&self) {
        self.0.write().unwrap().guarded = true;
    }
    pub(crate) fn activate(&self, program: Scope, owner: Owner) {
        let mut state = self.0.write().unwrap();
        state.guarded = true;
        state.owner = Some((program, owner));
        state.receipts.clear();
        state.revision += 1;
    }
    pub(crate) fn clear(&self, program: Scope, owner: Owner) {
        let mut state = self.0.write().unwrap();
        if state.owner == Some((program, owner)) {
            state.owner = None;
            state.receipts.clear();
            state.revision += 1;
        }
    }

    pub(crate) async fn register(
        &self,
        program: Scope,
        owner: Owner,
        receipts: Vec<Receipt>,
    ) -> Result<usize, &'static str> {
        let mut snapshot = self.0.read().unwrap().clone();
        if snapshot.owner != Some((program, owner)) {
            return Err(REFUSED);
        }
        if receipts.is_empty() {
            let mut state = self.0.write().unwrap();
            if state.owner != Some((program, owner)) {
                return Err(REFUSED);
            }
            state.receipts.clear();
            state.revision += 1;
            return Ok(0);
        }
        let revision = snapshot.revision;
        let checked = tokio::task::spawn_blocking(move || {
            for receipt in receipts {
                let path = PathBuf::from(&receipt.path);
                validate_path(&path, program, &snapshot.root)?;
                verify(&path, &receipt)?;
                if snapshot.receipts.get(&path).is_some_and(|old| {
                    old.content_ref != receipt.content_ref || old.byte_size != receipt.byte_size
                }) {
                    return Err(REFUSED);
                }
                snapshot.receipts.insert(path, receipt);
            }
            Ok(snapshot.receipts)
        })
        .await
        .map_err(|_| REFUSED)??;
        let mut state = self.0.write().unwrap();
        if state.owner != Some((program, owner)) || state.revision != revision {
            return Err(REFUSED);
        }
        // Concurrent registrations cannot remove already admitted files.
        for (path, receipt) in &checked {
            if state.receipts.get(path).is_some_and(|old| {
                old.content_ref != receipt.content_ref || old.byte_size != receipt.byte_size
            }) {
                return Err(REFUSED);
            }
        }
        for (path, receipt) in checked {
            state.receipts.insert(path, receipt);
        }
        state.revision += 1;
        Ok(state.receipts.len())
    }

    pub(crate) async fn paths(&self, paths: Vec<String>) -> Result<(), &'static str> {
        if paths.is_empty() {
            return Ok(());
        }
        let snapshot = self.0.read().unwrap().clone();
        if !snapshot.guarded {
            return Ok(());
        }
        let owner = snapshot.owner.ok_or(REFUSED)?;
        let revision = snapshot.revision;
        tokio::task::spawn_blocking(move || {
            for path in paths {
                consume(
                    &PathBuf::from(path),
                    owner.0,
                    &snapshot.root,
                    &snapshot.receipts,
                )?;
            }
            Ok::<_, &'static str>(())
        })
        .await
        .map_err(|_| REFUSED)??;
        let current = self.0.read().unwrap();
        if current.owner != Some(owner) || current.revision != revision {
            return Err(REFUSED);
        }
        Ok(())
    }

    pub(crate) async fn url(&self, url: &str) -> Result<(), &'static str> {
        let url = url.trim_start();
        if url
            .get(..12)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("view-source:"))
        {
            return Box::pin(self.url(url[12..].trim_start())).await;
        }
        let Ok(url) = url::Url::parse(url) else {
            return Ok(());
        };
        if url.scheme() != "file" {
            return Ok(());
        }
        let path = url.to_file_path().map_err(|_| REFUSED)?;
        self.paths(vec![path.to_str().ok_or(REFUSED)?.to_owned()])
            .await
    }

    pub(crate) async fn command(
        &self,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<(), &'static str> {
        if !self.guarded() {
            return Ok(());
        }
        if method == "Browser.setDownloadBehavior" {
            if params["behavior"] == "deny" {
                return Ok(());
            }
            let state = self.0.read().unwrap();
            if state.download_root.as_ref().and_then(|path| path.to_str())
                != params["downloadPath"].as_str()
                || state.download_root.is_none()
            {
                return Err(REFUSED);
            }
        }
        if method == "DOM.setFileInputFiles" {
            let paths = params["files"]
                .as_array()
                .ok_or(REFUSED)?
                .iter()
                .map(|path| path.as_str().map(str::to_owned).ok_or(REFUSED))
                .collect::<Result<Vec<_>, _>>()?;
            return self.paths(paths).await;
        }
        if matches!(
            method,
            "Input.dispatchDragEvent" | "Input.setInterceptDrags"
        ) {
            if let Some(paths) = params.pointer("/data/files") {
                let paths = paths
                    .as_array()
                    .ok_or(REFUSED)?
                    .iter()
                    .map(|path| path.as_str().map(str::to_owned).ok_or(REFUSED))
                    .collect::<Result<Vec<_>, _>>()?;
                self.paths(paths).await?;
            }
        }
        if matches!(
            method,
            "Page.navigate"
                | "Target.createTarget"
                | "Page.getResourceContent"
                | "Network.loadNetworkResource"
                | "Page.setDocumentContent"
        ) {
            if let Some(url) = params["url"].as_str() {
                self.url(url).await?;
            }
        }
        Ok(())
    }
}

fn validate_path(path: &Path, program: Scope, base: &Path) -> Result<(), &'static str> {
    let root = base.join(program.to_string());
    let relative = path.strip_prefix(&root).map_err(|_| REFUSED)?;
    let mut parts = relative.components();
    let Some(Component::Normal(digest)) = parts.next() else {
        return Err(REFUSED);
    };
    if !digest.to_str().is_some_and(lower_hex_digest) || (parts.next().is_none() && !path.is_dir())
    {
        return Err(REFUSED);
    }
    if path.components().any(|part| {
        matches!(
            part,
            Component::CurDir | Component::ParentDir | Component::Prefix(_)
        )
    }) {
        return Err(REFUSED);
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let metadata = std::fs::symlink_metadata(&current).map_err(|_| REFUSED)?;
        if metadata.file_type().is_symlink() {
            return Err(REFUSED);
        }
        #[cfg(unix)]
        if current.starts_with(root.join(digest)) {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o222 != 0 {
                return Err(REFUSED);
            }
        }
    }
    Ok(())
}

fn lower_hex_digest(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn verify(path: &Path, receipt: &Receipt) -> Result<(), &'static str> {
    let digest = receipt
        .content_ref
        .strip_prefix("sha256:")
        .filter(|digest| lower_hex_digest(digest))
        .ok_or(REFUSED)?;
    let metadata = std::fs::symlink_metadata(path).map_err(|_| REFUSED)?;
    if receipt.byte_size > ((1u64 << 53) - 1)
        || !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() != receipt.byte_size
    {
        return Err(REFUSED);
    }
    let mut file = std::fs::File::open(path).map_err(|_| REFUSED)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 << 10];
    loop {
        let count = std::io::Read::read(&mut file, &mut buffer).map_err(|_| REFUSED)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    if format!("{:x}", hasher.finalize()) != digest {
        return Err(REFUSED);
    }
    Ok(())
}

fn consume(
    path: &Path,
    program: Scope,
    base: &Path,
    receipts: &BTreeMap<PathBuf, Receipt>,
) -> Result<(), &'static str> {
    validate_path(path, program, base)?;
    let metadata = std::fs::symlink_metadata(path).map_err(|_| REFUSED)?;
    if metadata.is_file() {
        return verify(path, receipts.get(path).ok_or(REFUSED)?);
    }
    if !metadata.is_dir() {
        return Err(REFUSED);
    }
    for child in std::fs::read_dir(path).map_err(|_| REFUSED)? {
        consume(&child.map_err(|_| REFUSED)?.path(), program, base, receipts)?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::native::agent_channel::frame::ActionId;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn owner(generation: u64) -> Owner {
        Owner {
            action: ActionId::for_test(93),
            generation,
        }
    }
    fn permissions(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    fn receipt(path: &Path, bytes: &[u8]) -> Receipt {
        Receipt {
            path: path.to_str().unwrap().into(),
            byte_size: bytes.len() as u64,
            content_ref: format!("sha256:{:x}", Sha256::digest(bytes)),
        }
    }

    #[tokio::test]
    async fn two_large_files_have_no_individual_or_aggregate_upload_ceiling() {
        let temporary = tempfile::tempdir().unwrap();
        let files = StagedFiles::with_root(temporary.path().to_path_buf());
        let scope = Scope::parse("6b87fd14-4712-45e1-829e-95ee008fd783").unwrap();
        let directory = temporary
            .path()
            .join(scope.to_string())
            .join("3".repeat(64));
        std::fs::create_dir_all(&directory).unwrap();
        let bytes = 33u64 << 20;
        let chunk = [0u8; 64 << 10];
        let mut hash = Sha256::new();
        for _ in 0..bytes / chunk.len() as u64 {
            hash.update(chunk);
        }
        let digest = format!("sha256:{:x}", hash.finalize());
        let mut receipts = Vec::new();
        for name in ["first.bin", "second.bin"] {
            let path = directory.join(name);
            std::fs::File::create(&path)
                .unwrap()
                .set_len(bytes)
                .unwrap();
            permissions(&path, 0o444);
            receipts.push(Receipt {
                path: path.to_str().unwrap().into(),
                byte_size: bytes,
                content_ref: digest.clone(),
            });
        }
        permissions(&directory, 0o555);
        files.activate(scope, owner(1));
        assert_eq!(
            files
                .register(scope, owner(1), receipts.clone())
                .await
                .unwrap(),
            2
        );
        files
            .paths(receipts.iter().map(|file| file.path.clone()).collect())
            .await
            .unwrap();
        files
            .paths(vec![directory.to_str().unwrap().into()])
            .await
            .unwrap();
        files.downloads(Some(directory.clone()));
        files
            .command(
                "Browser.setDownloadBehavior",
                &serde_json::json!({"behavior":"allowAndName","downloadPath":directory}),
            )
            .await
            .unwrap();
        assert!(files
            .command(
                "Browser.setDownloadBehavior",
                &serde_json::json!({"behavior":"allowAndName","downloadPath":temporary.path()})
            )
            .await
            .is_err());
        assert!(files
            .command(
                "Browser.setDownloadBehavior",
                &serde_json::json!({"behavior":"default"})
            )
            .await
            .is_err());
        files
            .command(
                "Browser.setDownloadBehavior",
                &serde_json::json!({"behavior":"deny"}),
            )
            .await
            .unwrap();
        permissions(&directory, 0o755);
    }

    #[tokio::test]
    async fn exact_staging_requires_current_immutable_bytes_and_owner() {
        let temporary = tempfile::tempdir().unwrap();
        let files = StagedFiles::with_root(temporary.path().to_path_buf());
        let program = Scope::parse("6b87fd14-4712-45e1-829e-95ee008fd783").unwrap();
        let directory = temporary
            .path()
            .join(program.to_string())
            .join("1".repeat(64));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("fixture.txt");
        std::fs::write(&path, b"nosecret-original").unwrap();
        permissions(&path, 0o444);
        permissions(&directory, 0o555);
        files.activate(program, owner(1));
        assert!(
            files
                .paths(vec![path.to_str().unwrap().into()])
                .await
                .is_err(),
            "path alone grants nothing"
        );
        let exact = receipt(&path, b"nosecret-original");
        assert!(files
            .register(program, owner(2), vec![exact.clone()])
            .await
            .is_err());
        assert_eq!(
            files
                .register(program, owner(1), vec![exact.clone()])
                .await
                .unwrap(),
            1
        );
        files
            .paths(vec![path.to_str().unwrap().into()])
            .await
            .unwrap();
        files
            .url(url::Url::from_file_path(&path).unwrap().as_str())
            .await
            .unwrap();
        files
            .paths(vec![directory.to_str().unwrap().into()])
            .await
            .unwrap();
        permissions(&path, 0o644);
        std::fs::write(&path, b"nosecret-replaced").unwrap();
        permissions(&path, 0o444);
        assert!(
            files
                .paths(vec![path.to_str().unwrap().into()])
                .await
                .is_err(),
            "same-size mutation must fail its digest"
        );
        files.clear(program, owner(1));
        assert!(
            files
                .paths(vec![path.to_str().unwrap().into()])
                .await
                .is_err(),
            "no receipt outlives its program"
        );
        assert!(
            files.guarded(),
            "local-file guarding outlives a page timer and its originating program"
        );
        permissions(&directory, 0o755);
    }

    #[tokio::test]
    async fn tree_uploads_keep_relative_paths_and_refuse_extras_symlinks_and_traversal() {
        let temporary = tempfile::tempdir().unwrap();
        let files = StagedFiles::with_root(temporary.path().to_path_buf());
        let program = Scope::parse("6b87fd14-4712-45e1-829e-95ee008fd783").unwrap();
        let directory = temporary
            .path()
            .join(program.to_string())
            .join("2".repeat(64));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("empty.txt");
        std::fs::write(&path, []).unwrap();
        let extra = directory.join("extra.txt");
        std::fs::write(&extra, b"nosecret-extra").unwrap();
        permissions(&path, 0o444);
        permissions(&extra, 0o444);
        permissions(&directory, 0o555);
        files.activate(program, owner(1));
        files
            .register(program, owner(1), vec![receipt(&path, &[])])
            .await
            .unwrap();
        assert!(files
            .paths(vec![directory.to_str().unwrap().into()])
            .await
            .is_err());
        files
            .register(program, owner(1), vec![receipt(&extra, b"nosecret-extra")])
            .await
            .unwrap();
        files
            .paths(vec![directory.to_str().unwrap().into()])
            .await
            .unwrap();
        permissions(&directory, 0o755);
        let link = directory.join("link.txt");
        symlink(&extra, &link).unwrap();
        permissions(&directory, 0o555);
        assert!(files
            .register(program, owner(1), vec![receipt(&link, b"nosecret-extra")])
            .await
            .is_err());
        assert!(files
            .paths(vec![directory.to_str().unwrap().into()])
            .await
            .is_err());
        assert!(files
            .paths(vec![format!(
                "{}/../{}/empty.txt",
                directory.display(),
                "2".repeat(64)
            )])
            .await
            .is_err());
        files.paths(vec![]).await.unwrap();
        permissions(&directory, 0o755);
    }
}
