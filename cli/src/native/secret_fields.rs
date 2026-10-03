//! Secret fields are the person's: a password, a one-time code, a payment
//! field. On the host-bound path the agent never enters a value into one or
//! reads what one holds, whichever transport carried the call. A call that
//! names such a field is refused before it does anything
//! (`named_field_refusal`); a key that would type into the field that has
//! focus is never sent, however focus got there (`FocusedField`, which
//! `BrowserControl` asks before each key the agent sends).
//!
//! A field is secret by the agent channel's own reading: `target::FACTS`,
//! read in the channel's isolated world, whose recorder keeps a field secret
//! once it was a password field. What a key does to it is judged as the
//! channel judges a step (`ceiling::key_interaction`, `ceiling::required`):
//! typing, a deletion, a paste or Space goes into a secret field under no
//! ceiling, while Tab, the arrows, Escape and Enter do not type. The reading
//! is structural, so its coverage is a measured rate: a site that takes a
//! password in a plain text field is its known miss. Page script is outside
//! it: `eval` and a Playwright program can still read or set a value by
//! script.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::broadcast;

use super::actions::{CommandError, DaemonState};
use super::agent_channel::ceiling::{self, Interaction, TargetFacts};
use super::agent_channel::target::{self, Recorders, FACTS, FOCUSED, WORLD};
use super::cdp::client::CdpClient;
use super::cdp::types::CdpEvent;
use super::documents::{self, NAVIGATION_PENDING, NAVIGATION_PENDING_MESSAGE};
use super::error_pages::ErrorPage;

/// The refusal of an effect no ceiling admits, as the channel answers it.
const REFUSED: &str = "browser_effect_refused";

/// A target that could not be read to be checked, as the channel answers it.
const UNREAD: &str = "browser_observation_stale";

/// Frames within frames followed to the element that has focus.
const MOST_FRAMES: usize = 8;

/// What a host-bound command does with a field before it acts.
enum FieldUse<'a> {
    /// Enters a value into the field its selector names: `fill`, `type` and
    /// `select`, which the channel judges as typing into that element.
    Enters(&'a str),
    /// Reads what the field its selector names holds: `get value`.
    Reads(&'a str),
    /// Types into whatever has focus, with a key that may type.
    Types,
}

fn field_use(command: &Value) -> Option<FieldUse<'_>> {
    match command["action"].as_str()? {
        "fill" | "type" | "select" => Some(FieldUse::Enters(command["selector"].as_str()?)),
        "inputvalue" => Some(FieldUse::Reads(command["selector"].as_str()?)),
        "press" | "keydown" => {
            may_type(ceiling::key_interaction(command["key"].as_str()?)).then_some(FieldUse::Types)
        }
        "keyboard" => Some(FieldUse::Types),
        _ => None,
    }
}

/// Whether the host path reads the page for `command` before it acts: the
/// field the command names, or the field that has focus before a key that
/// may type. Such a command is refused while the page is between documents
/// (`documents`), which answers nothing until it commits.
pub(crate) fn reads_page(command: &Value) -> bool {
    field_use(command).is_some()
}

/// The refusal of a host-bound call that enters a value into a secret field
/// it names or reads the value one holds, before the call does anything;
/// `None` admits it. A name that matches nothing is admitted, and the call
/// reports its own miss. A page a dialog blocks is not read: the call meets
/// that dialog's refusal instead.
pub(crate) async fn named_field_refusal(command: &Value, state: &DaemonState) -> Option<Value> {
    let (selector, reads) = match field_use(command)? {
        FieldUse::Enters(selector) => (selector, false),
        FieldUse::Reads(selector) => (selector, true),
        FieldUse::Types => return None,
    };
    let browser = state.browser.as_ref()?;
    if state.dialog_blocks_active_page() {
        return None;
    }
    let page = browser.active_session_id().ok()?;
    let client = &browser.client;
    super::element::set_active_frame(state.active_frame.as_ref());
    let (object, session) = super::element::resolve_element_object_id(
        client,
        page,
        &state.ref_map,
        selector,
        &state.iframe_sessions,
    )
    .await
    .ok()?;
    let recorders = state.browser_control.lock().await.recorders();
    let (code, error) = match element(client, &session, &object, &recorders).await {
        Ok(facts) if !facts.secret => return None,
        Ok(_) if reads => (
            REFUSED,
            format!("'{selector}' is a password, one-time-code or payment field: what it holds is the person's and is never read."),
        ),
        Ok(_) => (
            REFUSED,
            format!("'{selector}' is a password, one-time-code or payment field. The agent never types into one; it is the person's to fill. Nothing was done."),
        ),
        Err(error) => (
            UNREAD,
            format!("'{selector}' could not be read to check that it is not a password, one-time-code or payment field ({error}), so nothing was done. Observe the page again."),
        ),
    };
    Some(json!({ "id": command["id"], "success": false, "code": code, "error": error }))
}

