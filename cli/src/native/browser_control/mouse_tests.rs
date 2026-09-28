use super::*;
use crate::native::browser_control::BrowserControl;
use futures_util::{SinkExt, StreamExt};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

fn mapping(scale: f64, screen_y: f64) -> Mapping {
    Mapping {
        session: "page".into(),
        pointer: NativePointer {
            source_page: None,
            context: 7,
            page_generation: "page".into(),
            client_x: 210.0,
            client_y: 175.0,
            screen_x: 231.0,
            screen_y,
            geometry: json!({"scale":scale}),
        },
        surface: "surface".into(),
        window: (1, 0, 0, 2560, 1440),
    }
}

#[test]
fn measured_native_origin_covers_zoom_fullscreen_and_devtools_without_chrome_height() {
    let surface = Surface::new(2560, 1440);
    for (screen_y, expected_y) in [(279.5, 559.0), (192.5, 385.0)] {
        let mapping = mapping(2.2, screen_y);
        assert_eq!(
            mapping.point(210.0, 175.0, &surface).unwrap(),
            (462.0, expected_y)
        );
        assert_eq!(
            mapping.point(260.0, 175.0, &surface).unwrap(),
            (572.0, expected_y)
        );
        // The inverse names the page point a display point shows.
        let (x, y) = mapping.page((572.0, expected_y), &surface).unwrap();
        assert!((x - 260.0).abs() < 1e-9 && (y - 175.0).abs() < 1e-9);
    }
}

/// Inside a size class the framebuffer is larger than the window: a
/// point past the window's edge is refused, not pressed on the root.
#[test]
fn pointer_mapping_refuses_points_past_the_window_inside_a_larger_framebuffer() {
    let surface = Surface::new(1792, 1280);
    let mut mapping = mapping(2.0, 262.0);
    mapping.window = (1, 0, 0, 1560, 1200);
    // Page x 210 is display x 462; 1 CSS px is 2 display px.
    assert_eq!(mapping.point(758.5, 175.0, &surface).unwrap().0, 1559.0);
    for (x, y) in [(759.0, 175.0), (850.0, 300.0), (210.0, 644.0)] {
        assert_eq!(
            mapping.point(x, y, &surface).unwrap_err(),
            "The mouse position is outside the current browser window."
        );
    }
}

#[test]
fn pointer_mapping_rejects_outside_and_nonfinite_coordinates() {
    let surface = Surface::new(2560, 1440);
    let mapping = mapping(2.2, 279.5);
    for (x, y) in [
        (f64::NAN, 1.0),
        (f64::INFINITY, 1.0),
        (-1000.0, 0.0),
        (10000.0, 0.0),
        (0.0, 10000.0),
    ] {
        assert!(mapping.point(x, y, &surface).is_err());
    }
}

