use super::*;

const OWNER: &str = "aabbccdd-1111-4222-8333-123456789abc";
const OTHER: &str = "aabbccdd-1111-4222-8333-123456789abd";

fn pending(files: &FileDestinations) -> String {
    files.begin(OWNER);
    files.intercepted("page");
    files.observe(
        "Page.fileChooserOpened",
        &json!({"frameId":"main","backendNodeId":7}),
        Some("page"),
    );
    files.pending(OWNER).unwrap().id
}

#[test]
fn file_destinations_follow_controller_and_renderer_lifecycle() {
    for (method, params, session) in [
        (
            "Page.frameNavigated",
            json!({"frame":{"id":"main"}}),
            Some("page"),
        ),
        (
            "Page.frameDetached",
            json!({"frameId":"main","reason":"swap"}),
            Some("page"),
        ),
        (
            "Page.documentOpened",
            json!({"frame":{"id":"main"}}),
            Some("page"),
        ),
        ("Runtime.executionContextsCleared", json!({}), Some("page")),
        (
            "Target.detachedFromTarget",
            json!({"sessionId":"page"}),
            None,
        ),
    ] {
        let files = FileDestinations::default();
        let id = pending(&files);
        assert!(files.current(OTHER, &id).is_err());
        files.observe(method, &params, session);
        assert!(files.current(OWNER, &id).is_err(), "{method}");
    }
    let files = FileDestinations::default();
    let id = pending(&files);
    files.begin(OWNER);
    assert!(files.current(OWNER, &id).is_ok());
    files.begin(OTHER);
    assert!(files.current(OWNER, &id).is_err());
    files.end();
    files.observe(
        "Page.fileChooserOpened",
        &json!({"frameId":"main","backendNodeId":7}),
        Some("page"),
    );
    assert!(files.pending(OTHER).is_none());
}

#[test]
fn chooser_replacement_and_dismissal_never_reuse_identity() {
    let files = FileDestinations::default();
    let first = pending(&files);
    let second = pending(&files);
    assert_ne!(first, second);
    assert!(files.dismiss(OWNER, &first).is_err());
    assert!(files.current(OWNER, &second).is_ok());
    files.dismiss(OWNER, &second).unwrap();
    assert!(files.current(OWNER, &second).is_err());
}

#[test]
fn staged_paths_must_be_canonical_regular_files() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("receipt.bin");
    std::fs::write(&file, [0, 255, 1]).unwrap();
    let path = file.to_str().unwrap().to_string();
    validate_paths(std::slice::from_ref(&path)).unwrap();
    for paths in [
        vec![],
        vec!["relative.bin".into()],
        vec![dir.path().to_str().unwrap().into()],
        vec!["/missing-upload-receipt".into()],
        vec![path.clone(); 65],
    ] {
        assert!(validate_paths(&paths).is_err(), "{paths:?}");
    }
    #[cfg(unix)]
    {
        let link = dir.path().join("linked.bin");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(validate_paths(&[link.to_str().unwrap().into()]).is_err());
    }
    assert!(check_deadline(Instant::now()).is_err());
}

#[test]
fn unrelated_iframe_navigation_does_not_cancel_file_selection() {
    let files = FileDestinations::default();
    let id = pending(&files);
    files.observe(
        "Page.frameNavigated",
        &json!({"frame":{"id":"advertisement","parentId":"main"}}),
        Some("page"),
    );
    files.observe(
        "Page.frameDetached",
        &json!({"frameId":"advertisement"}),
        Some("page"),
    );
    assert!(files.current(OWNER, &id).is_ok());
    files.observe(
        "Page.frameNavigated",
        &json!({"frame":{"id":"main"}}),
        Some("page"),
    );
    assert!(files.current(OWNER, &id).is_err());
}