/// What the element `object` of `session` structurally is, read in the
/// channel's world.
async fn element(
    client: &CdpClient,
    session: &str,
    object: &str,
    recorders: &Recorders,
) -> Result<TargetFacts, String> {
    let described = client
        .send_command(
            "DOM.describeNode",
            Some(json!({ "objectId": object })),
            Some(session),
        )
        .await?;
    let backend_node_id = described["node"]["backendNodeId"]
        .as_i64()
        .ok_or("the element is gone")?;
    let context = recorders.world(client, session).await?;
    let node = target::node(client, session, context, backend_node_id).await?;
    Ok(target::facts(client, &node).await?.target)
}

/// Why a key the agent would send never reaches the field that has focus.
#[derive(Debug, PartialEq)]
pub(crate) enum KeyRefusal {
    /// The field that has focus is secret, and the key would type into it.
    Secret,
    /// A JavaScript dialog opened for the page: nothing in it answers, and
    /// keys would go to the dialog, until someone answers it.
    Dialog,
    /// The page began loading another document: it answers nothing until
    /// that commits, and its keys would reach whichever document then shows.
    Leaving,
    /// The page shows Chrome's error page, where no key is the agent's to
    /// press (`error_pages`).
    ErrorPage(ErrorPage),
    /// The field that has focus could not be read.
    Unread(String),
}

impl KeyRefusal {
    /// The refusal as the command's failure: refused when none of the
    /// command's input was sent (`started` false) and the page was read,
    /// otherwise stopped after exactly `typed` of its `total` characters.
    pub(crate) fn stopped(self, started: bool, typed: usize, total: usize) -> CommandError {
        const SECRET: &str =
            "a password, one-time-code or payment field, which is the person's to fill";
        let why = match (self, started) {
            (Self::Secret, false) => {
                return format!("{REFUSED}: The field that has focus is {SECRET}. The agent never types into one; nothing was typed.").into()
            }
            (Self::Unread(error), false) => {
                return format!("{UNREAD}: The field that has focus could not be read ({error}), so nothing was typed. Observe the page again.").into()
            }
            (Self::Leaving, false) => {
                return format!("{NAVIGATION_PENDING}: {NAVIGATION_PENDING_MESSAGE}").into()
            }
            (Self::ErrorPage(page), false) => return page.refused(),
            (Self::Secret, true) => format!("focus moved into {SECRET}"),
            (Self::ErrorPage(page), true) => {
                format!("{} began showing, and agent input is refused on it", page.describe())
            }
            (Self::Unread(error), true) => format!("the field that has focus could not be read ({error})"),
            (Self::Leaving, true) => "the page began loading another document".to_string(),
            (Self::Dialog, _) => "a JavaScript dialog opened for the page; answer it first".to_string(),
        };
        CommandError::with_data(
            format!("browser_operation_interrupted: Typing stopped after {typed} of {total} characters: {why}. Inspect the page before continuing; do not replay the text."),
            json!({ "executionStopped": true, "effectsMayHaveOccurred": started, "charactersTyped": typed }),
        )
    }
}

/// What a keyboard event in the display helper's shape does to the field
/// that has focus, as the channel judges a key; `None` for an event that
/// neither presses nor enters anything (a key going up). A chord is judged
/// by its key, as the channel judges `Control+v`: a character in a chord
/// edits text.
fn key_interaction(event: &Value) -> Option<Interaction> {
    match event["eventType"].as_str()? {
        "insertText" => Some(Interaction::Type),
        "keyDown" | "rawKeyDown" | "char" => Some(
            event["key"]
                .as_str()
                .map_or(Interaction::Type, ceiling::key_interaction),
        ),
        _ => None,
    }
}