/// A point the page's viewport does not show is refused before any travel
/// toward it, even where the window would still take it (the toolbar).
#[test]
fn a_point_outside_the_viewport_is_refused_before_it_is_travelled_to() {
    let surface = Surface::new(2560, 1440);
    let mut mapping = mapping(2.0, 262.0);
    mapping.pointer.geometry = json!({"scale":2.0,"width":1280.0,"height":640.0});
    for (x, y) in [(10.0, -40.0), (1280.0, 10.0), (10.0, 640.0)] {
        let refused = mapping.point(x, y, &surface).unwrap_err();
        assert!(refused.contains("is outside the visible page"), "{refused}");
        assert!(refused.contains("1280x640 CSS pixels"), "{refused}");
    }
    assert!(mapping.point(1000.0, 400.0, &surface).is_ok());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn failed_readback_releases_held_input_but_preserves_the_original_failure() {
    let (display, peer, _frames) = DisplayClient::test_channel();
    let server = tokio::spawn(async move {
        let mut peer = BufReader::new(peer);
        let mut line = String::new();
        peer.read_line(&mut line).await.unwrap();
        let command: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(command["op"], "reset");
        let reply = format!("{}\n", json!({"id":command["id"],"success":true,"data":{}}));
        peer.get_mut().write_all(reply.as_bytes()).await.unwrap();
    });
    let mut mouse = NativeMouse {
        buttons: 1,
        modifiers: 2,
        ..Default::default()
    };
    mouse.mappings.insert("page".into(), mapping(2.0, 262.0));
    let original = "browser_control_outcome_unknown: Renderer readback failed after the press";
    assert_eq!(
        mouse
            .finish(Err(original.into()), &display)
            .await
            .unwrap_err()
            .error,
        original
    );
    assert!(!mouse.needs_release());
    assert!(mouse.mappings.is_empty());
    server.await.unwrap();
}

/// A stop the driver chose is not an unknown outcome: it knows what it
/// sent. Its held button is released and it keeps its own report.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_interrupted_held_gesture_releases_and_stays_interrupted() {
    let (display, peer, _frames) = DisplayClient::test_channel();
    let server = tokio::spawn(async move {
        let mut peer = BufReader::new(peer);
        let mut line = String::new();
        peer.read_line(&mut line).await.unwrap();
        let command: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(command["op"], "reset");
        let reply = format!("{}\n", json!({"id":command["id"],"success":true,"data":{}}));
        peer.get_mut().write_all(reply.as_bytes()).await.unwrap();
    });
    let mut mouse = NativeMouse {
        buttons: 1,
        ..Default::default()
    };
    let failure = mouse
        .finish(
            Err(stopped(InterruptReason::HumanControl, Acted::Held)),
            &display,
        )
        .await
        .unwrap_err();
    assert!(
        failure.error.starts_with("browser_operation_interrupted: "),
        "{}",
        failure.error
    );
    assert_eq!(
        failure.data,
        Some(
            json!({"interruptedBy":"human","executionStopped":true,"effectsMayHaveOccurred":true})
        )
    );
    assert!(!mouse.needs_release());
    server.await.unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn lost_reset_receipt_retains_unknown_hold_and_fences_new_input() {
    let (display, peer, _frames) = DisplayClient::test_channel();
    let server = tokio::spawn(async move {
        let mut peer = BufReader::new(peer);
        let mut line = String::new();
        peer.read_line(&mut line).await.unwrap();
        let command: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(command["op"], "reset");
        // The helper closes without acknowledging this cleanup.
    });
    let mut mouse = NativeMouse {
        buttons: 1,
        ..Default::default()
    };
    mouse.mappings.insert("page".into(), mapping(2.0, 262.0));
    let error = mouse
        .finish(Err("Original mouse action failed".into()), &display)
        .await
        .unwrap_err()
        .error;
    assert!(error.starts_with("browser_control_outcome_unknown:"));
    assert!(error.contains("Original mouse action failed"));
    assert!(error.contains("release is unconfirmed"));
    assert!(mouse.unknown);
    assert_eq!(mouse.buttons, 1);
    assert_eq!(mouse.mappings.len(), 1);
    assert!(mouse
        .require_known()
        .unwrap_err()
        .starts_with("browser_control_outcome_unknown:"));
    assert!(!display.available());
    server.await.unwrap();
}

/// A fake page and display helper that behave as the real pair does: the
/// page is shown at display (2x, 160 + 2y) for CSS point (x, y) in a
/// 1280x640 viewport, and while its pointer realm is armed every native
/// pointer event the helper performs over it comes back as the renderer's
/// trusted report.
#[cfg(target_os = "linux")]
struct Fake {
    client: CdpClient,
    display: Arc<DisplayClient>,
    page: Arc<StdMutex<Page>>,
    helper: Arc<StdMutex<Vec<(Instant, Value)>>>,
    window: Arc<StdMutex<Value>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

#[cfg(target_os = "linux")]
impl Drop for Fake {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[cfg(target_os = "linux")]
struct Page {
    commands: Vec<Value>,
    geometry: Value,
    blocker: Option<String>,
    fail_readback: bool,
    token: Option<String>,
    scroller: Scroller,
}

/// The page's vertical scroller as Chrome drives it: each wheel notch moves
/// its target 120 CSS px, and each read finds it at most 40 px nearer the
/// target, the way smooth scrolling animates frame by frame.
#[cfg(target_os = "linux")]
struct Scroller {
    at: f64,
    target: f64,
    most: f64,
    /// Whether the wheel reaches it (a canvas that takes the wheel does not).
    wheel: bool,
    /// What the wheel-point probe answers.
    point: Value,
}

#[cfg(target_os = "linux")]
impl Scroller {
    fn turned(&mut self, notches: f64) {
        if self.wheel {
            self.target = (self.target + 120.0 * notches).clamp(0.0, self.most);
        }
    }

    fn read(&mut self) -> Value {
        self.at += (self.target - self.at).clamp(-40.0, 40.0);
        json!([0.0, self.at, 0.0, self.most])
    }
}

#[cfg(target_os = "linux")]
const TOOLBAR: f64 = 160.0;

#[cfg(target_os = "linux")]
fn shown(x: f64, y: f64) -> (f64, f64) {
    (2.0 * x, TOOLBAR + 2.0 * y)
}

#[cfg(target_os = "linux")]
fn page_geometry() -> Value {
    json!({"scale":2.0,"width":1280.0,"height":640.0,"offsetX":0.0,"offsetY":0.0})
}

#[cfg(target_os = "linux")]
fn window(x: i32) -> Value {
    json!({"id":1,"pid":1,"x":x,"y":0,"width":2560,"height":1440,"mapped":true,
        "focused":true,"overrideRedirect":false,"windowType":"normal"})
}

#[cfg(target_os = "linux")]
fn answer(page: &mut Page, command: &Value) -> Value {
    let method = command["method"].as_str().unwrap_or_default();
    let expression = command["params"]["expression"].as_str().unwrap_or_default();
    let result = match method {
        "Page.getFrameTree" => json!({"frameTree":{"frame":{"id":"frame"}}}),
        "Page.createIsolatedWorld" => json!({"executionContextId":7}),
        "DOM.getNodeForLocation" => json!({"backendNodeId":99}),
        "Runtime.evaluate" if expression == GEOMETRY => {
            json!({"result":{"value":page.geometry}})
        }
        "Runtime.evaluate" if expression.contains("__ambitPointerToken = ") => {
            let token = expression
                .split("__ambitPointerToken = \"")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .unwrap();
            page.token = Some(token.into());
            json!({"result":{"value":true}})
        }
        "Runtime.evaluate" if expression.contains("globalThis.__ambitAim = hit") => {
            json!({"result":{"value":40}})
        }
        "Runtime.evaluate" if expression.contains("el = globalThis.__ambitAim") => {
            let found = page.blocker.as_ref().map_or(
                Value::Null,
                |blocker| json!({"blocker":blocker,"scrollX":0,"scrollY":0}),
            );
            json!({"result":{"value":found}})
        }
        "Runtime.evaluate"
            if (expression == "true" || expression == NEXT_FRAME) && page.fail_readback =>
        {
            return json!({"id":command["id"],"error":{"code":-32000,"message":"Renderer readback timeout after native press"}});
        }
        "Runtime.evaluate" if expression == "true" || expression == NEXT_FRAME => {
            json!({"result":{"value":true}})
        }
        "Runtime.evaluate" if expression.contains("document.scrollingElement") => {
            json!({"result":{"objectId":"scroller"}})
        }
        "Runtime.callFunctionOn" => {
            let function = command["params"]["functionDeclaration"]
                .as_str()
                .unwrap_or_default();
            let scroller = &mut page.scroller;
            if function == super::scroll::OFFSETS {
                json!({"result":{"value":scroller.read()}})
            } else if function == super::scroll::WHEEL_POINT {
                json!({"result":{"value":scroller.point}})
            } else if function.contains("scrollBy") {
                let by = command["params"]["arguments"][1]["value"].as_f64().unwrap();
                scroller.at = (scroller.at + by).clamp(0.0, scroller.most);
                scroller.target = scroller.at;
                json!({"result":{}})
            } else {
                json!({})
            }
        }
        _ => json!({}),
    };
    json!({"id":command["id"],"result":result})
}

#[cfg(target_os = "linux")]
impl Fake {
    async fn new() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let page = Arc::new(StdMutex::new(Page {
            commands: Vec::new(),
            geometry: page_geometry(),
            blocker: None,
            fail_readback: false,
            token: None,
            scroller: Scroller {
                at: 0.0,
                target: 0.0,
                most: 5000.0,
                wheel: true,
                point: json!([640.0, 320.0, 640.0]),
            },
        }));
        let (pointer_to, mut pointer) = mpsc::unbounded_channel::<(f64, f64)>();
        let browser_page = page.clone();
        let browser = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
            loop {
                tokio::select! {
                    message = socket.next() => {
                        let Some(Ok(Message::Text(text))) = message else { break };
                        let command: Value = serde_json::from_str(&text).unwrap();
                        let reply = {
                            let mut page = browser_page.lock().unwrap();
                            page.commands.push(command.clone());
                            answer(&mut page, &command)
                        };
                        if socket.send(Message::Text(reply.to_string())).await.is_err() { break }
                    }
                    moved = pointer.recv() => {
                        let Some((x, y)) = moved else { break };
                        let report = {
                            let page = browser_page.lock().unwrap();
                            page.token.clone().filter(|_| y >= TOOLBAR).map(|token| json!({
                                "method":"Runtime.bindingCalled","sessionId":"page","params":{
                                "name":crate::native::activity::POINTER_BINDING,
                                "payload":json!({"token":token,"eventType":"move",
                                    "clientX":x / 2.0,"clientY":(y - TOOLBAR) / 2.0,
                                    "screenX":x / 2.0,"screenY":y / 2.0,"geometry":page.geometry}).to_string()}}))
                        };
                        if let Some(report) = report {
                            if socket.send(Message::Text(report.to_string())).await.is_err() { break }
                        }
                    }
                }
            }
        });
        let client = CdpClient::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        let (display, peer, _frames) = DisplayClient::test_channel();
        let helper = Arc::new(StdMutex::new(Vec::new()));
        let window = Arc::new(StdMutex::new(self::window(0)));
        let helper_log = helper.clone();
        let helper_window = window.clone();
        let helper_page = page.clone();
        let helper_task = tokio::spawn(async move {
            let _frames = _frames;
            let mut peer = BufReader::new(peer);
            let mut line = String::new();
            while peer.read_line(&mut line).await.is_ok_and(|read| read > 0) {
                let request: Value = serde_json::from_str(&line).unwrap();
                line.clear();
                helper_log
                    .lock()
                    .unwrap()
                    .push((Instant::now(), request.clone()));
                let data = match request["op"].as_str().unwrap() {
                    "info" => json!({"width":2560,"height":1440,
                        "windows":[helper_window.lock().unwrap().clone()]}),
                    "input" => {
                        for event in request["events"].as_array().unwrap() {
                            if event["type"] == "input_mouse" {
                                let _ = pointer_to.send((
                                    event["x"].as_f64().unwrap(),
                                    event["y"].as_f64().unwrap(),
                                ));
                            }
                            if event["eventType"] == "mouseWheel" {
                                let notches = event["deltaY"].as_f64().unwrap_or(0.0) / 100.0;
                                helper_page.lock().unwrap().scroller.turned(notches);
                            }
                        }
                        json!({})
                    }
                    _ => json!({}),
                };
                let reply = json!({"id":request["id"],"success":true,"data":data});
                if peer
                    .get_mut()
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        // The command in flight proved the page at the current layout.
        display.record_proof(display.layout_epoch(), false);
        Self {
            client,
            display,
            page,
            helper,
            window,
            tasks: vec![browser, helper_task],
        }
    }

    fn control(&self) -> BrowserControl {
        let mut control = BrowserControl::default();
        control.set_display(Some(self.display.clone()));
        control
    }

    /// Puts the X pointer at a display point, as a person's move would.
    async fn place_pointer(&self, point: (f64, f64)) {
        self.display
            .input(&[
                json!({"type":"input_mouse","eventType":"mouseMoved","x":point.0,"y":point.1}),
            ])
            .await
            .unwrap();
        self.helper.lock().unwrap().clear();
    }

    fn helper_inputs(&self) -> Vec<(Instant, Value)> {
        self.helper
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, request)| request["op"] == "input")
            .flat_map(|(at, request)| {
                request["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|event| (*at, event.clone()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn helper_ops(&self) -> Vec<String> {
        self.helper
            .lock()
            .unwrap()
            .iter()
            .map(|(_, request)| request["op"].as_str().unwrap().to_owned())
            .collect()
    }

    fn page_commands(&self, needle: &str) -> usize {
        self.page
            .lock()
            .unwrap()
            .commands
            .iter()
            .filter(|command| command.to_string().contains(needle))
            .count()
    }
}

#[cfg(target_os = "linux")]
fn press(x: f64, y: f64) -> Value {
    json!({"type":"mousePressed","x":x,"y":y,"button":"left","buttons":1,"clickCount":1})
}

#[cfg(target_os = "linux")]
fn moved(x: f64, y: f64) -> Value {
    json!({"type":"mouseMoved","x":x,"y":y})
}

/// A click far from the pointer: the pointer travels there along a
/// straight minimum-jerk path, one helper sample per frame, each published
/// as the agent's pointer with its media-clock time, and the button goes
/// down only at the end of the travel. The page is measured natively where
/// the pointer was (no CDP hover anywhere), and intermediate samples read
/// nothing back from the page.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_click_far_from_the_pointer_travels_there_before_its_press() {
    let fake = Fake::new().await;
    let mut control = fake.control();
    let mut activity = fake.client.subscribe();
    fake.place_pointer((100.0, 400.0)).await;
    let target = (600.0, 300.0);
    assert!(!control
        .agent_native_mouse(moved(target.0, target.1), &fake.client, "page", &["page"])
        .await
        .unwrap());
    assert!(!control
        .agent_native_mouse(press(target.0, target.1), &fake.client, "page", &["page"])
        .await
        .unwrap());

    let inputs = fake.helper_inputs();
    // The measurement: one pixel toward the window's centre.
    assert_eq!(
        (inputs[0].1["x"].as_f64(), inputs[0].1["y"].as_f64()),
        (Some(101.0), Some(400.0))
    );
    let pressed = inputs
        .iter()
        .position(|(_, event)| event["eventType"] == "mousePressed")
        .unwrap();
    assert_eq!(pressed, inputs.len() - 1, "the press is the last input");
    let travel: Vec<_> = inputs[1..pressed].iter().collect();
    let destination = shown(target.0, target.1);
    let (last_at, last) = travel.last().unwrap();
    assert_eq!(
        (last["x"].as_f64().unwrap(), last["y"].as_f64().unwrap()),
        destination
    );
    assert_eq!(
        (
            inputs[pressed].1["x"].as_f64().unwrap(),
            inputs[pressed].1["y"].as_f64().unwrap()
        ),
        destination
    );
    // Straight toward the destination, never back.
    let distance = |event: &Value| {
        (destination.0 - event["x"].as_f64().unwrap())
            .hypot(destination.1 - event["y"].as_f64().unwrap())
    };
    assert!(travel
        .windows(2)
        .all(|pair| distance(&pair[1].1) <= distance(&pair[0].1)));
    // (50 + 100·log2(D/W + 1)) / 2.5 with D ≈ 1157 and W = 80 display px:
    // 178 ms, at one sample per 16.7 ms frame, and the move itself.
    assert!((8..=14).contains(&travel.len()), "{} samples", travel.len());
    let spent = *last_at - travel[0].0;
    assert!(
        spent >= Duration::from_millis(120) && spent <= Duration::from_millis(260),
        "the travel took {spent:?}"
    );
    // No hover reached the page before the pointer did: the mapping came
    // from the pointer's own trusted report, not a CDP mouse event.
    assert_eq!(fake.page_commands("Input.dispatchMouseEvent"), 0);
    // The travel read the page's geometry at its start and before the
    // press, never per sample.
    assert!(
        fake.page_commands(GEOMETRY) <= 4,
        "{}",
        fake.page_commands(GEOMETRY)
    );

    // Every acknowledged sample reached viewers in order, on the media
    // clock, at its display point.
    let mut published = Vec::new();
    while let Ok(event) = activity.try_recv() {
        if event.method == crate::native::activity::EVENT {
            published.push(event.params);
        }
    }
    let moves: Vec<_> = published
        .iter()
        .filter(|event| event["eventType"] == "move" && event["source"] == "agent")
        .collect();
    assert!(moves.len() >= travel.len(), "{} published", moves.len());
    assert!(moves
        .windows(2)
        .all(|pair| pair[1]["ts"].as_u64() >= pair[0]["ts"].as_u64()));
    let screen = |event: &Value| {
        (
            event["screenX"].as_f64().unwrap() * 2.0,
            event["screenY"].as_f64().unwrap() * 2.0,
        )
    };
    assert_eq!(screen(moves.last().unwrap()), destination);
    assert_eq!(
        published.last().unwrap()["eventType"],
        "press",
        "the press is published after its travel"
    );
}

/// A pointer with no known place (the first gesture of a display) appears
/// at the window's centre and is measured there by its own trusted report:
/// no CDP hover reaches the page, and the travel starts where it appeared.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pointer_with_no_known_place_appears_at_the_windows_centre() {
    let fake = Fake::new().await;
    let mut control = fake.control();
    assert_eq!(fake.display.pointer(), None);
    control
        .agent_native_mouse(moved(100.0, 100.0), &fake.client, "page", &["page"])
        .await
        .unwrap();
    let inputs = fake.helper_inputs();
    assert_eq!(
        (inputs[0].1["x"].as_f64(), inputs[0].1["y"].as_f64()),
        (Some(1280.0), Some(720.0))
    );
    assert!(inputs.len() > 3, "it travelled from there");
    assert_eq!(fake.page_commands("Input.dispatchMouseEvent"), 0);
    assert_eq!(fake.display.pointer(), Some(shown(100.0, 100.0)));
}

/// A takeover stops the travel at its next sample: the press never goes,
/// and the agent learns that a person holds the browser.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_takeover_stops_a_travel_within_one_sample_and_presses_nothing() {
    let fake = Fake::new().await;
    let mut control = fake.control();
    let interrupts = control.interrupts();
    fake.place_pointer((20.0, 1300.0)).await;
    let gesture = async {
        let result = control
            .agent_native_mouse(press(1270.0, 10.0), &fake.client, "page", &["page"])
            .await;
        (result, Instant::now())
    };
    let takeover = async {
        // Let a few samples go.
        while fake.helper_inputs().len() < 4 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        (
            interrupts.raise(InterruptReason::HumanControl),
            Instant::now(),
        )
    };
    let ((result, stopped_at), (takeover, raised_at)) =
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(gesture, takeover)
        })
        .await
        .unwrap();
    drop(takeover);
    let failure = result.unwrap_err();
    let stopped_after = stopped_at - raised_at;
    assert!(
        stopped_after <= motion::FRAME + Duration::from_millis(15),
        "stopped {stopped_after:?} after the takeover"
    );
    assert!(
        failure.error.starts_with("browser_controlled_by_user: "),
        "{}",
        failure.error
    );
    let inputs = fake.helper_inputs();
    assert!(
        inputs.len() < 10,
        "the travel stopped early: {}",
        inputs.len()
    );
    assert!(inputs
        .iter()
        .all(|(_, event)| event["eventType"] == "mouseMoved"));
    assert!(
        !fake.helper_ops().contains(&"reset".to_string()),
        "nothing was held"
    );
}

