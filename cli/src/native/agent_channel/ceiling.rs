//! The effect ceiling the daemon holds (agent-channel contract §4
//! "Ceiling"): whether a step may act on its target under the ceiling its
//! plan holds. The daemon judges targets, from what the target structurally
//! is (`TargetFacts`, read through the DOM in an isolated world); the host
//! judges operations and origins. The classifier is structural, so its
//! coverage is a measured rate: JavaScript-driven links, `role=button`
//! elements and routers that post through `fetch` are its known misses.

use serde::{Deserialize, Serialize};

/// How far a step may act: read, fill fields, or commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Ceiling {
    Read,
    Fill,
    Commit,
}

impl Ceiling {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Fill => "fill",
            Self::Commit => "commit",
        }
    }
}

/// What a step does to its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Interaction {
    /// The pointer rests on it, it takes focus, or the page scrolls to it.
    Rest,
    /// A key that neither types nor submits: Tab, an arrow, Escape, Home,
    /// End, a page key or a modifier alone.
    Navigate,
    /// A pointer press on it: a click, a tap, a checkbox toggle, a button
    /// pressed or released where the pointer is.
    Press,
    /// Text typed, deleted or pasted into it, or a value selected in it.
    Type,
    /// Enter pressed while it has focus.
    Enter,
    /// Space pressed while it has focus.
    Space,
    /// A file handed to it or taken from it: an upload or a download.
    Transfer,
    /// A key that neither types, submits, activates nor navigates: a
    /// function, media or browser key, whose effect page script decides.
    OtherKey,
}

/// What a target structurally is, read through the DOM in an isolated
/// world, so no page script can redefine what is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    /// An `<a>` or `<area>` with an `href` and no `download` attribute.
    Link,
    /// A link with a `download` attribute.
    Download,
    /// A `<summary>`, or an element with an `expanded` state or an
    /// `aria-controls` that names an element of the document.
    Disclosure,
    /// An element with `role=tab`.
    Tab,
    /// A form's submit control: a submit button or an image or submit input
    /// that belongs to a form.
    Submit,
    /// A field one types or selects into: an input that is not a button,
    /// reset, file or hidden input, a textarea, a select, an editable
    /// element, or an element with a text-entry role.
    Field,
    /// An input of type file.
    File,
    /// Anything else: a plain button, a `role=button` element, a link
    /// without an `href`, the document itself.
    Other,
}

/// The effective method of the form a submit control or a field belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Method {
    Get,
    Post,
    Dialog,
}

/// The facts the classifier judges a target by.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TargetFacts {
    pub(crate) kind: Kind,
    /// The form's method, for a submit control or a field in a form.
    #[serde(default)]
    pub(crate) method: Option<Method>,
    /// An `<input>`'s type, lower-cased.
    #[serde(default)]
    pub(crate) input_type: Option<String>,
    /// Whether Enter adds a line rather than submitting: a textarea or an
    /// editable element.
    #[serde(default)]
    pub(crate) multiline: bool,
    /// A secret field: a password, a one-time code or a payment field.
    #[serde(default)]
    pub(crate) secret: bool,
}

impl TargetFacts {
    fn checkable(&self) -> bool {
        matches!(self.input_type.as_deref(), Some("checkbox" | "radio"))
    }

    /// A field a press only focuses: one that takes typing or opens a list.
    fn focus_field(&self) -> bool {
        self.kind == Kind::Field && !self.checkable()
    }

    /// A submission through a form: GET is a navigation that reads; POST
    /// and dialog forms commit.
    fn submission(&self) -> Ceiling {
        match self.method {
            Some(Method::Get) => Ceiling::Read,
            Some(Method::Post | Method::Dialog) | None => Ceiling::Commit,
        }
    }
}

