//! Sign-in mode: while a person signs in to a site, the owned window runs the
//! same Chrome (executable, flags, profile, private display and sandbox) with
//! no remote-debugging switch at all, so the browser has no automation
//! channel and truthfully reports `navigator.webdriver === false`. Handing
//! back relaunches it with DevTools. Nothing is masked in either direction.
//!
//! Tabs, cookies and the profile survive both relaunches. Isolated windows
//! (CDP browser contexts), unsaved page state and a temporary download
//! directory's contents do not; this is why sign-in is an explicit mode.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::time::Instant;

use super::{adopt_launched_browser, apply_session_setup, close_current_browser, DaemonState};
use crate::native::browser::BrowserManager;
use crate::native::browser_control::{ControlError, ControlRequest, SignInAdmission};
use crate::native::cdp::chrome::{self, ChromeProcess, LaunchOptions};
use crate::native::display::{DisplayClient, DEVICE_SCALE_FACTOR};

/// Every transition answers inside the control relay's ten-second deadline.
const TRANSITION: Duration = Duration::from_secs(8);
/// The sign-in window must be up by then, leaving the rest of the transition
/// to bring automation back if it is not.
const SIGN_IN_READY: Duration = Duration::from_millis(6500);
/// Graceful close before the browser's process group is killed.
const STOP: Duration = Duration::from_secs(2);

const NOT_OWNED: &str = "Sign-in mode needs a locally launched browser with its own window.";

/// The browser a person signs in to: the owned window, without DevTools.
pub(crate) struct SignInBrowser {
    chrome: ChromeProcess,
}

impl SignInBrowser {
    pub(crate) fn display(&self) -> Option<Arc<DisplayClient>> {
        #[cfg(target_os = "linux")]
        return self.chrome.display_client();
        #[cfg(not(target_os = "linux"))]
        None
    }

    pub(crate) fn has_exited(&mut self) -> bool {
        self.chrome.has_exited()
    }

    /// Close it as a person would; its profile and display stay retained by
    /// whoever relaunches into them.
    pub(crate) async fn stop(self) {
        let mut chrome = self.chrome;
        let _ = tokio::task::spawn_blocking(move || chrome.terminate(STOP)).await;
    }
}

/// What the automation browser hands over, checked before it stops.
struct Transition {
    /// Relaunches automation: the same launch with DevTools.
    automation: LaunchOptions,
    /// The same launch without any automation channel.
    sign_in: LaunchOptions,
    /// The window's CSS size, kept across both relaunches.
    window: (u32, u32),
}

impl Transition {
    async fn plan(state: &DaemonState) -> Result<Self, String> {
        let browser = state.browser.as_ref().ok_or(NOT_OWNED)?;
        let display = browser.display_client().ok_or(NOT_OWNED)?;
        let automation = browser.relaunch_options().map_err(|_| NOT_OWNED)?;
        if state.launch_configuration.is_none() {
            return Err(NOT_OWNED.to_string());
        }
        // Allowlists and proxy sign-in are enforced through DevTools. Without
        // it they would silently lapse, so this session cannot sign in here.
        if state.domain_filter.read().await.is_some()
            || state.proxy_credentials.read().await.is_some()
        {
            return Err("This browser's network policy is enforced through the automation channel, so sign-in mode is unavailable.".to_string());
        }
        let sign_in = automation.clone().without_automation()?;
        let surface = display.surface();
        Ok(Self {
            automation,
            sign_in,
            window: (
                surface.width / DEVICE_SCALE_FACTOR,
                surface.height / DEVICE_SCALE_FACTOR,
            ),
        })
    }
}