/// Whether some secret field refuses `interaction` under every ceiling
/// (`ceiling::required`): typing and Space into any field, Enter into one
/// that takes lines. Every other key is admitted whatever has focus, so it
/// needs no reading.
fn may_type(interaction: Interaction) -> bool {
    matches!(
        interaction,
        Interaction::Type | Interaction::Space | Interaction::Enter
    )
}

/// The facts of the element that has focus in a document, and whether it is
/// a frame whose own document holds the focus instead.
#[derive(Deserialize)]
struct Focused {
    #[serde(flatten)]
    target: TargetFacts,
    frame: bool,
}

/// The expression that reads `Focused` in the channel's world of a document.
fn focused_facts() -> String {
    format!(
        "(() => {{ const el = {FOCUSED}; const facts = {FACTS}(el); \
         facts.frame = el.localName === 'iframe' || el.localName === 'frame'; return facts; }})()"
    )
}

/// The field that has focus in a page, read in the channel's world of each
/// document focus passes through: a frame whose document this one cannot
/// read (another origin, or another process with its own session) is
/// followed into. A world's context is kept while its document lasts.
pub(crate) struct FocusedField {
    recorders: Arc<Recorders>,
    contexts: HashMap<(String, Option<String>), i64>,
}

impl FocusedField {
    pub(crate) fn new(recorders: Arc<Recorders>) -> Self {
        Self {
            recorders,
            contexts: HashMap::new(),
        }
    }

    /// Why `event`, a keyboard event in the display helper's shape, must not
    /// reach the field that has focus in `session`'s page; `None` when it
    /// may. No key is pressed while the page shows Chrome's error page. An
    /// event that cannot type is otherwise admitted without a reading. A
    /// JavaScript dialog opening for the page, or the page beginning to load
    /// another document, ends the reading: the page answers nothing until
    /// the dialog is answered or the document commits.
    pub(crate) async fn refuses(
        &mut self,
        client: &CdpClient,
        session: &str,
        event: &Value,
        events: &mut broadcast::Receiver<CdpEvent>,
    ) -> Option<KeyRefusal> {
        let interaction = key_interaction(event)?;
        if let Some(page) = client.error_page(session) {
            return Some(KeyRefusal::ErrorPage(page));
        }
        if !may_type(interaction) {
            return None;
        }
        let page = client.page_of(session);
        let facts = tokio::select! {
            biased;
            blocked = page_blocks(client, &page, events) => return Some(blocked),
            facts = self.facts(client, &page) => facts,
        };
        match facts {
            Ok(facts) if ceiling::required(interaction, &facts).is_none() => {
                Some(KeyRefusal::Secret)
            }
            Ok(_) => None,
            Err(error) => Some(KeyRefusal::Unread(error)),
        }
    }

    async fn facts(&mut self, client: &CdpClient, page: &str) -> Result<TargetFacts, String> {
        let (mut session, mut frame) = (page.to_owned(), None::<String>);
        for _ in 0..MOST_FRAMES {
            let read = self
                .evaluate(client, &session, frame.as_deref(), &focused_facts(), true)
                .await?;
            let focused: Focused =
                serde_json::from_value(read["value"].clone()).map_err(|error| error.to_string())?;
            if !focused.frame {
                return Ok(focused.target);
            }
            let element = self
                .evaluate(client, &session, frame.as_deref(), FOCUSED, false)
                .await?;
            let object = element["objectId"]
                .as_str()
                .ok_or("the focused frame is gone")?;
            let described = client
                .send_command(
                    "DOM.describeNode",
                    Some(json!({ "objectId": object })),
                    Some(&session),
                )
                .await?;
            // A frame without a document of its own holds no focus.
            let Some(child) = described["node"]["frameId"].as_str() else {
                return Ok(focused.target);
            };
            // A frame in another process is a target with its own session.
            if let Some(own) = client.session_for_target(child) {
                (session, frame) = (own, None);
            } else {
                frame = Some(child.to_owned());
            }
        }
        Err("focus is nested in too many frames".into())
    }