/// A takeover during a drag's held travel stops it, releases the held
/// button, and reports the drag interrupted, not unknown.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_takeover_during_a_drag_releases_its_button_and_reports_it_interrupted() {
    let fake = Fake::new().await;
    let mut control = fake.control();
    let interrupts = control.interrupts();
    fake.place_pointer(shown(20.0, 20.0)).await;
    let drag = async {
        control
            .agent_native_drag(
                &fake.client,
                "page",
                ("page", 20.0, 20.0),
                ("page", 1270.0, 630.0),
            )
            .await
    };
    let takeover = async {
        loop {
            let held = fake
                .helper_inputs()
                .iter()
                .skip_while(|(_, event)| event["eventType"] != "mousePressed")
                .count();
            if held >= 4 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        interrupts.raise(InterruptReason::HumanControl)
    };
    let (result, takeover) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(drag, takeover)
    })
    .await
    .unwrap();
    drop(takeover);
    let failure = result.unwrap_err();
    assert!(
        failure.error.starts_with("browser_operation_interrupted: "),
        "{}",
        failure.error
    );
    assert_eq!(failure.data.unwrap()["interruptedBy"], "human");
    assert_eq!(fake.helper_ops().last().map(String::as_str), Some("reset"));
    assert!(!fake
        .helper_inputs()
        .iter()
        .any(|(_, event)| event["eventType"] == "mouseReleased"));
    assert!(!control.native_mouse.needs_release());
}

