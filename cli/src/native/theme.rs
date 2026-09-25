//! The browser theme: the browser is dark when the person's app is dark and
//! light when it is light. The theme reaches Chrome's own window UI (tab
//! strip, omnibox, infobars) and every page's `prefers-color-scheme`, so a
//! site renders its own dark or light design. Page content is never
//! repainted: Chrome's auto dark mode (`Emulation.setAutoDarkModeOverride`)
//! is not used, and a page with no dark design stays as it is.
//!
//! Measured on Chrome for Testing 152 in the workspace image:
//! `--force-dark-mode` draws a headed window's UI dark and makes pages prefer
//! dark without repainting them, and nothing switches the UI of a running
//! window (the image has no GTK and no settings portal, and the switch holds
//! for the process lifetime). So pages switch live through
//! `Emulation.setEmulatedMedia`, and the window UI follows at the next launch.
//!
//! The session theme is the last `set_theme`, or the theme the last launch
//! carried. It is session state, not launch configuration: changing it never
//! relaunches the browser, and every launch the daemon performs on its own
//! (sign-in and hand-back, lifecycle and restore relaunches) uses it.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::actions::{DaemonState, EmulatedMedia};
use super::browser::BrowserManager;

/// The daemon action that sets the session theme. It is session state, not
/// agent activity: admitted in every browser state, it never starts a daemon
/// or launches a browser, and it is neither input nor an observation.
pub(crate) const ACTION: &str = "set_theme";

/// The media feature the theme decides for pages.
const COLOR_SCHEME: &str = "prefers-color-scheme";

/// How long a theme change waits for pages to acknowledge it. A page whose
/// renderer is paused, as by an open JavaScript dialog, applies the change
/// when it resumes; it never holds the daemon longer than this.
const PAGE_ACKNOWLEDGMENT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Theme {
    Dark,
    Light,
}

impl Theme {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }

    /// The Chrome switch that draws a headed browser's own window UI in this
    /// theme. Chrome's UI is light by default, and a headless browser has no
    /// UI to draw.
    pub(crate) fn chrome_switch(self, headless: bool) -> Option<&'static str> {
        (self == Self::Dark && !headless).then_some("--force-dark-mode")
    }
}

/// The `prefers-color-scheme` pages get: an explicit `dark` or `light`
/// request wins, `no-preference` or no request follows the session theme,
/// and without a theme the request stands as it is.
pub(crate) fn page_color_scheme(requested: Option<&str>, theme: Option<Theme>) -> Option<String> {
    match (requested, theme) {
        (Some(scheme @ ("dark" | "light")), _) => Some(scheme.to_string()),
        (_, Some(theme)) => Some(theme.as_str().to_string()),
        (requested, None) => requested.map(str::to_string),
    }
}

/// The media emulation pages get: the session's requested emulation (from
/// `--color-scheme` or `set media`) with its `prefers-color-scheme` resolved
/// by [`page_color_scheme`], every other feature and the media type kept.
/// With a theme, pages never fall back to Chrome's native preference.
pub(crate) fn page_media(
    requested: Option<&EmulatedMedia>,
    theme: Option<Theme>,
) -> Option<EmulatedMedia> {
    if theme.is_none() {
        return requested.cloned();
    }
    let mut media = requested.cloned().unwrap_or_default();
    let scheme = media
        .features
        .iter()
        .position(|(name, _)| name == COLOR_SCHEME)
        .map(|index| media.features.remove(index).1);
    media.features.extend(
        page_color_scheme(scheme.as_deref(), theme).map(|scheme| (COLOR_SCHEME.into(), scheme)),
    );
    Some(media)
}

