//! Real Chrome requests are the file boundary, including page-script loads.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use sha2::{Digest, Sha256};

use super::files::{Receipt, Scope, StagedFiles};
use super::remote::{CloseReason, ProgramId, Programs, Request as ProgramRequest};
use crate::native::actions::{execute_command, DaemonState};
use crate::native::agent_channel::frame::{ActionId, ChannelId, Owner};
use crate::native::site_sessions::{custody::Custody, protocol::Request};

fn permissions(path: &std::path::Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_remote_staging_requires_live_program_and_exact_action_owner() {
    let fixture = tempfile::tempdir().unwrap();
    let base = fixture.path().join("staged");
    let programme = ProgramId::parse("50fac831-3be0-45ec-8b2e-ae57c3a3ddfe").unwrap();
    let directory = base.join(programme.to_string()).join("a".repeat(64));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("input.txt");
    std::fs::write(&path, b"nosecret-code-file").unwrap();
    permissions(&path, 0o444);
    permissions(&directory, 0o555);
    let receipt = Receipt {
        path: path.to_str().unwrap().into(),
        byte_size: b"nosecret-code-file".len() as u64,
        content_ref: format!("sha256:{:x}", Sha256::digest(b"nosecret-code-file")),
    };
    let files = StagedFiles::with_root(base);
    let mut state = DaemonState::new();
    let launched = Box::pin(execute_command(
        &json!({"action":"launch","headless":true}),
        &mut state,
    ))
    .await;
    assert_eq!(launched["success"], true, "{launched}");
    let client = state.browser.as_ref().unwrap().client.clone();
    let mut context = client.site_context();
    context.files = files.clone();
    client.set_site_custody(std::sync::Weak::new(), context);
    let state = Arc::new(tokio::sync::Mutex::new(state));
    let programmes = Programs::default();
    let owner = Owner {
        action: ActionId::for_test(201),
        generation: 1,
    };
    let channel = ChannelId::parse("6825cefb-83c7-4397-86a7-085ebf0b0ad3").unwrap();
    assert!(programmes
        .request_current(
            &state,
            channel,
            owner,
            ProgramRequest::Files {
                program: programme,
                files: vec![receipt.clone()]
            },
            None
        )
        .await
        .is_err());
    programmes
        .request_current(
            &state,
            channel,
            owner,
            ProgramRequest::Open {
                program: programme,
                timeout_ms: 10000,
                target: None,
            },
            None,
        )
        .await
        .unwrap();
    for invalid in [
        Owner {
            action: ActionId::for_test(202),
            generation: 1,
        },
        Owner {
            action: owner.action,
            generation: 2,
        },
    ] {
        assert!(programmes
            .request_current(
                &state,
                channel,
                invalid,
                ProgramRequest::Files {
                    program: programme,
                    files: vec![receipt.clone()]
                },
                None
            )
            .await
            .is_err());
    }
    let registered = programmes
        .request_current(
            &state,
            channel,
            owner,
            ProgramRequest::Files {
                program: programme,
                files: vec![receipt.clone()],
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(registered["registered"], 1);
    files
        .paths(vec![path.to_str().unwrap().into()])
        .await
        .unwrap();
    let cleared = programmes
        .request_current(
            &state,
            channel,
            owner,
            ProgramRequest::Files {
                program: programme,
                files: vec![],
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(cleared["registered"], 0);
    assert!(files
        .paths(vec![path.to_str().unwrap().into()])
        .await
        .is_err());
    programmes
        .request_current(
            &state,
            channel,
            owner,
            ProgramRequest::Files {
                program: programme,
                files: vec![receipt.clone()],
            },
            None,
        )
        .await
        .unwrap();
    programmes
        .request_current(
            &state,
            channel,
            owner,
            ProgramRequest::Close {
                program: programme,
                reason: CloseReason::Complete,
            },
            None,
        )
        .await
        .unwrap();
    assert!(files
        .paths(vec![path.to_str().unwrap().into()])
        .await
        .is_err());
    assert!(programmes
        .request_current(
            &state,
            channel,
            owner,
            ProgramRequest::Files {
                program: programme,
                files: vec![receipt]
            },
            None
        )
        .await
        .is_err());
    let closed = Box::pin(execute_command(
        &json!({"action":"close"}),
        &mut *state.lock().await,
    ))
    .await;
    assert_eq!(closed["success"], true, "{closed}");
    permissions(&directory, 0o755);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_staged_files_guard_script_resources_and_outlive_program_close() {
    let fixture = tempfile::tempdir().unwrap();
    let base = fixture.path().join("staged");
    let scope = Scope::parse("50fac831-3be0-45ec-8b2e-ae57c3a3ddfe").unwrap();
    let directory = base.join(scope.to_string()).join("a".repeat(64));
    std::fs::create_dir_all(&directory).unwrap();
    let secret = fixture.path().join("host-private.js");
    std::fs::write(&secret, "globalThis.hostPrivateCanary='nosecret-host-file'").unwrap();
    let secret_url = url::Url::from_file_path(&secret).unwrap().to_string();
    let page = directory.join("shared.html");
    let public =
        format!("<title>Staged file</title><input type=file><script src='{secret_url}'></script>");
    std::fs::write(&page, &public).unwrap();
    permissions(&page, 0o444);
    permissions(&directory, 0o555);
    let page_url = url::Url::from_file_path(&page).unwrap().to_string();
    let owner = Owner {
        action: ActionId::for_test(201),
        generation: 1,
    };
    let files = StagedFiles::with_root(base);
    files.activate(scope, owner).unwrap();
    files
        .register(
            scope,
            owner,
            vec![Receipt {
                path: page.to_str().unwrap().into(),
                byte_size: public.len() as u64,
                content_ref: format!("sha256:{:x}", Sha256::digest(public.as_bytes())),
            }],
        )
        .await
        .unwrap();
    let mut state = DaemonState::new();
    let launched = Box::pin(execute_command(
        &json!({"action":"launch","headless":true}),
        &mut state,
    ))
    .await;
    assert_eq!(launched["success"], true, "{launched}");
    let browser = state.browser.as_ref().unwrap();
    let client = browser.client.clone();
    let session = browser.active_session_id().unwrap().to_owned();
    let custody = Custody::new();
    let channel = ChannelId::parse("6825cefb-83c7-4397-86a7-085ebf0b0ad3").unwrap();
    custody
        .request(
            channel,
            Request::read("site_sessions.offer", json!({"sites":[]})).unwrap(),
        )
        .await
        .unwrap();
    custody
        .browser_ready(client.clone(), vec![session.clone()])
        .await
        .unwrap();
    let mut context = client.site_context();
    context.files = files.clone();
    client.set_site_custody(Arc::downgrade(&custody), context);
    client
        .send_command("Fetch.enable", Some(json!({"patterns":[]})), Some(&session))
        .await
        .unwrap();
    let entered = Box::pin(execute_command(
        &json!({"action":"navigate","url":page_url}),
        &mut state,
    ))
    .await;
    assert_eq!(entered["success"], true, "{entered}");
    let observed=client.send_command("Runtime.evaluate",Some(json!({"expression":"({title:document.title,secretAbsent:globalThis.hostPrivateCanary===undefined})","returnByValue":true})),Some(&session)).await.unwrap();
    assert_eq!(observed["result"]["value"]["title"], "Staged file");
    assert_eq!(
        observed["result"]["value"]["secretAbsent"], true,
        "a registered HTML file cannot read an unregistered host script"
    );
    let requested=format!("new Promise(resolve=>{{const script=document.createElement('script');script.src={};script.onload=()=>resolve('loaded');script.onerror=()=>resolve('blocked');document.head.append(script)}})",serde_json::to_string(&secret_url).unwrap());
    let loaded = tokio::time::timeout(
        Duration::from_secs(3),
        client.send_command(
            "Runtime.evaluate",
            Some(json!({"expression":requested,"awaitPromise":true,"returnByValue":true})),
            Some(&session),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        loaded["result"]["value"], "blocked",
        "Runtime script bypass must fail at the actual request boundary"
    );
    files.clear(scope, owner);
    let command = json!({"files":[page.to_str().unwrap()]});
    assert!(files
        .command("DOM.setFileInputFiles", &command)
        .await
        .is_err());
    let mut events = client.subscribe();
    let expression = format!(
        "location.href={}",
        serde_json::to_string(&page_url).unwrap()
    );
    client
        .send_command(
            "Runtime.evaluate",
            Some(json!({"expression":expression})),
            Some(&session),
        )
        .await
        .unwrap();
    let failed = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.method == "Network.loadingFailed" {
                break event.params;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        failed["errorText"]
            .as_str()
            .is_some_and(|error| error.contains("BLOCKED")),
        "expired receipts must stay blocked after program close: {failed}"
    );
    custody.end(channel).await;
    let closed = Box::pin(execute_command(&json!({"action":"close"}), &mut state)).await;
    assert_eq!(closed["success"], true, "{closed}");
    permissions(&directory, 0o755);
}