/// A layer that opened while the pointer travelled refuses the press at
/// the end of travel: nothing is pressed and the refusal names the check.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_press_is_refused_when_its_target_is_covered_at_the_end_of_travel() {
    let fake = Fake::new().await;
    let mut control = fake.control();
    fake.place_pointer((100.0, 400.0)).await;
    fake.page.lock().unwrap().blocker = Some("div.cookie-banner".into());
    let failure = control
        .agent_native_mouse(press(600.0, 300.0), &fake.client, "page", &["page"])
        .await
        .unwrap_err();
    assert!(
        failure.error.starts_with("browser_observation_stale: "),
        "{}",
        failure.error
    );
    assert!(
        failure.error.contains("div.cookie-banner"),
        "{}",
        failure.error
    );
    assert_eq!(
        failure.data,
        Some(json!({"precondition":"hitTest","observed":99}))
    );
    let inputs = fake.helper_inputs();
    assert!(inputs.len() > 3, "the pointer travelled");
    assert!(inputs
        .iter()
        .all(|(_, event)| event["eventType"] == "mouseMoved"));
    assert!(!control.needs_observation(), "nothing uncertain was sent");
}

/// The mapping is kept across commands while its page generation, window
/// and page geometry stand: later gestures measure nothing. Each of the
/// three changing makes the next gesture measure again.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_mapping_is_kept_until_its_page_window_or_geometry_changes() {
    let fake = Fake::new().await;
    let mut control = fake.control();
    fake.place_pointer((100.0, 400.0)).await;
    let measurements = || fake.page_commands("__ambitPointerToken = ");
    let mut point = 100.0;
    async fn gesture(control: &mut BrowserControl, fake: &Fake, point: &mut f64) {
        *point += 40.0;
        control
            .agent_native_mouse(moved(*point, 200.0), &fake.client, "page", &["page"])
            .await
            .unwrap();
    }
    gesture(&mut control, &fake, &mut point).await;
    assert_eq!(measurements(), 1);
    gesture(&mut control, &fake, &mut point).await;
    gesture(&mut control, &fake, &mut point).await;
    assert_eq!(measurements(), 1, "kept across commands");

    fake.client.rotate_page_generation("page");
    gesture(&mut control, &fake, &mut point).await;
    assert_eq!(measurements(), 2, "a new page generation");

    *fake.window.lock().unwrap() = window(4);
    gesture(&mut control, &fake, &mut point).await;
    assert_eq!(measurements(), 3, "the window moved");

    fake.page.lock().unwrap().geometry["scale"] = json!(2.5);
    gesture(&mut control, &fake, &mut point).await;
    assert_eq!(measurements(), 4, "the page zoomed");
    gesture(&mut control, &fake, &mut point).await;
    assert_eq!(measurements(), 4);
}