/// The weakest ceiling that admits `interaction` on `target`, or `None` when
/// none does: nothing types into a secret field, under any ceiling.
pub(crate) fn required(interaction: Interaction, target: &TargetFacts) -> Option<Ceiling> {
    use Ceiling::{Commit, Fill, Read};
    Some(match interaction {
        Interaction::Rest | Interaction::Navigate => Read,
        Interaction::Transfer | Interaction::OtherKey => Commit,
        Interaction::Type => return typing(target),
        Interaction::Enter if target.kind == Kind::Field && target.multiline => {
            return typing(target)
        }
        Interaction::Space if target.kind == Kind::Field && !target.checkable() => {
            return typing(target)
        }
        Interaction::Press | Interaction::Enter | Interaction::Space => match target.kind {
            Kind::Link | Kind::Disclosure | Kind::Tab => Read,
            Kind::Submit => target.submission(),
            // Enter in a field submits its form (implicit submission); a
            // field outside any form leaves Enter to page script.
            Kind::Field if interaction == Interaction::Enter => target.submission(),
            Kind::Field if target.focus_field() => Read,
            // A checkbox or radio: pressing or Space selects it.
            Kind::Field => Fill,
            Kind::Download | Kind::File | Kind::Other => Commit,
        },
    })
}

/// Typing into `target`: its form's fields under a GET form are a search a
/// plan may make while reading; any other field that is not secret needs
/// `fill`; typing where no field has focus needs `commit`, since a page's
/// key handlers may do anything.
fn typing(target: &TargetFacts) -> Option<Ceiling> {
    if target.secret {
        return None;
    }
    Some(match (target.kind, target.method) {
        (Kind::Field, Some(Method::Get)) => Ceiling::Read,
        (Kind::Field, _) => Ceiling::Fill,
        _ => Ceiling::Commit,
    })
}

/// Whether `ceiling` admits `interaction` on `target`.
pub(crate) fn admits(ceiling: Ceiling, interaction: Interaction, target: &TargetFacts) -> bool {
    required(interaction, target).is_some_and(|required| required <= ceiling)
}