/// `set_theme`: set the session theme for every later launch and, while an
/// automated browser runs, switch every page it has adopted now. Pages it
/// adopts later get the theme with the rest of the session setup. A theme
/// supersedes an explicit page scheme (`set media dark`).
///
/// The result says where the theme took effect: `pages` is `live` when page
/// sessions switched now and `next_launch` when no automated browser runs
/// (none, or a window a person is signing in to, which has no DevTools);
/// `ui` is `none` for a browser without a window UI of its own (headless or
/// attached) and otherwise `next_launch`, as the UI cannot switch live.
pub(crate) async fn set(command: &Value, state: &mut DaemonState) -> Value {
    let id = &command["id"];
    let Some(theme) = command["theme"].as_str().and_then(Theme::parse) else {
        return json!({ "id": id, "success": false,
            "error": "set_theme requires theme dark or light." });
    };
    state.theme = Some(theme);
    if let Some(requested) = state.session_setup.emulated_media.as_mut() {
        requested.features.retain(|(name, _)| name != COLOR_SCHEME);
    }
    let (pages, ui) = match state.browser.as_ref() {
        Some(browser) => {
            if let Some(media) =
                page_media(state.session_setup.emulated_media.as_ref(), state.theme)
            {
                switch_pages(browser, &media).await;
            }
            let ui = if browser.draws_window_ui() {
                "next_launch"
            } else {
                "none"
            };
            ("live", ui)
        }
        None => ("next_launch", "next_launch"),
    };
    json!({ "id": id, "success": true,
        "data": { "theme": theme, "pages": pages, "ui": ui } })
}