/// A wheel without coordinates turns where the pointer is, not at (0, 0),
/// and does not travel.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wheel_without_coordinates_turns_where_the_pointer_is() {
    let fake = Fake::new().await;
    let mut control = fake.control();
    fake.place_pointer((700.0, 900.0)).await;
    control
        .agent_input(
            "input_mouse",
            json!({"type":"mouseWheel","deltaX":0,"deltaY":100}),
            &fake.client,
            "page",
        )
        .await
        .unwrap();
    let inputs = fake.helper_inputs();
    let (_, wheel) = inputs.last().unwrap();
    assert_eq!(wheel["eventType"], "mouseWheel");
    // Within a pixel of the pointer: the measurement nudged it by one.
    assert!(
        (wheel["x"].as_f64().unwrap() - 700.0).abs() <= 1.0,
        "{wheel}"
    );
    assert_eq!(wheel["y"].as_f64(), Some(900.0));
    assert!(
        inputs
            .iter()
            .filter(|(_, event)| event["eventType"] == "mouseMoved")
            .count()
            <= 1,
        "no travel: {inputs:?}"
    );
}

#[cfg(target_os = "linux")]
fn wheels(inputs: &[(Instant, Value)]) -> Vec<&(Instant, Value)> {
    inputs
        .iter()
        .filter(|(_, event)| event["eventType"] == "mouseWheel")
        .collect()
}

