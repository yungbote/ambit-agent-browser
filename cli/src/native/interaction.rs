use std::collections::HashMap;

use serde_json::{json, Value};

use super::actions::CommandError;
use super::browser_control::BrowserControl;
use super::cdp::client::CdpClient;
use super::cdp::types::*;
use super::element::{resolve_element_center, resolve_element_object_id, RefMap};
use tokio::sync::Mutex;

/// Outcome of a click. `dialog_opened` is true if a JavaScript dialog opened
/// mid-sequence (the page is then blocked until `dialog accept`/`dismiss`).
/// `pending_release` is set only when the dialog opened after mousePressed but
/// before mouseReleased: the button is logically held until the caller
/// dispatches the release (done once the dialog is resolved), otherwise the
/// next click would register as a drag or double-click.
#[derive(Default)]
pub struct ClickResult {
    pub dialog_opened: bool,
    pub pending_release: Option<PendingRelease>,
}

pub struct PendingRelease {
    pub session_id: String,
    pub x: f64,
    pub y: f64,
    pub button: String,
}

/// The point native pointer input on an element goes to, in its session:
/// the owned window's wheel first brings it into view, as a person scrolls
/// to what they click, so the page never jumps; then its centre is read
/// where it is. `None` when a JavaScript dialog opened on the way.
pub(crate) async fn native_point(
    client: &CdpClient,
    control: &Mutex<BrowserControl>,
    session_id: &str,
    (object_id, effective_session_id): (&str, &str),
    target: &str,
) -> Result<Option<(f64, f64)>, CommandError> {
    if control
        .lock()
        .await
        .agent_native_scroll_into_view(client, session_id, (object_id, effective_session_id))
        .await?
    {
        return Ok(None);
    }
    Ok(Some(
        super::element::object_center(client, effective_session_id, object_id, target).await?,
    ))
}

#[allow(clippy::too_many_arguments)]
pub async fn click(
    client: &CdpClient,
    control: &Mutex<BrowserControl>,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    button: &str,
    click_count: i32,
    iframe_sessions: &HashMap<String, String>,
) -> Result<ClickResult, CommandError> {
    let native = control.lock().await.has_native_display();
    let (x, y, effective_session_id) = if native {
        let (object_id, effective_session_id) = resolve_element_object_id(
            client,
            session_id,
            ref_map,
            selector_or_ref,
            iframe_sessions,
        )
        .await?;
        let target = (object_id.as_str(), effective_session_id.as_str());
        let Some((x, y)) =
            native_point(client, control, session_id, target, selector_or_ref).await?
        else {
            return Ok(ClickResult {
                dialog_opened: true,
                pending_release: None,
            });
        };
        (x, y, effective_session_id)
    } else {
        resolve_element_center(
            client,
            session_id,
            ref_map,
            selector_or_ref,
            iframe_sessions,
        )
        .await?
    };
    if native && click_count > 1 {
        for _ in 0..click_count {
            let result = dispatch_click(
                client,
                control,
                &effective_session_id,
                &[effective_session_id.as_str(), session_id],
                x,
                y,
                button,
                1,
            )
            .await?;
            if result.dialog_opened {
                return Ok(result);
            }
        }
        return Ok(ClickResult::default());
    }
    // A click-triggered dialog can fire on the frame's own session (OOPIF) or
    // on the top-level page session; both count as "ours". A dialog on any
    // other session belongs to a background tab and must not abort this click.
    dispatch_click(
        client,
        control,
        &effective_session_id,
        &[effective_session_id.as_str(), session_id],
        x,
        y,
        button,
        click_count,
    )
    .await
}

