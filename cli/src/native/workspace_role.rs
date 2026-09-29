//! A reserved process-start role, derived by the host from its workspace
//! manifest. It is not a grant and is never read from a model command.

use std::sync::OnceLock;

pub(crate) const VARIABLE: &str = "AMBIT_WORKSPACE_ROLE";
const INVALID: &str = "AMBIT_WORKSPACE_ROLE must be code or browser_host when set";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkspaceRole {
    Standalone,
    Code,
    BrowserHost,
}

impl WorkspaceRole {
    pub(crate) fn is_browser_host(self) -> bool {
        self == Self::BrowserHost
    }

    pub(crate) fn admit_local_program(self) -> Result<(), &'static str> {
        if self.is_browser_host() {
            Err("browser_operation_rejected: Browser programs must run in the conversation workspace.")
        } else {
            Ok(())
        }
    }
}

fn parse(value: Option<&str>) -> Result<WorkspaceRole, &'static str> {
    match value {
        None => Ok(WorkspaceRole::Standalone),
        Some("code") => Ok(WorkspaceRole::Code),
        Some("browser_host") => Ok(WorkspaceRole::BrowserHost),
        Some(_) => Err(INVALID),
    }
}

/// The first call at daemon startup fixes the role for this process. Later
/// envelopes and environment changes cannot downgrade a browser host.
pub(crate) fn current() -> Result<WorkspaceRole, &'static str> {
    static ROLE: OnceLock<Result<WorkspaceRole, &'static str>> = OnceLock::new();
    *ROLE.get_or_init(|| match std::env::var(VARIABLE) {
        Ok(value) => parse(Some(&value)),
        Err(std::env::VarError::NotPresent) => parse(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(INVALID),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_reserved_roles_are_admitted() {
        assert_eq!(parse(None), Ok(WorkspaceRole::Standalone));
        assert_eq!(parse(Some("code")), Ok(WorkspaceRole::Code));
        assert_eq!(parse(Some("browser_host")), Ok(WorkspaceRole::BrowserHost));
        for invalid in [
            "",
            " ",
            "standalone",
            "browser-host",
            "BROWSER_HOST",
            "browser_host ",
            "false",
            "ø",
        ] {
            assert_eq!(parse(Some(invalid)), Err(INVALID), "{invalid:?}");
        }
    }

    #[test]
    fn process_role_is_immutable() {
        const CHILD: &str = "AMBIT_ROLE_TEST_CHILD";
        if let Ok(expected) = std::env::var(CHILD) {
            let expected = if expected == "absent" {
                parse(None)
            } else {
                parse(Some(&expected))
            };
            assert_eq!(current(), expected);
            if expected == Ok(WorkspaceRole::BrowserHost) {
                let sentinel = tempfile::tempdir().unwrap();
                let marker = sentinel.path().join("program-ran");
                let code = format!(
                    "(await import('node:fs')).writeFileSync({}, 'ran')",
                    serde_json::to_string(&marker.to_string_lossy()).unwrap()
                );
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async {
                    let mut state = super::super::actions::DaemonState::new();
                    let response = Box::pin(super::super::actions::execute_command(&serde_json::json!({"id":"host-before-offer","action":"run_playwright","code":code,"workspaceRole":"code","hostConfig":{"browserHost":false}}), &mut state)).await;
                    assert_eq!(response["success"], false, "{response}");
                    assert_eq!(response["code"], "browser_operation_rejected", "{response}");
                    assert!(response["error"].as_str().unwrap().contains("conversation workspace"), "{response}");
                    assert!(state.browser.is_none(), "Chrome was never launched");
                    assert!(!marker.exists(), "the local program never started");
                    let direct = super::super::playwright::run(&serde_json::json!({"action":"run_playwright","code":"return true"}), &mut state).await.unwrap_err();
                    assert!(direct.error.starts_with("browser_operation_rejected:"));
                    assert!(direct.error.contains("conversation workspace"));
                    assert!(state.browser.is_none(), "direct callers cannot bypass local program admission");
                });
            }
            if expected.is_err() {
                let directory = tempfile::tempdir().unwrap();
                std::env::set_var("AGENT_BROWSER_SOCKET_DIR", directory.path());
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let result = runtime.block_on(super::super::daemon::run_daemon(
                    "invalid-role-fixture",
                    super::super::daemon::DaemonMode::Foreground,
                ));
                assert_eq!(result, Err(INVALID.to_string()));
                assert_eq!(
                    std::fs::read_dir(directory.path()).unwrap().count(),
                    0,
                    "invalid startup wrote no lock, socket or metadata"
                );
            }
            std::env::set_var(VARIABLE, "code");
            assert_eq!(current(), expected);
            std::env::remove_var(VARIABLE);
            assert_eq!(current(), expected);
            return;
        }
        for role in ["absent", "code", "browser_host", "invalid"] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child.args([
                "native::workspace_role::tests::process_role_is_immutable",
                "--exact",
            ]);
            child.env(CHILD, role);
            if role == "absent" {
                child.env_remove(VARIABLE);
            } else {
                child.env(VARIABLE, role);
            }
            let output = child.output().unwrap();
            assert!(
                output.status.success(),
                "{role}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