/// Enter sign-in mode on the controller's `sign_in` input. The lease keeps
/// its controller and sequence line; the acknowledgment names the new window.
pub(super) async fn enter(
    state: &mut DaemonState,
    request: &ControlRequest,
    event: Result<Duration, ControlError>,
) -> Result<Value, ControlError> {
    let started = Instant::now();
    let admission = state
        .browser_control
        .lock()
        .await
        .admit_sign_in(request, event)?;
    let (sequence, idle_timeout) = match admission {
        SignInAdmission::Duplicate(acknowledgment) => return Ok(acknowledgment),
        SignInAdmission::Admitted {
            sequence,
            idle_timeout,
        } => (sequence, idle_timeout),
    };
    let Transition {
        automation,
        sign_in,
        window,
    } = Transition::plan(state)
        .await
        .map_err(ControlError::invalid)?;

    // The stream keeps showing the retired window, without failing, until
    // the sign-in window replaces it.
    if let Some(mut browser) = state.browser.take() {
        let _ = browser.close_within(STOP).await;
    }
    super::forget_browser_session(state);
    let ready_by = started + SIGN_IN_READY;
    let entered = async {
        // A launch that runs out of time is awaited, not dropped: a sign-in
        // browser that never showed its window is gone before automation
        // relaunches into the same profile.
        let chrome = chrome::launch_chrome_by(sign_in, ready_by).await?;
        state.sign_in = Some(SignInBrowser { chrome });
        tokio::time::timeout_at(
            ready_by,
            state.apply_window_layout(window.0, window.1, None),
        )
        .await
        .unwrap_or_else(|_| Err("The sign-in window was not laid out in time".to_string()))
    }
    .await;
    let admitted = match entered {
        Ok(_) => {
            state.update_stream_client().await;
            state.browser_control.lock().await.begin_sign_in(
                request.controller_id(),
                sequence,
                idle_timeout,
            )
        }
        Err(error) => Err(ControlError::new("browser_control_unavailable", error)),
    };
    if let Err(error) = &admitted {
        eprintln!("[sign-in] could not start: {}", error.message);
        if let Some(sign_in) = state.sign_in.take() {
            sign_in.stop().await;
        }
        let restored = relaunch_automation(state, automation, window, started + TRANSITION).await;
        state.browser_control.lock().await.end_lease();
        return Err(ControlError::new(
            "browser_control_unavailable",
            if restored {
                "Sign-in mode could not start. The browser is back under the agent's control."
            } else {
                "Sign-in mode could not start, and the browser could not be restarted. The agent's next action starts it again."
            },
        ));
    }
    admitted
}

/// Hand the browser back to the agent: close the sign-in browser as a person
/// would, relaunch automation into the same profile and window, require a
/// fresh observation and end the lease. If the relaunch fails, custody still
/// returns and the agent's next command launches the browser normally.
pub(super) async fn hand_back(state: &mut DaemonState) {
    let deadline = Instant::now() + TRANSITION;
    if let Some(sign_in) = state.sign_in.take() {
        let window = sign_in
            .display()
            .map(|display| display.surface())
            .map(|surface| {
                (
                    surface.width / DEVICE_SCALE_FACTOR,
                    surface.height / DEVICE_SCALE_FACTOR,
                )
            });
        let automation = sign_in.chrome.relaunch_options();
        sign_in.stop().await;
        match (automation, window) {
            (Ok(options), Some(window)) => {
                let options = LaunchOptions {
                    remote_debugging: true,
                    ..options
                };
                relaunch_automation(state, options, window, deadline).await;
            }
            _ => {
                let _ = close_current_browser(state).await;
            }
        }
    }
    state.browser_control.lock().await.end_lease();
}

/// The watchdog, run by every command and maintenance tick: a person closing
/// the sign-in browser ends sign-in like closing any browser; a lapsed lease
/// or an idle person hands the browser back to the agent.
pub(super) async fn maintain(state: &mut DaemonState) {
    let Some(sign_in) = state.sign_in.as_mut() else {
        return;
    };
    if sign_in.has_exited() {
        let _ = close_current_browser(state).await;
    } else if state
        .browser_control
        .lock()
        .await
        .sign_in_due(std::time::Instant::now())
    {
        hand_back(state).await;
    }
}

/// Relaunch automation into the retained profile and display through the
/// post-launch sequence every local launch shares, then replay the session's
/// page setup onto the restored tabs. Storage state and auto-restore are not
/// reloaded: the profile already holds the session. On failure nothing is
/// left running.
async fn relaunch_automation(
    state: &mut DaemonState,
    options: LaunchOptions,
    window: (u32, u32),
    deadline: Instant,
) -> bool {
    let relaunched = match tokio::time::timeout_at(
        deadline,
        BrowserManager::launch_window(options, Some("chrome"), window),
    )
    .await
    {
        Ok(Ok(browser)) => adopt(state, browser).await,
        Ok(Err(error)) => Err(error),
        Err(_) => Err("The browser did not restart in time".to_string()),
    };
    if let Err(error) = &relaunched {
        eprintln!("[sign-in] automation relaunch failed: {error}");
        let _ = close_current_browser(state).await;
    }
    relaunched.is_ok()
}

async fn adopt(state: &mut DaemonState, browser: BrowserManager) -> Result<(), String> {
    let configuration = state.launch_configuration.clone().ok_or(NOT_OWNED)?;
    let retain_profile = state.retained_profile.is_some();
    let has_proxy_auth = state.proxy_credentials.read().await.is_some();
    adopt_launched_browser(
        state,
        browser,
        configuration,
        retain_profile,
        has_proxy_auth,
    )
    .await?;
    let sessions: Vec<String> = state
        .browser
        .as_ref()
        .map(|browser| {
            browser
                .pages_list()
                .into_iter()
                .map(|page| page.session_id)
                .collect()
        })
        .unwrap_or_default();
    for session in sessions {
        apply_session_setup(state, &session).await?;
    }
    Ok(())
}