pub async fn dblclick(
    client: &CdpClient,
    control: &Mutex<BrowserControl>,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<ClickResult, CommandError> {
    click(
        client,
        control,
        session_id,
        ref_map,
        selector_or_ref,
        "left",
        2,
        iframe_sessions,
    )
    .await
}

pub async fn hover(
    client: &CdpClient,
    control: &Mutex<BrowserControl>,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(), CommandError> {
    if control.lock().await.has_native_display() {
        let (object_id, effective_session_id) = resolve_element_object_id(
            client,
            session_id,
            ref_map,
            selector_or_ref,
            iframe_sessions,
        )
        .await?;
        let target = (object_id.as_str(), effective_session_id.as_str());
        let Some((x, y)) =
            native_point(client, control, session_id, target, selector_or_ref).await?
        else {
            return Ok(());
        };
        control
            .lock()
            .await
            .agent_native_mouse(
                serde_json::json!({"type":"mouseMoved","x":x,"y":y,"buttons":0}),
                client,
                &effective_session_id,
                &[effective_session_id.as_str(), session_id],
            )
            .await?;
        return Ok(());
    }
    let (x, y, effective_session_id) = resolve_element_center(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;
    client
        .send_command_typed::<_, Value>(
            "Input.dispatchMouseEvent",
            &DispatchMouseEventParams {
                event_type: "mouseMoved".to_string(),
                x,
                y,
                button: None,
                buttons: None,
                click_count: None,
                delta_x: None,
                delta_y: None,
                modifiers: None,
            },
            Some(&effective_session_id),
        )
        .await?;
    Ok(())
}

pub async fn fill(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    value: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(), String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    // Focus the element
    client
        .send_command_typed::<_, Value>(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: "function() { this.focus(); }".to_string(),
                object_id: Some(object_id.clone()),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    // Select all + delete to clear
    client
        .send_command_typed::<_, Value>(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: r#"function() {
                    this.select && this.select();
                    this.value = '';
                    this.dispatchEvent(new Event('input', { bubbles: true }));
                }"#
                .to_string(),
                object_id: Some(object_id),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    // Insert text (keyboard input dispatched at page level, use parent session_id)
    client
        .send_command_typed::<_, Value>(
            "Input.insertText",
            &InsertTextParams {
                text: value.to_string(),
            },
            Some(session_id),
        )
        .await?;

    Ok(())
}

/// A field's state as its page reports it: whether it holds keyboard focus,
/// what text it holds and whether that equals `expected`. A secret field
/// (a password, a one-time code, a card number) never reports its text.
const FIELD_STATE: &str = r#"function(expected) {
    let active = document.activeElement;
    while (active && active.shadowRoot && active.shadowRoot.activeElement) active = active.shadowRoot.activeElement;
    let focused = false;
    for (let node = active; node; node = node.parentNode || node.host || null) {
        if (node === this) { focused = true; break; }
    }
    const text = typeof this.value === 'string' ? this.value
        : this.isContentEditable ? this.innerText : (this.textContent ?? '');
    const kind = String(this.type || '').toLowerCase();
    const autocomplete = String(this.autocomplete || '').toLowerCase();
    const secret = kind === 'password'
        || /(^|\s)(one-time-code|current-password|new-password|cc-number|cc-csc|cc-exp)(\s|$)/.test(autocomplete);
    return { focused, empty: text.length === 0, multiline: this.tagName === 'TEXTAREA' || this.isContentEditable,
        matches: text === expected, value: secret ? null : text };
}"#;

/// A field's state, from `FIELD_STATE`.
pub struct FieldState {
    pub focused: bool,
    pub empty: bool,
    pub multiline: bool,
    /// Whether the field holds exactly the text the caller entered.
    pub matches: bool,
    /// The text it holds; `None` for a secret field.
    pub value: Option<String>,
}

/// How long a field is read again while the page catches up with native
/// input: key and pointer events reach the renderer on their own path, not
/// behind the DevTools command that reads it.
const FIELD_SETTLE: std::time::Duration = std::time::Duration::from_millis(150);
const FIELD_POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// Reads the field until `settled` holds for its state or `FIELD_SETTLE`
/// passes, and returns the last state read.
async fn field_state(
    client: &CdpClient,
    session_id: &str,
    object_id: &str,
    expected: &str,
    settled: impl Fn(&FieldState) -> bool,
) -> Result<FieldState, String> {
    let deadline = tokio::time::Instant::now() + FIELD_SETTLE;
    loop {
        let read = client
            .send_command(
                "Runtime.callFunctionOn",
                Some(json!({
                    "objectId": object_id, "functionDeclaration": FIELD_STATE,
                    "arguments": [{ "value": expected }], "returnByValue": true,
                })),
                Some(session_id),
            )
            .await?;
        let value = &read["result"]["value"];
        if read.get("exceptionDetails").is_some() || !value.is_object() {
            return Err("The field could not be read.".into());
        }
        let state = FieldState {
            focused: value["focused"] == true,
            empty: value["empty"] == true,
            multiline: value["multiline"] == true,
            matches: value["matches"] == true,
            value: value["value"].as_str().map(str::to_owned),
        };
        if settled(&state) || tokio::time::Instant::now() >= deadline {
            return Ok(state);
        }
        tokio::time::sleep(FIELD_POLL).await;
    }
}

/// A field the pointer clicked, ready for keys.
const TEXT_FIELD: &str = "this.isContentEditable || this.matches('textarea,input:not([type=button],[type=submit],[type=reset],[type=image],[type=checkbox],[type=radio],[type=range],[type=color],[type=file],[type=hidden])')";

struct ClickedField {
    object_id: String,
    session_id: String,
    state: FieldState,
}

enum Clicked {
    /// The click opened a JavaScript dialog; nothing was typed.
    Dialog(ClickResult),
    Field(ClickedField),
}

/// Clicks the field `selector_or_ref` names as a person does before typing
/// into it: the pointer travels to its centre and presses. The field must
/// then hold keyboard focus, or nothing is typed.
async fn click_field(
    client: &CdpClient,
    control: &Mutex<BrowserControl>,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
    text_only: bool,
) -> Result<Clicked, CommandError> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;
    if text_only {
        let read = client
            .send_command(
                "Runtime.callFunctionOn",
                Some(json!({
                    "objectId": object_id, "returnByValue": true,
                    "functionDeclaration": format!("function() {{ return {TEXT_FIELD}; }}"),
                })),
                Some(&effective_session_id),
            )
            .await?;
        if read["result"]["value"] != true {
            return Err(
                "Choose a text field; no control was clicked and nothing was typed.".into(),
            );
        }
    }
    let target = (object_id.as_str(), effective_session_id.as_str());
    let Some((x, y)) = native_point(client, control, session_id, target, selector_or_ref).await?
    else {
        return Ok(Clicked::Dialog(ClickResult {
            dialog_opened: true,
            pending_release: None,
        }));
    };
    let click = dispatch_click(
        client,
        control,
        &effective_session_id,
        &[effective_session_id.as_str(), session_id],
        x,
        y,
        "left",
        1,
    )
    .await?;
    if click.dialog_opened {
        return Ok(Clicked::Dialog(click));
    }
    let state = field_state(client, &effective_session_id, &object_id, "", |state| {
        state.focused
    })
    .await?;
    if !state.focused {
        return Err(format!("'{selector_or_ref}' did not take keyboard focus when it was clicked, so nothing was typed. Choose the editable field itself.").into());
    }
    Ok(Clicked::Field(ClickedField {
        object_id,
        session_id: effective_session_id,
        state,
    }))
}

/// Keys that go after the field was clicked. Typing refused before its
/// first key (a person's takeover, a secret field that took focus) is
/// interrupted, not refused: the click happened.
async fn type_after_click(
    control: &Mutex<BrowserControl>,
    client: &CdpClient,
    session_id: &str,
    events: &[Value],
    interval: std::time::Duration,
) -> Result<(), CommandError> {
    let typed = control
        .lock()
        .await
        .agent_native_keys(events, interval, client, session_id)
        .await;
    typed.map_err(|error| match error.error.split_once(": ") {
        Some(("browser_controlled_by_user", _)) => CommandError::with_data(
            "browser_operation_interrupted: The user took control of this browser after the field was clicked and before any text was typed. Inspect the page before continuing; do not replay the text.",
            json!({"interruptedBy":"human","executionStopped":true,"effectsMayHaveOccurred":true,"charactersTyped":0}),
        ),
        Some(("browser_effect_refused" | "browser_observation_stale", why)) => {
            CommandError::with_data(
                format!("browser_operation_interrupted: The field was clicked, then typing was refused: {why}"),
                json!({"executionStopped":true,"effectsMayHaveOccurred":true,"charactersTyped":0}),
            )
        }
        _ => error,
    })
}

/// What a native fill left: the click's dialog, if one opened, or the
/// field's state read back after the value went in.
pub struct NativeFill {
    pub click: ClickResult,
    pub field: Option<FieldState>,
}

/// Fills a field as a person does in the owned window: the pointer travels
/// to it and clicks it, a select-all chord takes what it held, and the value
/// goes in key by key (a value over `motion::PASTE_ABOVE_CHARS` characters
/// as one visible paste). The field is read back, never rewritten: a page
/// that reformats, completes or refuses input shows in the result.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn native_fill(
    client: &CdpClient,
    control: &Mutex<BrowserControl>,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    value: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<NativeFill, CommandError> {
    use super::browser_control::motion::KEY_INTERVAL;
    let field = match click_field(
        client,
        control,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
        true,
    )
    .await?
    {
        Clicked::Dialog(click) => return Ok(NativeFill { click, field: None }),
        Clicked::Field(field) => field,
    };
    if !field.state.empty {
        type_after_click(
            control,
            client,
            session_id,
            &native_key_chord_events("a", Some(2)),
            KEY_INTERVAL,
        )
        .await?;
        if value.is_empty() {
            type_after_click(
                control,
                client,
                session_id,
                &native_key_chord_events("Backspace", None),
                KEY_INTERVAL,
            )
            .await?;
        }
    }
    if !value.is_empty() {
        let events = native_inserted_events(value);
        type_after_click(control, client, session_id, &events, KEY_INTERVAL).await?;
    }
    let state = field_state(
        client,
        &field.session_id,
        &field.object_id,
        value,
        |state| state.matches,
    )
    .await?;
    Ok(NativeFill {
        click: ClickResult::default(),
        field: Some(state),
    })
}

/// Types into a field as a person does in the owned window: the pointer
/// travels to it and clicks it; with `clear`, a select-all chord and
/// Backspace empty it, otherwise End (Control+End in a multi-line field)
/// puts the caret after what it holds; then the text goes key by key, one
/// key per `interval`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn native_type_into(
    client: &CdpClient,
    control: &Mutex<BrowserControl>,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    text: &str,
    clear: bool,
    interval: std::time::Duration,
    iframe_sessions: &HashMap<String, String>,
) -> Result<ClickResult, CommandError> {
    use super::browser_control::motion::KEY_INTERVAL;
    let field = match click_field(
        client,
        control,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
        true,
    )
    .await?
    {
        Clicked::Dialog(click) => return Ok(click),
        Clicked::Field(field) => field,
    };
    if !field.state.empty {
        if clear {
            type_after_click(
                control,
                client,
                session_id,
                &native_key_chord_events("a", Some(2)),
                KEY_INTERVAL,
            )
            .await?;
            type_after_click(
                control,
                client,
                session_id,
                &native_key_chord_events("Backspace", None),
                KEY_INTERVAL,
            )
            .await?;
        } else {
            let modifiers = field.state.multiline.then_some(2);
            type_after_click(
                control,
                client,
                session_id,
                &native_key_chord_events("End", modifiers),
                KEY_INTERVAL,
            )
            .await?;
        }
    }
    type_after_click(
        control,
        client,
        session_id,
        &native_text_events(text),
        interval,
    )
    .await?;
    Ok(ClickResult::default())
}

#[allow(clippy::too_many_arguments)]
pub async fn type_text(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    text: &str,
    clear: bool,
    delay_ms: Option<u64>,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(), String> {
    focus_for_typing(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        clear,
        iframe_sessions,
    )
    .await?;
    type_text_into_active_context(client, session_id, text, delay_ms).await
}

/// Focus through native input without activating a button or link. Text
/// fields use their normal pointer focus; other tab stops use Tab keys.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn native_focus(
    client: &CdpClient,
    control: &Mutex<BrowserControl>,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<ClickResult, CommandError> {
    use super::browser_control::motion::KEY_INTERVAL;
    let (object_id, element_session) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;
    let read = client.send_command("Runtime.callFunctionOn", Some(json!({
        "objectId": object_id, "returnByValue": true,
        "functionDeclaration": format!("function() {{ return {{text:{TEXT_FIELD},tabbable:this.tabIndex>=0&&!this.matches(':disabled')&&!this.closest('[inert],[aria-disabled=\"true\"]')}}; }}"),
    })),Some(&element_session)).await?;
    if field_state(client, &element_session, &object_id, "", |_| true)
        .await?
        .focused
    {
        return Ok(ClickResult::default());
    }
    if read["result"]["value"]["text"] == true {
        return Ok(
            match click_field(
                client,
                control,
                session_id,
                ref_map,
                selector_or_ref,
                iframe_sessions,
                true,
            )
            .await?
            {
                Clicked::Dialog(click) => click,
                Clicked::Field(_) => ClickResult::default(),
            },
        );
    }
    if read["result"]["value"]["tabbable"] != true {
        return Err(
            "This control cannot take physical keyboard focus. Choose a visible, enabled control."
                .into(),
        );
    }
    // Count tab stops across the page's frame sessions; the browser owns
    // their actual tab order, including its toolbar and shadow trees.
    let mut sessions = vec![session_id, element_session.as_str()];
    sessions.extend(iframe_sessions.values().map(String::as_str));
    sessions.sort_unstable();
    sessions.dedup();
    let mut stops = 8;
    for session in sessions {
        let count = client.send_command("Runtime.evaluate",Some(json!({
            "expression":"document.querySelectorAll('a[href],button,input,select,textarea,[tabindex],[contenteditable]').length",
            "returnByValue":true,
        })),Some(session)).await?;
        stops += count["result"]["value"].as_u64().unwrap_or(0) as usize;
    }
    for _ in 0..stops {
        control
            .lock()
            .await
            .agent_native_keys(
                &native_key_chord_events("Tab", None),
                KEY_INTERVAL,
                client,
                session_id,
            )
            .await?;
        if field_state(client, &element_session, &object_id, "", |_| true)
            .await?
            .focused
        {
            return Ok(ClickResult::default());
        }
    }
    Err("The control did not take focus through the browser's keyboard order. Inspect the current page before retrying.".into())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn native_select_all(
    client: &CdpClient,
    control: &Mutex<BrowserControl>,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<ClickResult, CommandError> {
    let (object_id, element_session) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;
    let text_field = client
        .send_command(
            "Runtime.callFunctionOn",
            Some(json!({
                "objectId":object_id,"returnByValue":true,
                "functionDeclaration":format!("function() {{ return {TEXT_FIELD}; }}"),
            })),
            Some(&element_session),
        )
        .await?;
    if text_field["result"]["value"] != true {
        return native_select_text(client, control, session_id, &element_session, &object_id).await;
    }
    let field = match click_field(
        client,
        control,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
        true,
    )
    .await?
    {
        Clicked::Dialog(click) => return Ok(click),
        Clicked::Field(field) => field,
    };
    type_after_click(
        control,
        client,
        session_id,
        &native_key_chord_events("a", Some(2)),
        super::browser_control::motion::KEY_INTERVAL,
    )
    .await?;
    if !field_state(client, &field.session_id, &field.object_id, "", |_| true)
        .await?
        .focused
    {
        return Err("The field lost focus while its text was selected. Inspect the current page before typing.".into());
    }
    Ok(ClickResult::default())
}

/// A read-only geometry projection of one visible text boundary. The wheel
/// owner needs the boundary's box and ancestry, not a DOM mutation or a
/// synthetic selection. Its fallback refuses instead of jumping the page.
const TEXT_ENDPOINT: &str = r#"function(last) {
    const walker=this.ownerDocument.createTreeWalker(this,NodeFilter.SHOW_TEXT),nodes=[];
    while(walker.nextNode())nodes.push(walker.currentNode);
    if(last)nodes.reverse();
    for(const node of nodes) {
        const parent=node.parentElement,style=parent&&getComputedStyle(parent);
        if(!parent||style.display==='none'||style.visibility==='hidden'||style.userSelect==='none')continue;
        for(let n=0;n<node.length;n++) {
            const index=last?node.length-1-n:n,range=this.ownerDocument.createRange();
            range.setStart(node,index);range.setEnd(node,index+1);
            const rect=range.getBoundingClientRect();
            if(!(rect.width>0&&rect.height>0))continue;
            const offset=last?index+1:index;
            return {ownerDocument:this.ownerDocument,parentElement:parent,node,offset,
                getBoundingClientRect(){if(!node.isConnected)throw Error('The text is no longer on this page.');return range.getBoundingClientRect()},
                point(){
                    const box=this.getBoundingClientRect(),y=(box.top+box.bottom)/2;
                    for(const x of [box.left+.1,box.right-.1]) {
                        let caret=null;
                        if(typeof this.ownerDocument.caretPositionFromPoint==='function') {
                            const root=node.getRootNode(),roots=root instanceof ShadowRoot?[root]:[];
                            caret=this.ownerDocument.caretPositionFromPoint(x,y,{shadowRoots:roots});
                        }
                        const old=!caret&&this.ownerDocument.caretRangeFromPoint?.(x,y);
                        if((caret?.offsetNode??old?.startContainer)===node&&(caret?.offset??old?.startOffset)===offset)return [x,y];
                    }
                    throw Error('The browser could not place the pointer at this text boundary.');
                }};
        }
    }
    return null;
}"#;

async fn text_endpoint(
    client: &CdpClient,
    session: &str,
    element: &str,
    last: bool,
) -> Result<String, String> {
    let read = client
        .send_command(
            "Runtime.callFunctionOn",
            Some(json!({
                "objectId":element,"functionDeclaration":TEXT_ENDPOINT,
                "arguments":[{"value":last}],"returnByValue":false,
            })),
            Some(session),
        )
        .await?;
    read["result"]["objectId"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| {
            "The element has no visible text to select; the existing selection was not changed."
                .into()
        })
}

async fn text_endpoint_point(
    client: &CdpClient,
    session: &str,
    endpoint: &str,
) -> Result<(f64, f64), String> {
    let read=client.send_command("Runtime.callFunctionOn",Some(json!({
        "objectId":endpoint,"functionDeclaration":"function(){return this.point()}","returnByValue":true,
    })),Some(session)).await?;
    let point = &read["result"]["value"];
    point[0].as_f64().zip(point[1].as_f64()).ok_or_else(||"The text boundary could not be reached. Inspect the current page before selecting again.".into())
}

async fn native_select_text(
    client: &CdpClient,
    control: &Mutex<BrowserControl>,
    page_session: &str,
    element_session: &str,
    element: &str,
) -> Result<ClickResult, CommandError> {
    let first = text_endpoint(client, element_session, element, false).await?;
    let last = text_endpoint(client, element_session, element, true).await?;
    let selection=async {
        if control.lock().await.agent_native_scroll_into_view(client,page_session,(&first,element_session)).await? {
            return Ok(ClickResult{dialog_opened:true,pending_release:None});
        }
        let (sx,sy)=text_endpoint_point(client,element_session,&first).await?;
        let pending=||Some(PendingRelease{session_id:element_session.into(),x:sx,y:sy,button:"left".into()});
        if control.lock().await.agent_native_mouse(json!({"type":"mousePressed","x":sx,"y":sy,"button":"left","buttons":1,"clickCount":1}),client,element_session,&[element_session,page_session]).await? {
            return Ok(ClickResult{dialog_opened:true,pending_release:pending()});
        }
        if control.lock().await.agent_native_scroll_into_view(client,page_session,(&last,element_session)).await? {
            return Ok(ClickResult{dialog_opened:true,pending_release:pending()});
        }
        let (tx,ty)=text_endpoint_point(client,element_session,&last).await?;
        for event in ["mouseMoved","mouseReleased"] {
            if control.lock().await.agent_native_mouse(json!({"type":event,"x":tx,"y":ty,"button":"left","buttons":if event=="mouseReleased"{0}else{1},"clickCount":1}),client,element_session,&[element_session,page_session]).await? {
                return Ok(ClickResult{dialog_opened:true,pending_release:pending()});
            }
        }
        let deadline=tokio::time::Instant::now()+FIELD_SETTLE;
        loop {
            let read=client.send_command("Runtime.callFunctionOn",Some(json!({
                "objectId":first,"returnByValue":true,"arguments":[{"objectId":last}],
                "functionDeclaration":"function(end){const selection=this.ownerDocument.getSelection();if(selection.rangeCount!==1)return false;const expected=this.ownerDocument.createRange();expected.setStart(this.node,this.offset);expected.setEnd(end.node,end.offset);const actual=selection.getRangeAt(0);return actual.compareBoundaryPoints(Range.START_TO_START,expected)===0&&actual.compareBoundaryPoints(Range.END_TO_END,expected)===0}",
            })),Some(element_session)).await?;
            if read["result"]["value"]==true{return Ok(ClickResult::default())}
            if tokio::time::Instant::now()>=deadline {
                return Err("The browser did not select the exact requested text range. Inspect its current selection before retrying.".into());
            }
            tokio::time::sleep(FIELD_POLL).await;
        }
    }.await;
    if selection.is_err() {
        control
            .lock()
            .await
            .cancel_native_input()
            .await
            .map_err(CommandError::from)?;
    }
    selection
}

/// Focus the target element (and optionally clear it) so that keystrokes,
/// whether dispatched through CDP or the owned native window, reach it.
pub async fn focus_for_typing(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    clear: bool,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(), String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    // Focus
    client
        .send_command_typed::<_, Value>(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: "function() { this.focus(); }".to_string(),
                object_id: Some(object_id.clone()),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    if clear {
        client
            .send_command_typed::<_, Value>(
                "Runtime.callFunctionOn",
                &CallFunctionOnParams {
                    function_declaration: r#"function() {
                        this.select && this.select();
                        this.value = '';
                        this.dispatchEvent(new Event('input', { bubbles: true }));
                    }"#
                    .to_string(),
                    object_id: Some(object_id),
                    arguments: None,
                    return_by_value: Some(true),
                    await_promise: Some(false),
                },
                Some(&effective_session_id),
            )
            .await?;
    }
    Ok(())
}

pub async fn type_text_into_active_context(
    client: &CdpClient,
    session_id: &str,
    text: &str,
    delay_ms: Option<u64>,
) -> Result<(), String> {
    let delay = delay_ms.unwrap_or(0);

    for ch in text.chars() {
        if matches!(ch, '\n' | '\r' | '\t') {
            let (key, code, key_code) = char_to_key_info(ch);
            let text_str = key_text(&key);
            client
                .send_command_typed::<_, Value>(
                    "Input.dispatchKeyEvent",
                    &DispatchKeyEventParams {
                        event_type: "keyDown".to_string(),
                        key: Some(key.clone()),
                        code: Some(code.clone()),
                        text: text_str.clone(),
                        unmodified_text: text_str,
                        windows_virtual_key_code: Some(key_code),
                        native_virtual_key_code: Some(key_code),
                        modifiers: None,
                    },
                    Some(session_id),
                )
                .await?;

            client
                .send_command_typed::<_, Value>(
                    "Input.dispatchKeyEvent",
                    &DispatchKeyEventParams {
                        event_type: "keyUp".to_string(),
                        key: Some(key),
                        code: Some(code),
                        text: None,
                        unmodified_text: None,
                        windows_virtual_key_code: Some(key_code),
                        native_virtual_key_code: Some(key_code),
                        modifiers: None,
                    },
                    Some(session_id),
                )
                .await?;
        } else {
            // VS Code/Electron webviews reject repeated dispatchKeyEvent calls
            // carrying printable `text`. Insert printable characters directly
            // and reserve key events for controls like Enter and Tab.
            client
                .send_command_typed::<_, Value>(
                    "Input.insertText",
                    &InsertTextParams {
                        text: ch.to_string(),
                    },
                    Some(session_id),
                )
                .await?;
        }

        if delay > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(delay)).await;
        }
    }

    Ok(())
}

pub async fn press_key(client: &CdpClient, session_id: &str, key: &str) -> Result<(), String> {
    press_key_with_modifiers(client, session_id, key, None).await
}

/// Dispatch a keyDown+keyUp sequence for `key` with an optional CDP modifier bitmask.
///
/// Modifier values follow the CDP `Input.dispatchKeyEvent` spec:
/// 1 = Alt, 2 = Control, 4 = Meta (Cmd), 8 = Shift.
///
/// Callers that need a platform-appropriate modifier (e.g. Cmd on macOS,
/// Ctrl elsewhere) must choose the value themselves -- see `cfg!(target_os)`.
pub async fn press_key_with_modifiers(
    client: &CdpClient,
    session_id: &str,
    key: &str,
    modifiers: Option<i32>,
) -> Result<(), String> {
    let (key_name, code, key_code) = named_key_info(key);

    // Suppress text insertion when Control (2) or Meta (4) modifiers are active,
    // since these are command chords (e.g. Ctrl+A = select-all), not text input.
    let has_command_modifier = modifiers.is_some_and(|m| m & (2 | 4) != 0);
    let text = if has_command_modifier {
        None
    } else {
        key_text(&key_name)
    };

    client
        .send_command_typed::<_, Value>(
            "Input.dispatchKeyEvent",
            &DispatchKeyEventParams {
                event_type: "keyDown".to_string(),
                key: Some(key_name.clone()),
                code: Some(code.clone()),
                text: text.clone(),
                unmodified_text: text.clone(),
                windows_virtual_key_code: Some(key_code),
                native_virtual_key_code: Some(key_code),
                modifiers,
            },
            Some(session_id),
        )
        .await?;

    client
        .send_command_typed::<_, Value>(
            "Input.dispatchKeyEvent",
            &DispatchKeyEventParams {
                event_type: "keyUp".to_string(),
                key: Some(key_name),
                code: Some(code),
                text: None,
                unmodified_text: None,
                windows_virtual_key_code: Some(key_code),
                native_virtual_key_code: Some(key_code),
                modifiers,
            },
            Some(session_id),
        )
        .await?;

    Ok(())
}

/// Scrolls by script, published on the tab's page as `scrolling`: the view
/// moves with no pointer path. Headless and DevTools-only browsers scroll
/// this way. The owned window's wheel (`BrowserControl::agent_native_scroll`)
/// uses trusted input and does not enter this programmatic path.
pub async fn scroll(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: Option<&str>,
    delta_x: f64,
    delta_y: f64,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(), String> {
    if let Some(sel) = selector_or_ref {
        let (object_id, effective_session_id) =
            resolve_element_object_id(client, session_id, ref_map, sel, iframe_sessions).await?;
        return scroll_by(
            client,
            session_id,
            &effective_session_id,
            &object_id,
            delta_x,
            delta_y,
        )
        .await;
    }
    let observation = client.observe_activity(
        serde_json::json!({ "type": "activity", "kind": "scrolling" }),
        session_id,
        client.page_generation(session_id),
        super::activity::InputSource::Agent,
    );
    client
        .send_command_typed::<_, Value>(
            "Runtime.evaluate",
            &EvaluateParams {
                expression: format!("window.scrollBy({}, {})", delta_x, delta_y),
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(session_id),
        )
        .await?;
    observation.acknowledged();
    Ok(())
}

/// Scrolls `object_id` (a scroller in `session_id`) by script, published on
/// the tab's page (`page_session`) as `scrolling`.
pub(crate) async fn scroll_by(
    client: &CdpClient,
    page_session: &str,
    session_id: &str,
    object_id: &str,
    delta_x: f64,
    delta_y: f64,
) -> Result<(), String> {
    let observation = client.observe_activity(
        serde_json::json!({ "type": "activity", "kind": "scrolling" }),
        page_session,
        client.page_generation(page_session),
        super::activity::InputSource::Agent,
    );
    client
        .send_command_typed::<_, Value>(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: "function(dx, dy) { this.scrollBy(dx, dy); }".into(),
                object_id: Some(object_id.into()),
                arguments: Some(vec![
                    CallArgument {
                        value: Some(serde_json::json!(delta_x)),
                        object_id: None,
                    },
                    CallArgument {
                        value: Some(serde_json::json!(delta_y)),
                        object_id: None,
                    },
                ]),
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(session_id),
        )
        .await?;
    observation.acknowledged();
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
struct NativeSelectOption {
    value: String,
    label: String,
    disabled: bool,
    selected: bool,
}

struct NativeSelectState {
    multiple: bool,
    popup: bool,
    disabled: bool,
    focused: bool,
    options: Vec<NativeSelectOption>,
}

async fn native_select_state(
    client: &CdpClient,
    session_id: &str,
    object_id: &str,
) -> Result<NativeSelectState, String> {
    let read = client.send_command("Runtime.callFunctionOn", Some(json!({
        "objectId": object_id, "returnByValue": true,
        "functionDeclaration": r#"function() {
            if (!this.isConnected || this.tagName !== 'SELECT') return {error:'Choose a select control on the current page.'};
            return {multiple:this.multiple,popup:this.size<=1,disabled:this.matches(':disabled')||Boolean(this.closest('[inert],[aria-disabled="true"]')),
                focused:document.activeElement===this||this.getRootNode().activeElement===this,
                options:Array.from(this.options,o=>({value:o.value,label:o.textContent.trim(),disabled:o.disabled||Boolean(o.closest('optgroup:disabled')),selected:o.selected}))};
        }"#,
    })), Some(session_id)).await?;
    let value = &read["result"]["value"];
    if let Some(error) = value["error"].as_str() {
        return Err(error.into());
    }
    let options = value["options"]
        .as_array()
        .ok_or("The select control could not be read.")?
        .iter()
        .map(|option| NativeSelectOption {
            value: option["value"].as_str().unwrap_or_default().into(),
            label: option["label"].as_str().unwrap_or_default().into(),
            disabled: option["disabled"] == true,
            selected: option["selected"] == true,
        })
        .collect();
    Ok(NativeSelectState {
        multiple: value["multiple"] == true,
        popup: value["popup"] == true,
        disabled: value["disabled"] == true,
        focused: value["focused"] == true,
        options,
    })
}

fn native_select_indices(
    options: &[NativeSelectOption],
    values: &[String],
    multiple: bool,
) -> Result<Vec<usize>, String> {
    if values.is_empty() {
        return Err("Select at least one option.".into());
    }
    let mut indices: Vec<_> = options
        .iter()
        .enumerate()
        .filter_map(|(index, option)| {
            values
                .iter()
                .any(|value| option.value == *value || option.label == *value)
                .then_some(index)
        })
        .collect();
    if indices.is_empty() {
        return Err(format!(
            "No option matched {}. Available options: {}",
            serde_json::to_string(values).unwrap_or_default(),
            options
                .iter()
                .map(|option| format!("{} (\"{}\")", option.value, option.label))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    // The existing DOM path visits matching options in document order. A
    // single-select keeps its last match; preserve that browser cardinality
    // rule for duplicate values/labels and multi-value callers too.
    if !multiple {
        indices = vec![*indices.last().unwrap()];
    }
    Ok(indices)
}

fn selected_indices(state: &NativeSelectState) -> Vec<usize> {
    state
        .options
        .iter()
        .enumerate()
        .filter_map(|(index, option)| option.selected.then_some(index))
        .collect()
}

fn same_select_options(left: &NativeSelectState, right: &NativeSelectState) -> bool {
    left.multiple == right.multiple
        && left.popup == right.popup
        && left.options.len() == right.options.len()
        && left
            .options
            .iter()
            .zip(&right.options)
            .all(|(left, right)| {
                left.value == right.value
                    && left.label == right.label
                    && left.disabled == right.disabled
            })
}

/// Selects through the owned window's pointer and keyboard. Reading option
/// identities is harmless; all selection changes are Chrome's native key
/// behavior, and a changed control is reported rather than repaired by DOM.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn native_select_option(
    client: &CdpClient,
    control: &Mutex<BrowserControl>,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    values: &[String],
    iframe_sessions: &HashMap<String, String>,
) -> Result<ClickResult, CommandError> {
    use super::browser_control::motion::KEY_INTERVAL;
    let (object_id, element_session) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;
    let before = native_select_state(client, &element_session, &object_id).await?;
    let desired = native_select_indices(&before.options, values, before.multiple)?;
    if selected_indices(&before) == desired {
        return Ok(ClickResult::default());
    }
    if before.disabled || desired.iter().any(|index| before.options[*index].disabled) {
        return Err(
            "The requested select control or option is disabled; nothing was changed.".into(),
        );
    }
    match click_field(
        client,
        control,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
        false,
    )
    .await?
    {
        Clicked::Dialog(click) => return Ok(click),
        Clicked::Field(_) => {}
    }
    let current = native_select_state(client, &element_session, &object_id).await?;
    if !same_select_options(&before, &current) || !current.focused || current.disabled {
        return Err("The select control changed after it was clicked. Inspect the current page before choosing again.".into());
    }
    let enabled: Vec<_> = current
        .options
        .iter()
        .enumerate()
        .filter_map(|(index, option)| (!option.disabled).then_some(index))
        .collect();
    if before.multiple {
        type_after_click(
            control,
            client,
            session_id,
            &native_key_chord_events("Home", Some(2)),
            KEY_INTERVAL,
        )
        .await?;
        let current = native_select_state(client, &element_session, &object_id).await?;
        if !same_select_options(&before, &current) || !current.focused {
            return Err("The select control changed while it was taking keyboard focus. Inspect the current page before choosing again.".into());
        }
        for (position, index) in enabled.iter().enumerate() {
            if position > 0 {
                type_after_click(
                    control,
                    client,
                    session_id,
                    &native_key_chord_events("ArrowDown", Some(2)),
                    KEY_INTERVAL,
                )
                .await?;
            }
            if current.options[*index].selected != desired.contains(index) {
                type_after_click(
                    control,
                    client,
                    session_id,
                    &native_key_chord_events(" ", Some(2)),
                    KEY_INTERVAL,
                )
                .await?;
            }
        }
    } else {
        let position = enabled
            .iter()
            .position(|index| *index == desired[0])
            .ok_or("The requested option is no longer available.")?;
        let mut events = native_key_chord_events("Home", None);
        for _ in 0..position {
            events.extend(native_key_chord_events("ArrowDown", None));
        }
        if before.popup {
            events.extend(native_key_chord_events("Enter", None));
        }
        type_after_click(control, client, session_id, &events, KEY_INTERVAL).await?;
    }
    let deadline = tokio::time::Instant::now() + FIELD_SETTLE;
    loop {
        let final_state = native_select_state(client, &element_session, &object_id).await?;
        if !same_select_options(&before, &final_state) {
            return Err("The options changed during selection. Inspect the current page before choosing again.".into());
        }
        if selected_indices(&final_state) == desired {
            return Ok(ClickResult::default());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("The page did not accept the requested selection. Inspect its current values before retrying; no DOM repair was attempted.".into());
        }
        tokio::time::sleep(FIELD_POLL).await;
    }
}

pub async fn select_option(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    values: &[String],
    iframe_sessions: &HashMap<String, String>,
) -> Result<(), String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    // Matching nothing must be an error, not a silent success: an agent that
    // selects a misspelled option otherwise sees "Done", and only discovers
    // the page state is wrong after more commands. List what was available.
    let js = r#"function(vals) {
            const options = Array.from(this.options);
            let matched = 0;
            for (const opt of options) {
                opt.selected = vals.includes(opt.value) || vals.includes(opt.textContent.trim());
                if (opt.selected) matched += 1;
            }
            if (matched === 0) {
                const available = options.map(o => o.value + ' ("' + o.textContent.trim() + '")').join(', ');
                return { error: 'No option matched ' + JSON.stringify(vals) + '. Available options: ' + available };
            }
            this.dispatchEvent(new Event('change', { bubbles: true }));
            return { matched };
        }"#
    .to_string();

    let result = client
        .send_command_typed::<_, Value>(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: js,
                object_id: Some(object_id),
                arguments: Some(vec![CallArgument {
                    value: Some(serde_json::json!(values)),
                    object_id: None,
                }]),
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    if let Some(error) = result
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.get("error"))
        .and_then(|e| e.as_str())
    {
        return Err(error.to_string());
    }

    Ok(())
}

fn checkbox_function(body: &str) -> String {
    [
        "function(desired,allowDom) { const el=this; const {input,aria,checked}=(",
        super::element::CHECKED_TARGET_JS,
        r#")(el);
        const pointerAccessible = node => {
            if (!node?.isConnected) return false;
            const style = getComputedStyle(node);
            return style.display !== 'none' && style.visibility !== 'hidden' && style.visibility !== 'collapse'
                && style.pointerEvents !== 'none' && Array.from(node.getClientRects()).some(rect=>rect.width>0 && rect.height>0);
        };
        const semanticTarget = pointerAccessible(el) && (aria || el.tabIndex >= 0 || typeof el.onclick === 'function') ? el : null;
        const pointer = input ? (pointerAccessible(input) ? input : Array.from(input.labels || []).find(pointerAccessible) || semanticTarget) : el;
        "#,
        body,
        "}",
    ].concat()
}

pub struct CheckResult {
    pub input: ClickResult,
    pub method: &'static str,
}

pub async fn check(
    client: &CdpClient,
    control: &Mutex<BrowserControl>,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<CheckResult, CommandError> {
    set_checked(
        client,
        control,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
        true,
    )
    .await
}

pub async fn uncheck(
    client: &CdpClient,
    control: &Mutex<BrowserControl>,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<CheckResult, CommandError> {
    set_checked(
        client,
        control,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
        false,
    )
    .await
}

/// Pick one activation method before acting. Hidden associated controls without
/// a visible activation path retain programmatic support; visible controls use
/// normal pointer input. A failed attempt never selects another transport.
async fn set_checked(
    client: &CdpClient,
    control: &Mutex<BrowserControl>,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
    desired: bool,
) -> Result<CheckResult, CommandError> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;
    let native = control.lock().await.has_native_display();
    // State, disabledness and the semantic action share one renderer task.
    // A state change before this task cannot turn an idempotent check into a
    // toggle. This is the existing associated-control DOM click, selected
    // before any physical button instead of retried after a failed click.
    let preflight = client.send_command("Runtime.callFunctionOn", Some(serde_json::json!({
        "objectId":object_id, "returnByValue":true,
        "arguments":[{"value":desired},{"value":!native}],
        "functionDeclaration":checkbox_function(r#"
            if (!el.isConnected) return {error:'The checkbox is no longer attached.'};
            if (checked() === desired) return {method:'unchanged'};
            if (el.closest('[inert],[aria-disabled="true"]') || input?.matches(':disabled') || input?.closest('[inert],[aria-disabled="true"]')) {
                return {error:'The checkbox is disabled.'};
            }
            if (pointer) return {method:'pointer'};
            if (!allowDom) return {error:'The checkbox has no visible control or label to click. Choose its visible control or label; nothing was changed.'};
            input.click();
            return {method:'dom'};
        "#),
    })), Some(&effective_session_id));
    let mut events = client.subscribe();
    tokio::pin!(preflight);
    let result = loop {
        tokio::select! {
            result = &mut preflight => break result?,
            event = events.recv() => match event {
                Ok(event) if event.method == "Page.javascriptDialogOpening" && event.session_id.as_deref().is_none_or(|id| id == session_id || id == effective_session_id) => {
                    return Ok(CheckResult {input:ClickResult {dialog_opened:true,pending_release:None},method:"dom"});
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return Err("The browser disconnected while checking the control.".into()),
                _ => {},
            },
        }
    };
    if result.get("exceptionDetails").is_some() {
        return Err("The checkbox could not be inspected.".into());
    }
    let value = &result["result"]["value"];
    if let Some(error) = value["error"].as_str() {
        return Err(error.into());
    }
    let method = match value["method"].as_str() {
        Some("unchanged") => {
            return Ok(CheckResult {
                input: ClickResult::default(),
                method: "unchanged",
            })
        }
        Some("dom") => "dom",
        Some("pointer") => {
            if control.lock().await.has_native_display() {
                "native"
            } else {
                "cdp"
            }
        }
        _ => return Err("The checkbox activation method is unavailable.".into()),
    };
    let input = if method == "dom" {
        ClickResult::default()
    } else {
        let target = client.send_command("Runtime.callFunctionOn", Some(serde_json::json!({
            "objectId":object_id, "functionDeclaration":checkbox_function("return pointer;"), "returnByValue":false,
        })), Some(&effective_session_id)).await?;
        let target_id = target["result"]["objectId"].as_str().ok_or(
            "The checkbox activation target changed. Inspect the current page before retrying.",
        )?;
        let point = if method == "native" {
            let target = (target_id, effective_session_id.as_str());
            native_point(client, control, session_id, target, selector_or_ref).await?
        } else {
            Some(
                super::element::resolve_object_center(
                    client,
                    &effective_session_id,
                    target_id,
                    selector_or_ref,
                )
                .await?,
            )
        };
        match point {
            Some((x, y)) => {
                dispatch_click(
                    client,
                    control,
                    &effective_session_id,
                    &[effective_session_id.as_str(), session_id],
                    x,
                    y,
                    "left",
                    1,
                )
                .await?
            }
            None => ClickResult {
                dialog_opened: true,
                pending_release: None,
            },
        }
    };
    if !input.dialog_opened
        && super::element::is_element_checked(
            client,
            session_id,
            ref_map,
            selector_or_ref,
            iframe_sessions,
        )
        .await?
            != desired
    {
        return Err("The checkbox did not reach the requested state after its activation. Inspect the current page before retrying.".into());
    }
    Ok(CheckResult { input, method })
}

pub async fn focus(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(), String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    client
        .send_command_typed::<_, Value>(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: "function() { this.focus(); }".to_string(),
                object_id: Some(object_id),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    Ok(())
}

pub async fn clear(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(), String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    client
        .send_command_typed::<_, Value>(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: r#"function() {
                    this.focus();
                    this.value = '';
                    this.dispatchEvent(new Event('input', { bubbles: true }));
                    this.dispatchEvent(new Event('change', { bubbles: true }));
                }"#
                .to_string(),
                object_id: Some(object_id),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    Ok(())
}

pub async fn select_all(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(), String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    client
        .send_command_typed::<_, Value>(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: r#"function() {
                    this.focus();
                    if (typeof this.select === 'function') {
                        this.select();
                    } else {
                        const range = document.createRange();
                        range.selectNodeContents(this);
                        const sel = window.getSelection();
                        sel.removeAllRanges();
                        sel.addRange(range);
                    }
                }"#
                .to_string(),
                object_id: Some(object_id),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    Ok(())
}

pub async fn scroll_into_view(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(), String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    client
        .send_command_typed::<_, Value>(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration:
                    "function() { this.scrollIntoView({ block: 'center', inline: 'center' }); }"
                        .to_string(),
                object_id: Some(object_id),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    Ok(())
}

pub async fn dispatch_event(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    event_type: &str,
    event_init: Option<&Value>,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(), String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    let init_json = event_init
        .map(|v| serde_json::to_string(v).unwrap_or("{}".to_string()))
        .unwrap_or_else(|| "{ bubbles: true }".to_string());

    let js = format!(
        "function() {{ this.dispatchEvent(new Event({}, {})); }}",
        serde_json::to_string(event_type).unwrap_or_default(),
        init_json
    );

    client
        .send_command_typed::<_, Value>(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: js,
                object_id: Some(object_id),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    Ok(())
}

pub async fn highlight(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(), String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    client
        .send_command_typed::<_, Value>(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: r#"function() {
                    this.style.outline = '2px solid red';
                    this.style.outlineOffset = '2px';
                    const el = this;
                    setTimeout(() => {
                        el.style.outline = '';
                        el.style.outlineOffset = '';
                    }, 3000);
                }"#
                .to_string(),
                object_id: Some(object_id),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    Ok(())
}

pub async fn tap_touch(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(), String> {
    let (x, y, effective_session_id) = resolve_element_center(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    client
        .send_command(
            "Input.dispatchTouchEvent",
            Some(serde_json::json!({
                "type": "touchStart",
                "touchPoints": [{ "x": x, "y": y }],
            })),
            Some(&effective_session_id),
        )
        .await?;

    client
        .send_command(
            "Input.dispatchTouchEvent",
            Some(serde_json::json!({
                "type": "touchEnd",
                "touchPoints": [],
            })),
            Some(&effective_session_id),
        )
        .await?;

    Ok(())
}

/// Dispatches one mouse event and waits for the browser to ack it, but
/// returns Ok(true) if a JavaScript dialog opens first. A synchronous dialog
/// (confirm/prompt/alert in the event handler) blocks the renderer's main
/// thread, so the input ack cannot arrive until the dialog is resolved;
/// without this the command hangs until the client read timeout and the agent
/// never sees the pending-dialog warning.
async fn dispatch_mouse_or_dialog(
    client: &CdpClient,
    control: &Mutex<BrowserControl>,
    session_id: &str,
    accept_sessions: &[&str],
    params: &DispatchMouseEventParams,
) -> Result<bool, CommandError> {
    use tokio::sync::broadcast::error::RecvError;

    if control.lock().await.has_native_display() {
        return control
            .lock()
            .await
            .agent_native_mouse(
                serde_json::to_value(params).map_err(|error| error.to_string())?,
                client,
                session_id,
                accept_sessions,
            )
            .await;
    }
    // Subscribe before sending so the dialog event cannot slip past us.
    let mut events = client.subscribe();
    let send =
        client.send_command_typed::<_, Value>("Input.dispatchMouseEvent", params, Some(session_id));
    tokio::pin!(send);
    loop {
        tokio::select! {
            res = &mut send => {
                res?;
                return Ok(false);
            }
            event = events.recv() => {
                match event {
                    Ok(e) if e.method == "Page.javascriptDialogOpening" => {
                        // Only a dialog on this click's frame/page session
                        // aborts it; a background-tab dialog must not. A
                        // session-less event has no flat session and is
                        // treated as the top-level page (i.e. ours).
                        let ours = match e.session_id.as_deref() {
                            Some(sid) => accept_sessions.contains(&sid),
                            None => true,
                        };
                        if ours {
                            return Ok(true);
                        }
                        continue;
                    }
                    Ok(_) => continue,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => {
                        (&mut send).await?;
                        return Ok(false);
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_click(
    client: &CdpClient,
    control: &Mutex<BrowserControl>,
    session_id: &str,
    accept_sessions: &[&str],
    x: f64,
    y: f64,
    button: &str,
    click_count: i32,
) -> Result<ClickResult, CommandError> {
    // Move
    if dispatch_mouse_or_dialog(
        client,
        control,
        session_id,
        accept_sessions,
        &DispatchMouseEventParams {
            event_type: "mouseMoved".to_string(),
            x,
            y,
            button: None,
            buttons: None,
            click_count: None,
            delta_x: None,
            delta_y: None,
            modifiers: None,
        },
    )
    .await?
    {
        // No button was pressed yet, nothing to release.
        return Ok(ClickResult {
            dialog_opened: true,
            pending_release: None,
        });
    }

    let button_value = match button {
        "right" => 2,
        "middle" => 4,
        _ => 1,
    };

    // Press
    if dispatch_mouse_or_dialog(
        client,
        control,
        session_id,
        accept_sessions,
        &DispatchMouseEventParams {
            event_type: "mousePressed".to_string(),
            x,
            y,
            button: Some(button.to_string()),
            buttons: Some(button_value),
            click_count: Some(click_count),
            delta_x: None,
            delta_y: None,
            modifiers: None,
        },
    )
    .await?
    {
        // Dialog opened from the mousedown handler: the button is held and the
        // release will never arrive on its own. Hand the caller what it needs
        // to release once the dialog is resolved.
        return Ok(ClickResult {
            dialog_opened: true,
            pending_release: Some(PendingRelease {
                session_id: session_id.to_string(),
                x,
                y,
                button: button.to_string(),
            }),
        });
    }

    // Release. A dialog here fired from the click/mouseup handler, which runs
    // after the button is already up, so there is nothing left to release.
    let dialog_opened = dispatch_mouse_or_dialog(
        client,
        control,
        session_id,
        accept_sessions,
        &DispatchMouseEventParams {
            event_type: "mouseReleased".to_string(),
            x,
            y,
            button: Some(button.to_string()),
            buttons: Some(0),
            click_count: Some(click_count),
            delta_x: None,
            delta_y: None,
            modifiers: None,
        },
    )
    .await?;
    Ok(ClickResult {
        dialog_opened,
        pending_release: None,
    })
}

/// Best-effort mouseReleased to clear a button left logically down when a
/// dialog opened mid-click. Called after the dialog is resolved.
pub async fn dispatch_pending_release(
    client: &CdpClient,
    control: &Mutex<BrowserControl>,
    release: &PendingRelease,
) -> Result<(), String> {
    if control.lock().await.has_native_display() {
        return control.lock().await.finish_native_dialog().await;
    }
    client
        .send_command_typed::<_, Value>(
            "Input.dispatchMouseEvent",
            &DispatchMouseEventParams {
                event_type: "mouseReleased".to_string(),
                x: release.x,
                y: release.y,
                button: Some(release.button.clone()),
                buttons: Some(0),
                click_count: Some(1),
                delta_x: None,
                delta_y: None,
                modifiers: None,
            },
            Some(&release.session_id),
        )
        .await?;
    Ok(())
}

fn char_to_key_info(ch: char) -> (String, String, i32) {
    match ch {
        '\n' | '\r' => ("Enter".to_string(), "Enter".to_string(), 13),
        '\t' => ("Tab".to_string(), "Tab".to_string(), 9),
        ' ' => (" ".to_string(), "Space".to_string(), 32),
        _ => {
            let key = ch.to_string();
            if ch.is_ascii_alphabetic() {
                // For letters the Windows VK code equals the uppercase ASCII value.
                let upper = ch.to_ascii_uppercase();
                let code = format!("Key{}", upper);
                let key_code = upper as i32;
                (key, code, key_code)
            } else if ch.is_ascii_digit() {
                let code = format!("Digit{}", ch);
                let key_code = ch as i32;
                (key, code, key_code)
            } else {
                let (code, key_code) = punctuation_key_info(ch);
                (key, code.to_string(), key_code)
            }
        }
    }
}

/// Return the DOM `KeyboardEvent.code` value and Windows virtual-key code for
/// a punctuation / symbol character assuming a US keyboard layout.
///
/// The Windows virtual-key codes (VK_OEM_*) differ from ASCII values for
/// punctuation.  Using the raw ASCII code would misidentify characters – e.g.
/// '.' (ASCII 46) collides with VK_DELETE (0x2E = 46), causing the period to
/// be swallowed.
fn punctuation_key_info(ch: char) -> (&'static str, i32) {
    match ch {
        // VK_OEM_1 (0xBA = 186) — ";:" key on US layout
        ';' | ':' => ("Semicolon", 186),
        // VK_OEM_PLUS (0xBB = 187) — "=+" key
        '=' | '+' => ("Equal", 187),
        // VK_OEM_COMMA (0xBC = 188) — ",<" key
        ',' | '<' => ("Comma", 188),
        // VK_OEM_MINUS (0xBD = 189) — "-_" key
        '-' | '_' => ("Minus", 189),
        // VK_OEM_PERIOD (0xBE = 190) — ".>" key
        '.' | '>' => ("Period", 190),
        // VK_OEM_2 (0xBF = 191) — "/?" key
        '/' | '?' => ("Slash", 191),
        // VK_OEM_3 (0xC0 = 192) — "`~" key
        '`' | '~' => ("Backquote", 192),
        // VK_OEM_4 (0xDB = 219) — "[{" key
        '[' | '{' => ("BracketLeft", 219),
        // VK_OEM_5 (0xDC = 220) — "\\|" key
        '\\' | '|' => ("Backslash", 220),
        // VK_OEM_6 (0xDD = 221) — "]}" key
        ']' | '}' => ("BracketRight", 221),
        // VK_OEM_7 (0xDE = 222) — "'\""" key
        '\'' | '"' => ("Quote", 222),
        _ => ("", 0),
    }
}

/// Return the `text` value that CDP `Input.dispatchKeyEvent` needs on the
/// `keyDown` event so that Chrome performs the default action for the key.
/// For example Enter needs `"\r"` to actually submit a form, and Tab needs
/// `"\t"` to move focus.  Non-printable / navigation keys return `None`.
fn key_text(key_name: &str) -> Option<String> {
    match key_name {
        "Enter" => Some("\r".to_string()),
        "Tab" => Some("\t".to_string()),
        " " => Some(" ".to_string()),
        _ => {
            // Single printable characters carry themselves as text.
            if key_name.len() == 1 {
                Some(key_name.to_string())
            } else {
                None
            }
        }
    }
}

/// Keyboard events for the owned native window, in the shape the display
/// helper accepts, one character at a time. Characters a US keymap types are
/// key presses with their text; any other character is an explicit text
/// insertion of its own, which the helper performs as a native paste.
pub(crate) fn native_text_events(text: &str) -> Vec<Value> {
    let mut events = Vec::new();
    for ch in text.chars() {
        // A US keymap types every ASCII graphic character; the helper
        // resolves the native key and level from the character itself.
        if matches!(ch, '\n' | '\r' | '\t' | ' ') || ch.is_ascii_graphic() {
            let (key, code, _) = char_to_key_info(ch);
            events.extend(native_key_events(&key, &code, 0));
        } else {
            events.push(json!({
                "type": "input_keyboard", "eventType": "insertText", "text": ch.to_string(),
            }));
        }
    }
    events
}

/// The events of one visible paste of `text`, as a person pastes a value.
/// Text inserted as a person enters it: typed key by key, or one visible
/// paste above `motion::PASTE_ABOVE_CHARS` characters.
pub(crate) fn native_inserted_events(text: &str) -> Vec<Value> {
    if text.chars().count() > super::browser_control::motion::PASTE_ABOVE_CHARS {
        native_paste_events(text)
    } else {
        native_text_events(text)
    }
}

pub(crate) fn native_paste_events(text: &str) -> Vec<Value> {
    vec![json!({ "type": "input_keyboard", "eventType": "insertText", "text": text })]
}

/// A single key press through the owned native window, with the CDP
/// modifier mask (1 Alt, 2 Control, 4 Meta, 8 Shift) the helper shares.
pub(crate) fn native_key_chord_events(key: &str, modifiers: Option<i32>) -> Vec<Value> {
    let (key_name, code, _) = named_key_info(key);
    native_key_events(&key_name, &code, modifiers.unwrap_or(0))
}

fn native_key_events(key: &str, code: &str, modifiers: i32) -> Vec<Value> {
    let mut down = json!({
        "type": "input_keyboard", "eventType": "keyDown", "key": key, "code": code,
        "modifiers": modifiers,
    });
    // Text rides only with an unmodified printable key; a chord names its
    // physical key, and the helper resolves the native level itself.
    if modifiers & 7 == 0 {
        if let Some(text) = key_text(key).filter(|text| text != "\r" && text != "\t") {
            down["text"] = json!(text);
        }
    }
    vec![
        down,
        json!({
            "type": "input_keyboard", "eventType": "keyUp", "key": key, "code": code,
            "modifiers": modifiers,
        }),
    ]
}

/// One key going down or up in the owned window, as `keydown` and `keyup`
/// send it: its physical key, and the text a printable key types as it goes
/// down.
pub(crate) fn native_key_transition(key: &str, event_type: &str) -> Value {
    let (key_name, code, _) = named_key_info(key);
    let mut event = serde_json::json!({
        "type": "input_keyboard", "eventType": event_type, "key": key_name, "code": code,
        "modifiers": 0,
    });
    if event_type != "keyUp" {
        if let Some(text) = key_text(&key_name).filter(|text| text != "\r" && text != "\t") {
            event["text"] = serde_json::json!(text);
        }
    }
    event
}

fn named_key_info(key: &str) -> (String, String, i32) {
    match key.to_lowercase().as_str() {
        "shift" => ("Shift".to_string(), "ShiftLeft".to_string(), 16),
        "control" | "ctrl" => ("Control".to_string(), "ControlLeft".to_string(), 17),
        "alt" | "option" => ("Alt".to_string(), "AltLeft".to_string(), 18),
        "meta" | "command" | "cmd" => ("Meta".to_string(), "MetaLeft".to_string(), 91),
        "enter" | "return" => ("Enter".to_string(), "Enter".to_string(), 13),
        "tab" => ("Tab".to_string(), "Tab".to_string(), 9),
        "escape" | "esc" => ("Escape".to_string(), "Escape".to_string(), 27),
        "backspace" => ("Backspace".to_string(), "Backspace".to_string(), 8),
        "delete" => ("Delete".to_string(), "Delete".to_string(), 46),
        "arrowup" | "up" => ("ArrowUp".to_string(), "ArrowUp".to_string(), 38),
        "arrowdown" | "down" => ("ArrowDown".to_string(), "ArrowDown".to_string(), 40),
        "arrowleft" | "left" => ("ArrowLeft".to_string(), "ArrowLeft".to_string(), 37),
        "arrowright" | "right" => ("ArrowRight".to_string(), "ArrowRight".to_string(), 39),
        "home" => ("Home".to_string(), "Home".to_string(), 36),
        "end" => ("End".to_string(), "End".to_string(), 35),
        "pageup" => ("PageUp".to_string(), "PageUp".to_string(), 33),
        "pagedown" => ("PageDown".to_string(), "PageDown".to_string(), 34),
        "space" | " " => (" ".to_string(), "Space".to_string(), 32),
        _ => {
            if key.len() == 1 {
                let ch = key.chars().next().unwrap();
                char_to_key_info(ch)
            } else {
                (key.to_string(), key.to_string(), 0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `inserttext` in the owned window: up to 64 characters are typed key
    /// by key, longer text is one visible paste.
    #[test]
    fn inserted_text_is_typed_up_to_the_paste_threshold() {
        let short = native_inserted_events("hi");
        assert!(short.iter().all(|event| event["eventType"] != "insertText"));
        assert_eq!(
            short
                .iter()
                .filter(|event| event["eventType"] == "keyDown")
                .count(),
            2
        );
        let long = "x".repeat(65);
        assert_eq!(
            native_inserted_events(&long),
            [serde_json::json!({"type":"input_keyboard","eventType":"insertText","text":long})]
        );
    }

    /// `keydown` and `keyup` name the physical key the helper knows, and a
    /// printable key types its text only as it goes down.
    #[test]
    fn a_key_transition_names_its_physical_key() {
        let shift = native_key_transition("Shift", "keyDown");
        assert_eq!(
            (shift["key"].as_str(), shift["code"].as_str()),
            (Some("Shift"), Some("ShiftLeft"))
        );
        assert!(shift.get("text").is_none());
        assert_eq!(
            native_key_transition("ctrl", "keyUp")["code"],
            "ControlLeft"
        );
        assert_eq!(native_key_transition("a", "keyDown")["text"], "a");
        assert!(native_key_transition("a", "keyUp").get("text").is_none());
        assert!(native_key_transition("Enter", "keyDown")
            .get("text")
            .is_none());
    }

    #[test]
    fn native_text_types_keymap_characters_and_pastes_the_rest_one_at_a_time() {
        let events = native_text_events("a1.\né漢字!");
        let kinds: Vec<(String, String)> = events
            .iter()
            .map(|event| {
                (
                    event["eventType"].as_str().unwrap().to_string(),
                    event["key"]
                        .as_str()
                        .or(event["text"].as_str())
                        .unwrap()
                        .to_string(),
                )
            })
            .collect();
        assert_eq!(
            kinds,
            [
                ("keyDown", "a"),
                ("keyUp", "a"),
                ("keyDown", "1"),
                ("keyUp", "1"),
                ("keyDown", "."),
                ("keyUp", "."),
                ("keyDown", "Enter"),
                ("keyUp", "Enter"),
                ("insertText", "é"),
                ("insertText", "漢"),
                ("insertText", "字"),
                ("keyDown", "!"),
                ("keyUp", "!"),
            ]
            .map(|(kind, key)| (kind.to_string(), key.to_string()))
        );
        assert_eq!(events[0]["text"], "a");
        assert_eq!(events[0]["code"], "KeyA");
        assert_eq!(events[0]["modifiers"], 0);
        assert!(events[1].get("text").is_none());
        // Enter carries no text: the helper presses the physical key.
        assert!(events[6].get("text").is_none());
        assert_eq!(events[6]["code"], "Enter");
        assert_eq!(native_text_events("").len(), 0);
    }

    #[test]
    fn native_chords_name_the_physical_key_without_text() {
        let events = native_key_chord_events("a", Some(2));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["eventType"], "keyDown");
        assert_eq!(events[0]["key"], "a");
        assert_eq!(events[0]["code"], "KeyA");
        assert_eq!(events[0]["modifiers"], 2);
        assert!(events[0].get("text").is_none());
        assert_eq!(events[1]["eventType"], "keyUp");
        assert_eq!(events[1]["modifiers"], 2);
        let shifted = native_key_chord_events("Enter", Some(8));
        assert_eq!(shifted[0]["code"], "Enter");
        assert_eq!(shifted[0]["modifiers"], 8);
        let space = native_key_chord_events("Space", None);
        assert_eq!(space[0]["key"], " ");
        assert_eq!(space[0]["text"], " ");
    }

    /// Verify that `char_to_key_info` returns the correct (key, code,
    /// windowsVirtualKeyCode) triple for every character in Playwright's
    /// USKeyboardLayout.  The expected values below are taken verbatim from
    /// playwright-core/lib/server/usKeyboardLayout.js so that any drift from
    /// Playwright's behaviour is caught immediately.
    #[test]
    fn test_char_to_key_info_matches_playwright_layout() {
        // (character, expected_code, expected_vk_code)
        let cases: &[(char, &str, i32)] = &[
            // Letters – VK code must equal the uppercase ASCII value.
            ('a', "KeyA", 65),
            ('z', "KeyZ", 90),
            ('A', "KeyA", 65),
            // Digits
            ('0', "Digit0", 48),
            ('9', "Digit9", 57),
            // Punctuation – these are the values from Playwright's layout.
            // The bug that prompted this test sent '.' as VK 46 (= VK_DELETE).
            ('.', "Period", 190),
            (',', "Comma", 188),
            ('/', "Slash", 191),
            (';', "Semicolon", 186),
            ('\'', "Quote", 222),
            ('[', "BracketLeft", 219),
            (']', "BracketRight", 221),
            ('\\', "Backslash", 220),
            ('`', "Backquote", 192),
            ('-', "Minus", 189),
            ('=', "Equal", 187),
            // Shifted variants produced by the same physical keys.
            ('>', "Period", 190),
            ('<', "Comma", 188),
            ('?', "Slash", 191),
            (':', "Semicolon", 186),
            ('"', "Quote", 222),
            ('{', "BracketLeft", 219),
            ('}', "BracketRight", 221),
            ('|', "Backslash", 220),
            ('~', "Backquote", 192),
            ('_', "Minus", 189),
            ('+', "Equal", 187),
            // Whitespace / control
            (' ', "Space", 32),
            ('\n', "Enter", 13),
            ('\t', "Tab", 9),
        ];

        for &(ch, expected_code, expected_vk) in cases {
            let (key, code, vk) = char_to_key_info(ch);
            assert_eq!(
                code, expected_code,
                "char {:?}: expected code {:?}, got {:?}",
                ch, expected_code, code
            );
            assert_eq!(
                vk, expected_vk,
                "char {:?}: expected VK {}, got {} (ASCII would be {})",
                ch, expected_vk, vk, ch as i32
            );
            // key should be the character itself (except control chars).
            if !ch.is_control() {
                assert_eq!(key, ch.to_string(), "char {:?}: key mismatch", ch);
            }
        }
    }

    /// Regression test: period must NEVER map to VK 46 (VK_DELETE).
    #[test]
    fn test_period_is_not_vk_delete() {
        let (_, _, vk) = char_to_key_info('.');
        assert_ne!(
            vk, 46,
            "Period must not use VK code 46 (VK_DELETE); expected 190 (VK_OEM_PERIOD)"
        );
        assert_eq!(vk, 190);
    }

    /// Characters outside the US keyboard layout should return (key, "", 0)
    /// so that `type_text` falls back to `Input.insertText`.
    #[test]
    fn test_unmapped_chars_return_zero_keycode() {
        for ch in ['@', '#', '$', '%', '^', '&', '*', '(', ')', '€', '£', '你'] {
            let (key, code, vk) = char_to_key_info(ch);
            assert_eq!(
                code, "",
                "char {:?}: unmapped char should have empty code, got {:?}",
                ch, code
            );
            assert_eq!(
                vk, 0,
                "char {:?}: unmapped char should have VK 0, got {}",
                ch, vk
            );
            assert_eq!(key, ch.to_string());
        }
    }

    #[test]
    fn test_key_text_returns_correct_text_for_special_keys() {
        assert_eq!(key_text("Enter"), Some("\r".to_string()));
        assert_eq!(key_text("Tab"), Some("\t".to_string()));
        assert_eq!(key_text(" "), Some(" ".to_string()));
        // Single printable characters carry themselves.
        assert_eq!(key_text("a"), Some("a".to_string()));
        assert_eq!(key_text("Z"), Some("Z".to_string()));
        // Non-printable named keys return None.
        assert_eq!(key_text("Escape"), None);
        assert_eq!(key_text("ArrowUp"), None);
        assert_eq!(key_text("Backspace"), None);
        assert_eq!(key_text("Delete"), None);
    }
}
