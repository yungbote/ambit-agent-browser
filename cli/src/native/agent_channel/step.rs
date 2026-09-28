//! A `sequence`'s steps made ready to run (agent-channel contract §4
//! "Execution" rule 1): every step is validated before any runs, by the same
//! host-bound preparation the spawned client applies to a call, then against
//! the channel's own rules: the operations it excludes, and the
//! preconditions a judged step must carry.

use serde_json::Value;

use super::ceiling::{key_interaction, Ceiling, Interaction};
use super::frame::{Preconditions, Step};
use crate::mcp::host_bound::{self, HostCall, HostFlags};

/// Host-bound tools this endpoint refuses. A Playwright program stays on the
/// file protocol: its runtime paths belong to the spawned client's
/// configuration, and its completion is not a channel reply yet.
pub(crate) const EXCLUDED_OPS: &[&str] = &["agent_browser_run_playwright"];

/// A step ready to run: its host-bound call, and how the daemon judges it.
#[derive(Debug, Clone)]
pub(crate) struct PreparedStep {
    pub(crate) op: String,
    pub(crate) call: HostCall,
    pub(crate) preconditions: Preconditions,
    pub(crate) judged: Option<Judged>,
}

/// What a judged step does, to what, under which ceiling.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Judged {
    pub(crate) interaction: Interaction,
    pub(crate) target: Target,
    pub(crate) ceiling: Ceiling,
}

/// The element a judged step acts on.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Target {
    /// The node its `selector` resolves to (a CSS or XPath selector, or a ref).
    Selector(String),
    /// Whatever has focus: the keyboard steps.
    Focus,
    /// The node under the pointer: a button pressed or released in place.
    Pointer,
}

/// A step refused before any step ran: which, and why.
#[derive(Debug, PartialEq)]
pub(crate) struct Rejected {
    pub(crate) step: usize,
    pub(crate) message: String,
}

/// Prepares every step of a sequence, or names the first one refused.
pub(crate) fn prepare(flags: &HostFlags, steps: &[Step]) -> Result<Vec<PreparedStep>, Rejected> {
    steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            prepare_step(flags, step).map_err(|message| Rejected {
                step: index,
                message,
            })
        })
        .collect()
}

fn prepare_step(flags: &HostFlags, step: &Step) -> Result<PreparedStep, String> {
    let call = flags.prepare(&step.op, &step.arguments)?;
    if EXCLUDED_OPS.contains(&step.op.as_str()) {
        return Err(format!(
            "{} runs on the file protocol, not on the agent channel.",
            step.op
        ));
    }
    let judged = judge(&step.op, &step.arguments, read_only(&step.op))
        .map(|(interaction, target)| {
            required_preconditions(&step.preconditions, &target).map(|ceiling| Judged {
                interaction,
                target,
                ceiling,
            })
        })
        .transpose()?;
    Ok(PreparedStep {
        op: step.op.clone(),
        call,
        preconditions: step.preconditions.clone(),
        judged,
    })
}

/// Whether the profile's tool `op` only reads (its `readOnlyHint`).
pub(crate) fn read_only(op: &str) -> bool {
    host_bound::tool(op).is_some_and(|tool| tool["annotations"]["readOnlyHint"] == true)
}

/// A judged step carries `pageGeneration` and `effects`, and `backendNodeId`
/// when it names its element; under `commit` it names its node whatever its
/// kind, and is admitted only on that node.
fn required_preconditions(
    preconditions: &Preconditions,
    target: &Target,
) -> Result<Ceiling, String> {
    let (Some(_), Some(ceiling)) = (&preconditions.page_generation, preconditions.effects) else {
        return Err(
            "A step that acts on an element carries the pageGeneration and effects preconditions."
                .into(),
        );
    };
    if preconditions.backend_node_id.is_none()
        && (matches!(target, Target::Selector(_)) || ceiling == Ceiling::Commit)
    {
        return Err(
            "A step that names an element, or any judged step under commit, carries the backendNodeId precondition."
                .into(),
        );
    }
    Ok(ceiling)
}