#[cfg(target_os = "linux")]
fn agent_activity(
    activity: &mut tokio::sync::broadcast::Receiver<crate::native::cdp::types::CdpEvent>,
) -> Vec<Value> {
    let mut published = Vec::new();
    while let Ok(event) = activity.try_recv() {
        if event.method == crate::native::activity::EVENT && event.params["source"] == "agent" {
            published.push(event.params);
        }
    }
    published
}

/// A wheel is never a burst: each notch goes one frame after the last, at
/// the same point, and each reaches viewers as it is acknowledged.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wheel_turns_one_notch_per_frame() {
    let fake = Fake::new().await;
    let mut control = fake.control();
    let mut activity = fake.client.subscribe();
    fake.place_pointer(shown(400.0, 300.0)).await;
    control
        .agent_input(
            "input_mouse",
            json!({"type":"mouseWheel","x":400.0,"y":300.0,"deltaX":0,"deltaY":250}),
            &fake.client,
            "page",
        )
        .await
        .unwrap();
    let inputs = fake.helper_inputs();
    let wheels = wheels(&inputs);
    let deltas: Vec<_> = wheels
        .iter()
        .map(|(_, event)| event["deltaY"].as_f64().unwrap())
        .collect();
    assert_eq!(deltas, [100.0, 100.0, 50.0]);
    assert!(wheels.iter().all(|(_, event)| {
        (event["x"].as_f64().unwrap(), event["y"].as_f64().unwrap()) == shown(400.0, 300.0)
    }));
    for pair in wheels.windows(2) {
        let gap = pair[1].0 - pair[0].0;
        assert!(gap >= motion::FRAME - Duration::from_millis(2), "{gap:?}");
    }
    let scrolls = agent_activity(&mut activity)
        .into_iter()
        .filter(|event| event["eventType"] == "scroll")
        .count();
    assert_eq!(scrolls, 3);
}