/// Sends the media emulation to every page at once. Failures are ignored as
/// in the session setup replay: a page that closed meanwhile has nothing to
/// switch.
async fn switch_pages(browser: &BrowserManager, media: &EmulatedMedia) {
    let params = media.params();
    let pages = browser.pages_list();
    let switches = pages.iter().map(|page| {
        browser.client.send_command(
            "Emulation.setEmulatedMedia",
            Some(params.clone()),
            Some(&page.session_id),
        )
    });
    let _ = tokio::time::timeout(
        PAGE_ACKNOWLEDGMENT,
        futures_util::future::join_all(switches),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media(features: &[(&str, &str)]) -> EmulatedMedia {
        EmulatedMedia {
            media: None,
            features: features
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
        }
    }

    #[test]
    fn theme_values_are_exactly_dark_and_light() {
        for theme in [Theme::Dark, Theme::Light] {
            assert_eq!(Theme::parse(theme.as_str()), Some(theme));
            assert_eq!(serde_json::to_value(theme).unwrap(), theme.as_str());
            assert_eq!(
                serde_json::from_value::<Theme>(json!(theme.as_str())).unwrap(),
                theme
            );
        }
        for invalid in ["", "Dark", "system", "no-preference", " dark"] {
            assert_eq!(Theme::parse(invalid), None, "{invalid:?}");
            assert!(serde_json::from_value::<Theme>(json!(invalid)).is_err());
        }
    }

    #[test]
    fn only_a_dark_headed_browser_needs_a_chrome_switch() {
        assert_eq!(Theme::Dark.chrome_switch(false), Some("--force-dark-mode"));
        assert_eq!(Theme::Dark.chrome_switch(true), None);
        assert_eq!(Theme::Light.chrome_switch(false), None);
        assert_eq!(Theme::Light.chrome_switch(true), None);
    }

    #[test]
    fn an_explicit_scheme_wins_and_anything_else_follows_the_theme() {
        for (requested, theme, expected) in [
            (Some("dark"), Some(Theme::Light), Some("dark")),
            (Some("light"), Some(Theme::Dark), Some("light")),
            (Some("no-preference"), Some(Theme::Dark), Some("dark")),
            (None, Some(Theme::Light), Some("light")),
            (Some("no-preference"), None, Some("no-preference")),
            (Some("dark"), None, Some("dark")),
            (None, None, None),
        ] {
            assert_eq!(
                page_color_scheme(requested, theme).as_deref(),
                expected,
                "{requested:?} under {theme:?}"
            );
        }
    }

    #[test]
    fn page_media_keeps_other_features_and_never_falls_back_to_native() {
        // A reduced-motion request that names no scheme, or no-preference,
        // leaves pages on the theme and keeps the motion preference.
        for requested in [
            media(&[("prefers-reduced-motion", "reduce")]),
            media(&[
                (COLOR_SCHEME, "no-preference"),
                ("prefers-reduced-motion", "reduce"),
            ]),
        ] {
            let resolved = page_media(Some(&requested), Some(Theme::Dark)).unwrap();
            assert_eq!(
                resolved.features,
                media(&[("prefers-reduced-motion", "reduce"), (COLOR_SCHEME, "dark")]).features
            );
        }
        // An explicit scheme overrides the theme; the media type stays.
        let print = EmulatedMedia {
            media: Some("print".into()),
            ..media(&[(COLOR_SCHEME, "light")])
        };
        let resolved = page_media(Some(&print), Some(Theme::Dark)).unwrap();
        assert_eq!(resolved.media.as_deref(), Some("print"));
        assert_eq!(
            resolved.features,
            media(&[(COLOR_SCHEME, "light")]).features
        );
        // Nothing requested: the theme alone.
        assert_eq!(
            page_media(None, Some(Theme::Light)).unwrap().features,
            media(&[(COLOR_SCHEME, "light")]).features
        );
        // No theme: the request exactly as it was, today's behavior.
        let request = media(&[(COLOR_SCHEME, "no-preference")]);
        assert_eq!(
            page_media(Some(&request), None).unwrap().features,
            request.features
        );
        assert!(page_media(None, None).is_none());
    }

    #[tokio::test]
    async fn set_theme_without_a_browser_takes_effect_at_the_next_launch() {
        let mut state = DaemonState::new();
        state.session_setup.emulated_media = Some(media(&[
            (COLOR_SCHEME, "light"),
            ("prefers-reduced-motion", "reduce"),
        ]));
        let response = set(
            &json!({ "id": "t1", "action": ACTION, "theme": "dark" }),
            &mut state,
        )
        .await;
        assert_eq!(
            response,
            json!({ "id": "t1", "success": true,
                "data": { "theme": "dark", "pages": "next_launch", "ui": "next_launch" } })
        );
        assert_eq!(state.theme, Some(Theme::Dark));
        // The theme supersedes the explicit scheme; motion stays requested.
        assert_eq!(
            state
                .session_setup
                .emulated_media
                .as_ref()
                .unwrap()
                .features,
            media(&[("prefers-reduced-motion", "reduce")]).features
        );
    }

    /// Dispatched before every gate: a pending observation requirement stays
    /// pending and a host feedback request captures nothing, valid or not.
    #[tokio::test]
    async fn set_theme_is_neither_input_nor_an_observation() {
        let mut state = DaemonState::new();
        state
            .browser_control
            .lock()
            .await
            .cancel_native_input()
            .await
            .unwrap();
        let mut command = json!({ "id": "t3", "action": ACTION, "theme": "light" });
        command[crate::native::feedback::REQUEST_FIELD] = json!({ "session": "elsewhere" });
        let response = super::super::actions::execute_command(&command, &mut state).await;
        assert_eq!(response["success"], true, "{response}");
        assert!(response.get("browser").is_none(), "{response}");
        assert!(state.browser_control.lock().await.needs_observation());
        assert_eq!(state.theme, Some(Theme::Light));
    }

    #[tokio::test]
    async fn set_theme_refuses_anything_but_dark_or_light() {
        let mut state = DaemonState::new();
        for command in [
            json!({ "id": "t2", "action": ACTION }),
            json!({ "id": "t2", "action": ACTION, "theme": "system" }),
            json!({ "id": "t2", "action": ACTION, "theme": true }),
        ] {
            let response = set(&command, &mut state).await;
            assert_eq!(response["success"], false, "{response}");
            assert_eq!(state.theme, None);
        }
    }
}