/// What a key does to the focused element: `key` as a step names it, with
/// any modifiers (`Control+a`, `Shift+Enter`).
pub(crate) fn key_interaction(key: &str) -> Interaction {
    let mut parts: Vec<&str> = key.split('+').collect();
    // A chord's last part is its key; a lone "+" is the plus key.
    let key = match parts.pop() {
        Some("") if parts.last() == Some(&"") => "+",
        Some(key) => key,
        None => key,
    };
    match key {
        "Enter" | "NumpadEnter" | "Return" => Interaction::Enter,
        " " | "Space" | "Spacebar" => Interaction::Space,
        "Tab" | "Escape" | "Esc" | "ArrowUp" | "ArrowDown" | "ArrowLeft" | "ArrowRight" | "Up"
        | "Down" | "Left" | "Right" | "Home" | "End" | "PageUp" | "PageDown" | "Shift"
        | "Control" | "Ctrl" | "Alt" | "Meta" | "Cmd" | "Command" | "CapsLock" => {
            Interaction::Navigate
        }
        // A character, alone or in a chord (Control+a selects, Control+v
        // pastes), and the keys that delete one, edit text.
        "Backspace" | "Delete" => Interaction::Type,
        key if key.chars().count() == 1 => Interaction::Type,
        _ => Interaction::OtherKey,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Ceiling::{Commit, Fill, Read};

    fn target(kind: Kind) -> TargetFacts {
        TargetFacts {
            kind,
            method: None,
            input_type: None,
            multiline: false,
            secret: false,
        }
    }

    fn field(input_type: Option<&str>, method: Option<Method>) -> TargetFacts {
        TargetFacts {
            input_type: input_type.map(str::to_string),
            method,
            ..target(Kind::Field)
        }
    }

    /// Exactly the ceilings that admit a step: the contract's table, one
    /// column per ceiling.
    fn admitted(interaction: Interaction, target: &TargetFacts) -> [bool; 3] {
        [Read, Fill, Commit].map(|ceiling| admits(ceiling, interaction, target))
    }

    const ALL: [bool; 3] = [true, true, true];
    const FILL_UP: [bool; 3] = [false, true, true];
    const COMMIT_ONLY: [bool; 3] = [false, false, true];
    const NONE: [bool; 3] = [false, false, false];

    #[test]
    fn a_link_a_disclosure_or_a_tab_is_admitted_under_every_ceiling() {
        for kind in [Kind::Link, Kind::Disclosure, Kind::Tab] {
            for interaction in [Interaction::Press, Interaction::Enter] {
                assert_eq!(admitted(interaction, &target(kind)), ALL, "{kind:?}");
            }
        }
        assert_eq!(admitted(Interaction::Space, &target(Kind::Disclosure)), ALL);
    }

    #[test]
    fn a_get_form_is_admitted_under_every_ceiling() {
        let submit = TargetFacts {
            method: Some(Method::Get),
            ..target(Kind::Submit)
        };
        let search = field(Some("search"), Some(Method::Get));
        assert_eq!(admitted(Interaction::Press, &submit), ALL);
        assert_eq!(admitted(Interaction::Enter, &submit), ALL);
        assert_eq!(admitted(Interaction::Space, &submit), ALL);
        assert_eq!(admitted(Interaction::Enter, &search), ALL);
        assert_eq!(admitted(Interaction::Type, &search), ALL);
    }

    #[test]
    fn typing_or_selecting_into_another_field_needs_fill() {
        for other in [
            field(Some("text"), None),
            field(Some("email"), Some(Method::Post)),
            field(Some("text"), Some(Method::Dialog)),
            TargetFacts {
                multiline: true,
                ..field(None, None)
            },
            field(None, Some(Method::Post)),
        ] {
            assert_eq!(admitted(Interaction::Type, &other), FILL_UP, "{other:?}");
        }
        // A checkbox or radio is selected by a press or Space.
        for input_type in ["checkbox", "radio"] {
            let checkable = field(Some(input_type), Some(Method::Post));
            assert_eq!(admitted(Interaction::Press, &checkable), FILL_UP);
            assert_eq!(admitted(Interaction::Space, &checkable), FILL_UP);
        }
        // Enter in a textarea or an editable element adds a line.
        let textarea = TargetFacts {
            multiline: true,
            ..field(None, Some(Method::Post))
        };
        assert_eq!(admitted(Interaction::Enter, &textarea), FILL_UP);
        // Space in a text field types a space.
        assert_eq!(
            admitted(Interaction::Space, &field(Some("text"), Some(Method::Post))),
            FILL_UP
        );
    }

    #[test]
    fn a_post_or_dialog_form_submission_needs_commit() {
        for method in [Method::Post, Method::Dialog] {
            let submit = TargetFacts {
                method: Some(method),
                ..target(Kind::Submit)
            };
            assert_eq!(admitted(Interaction::Press, &submit), COMMIT_ONLY);
            assert_eq!(admitted(Interaction::Enter, &submit), COMMIT_ONLY);
            assert_eq!(admitted(Interaction::Space, &submit), COMMIT_ONLY);
            let text = field(Some("text"), Some(method));
            assert_eq!(admitted(Interaction::Enter, &text), COMMIT_ONLY);
        }
    }

    #[test]
    fn a_download_link_or_a_file_input_needs_commit() {
        assert_eq!(
            admitted(Interaction::Press, &target(Kind::Download)),
            COMMIT_ONLY
        );
        assert_eq!(
            admitted(Interaction::Enter, &target(Kind::Download)),
            COMMIT_ONLY
        );
        let file = TargetFacts {
            input_type: Some("file".into()),
            ..target(Kind::File)
        };
        assert_eq!(admitted(Interaction::Press, &file), COMMIT_ONLY);
        for kind in [Kind::File, Kind::Link, Kind::Other] {
            assert_eq!(
                admitted(Interaction::Transfer, &target(kind)),
                COMMIT_ONLY,
                "{kind:?}"
            );
        }
    }

    #[test]
    fn anything_else_needs_commit() {
        let plain = target(Kind::Other);
        for interaction in [
            Interaction::Press,
            Interaction::Enter,
            Interaction::Space,
            Interaction::Type,
        ] {
            assert_eq!(
                admitted(interaction, &plain),
                COMMIT_ONLY,
                "{interaction:?}"
            );
        }
        // Enter in a field outside any form is left to page script.
        assert_eq!(
            admitted(Interaction::Enter, &field(Some("text"), None)),
            COMMIT_ONLY
        );
        // A submit button outside any form is a plain button.
        assert_eq!(
            admitted(Interaction::Press, &target(Kind::Submit)),
            COMMIT_ONLY
        );
    }

    #[test]
    fn nothing_types_into_a_secret_field_under_any_ceiling() {
        for secret in [
            TargetFacts {
                secret: true,
                ..field(Some("password"), Some(Method::Post))
            },
            TargetFacts {
                secret: true,
                ..field(Some("text"), Some(Method::Get))
            },
            TargetFacts {
                secret: true,
                multiline: true,
                ..field(None, None)
            },
        ] {
            assert_eq!(admitted(Interaction::Type, &secret), NONE, "{secret:?}");
            assert_eq!(required(Interaction::Type, &secret), None);
            // Space would type into it; Enter in a multiline one too.
            if !secret.checkable() {
                assert_eq!(admitted(Interaction::Space, &secret), NONE);
            }
            // A press only focuses it, and hovering, scrolling and moving
            // focus away stay admitted.
            assert_eq!(admitted(Interaction::Press, &secret), ALL);
            assert_eq!(admitted(Interaction::Rest, &secret), ALL);
            assert_eq!(admitted(Interaction::Navigate, &secret), ALL);
        }
        // Submitting a login form from its password field is a commit.
        let password = TargetFacts {
            secret: true,
            ..field(Some("password"), Some(Method::Post))
        };
        assert_eq!(admitted(Interaction::Enter, &password), COMMIT_ONLY);
    }

    #[test]
    fn hover_focus_scrolling_a_focusing_press_and_navigation_keys_are_admitted_everywhere() {
        for kind in [
            Kind::Link,
            Kind::Download,
            Kind::Disclosure,
            Kind::Tab,
            Kind::Submit,
            Kind::Field,
            Kind::File,
            Kind::Other,
        ] {
            assert_eq!(admitted(Interaction::Rest, &target(kind)), ALL, "{kind:?}");
            assert_eq!(
                admitted(Interaction::Navigate, &target(kind)),
                ALL,
                "{kind:?}"
            );
        }
        for focus in [
            field(Some("text"), Some(Method::Post)),
            field(None, None),
            TargetFacts {
                multiline: true,
                ..field(None, Some(Method::Post))
            },
        ] {
            assert_eq!(admitted(Interaction::Press, &focus), ALL, "{focus:?}");
        }
    }

    #[test]
    fn keys_are_typing_submission_activation_or_navigation() {
        for key in ["Enter", "NumpadEnter", "Control+Enter", "Shift+Enter"] {
            assert_eq!(key_interaction(key), Interaction::Enter, "{key}");
        }
        for key in [" ", "Space"] {
            assert_eq!(key_interaction(key), Interaction::Space, "{key}");
        }
        for key in [
            "Tab",
            "Shift+Tab",
            "Escape",
            "ArrowDown",
            "Home",
            "PageDown",
            "Shift",
            "Control",
        ] {
            assert_eq!(key_interaction(key), Interaction::Navigate, "{key}");
        }
        for key in [
            "a",
            "Z",
            "é",
            "Backspace",
            "Delete",
            "Control+a",
            "Meta+v",
            "+",
            "Control++",
        ] {
            assert_eq!(key_interaction(key), Interaction::Type, "{key}");
        }
        for key in [
            "F5",
            "Control+F5",
            "BrowserBack",
            "MediaPlayPause",
            "ContextMenu",
            "Insert",
        ] {
            assert_eq!(key_interaction(key), Interaction::OtherKey, "{key}");
            assert_eq!(
                admitted(
                    key_interaction(key),
                    &field(Some("text"), Some(Method::Get))
                ),
                COMMIT_ONLY,
                "{key}"
            );
        }
    }

    #[test]
    fn the_ceilings_are_ordered_and_named_as_the_wire_names_them() {
        assert!(Read < Fill && Fill < Commit);
        for ceiling in [Read, Fill, Commit] {
            assert_eq!(
                serde_json::to_value(ceiling).unwrap(),
                serde_json::json!(ceiling.as_str())
            );
        }
        assert_eq!(
            serde_json::from_value::<Ceiling>(serde_json::json!("fill")).unwrap(),
            Fill
        );
    }
}