/// `scroll` travels to where the wheel reaches the scroller, then turns it
/// one notch per frame: the nearest whole number of notches, read back from
/// the scroller, with no script scroll and nothing invented.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scroll_turns_the_wheel_where_it_reaches_the_scroller_until_it_is_there() {
    let fake = Fake::new().await;
    let mut control = fake.control();
    let mut activity = fake.client.subscribe();
    fake.place_pointer((100.0, 1200.0)).await;
    control
        .agent_native_scroll(&fake.client, "page", None, (0.0, 300.0))
        .await
        .unwrap();
    let inputs = fake.helper_inputs();
    let wheels = wheels(&inputs);
    // 300 CSS px at 120 a notch is 2.5 notches: three.
    assert_eq!(wheels.len(), 3, "{inputs:?}");
    assert!(wheels
        .iter()
        .all(|(_, event)| event["deltaY"] == 100.0 && event["deltaX"] == 0.0));
    let point = shown(640.0, 320.0);
    assert!(wheels.iter().all(|(_, event)| {
        (event["x"].as_f64().unwrap(), event["y"].as_f64().unwrap()) == point
    }));
    let first = inputs
        .iter()
        .position(|(_, event)| event["eventType"] == "mouseWheel")
        .unwrap();
    assert!(first > 2, "the pointer travelled first: {inputs:?}");
    let arrived = &inputs[first - 1].1;
    assert_eq!(
        (
            arrived["x"].as_f64().unwrap(),
            arrived["y"].as_f64().unwrap()
        ),
        point
    );
    for pair in wheels.windows(2) {
        let gap = pair[1].0 - pair[0].0;
        assert!(gap >= motion::FRAME - Duration::from_millis(2), "{gap:?}");
    }
    assert_eq!(fake.page.lock().unwrap().scroller.at, 360.0);
    assert_eq!(fake.page_commands("scrollBy"), 0);
    let published = agent_activity(&mut activity);
    assert_eq!(
        published
            .iter()
            .filter(|event| event["eventType"] == "scroll")
            .count(),
        3
    );
    assert!(!published.iter().any(|event| event["kind"] == "scrolling"));
}

/// A scroller the wheel does not move (a canvas takes it, overflow a person
/// cannot scroll) is scrolled by script once the stall passes: one notch
/// went, then the script, published as `scrolling` with no path.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scroll_the_wheel_cannot_move_goes_by_script_after_the_stall() {
    let fake = Fake::new().await;
    let mut control = fake.control();
    let mut activity = fake.client.subscribe();
    fake.place_pointer(shown(640.0, 320.0)).await;
    fake.page.lock().unwrap().scroller.wheel = false;
    let started = Instant::now();
    control
        .agent_native_scroll(&fake.client, "page", None, (0.0, 300.0))
        .await
        .unwrap();
    let spent = started.elapsed();
    assert!(
        spent >= motion::WHEEL_STALL && spent < motion::WHEEL_STALL + Duration::from_millis(300),
        "{spent:?}"
    );
    assert_eq!(wheels(&fake.helper_inputs()).len(), 1);
    assert_eq!(fake.page_commands("scrollBy"), 1);
    assert_eq!(fake.page.lock().unwrap().scroller.at, 300.0);
    assert!(agent_activity(&mut activity)
        .iter()
        .any(|event| event["kind"] == "scrolling"));
}

/// No wheel where no visible point reaches the scroller, and none for less
/// than half a notch: those go by script at once.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scroll_no_wheel_can_do_goes_by_script_at_once() {
    let fake = Fake::new().await;
    let mut control = fake.control();
    fake.place_pointer(shown(640.0, 320.0)).await;
    control
        .agent_native_scroll(&fake.client, "page", None, (0.0, 50.0))
        .await
        .unwrap();
    assert_eq!(fake.page.lock().unwrap().scroller.at, 50.0);
    fake.page.lock().unwrap().scroller.point = Value::Null;
    let started = Instant::now();
    control
        .agent_native_scroll(&fake.client, "page", None, (0.0, 600.0))
        .await
        .unwrap();
    assert!(started.elapsed() < Duration::from_millis(200));
    assert_eq!(fake.page.lock().unwrap().scroller.at, 650.0);
    assert_eq!(fake.page_commands("scrollBy"), 2);
    assert!(wheels(&fake.helper_inputs()).is_empty());
}