    /// Evaluates `expression` in the channel's world of `frame` (the main
    /// frame when `None`) of `session`: in the context kept for it, and in a
    /// fresh one when that context's document is gone.
    async fn evaluate(
        &mut self,
        client: &CdpClient,
        session: &str,
        frame: Option<&str>,
        expression: &str,
        by_value: bool,
    ) -> Result<Value, String> {
        let key = (session.to_owned(), frame.map(str::to_owned));
        let read = |context: i64| {
            client.send_command(
                "Runtime.evaluate",
                Some(json!({ "expression": expression, "contextId": context,
                    "returnByValue": by_value, "objectGroup": WORLD })),
                Some(session),
            )
        };
        if let Some(&context) = self.contexts.get(&key) {
            if let Ok(result) = read(context).await {
                if result.get("exceptionDetails").is_none() {
                    return Ok(result["result"].clone());
                }
            }
        }
        let context = match frame {
            Some(frame) => self.recorders.frame_world(client, session, frame).await?,
            None => self.recorders.world(client, session).await?,
        };
        self.contexts.insert(key, context);
        let result = read(context).await?;
        if result.get("exceptionDetails").is_some() {
            return Err("the page threw while its focused field was read".into());
        }
        Ok(result["result"].clone())
    }
}

/// Completes when a JavaScript dialog opens for `page` or one of its
/// frames, or when the page's main frame begins loading another document;
/// never when the events end.
async fn page_blocks(
    client: &CdpClient,
    page: &str,
    events: &mut broadcast::Receiver<CdpEvent>,
) -> KeyRefusal {
    let main_frame = client.target_for_session(page);
    loop {
        match events.recv().await {
            Ok(event)
                if event.method == "Page.javascriptDialogOpening"
                    && event
                        .session_id
                        .as_deref()
                        .is_none_or(|session| client.page_of(session) == page) =>
            {
                return KeyRefusal::Dialog
            }
            Ok(event)
                if event.session_id.as_deref() == Some(page)
                    && main_frame.as_deref().is_some_and(|frame| {
                        documents::change(&event, frame) == Some(documents::Change::Leaving)
                    }) =>
            {
                return KeyRefusal::Leaving
            }
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => std::future::pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::agent_channel::ceiling::Kind;

    fn keyboard(event_type: &str, key: Option<&str>, text: Option<&str>) -> Value {
        let mut event =
            json!({ "type": "input_keyboard", "eventType": event_type, "modifiers": 0 });
        if let Some(key) = key {
            event["key"] = json!(key);
        }
        if let Some(text) = text {
            event["text"] = json!(text);
        }
        event
    }

    /// Keys are judged as the channel judges a step's key: text, a paste,
    /// a deletion, a character chord and Space type; Tab, the arrows,
    /// Escape and a modifier alone navigate; Enter is Enter; a key going up
    /// is nothing.
    #[test]
    fn keys_are_judged_as_the_channel_judges_them() {
        for (event, expected) in [
            (
                keyboard("insertText", None, Some("hunter2")),
                Some(Interaction::Type),
            ),
            (
                keyboard("keyDown", Some("a"), Some("a")),
                Some(Interaction::Type),
            ),
            (
                keyboard("keyDown", Some("v"), None),
                Some(Interaction::Type),
            ),
            (
                keyboard("keyDown", Some("Backspace"), None),
                Some(Interaction::Type),
            ),
            (keyboard("char", None, Some("x")), Some(Interaction::Type)),
            (
                keyboard("keyDown", Some(" "), Some(" ")),
                Some(Interaction::Space),
            ),
            (
                keyboard("keyDown", Some("Enter"), None),
                Some(Interaction::Enter),
            ),
            (
                keyboard("rawKeyDown", Some("Tab"), None),
                Some(Interaction::Navigate),
            ),
            (
                keyboard("keyDown", Some("ArrowLeft"), None),
                Some(Interaction::Navigate),
            ),
            (
                keyboard("keyDown", Some("Escape"), None),
                Some(Interaction::Navigate),
            ),
            (
                keyboard("keyDown", Some("Shift"), None),
                Some(Interaction::Navigate),
            ),
            (
                keyboard("keyDown", Some("F5"), None),
                Some(Interaction::OtherKey),
            ),
            (keyboard("keyUp", Some("a"), None), None),
        ] {
            assert_eq!(key_interaction(&event), expected, "{event}");
        }
    }

    /// The host path reads the page for a command that names a field it
    /// enters or reads, and for keys that may type into the focused field;
    /// never for a key that cannot type or a command about no field.
    #[test]
    fn the_host_path_reads_the_page_for_a_named_field_and_keys_that_may_type() {
        for (command, reads) in [
            (
                json!({ "action": "fill", "selector": "#pw", "value": "x" }),
                true,
            ),
            (
                json!({ "action": "type", "selector": "#pw", "text": "x" }),
                true,
            ),
            (
                json!({ "action": "select", "selector": "#month", "values": ["02"] }),
                true,
            ),
            (json!({ "action": "inputvalue", "selector": "#pw" }), true),
            (json!({ "action": "press", "key": "a" }), true),
            (json!({ "action": "press", "key": "Control+v" }), true),
            (json!({ "action": "press", "key": "Enter" }), true),
            (json!({ "action": "keydown", "key": " " }), true),
            (
                json!({ "action": "keyboard", "subaction": "type", "text": "x" }),
                true,
            ),
            (json!({ "action": "press", "key": "Tab" }), false),
            (json!({ "action": "keydown", "key": "Shift" }), false),
            (json!({ "action": "keyup", "key": "a" }), false),
            (json!({ "action": "click", "selector": "#pw" }), false),
            (json!({ "action": "fill" }), false),
        ] {
            assert_eq!(reads_page(&command), reads, "{command}");
        }
    }

    /// Reading the focused field only for the keys `may_type` names loses
    /// nothing: every other interaction is admitted on every secret target
    /// under some ceiling, and each named one is refused on some.
    #[test]
    fn only_the_keys_that_may_type_need_a_reading() {
        let targets: Vec<TargetFacts> = [Kind::Field, Kind::Submit, Kind::Other, Kind::Link]
            .into_iter()
            .flat_map(|kind| {
                [None, Some("password"), Some("checkbox")]
                    .into_iter()
                    .flat_map(move |input_type| {
                        [false, true].map(|multiline| TargetFacts {
                            kind,
                            method: None,
                            input_type: input_type.map(str::to_owned),
                            multiline,
                            secret: true,
                        })
                    })
            })
            .collect();
        for interaction in [
            Interaction::Rest,
            Interaction::Navigate,
            Interaction::Press,
            Interaction::Type,
            Interaction::Enter,
            Interaction::Space,
            Interaction::Transfer,
            Interaction::OtherKey,
        ] {
            let refused = targets
                .iter()
                .any(|target| ceiling::required(interaction, target).is_none());
            assert_eq!(refused, may_type(interaction), "{interaction:?}");
        }
    }

    /// A refusal before any input says nothing was typed under the channel's
    /// codes; a stop says exactly how many characters went in, and whether
    /// any input did.
    #[test]
    fn a_refusal_before_input_is_refused_and_after_input_is_a_stop() {
        let refused = KeyRefusal::Secret.stopped(false, 0, 7);
        assert!(
            refused
                .error
                .starts_with("browser_effect_refused: The field that has focus is a password"),
            "{}",
            refused.error
        );
        assert_eq!(refused.data, None);
        let unread = KeyRefusal::Unread("gone".into()).stopped(false, 0, 7);
        assert!(
            unread.error.starts_with("browser_observation_stale: "),
            "{}",
            unread.error
        );
        let stopped = KeyRefusal::Secret.stopped(true, 4, 11);
        assert!(stopped.error.starts_with("browser_operation_interrupted: Typing stopped after 4 of 11 characters: focus moved into a password"), "{}", stopped.error);
        assert_eq!(
            stopped.data,
            Some(
                json!({ "executionStopped": true, "effectsMayHaveOccurred": true, "charactersTyped": 4 })
            )
        );
        let leaving = KeyRefusal::Leaving.stopped(false, 0, 3);
        assert!(
            leaving.error.starts_with("browser_navigation_pending: "),
            "{}",
            leaving.error
        );
        assert!(KeyRefusal::Leaving
            .stopped(true, 2, 3)
            .error
            .contains("after 2 of 3 characters: the page began loading another document"));
        let dialog = KeyRefusal::Dialog.stopped(false, 0, 3);
        assert!(dialog.error.starts_with("browser_operation_interrupted: Typing stopped after 0 of 3 characters: a JavaScript dialog"), "{}", dialog.error);
        assert_eq!(
            dialog.data,
            Some(
                json!({ "executionStopped": true, "effectsMayHaveOccurred": false, "charactersTyped": 0 })
            )
        );
    }
}