/// Whether the daemon judges a step, and what it does to which target:
/// non-read steps that name an element by `selector`, the keyboard steps,
/// and pointer buttons pressed or released in place.
pub(crate) fn judge(op: &str, arguments: &Value, read_only: bool) -> Option<(Interaction, Target)> {
    let keyboard = |interaction| Some((interaction, Target::Focus));
    match op {
        "agent_browser_press" | "agent_browser_keydown" | "agent_browser_keyup" => keyboard(
            key_interaction(arguments["key"].as_str().unwrap_or_default()),
        ),
        "agent_browser_keyboard_type" | "agent_browser_keyboard_insert_text" => {
            keyboard(Interaction::Type)
        }
        "agent_browser_mouse_down" | "agent_browser_mouse_up" => {
            Some((Interaction::Press, Target::Pointer))
        }
        _ => {
            let selector = arguments["selector"].as_str().filter(|_| !read_only)?;
            Some((
                selector_interaction(op),
                Target::Selector(selector.to_string()),
            ))
        }
    }
}

/// What a non-read step that names an element does to it. An operation not
/// listed here is judged as pressing its target, which only the weakest
/// targets admit below `commit`.
fn selector_interaction(op: &str) -> Interaction {
    match op {
        "agent_browser_click"
        | "agent_browser_dblclick"
        | "agent_browser_tap"
        | "agent_browser_check"
        | "agent_browser_uncheck" => Interaction::Press,
        "agent_browser_fill" | "agent_browser_type" | "agent_browser_select" => Interaction::Type,
        "agent_browser_upload" | "agent_browser_download" => Interaction::Transfer,
        // Observing or pointing at an element acts on nothing in the page.
        "agent_browser_hover"
        | "agent_browser_focus"
        | "agent_browser_scroll"
        | "agent_browser_scroll_into_view"
        | "agent_browser_highlight"
        | "agent_browser_screenshot"
        | "agent_browser_a11y"
        | "agent_browser_diff_screenshot"
        | "agent_browser_diff_snapshot"
        | "agent_browser_diff_url" => Interaction::Rest,
        _ => Interaction::Press,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn flags() -> HostFlags {
        HostFlags::new("host", "browser", None).unwrap()
    }

    fn step(op: &str, arguments: Value, preconditions: Value) -> Step {
        Step {
            op: op.into(),
            arguments,
            preconditions: serde_json::from_value(preconditions).unwrap(),
        }
    }

    const JUDGED: &str = r#"{"pageGeneration":"G","effects":"read","backendNodeId":530}"#;

    fn judged() -> Value {
        serde_json::from_str(JUDGED).unwrap()
    }

    /// The worked example's frame: navigation and a read carry no
    /// preconditions and are not judged.
    #[test]
    fn named_steps_are_prepared_as_the_spawned_client_prepares_them() {
        let prepared = prepare(
            &flags(),
            &[
                step(
                    "agent_browser_open",
                    json!({ "url": "https://example.com/pricing" }),
                    json!({}),
                ),
                step(
                    "agent_browser_get_text",
                    json!({ "selector": "main" }),
                    json!({}),
                ),
            ],
        )
        .unwrap();
        assert_eq!(prepared[0].call.command["action"], "navigate");
        assert!(prepared[0].judged.is_none() && !read_only(&prepared[0].op));
        assert_eq!(prepared[1].call.command["action"], "gettext");
        assert!(prepared[1].judged.is_none() && read_only(&prepared[1].op));
    }

    #[test]
    fn a_frame_is_refused_at_its_first_invalid_step_before_any_runs() {
        let open = step(
            "agent_browser_open",
            json!({ "url": "https://example.com/" }),
            json!({}),
        );
        for (bad, message) in [
            (
                step("agent_browser_batch", json!({}), json!({})),
                "Tool is not in the host-bound browser profile.",
            ),
            (
                step(
                    "agent_browser_click",
                    json!({ "selector": "#go", "session": "x" }),
                    judged(),
                ),
                "Tool arguments cannot override host browser settings.",
            ),
            (
                step(
                    "agent_browser_get_text",
                    json!({ "selector": "main", "timeoutMs": 120001 }),
                    json!({}),
                ),
                "timeoutMs must be at most 120000.",
            ),
            (
                step(
                    "agent_browser_run_playwright",
                    json!({ "code": "return 1" }),
                    json!({}),
                ),
                "agent_browser_run_playwright runs on the file protocol, not on the agent channel.",
            ),
        ] {
            let refused = prepare(&flags(), &[open.clone(), bad.clone()]).unwrap_err();
            assert_eq!(
                refused,
                Rejected {
                    step: 1,
                    message: message.into()
                },
                "{}",
                bad.op
            );
        }
    }

    #[test]
    fn a_judged_step_without_its_preconditions_is_refused() {
        let click = |preconditions: Value| {
            prepare(
                &flags(),
                &[step(
                    "agent_browser_click",
                    json!({ "selector": "@e21" }),
                    preconditions,
                )],
            )
        };
        assert!(click(judged()).is_ok());
        for missing in [
            json!({ "effects": "read", "backendNodeId": 530 }),
            json!({ "pageGeneration": "G", "backendNodeId": 530 }),
            json!({ "pageGeneration": "G", "effects": "read" }),
            json!({}),
        ] {
            assert_eq!(click(missing.clone()).unwrap_err().step, 0, "{missing}");
        }
        // A keyboard step names its node only under commit.
        let press = |preconditions: Value| {
            prepare(
                &flags(),
                &[step(
                    "agent_browser_press",
                    json!({ "key": "Enter" }),
                    preconditions,
                )],
            )
        };
        assert!(press(json!({ "pageGeneration": "G", "effects": "fill" })).is_ok());
        assert!(press(json!({ "pageGeneration": "G", "effects": "commit" })).is_err());
        assert!(
            press(json!({ "pageGeneration": "G", "effects": "commit", "backendNodeId": 3 }))
                .is_ok()
        );
        assert!(press(json!({})).is_err());
    }

    #[test]
    fn every_judged_step_of_the_profile_says_what_it_does_to_which_target() {
        let explicit = [
            "agent_browser_click",
            "agent_browser_dblclick",
            "agent_browser_tap",
            "agent_browser_check",
            "agent_browser_uncheck",
            "agent_browser_fill",
            "agent_browser_type",
            "agent_browser_select",
            "agent_browser_upload",
            "agent_browser_download",
            "agent_browser_hover",
            "agent_browser_focus",
            "agent_browser_scroll",
            "agent_browser_scroll_into_view",
            "agent_browser_highlight",
            "agent_browser_screenshot",
            "agent_browser_a11y",
            "agent_browser_diff_screenshot",
            "agent_browser_diff_snapshot",
            "agent_browser_diff_url",
        ];
        let mut selector_steps = Vec::new();
        for tool in crate::mcp::host_bound::tools() {
            let name = tool["name"].as_str().unwrap();
            let read_only = tool["annotations"]["readOnlyHint"] == true;
            if tool["inputSchema"]["properties"].get("selector").is_some() && !read_only {
                selector_steps.push(name.to_string());
                assert!(
                    explicit.contains(&name),
                    "{name} names an element and is not in the judged table"
                );
            }
        }
        for name in explicit {
            assert!(selector_steps.iter().any(|step| step == name), "{name}");
        }
        let with = |op: &str| judge(op, &json!({ "selector": "#x", "key": "a" }), false);
        assert_eq!(
            with("agent_browser_click"),
            Some((Interaction::Press, Target::Selector("#x".into())))
        );
        assert_eq!(with("agent_browser_fill").unwrap().0, Interaction::Type);
        assert_eq!(
            with("agent_browser_upload").unwrap().0,
            Interaction::Transfer
        );
        assert_eq!(with("agent_browser_hover").unwrap().0, Interaction::Rest);
        assert_eq!(
            with("agent_browser_press"),
            Some((Interaction::Type, Target::Focus))
        );
        assert_eq!(
            judge("agent_browser_press", &json!({ "key": "Tab" }), false),
            Some((Interaction::Navigate, Target::Focus))
        );
        assert_eq!(
            with("agent_browser_keyboard_insert_text"),
            Some((Interaction::Type, Target::Focus))
        );
        assert_eq!(
            with("agent_browser_mouse_up"),
            Some((Interaction::Press, Target::Pointer))
        );
        // A read never is, whatever it names; nor is a step naming nothing.
        assert_eq!(
            judge("agent_browser_get_text", &json!({ "selector": "#x" }), true),
            None
        );
        assert_eq!(
            judge(
                "agent_browser_scroll",
                &json!({ "direction": "down" }),
                false
            ),
            None
        );
        assert_eq!(
            judge("agent_browser_open", &json!({ "url": "https://x/" }), false),
            None
        );
    }
}
