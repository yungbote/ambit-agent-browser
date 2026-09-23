//! Explicit output paths name the file the caller wants, wherever it is.
//! A relative path means the daemon's working directory, which the caller
//! cannot see, so results always report the absolute path that was written.

use std::path::PathBuf;

/// Resolves `path` to an absolute path and creates its parent directories.
pub(crate) fn prepare(path: &str) -> Result<PathBuf, String> {
    let path = std::path::absolute(path)
        .map_err(|error| format!("Invalid output path {path}: {error}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("Failed to create directory {}: {error}", parent.display()))?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_missing_parents_and_reports_the_absolute_path() {
        let root = tempfile::tempdir().unwrap();
        let requested = root.path().join("work/wikirace/stop-00.jpg");
        let prepared = prepare(requested.to_str().unwrap()).unwrap();
        assert_eq!(prepared, requested);
        assert!(requested.parent().unwrap().is_dir());

        let relative = prepare("shots/01-windowswap.jpg").unwrap();
        assert!(relative.is_absolute());
        assert!(relative.ends_with("shots/01-windowswap.jpg"));
        std::fs::remove_dir(relative.parent().unwrap()).unwrap();
    }

    #[test]
    fn reports_a_parent_that_cannot_be_a_directory() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), b"x").unwrap();
        let error = prepare(root.path().join("file/shot.png").to_str().unwrap()).unwrap_err();
        assert!(error.starts_with("Failed to create directory "), "{error}");
    }
}