/// A takeover stops a turning wheel within a frame. The notches before it
/// went, so the scroll is interrupted part of the way, never unknown, and
/// nothing is held to release.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_takeover_stops_a_scroll_within_a_frame_part_of_the_way() {
    let fake = Fake::new().await;
    let mut control = fake.control();
    let interrupts = control.interrupts();
    fake.place_pointer(shown(640.0, 320.0)).await;
    let scroll = async {
        let result = control
            .agent_native_scroll(&fake.client, "page", None, (0.0, 3000.0))
            .await;
        (result, Instant::now())
    };
    let takeover = async {
        while wheels(&fake.helper_inputs()).len() < 3 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        (
            interrupts.raise(InterruptReason::HumanControl),
            Instant::now(),
        )
    };
    let ((result, stopped_at), (takeover, raised_at)) =
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(scroll, takeover)
        })
        .await
        .unwrap();
    drop(takeover);
    let failure = result.unwrap_err();
    assert!(
        stopped_at - raised_at <= motion::FRAME + Duration::from_millis(15),
        "stopped {:?} after the takeover",
        stopped_at - raised_at
    );
    assert!(
        failure.error.starts_with("browser_operation_interrupted: "),
        "{}",
        failure.error
    );
    assert_eq!(
        failure.data,
        Some(
            json!({"interruptedBy":"human","executionStopped":true,"effectsMayHaveOccurred":true})
        )
    );
    assert!(wheels(&fake.helper_inputs()).len() < 25);
    assert!(!fake.helper_ops().contains(&"reset".to_string()));
    assert!(!control.needs_observation());
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acknowledged_press_and_failed_page_readback_stay_unknown_through_mcp() {
    let fake = Fake::new().await;
    let mut control = fake.control();
    // The pointer is where the press lands: nothing travels.
    fake.place_pointer(shown(210.0, 175.0)).await;
    fake.page.lock().unwrap().fail_readback = true;
    let error = control
        .agent_native_mouse(press(210.0, 175.0), &fake.client, "page", &["page"])
        .await
        .unwrap_err()
        .error;
    assert!(error.starts_with("browser_control_outcome_unknown:"));
    assert!(error.contains("Renderer readback timeout"));
    assert!(!control.native_mouse.needs_release());
    assert!(control.needs_observation());
    let ops = fake.helper_ops();
    assert_eq!(ops.last().map(String::as_str), Some("reset"), "{ops:?}");
    let inputs = fake.helper_inputs();
    assert_eq!(
        inputs
            .iter()
            .filter(|(_, event)| event["eventType"] == "mousePressed")
            .count(),
        1
    );
    let result = crate::mcp::native_error_result_for_test(&error);
    assert_eq!(result["isError"], true);
    assert_eq!(result["structuredContent"]["response"]["success"], false);
    assert_eq!(
        result["structuredContent"]["response"]["code"],
        "browser_control_outcome_unknown"
    );
    assert!(result["structuredContent"]["response"]
        .get("operationPerformed")
        .is_none());
    if let Ok(path) = std::env::var("AMBIT_TEST_NATIVE_MOUSE_MCP_RESULT") {
        std::fs::write(path, serde_json::to_vec_pretty(&result).unwrap()).unwrap();
    }
}

/// Layouts no longer wait for a whole command: pointer input is refused
/// before anything is sent when a person's view resized the window after
/// the command's page proof, whether the layout landed while the command
/// ran or between its proof and its start. A new command alone does not
/// make the old proof current; the next proof does.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pointer_input_after_a_newer_layout_is_refused_before_it_is_sent() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let cdp_server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let _socket = tokio_tungstenite::accept_async(socket).await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    let client = CdpClient::connect(&format!("ws://{address}"))
        .await
        .unwrap();
    for layout_before_command in [false, true] {
        let (display, peer, _frames) = DisplayClient::test_channel();
        let mut control = BrowserControl::default();
        control.set_display(Some(display.clone()));
        // The command's preparation proved the page at the current layout.
        display.record_proof(display.layout_epoch(), false);
        let owner = display.clone();
        let resized = tokio::spawn(async move {
            let layout = owner.layout().await;
            owner
                .resize(&layout, 1560, 1200, Some(7), false)
                .await
                .map(|_| ())
        });
        let mut peer = BufReader::new(peer);
        let mut line = String::new();
        peer.read_line(&mut line).await.unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["op"], "resize");
        let response = json!({"id":request["id"],"success":true,"data":{"width":1560,"height":1200,"windows":[]}});
        peer.get_mut()
            .write_all(format!("{response}\n").as_bytes())
            .await
            .unwrap();
        resized.await.unwrap().unwrap();
        let error = control
            .agent_native_mouse(press(210.0, 175.0), &client, "page", &["page"])
            .await
            .unwrap_err()
            .error;
        assert_eq!(
            error, RESIZED,
            "layout before the command: {layout_before_command}"
        );
        line.clear();
        let quiet =
            tokio::time::timeout(Duration::from_millis(100), peer.read_line(&mut line)).await;
        assert!(quiet.is_err(), "nothing was sent: {line}");
        assert_eq!(
            control.native_mouse.same_layout(&display).unwrap_err(),
            RESIZED
        );
        display.record_proof(display.layout_epoch(), false);
        assert!(control.native_mouse.same_layout(&display).is_ok());
        // A proof a dialog blocked does not let pointer input through.
        display.record_proof(display.layout_epoch(), true);
        assert!(control.native_mouse.same_layout(&display).is_err());
    }
    cdp_server.abort();
}
