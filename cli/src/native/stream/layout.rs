//! The owned window follows its presenter outside daemon command custody.
//!
//! A layout waits only for the agent's atomic input in flight (its geometry
//! proof and native send) and a held button, never for a whole command, a
//! maintenance pass or a file observation. The frames keep flowing: the
//! helper gates them on the browser's own paint of the new geometry. The
//! agent's page proof moves to its next command (`prove_window_layout`).

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{watch, Mutex, Notify, RwLock};

use super::presentation::{Applied, Presentation};
use super::StreamMedia;
use crate::native::browser_control::custody_active;
use crate::native::display::{window_pixels, DisplayClient, DisplayInfo, DEVICE_SCALE_FACTOR};

/// Lays the owned window out at `width` × `height` CSS pixels: the shared
/// resize of a presenter's layout and a controller's `viewport` input. With
/// `size_class` the framebuffer may stay a size class larger than the window
/// (only when the helper supports it); otherwise it equals the window. An
/// unchanged window is not laid out again.
pub(crate) async fn apply(
    display: &DisplayClient,
    width: u32,
    height: u32,
    size_class: bool,
) -> Result<Applied, String> {
    let info = display.info().await.map_err(|error| error.to_string())?;
    apply_on(display, &info, width, height, size_class).await
}

/// `apply` on the display as `info` just described it.
async fn apply_on(
    display: &DisplayClient,
    info: &DisplayInfo,
    width: u32,
    height: u32,
    size_class: bool,
) -> Result<Applied, String> {
    let (display_width, display_height) = window_pixels(width, height)?;
    let window = info
        .active_window()
        .ok_or("The active browser window is ambiguous")?;
    let exact = (info.width, info.height) == (display_width, display_height);
    let unchanged = display.ready()
        && (window.x, window.y, window.width, window.height)
            == (0, 0, display_width, display_height)
        && display.window() == (display_width, display_height)
        && (exact
            || size_class
                && display.has("sizeClass")
                && info.width >= display_width
                && info.height >= display_height);
    if !unchanged {
        let layout = display.layout().await;
        display
            .resize(
                &layout,
                display_width,
                display_height,
                Some(window.id),
                size_class,
            )
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(Applied {
        surface: display.surface(),
        window: display.window(),
    })
}

/// The stream's record of the window size its viewers see, in CSS pixels.
pub(super) struct Viewport {
    pub width: Arc<Mutex<u32>>,
    pub height: Arc<Mutex<u32>>,
    pub changed: Arc<Notify>,
}

impl Viewport {
    pub(super) async fn set(&self, width: u32, height: u32) {
        let mut current_width = self.width.lock().await;
        let mut current_height = self.height.lock().await;
        if (*current_width, *current_height) == (width, height) {
            return;
        }
        *current_width = width;
        *current_height = height;
        drop(current_width);
        drop(current_height);
        self.changed.notify_one();
    }
}

/// Applies the presenter's layout whenever it, the display, the viewer
/// roster or the person's custody changes, until the stream shuts down. A
/// disconnected presenter's grace is expired on its own deadline.
#[allow(clippy::too_many_arguments)]
pub(super) async fn follow_presentation(
    presentation: Arc<Presentation>,
    display_slot: Arc<RwLock<Option<Arc<DisplayClient>>>>,
    mut display_changed: watch::Receiver<()>,
    media: Arc<StreamMedia>,
    mut custody: watch::Receiver<Option<Instant>>,
    viewport: Viewport,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut presentation_changed = presentation.subscribe();
    let mut roster = media.subscribe_roster();
    loop {
        presentation.expire();
        let display = display_slot.read().await.clone();
        if let Some(display) = display {
            let controlled = custody_active(*custody.borrow_and_update());
            follow_once(&presentation, &display, &media, controlled, &viewport).await;
        }
        let expiry = presentation.expiry();
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
            }
            changed = presentation_changed.changed() => {
                if changed.is_err() { return; }
            }
            changed = display_changed.changed() => {
                if changed.is_err() { return; }
            }
            changed = roster.changed() => {
                if changed.is_err() { return; }
            }
            changed = custody.changed() => {
                if changed.is_err() { return; }
            }
            _ = sleep_until(expiry) => {}
        }
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
        None => std::future::pending().await,
    }
}

/// One pass: the presenter's pending layout on the display's active window,
/// then the framebuffer back to the window while any viewer draws whole
/// frames.
async fn follow_once(
    presentation: &Presentation,
    display: &DisplayClient,
    media: &StreamMedia,
    controlled: bool,
    viewport: &Viewport,
) {
    if !display.available() {
        return;
    }
    let roster = media.roster();
    if presentation.configured() {
        // Frames are shared: the framebuffer may outgrow the window only
        // while every connected viewer crops to it.
        follow_presenter(
            presentation,
            display,
            roster.crops_visible(),
            controlled,
            viewport,
        )
        .await;
    }
    // Whoever presents, a viewer that draws whole frames never sees the part
    // of a size-class framebuffer outside the window.
    let surface = display.surface();
    let window = display.window();
    if roster.crop_visible < roster.viewers && (surface.width, surface.height) != window {
        if let Ok(applied) = apply(
            display,
            window.0 / DEVICE_SCALE_FACTOR,
            window.1 / DEVICE_SCALE_FACTOR,
            false,
        )
        .await
        {
            presentation.update_surface(&applied.surface, applied.window);
        }
    }
}

/// The connected presenter's pending layout, applied and acknowledged.
async fn follow_presenter(
    presentation: &Presentation,
    display: &DisplayClient,
    size_class: bool,
    controlled: bool,
    viewport: &Viewport,
) {
    let Ok(info) = display.info().await else {
        return;
    };
    let Some(window) = info.active_window() else {
        return;
    };
    let session = format!("{}:{}", display.identity(), window.id);
    let Some(request) = presentation.pending(&session, size_class, controlled) else {
        return;
    };
    let applied = apply_on(
        display,
        &info,
        request.config.width,
        request.config.height,
        size_class,
    )
    .await
    .ok();
    if let Some(applied) = applied.as_ref() {
        viewport
            .set(
                applied.window.0 / DEVICE_SCALE_FACTOR,
                applied.window.1 / DEVICE_SCALE_FACTOR,
            )
            .await;
    }
    presentation.complete(&request, session, applied);
}

// The display helper is a private X11 process: Linux only.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::native::stream::presentation::PresentationConfig;
    use serde_json::{json, Value};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    /// The helper's side of the control socket, which lists `features`.
    struct Helper(BufReader<UnixStream>, Vec<String>);

    impl Helper {
        async fn request(&mut self) -> Value {
            let mut line = String::new();
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                self.0.read_line(&mut line),
            )
            .await
            .expect("a helper request")
            .unwrap();
            serde_json::from_str(&line).unwrap()
        }

        async fn answer(&mut self, request: &Value, data: Value) {
            let reply = json!({"id": request["id"], "success": true, "data": data}).to_string();
            self.0
                .get_mut()
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
        }

        /// Answers `info` with one browser window of the given size on a
        /// display of the given size.
        async fn info(&mut self, display: (u32, u32), window: (u32, u32)) {
            let request = self.request().await;
            assert_eq!(request["op"], "info", "{request}");
            self.answer(
                &request,
                json!({
                    "width": display.0, "height": display.1, "focusWindow": 7, "features": self.1,
                    "windows": [{"id": 7, "pid": 1, "x": 0, "y": 0, "width": window.0,
                        "height": window.1, "mapped": true, "focused": true,
                        "overrideRedirect": false, "windowType": "normal"}],
                }),
            )
            .await;
        }
    }

    struct Setup {
        display: Arc<DisplayClient>,
        helper: Helper,
        presentation: Arc<Presentation>,
        media: Arc<StreamMedia>,
        viewport: Viewport,
        _frames: UnixStream,
    }

    fn setup(features: &[&str], crops: bool) -> Setup {
        let (display, control, frames) = DisplayClient::test_channel();
        display.advertise(features);
        let presentation = Arc::new(Presentation::new());
        presentation.configure(
            uuid::Uuid::new_v4(),
            PresentationConfig {
                viewer: uuid::Uuid::new_v4(),
                width: 780,
                height: 600,
                crops,
            },
        );
        let media = Arc::new(StreamMedia::new(Default::default()));
        media.viewer_joined(true, crops);
        Setup {
            display,
            helper: Helper(
                BufReader::new(control),
                features.iter().map(|f| f.to_string()).collect(),
            ),
            presentation,
            media,
            viewport: Viewport {
                width: Arc::new(Mutex::new(1280)),
                height: Arc::new(Mutex::new(720)),
                changed: Arc::new(Notify::new()),
            },
            _frames: frames,
        }
    }

    /// The presenter's layout is applied by the stream itself, with no
    /// daemon command: the framebuffer stays at a size class while every
    /// viewer crops, the window is exact, and the viewport follows it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_stream_lays_the_window_out_for_its_presenter_at_a_size_class() {
        let mut s = setup(&["sizeClass", "layoutGate"], true);
        let generation = s.display.surface().generation;
        let (display_sender, display_changed) = watch::channel(());
        let (_custody, custody) = watch::channel(None);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(follow_presentation(
            s.presentation.clone(),
            Arc::new(RwLock::new(Some(s.display.clone()))),
            display_changed,
            s.media.clone(),
            custody,
            Viewport {
                width: s.viewport.width.clone(),
                height: s.viewport.height.clone(),
                changed: s.viewport.changed.clone(),
            },
            shutdown_rx,
        ));
        s.helper.info((2560, 1440), (2560, 1440)).await;
        let resize = s.helper.request().await;
        assert_eq!(
            (
                resize["op"].as_str(),
                resize["width"].as_u64(),
                resize["height"].as_u64()
            ),
            (Some("resize"), Some(1560), Some(1200))
        );
        assert_eq!(resize["windowId"], 7);
        assert_eq!(resize["sizeClass"], true);
        s.helper
            .answer(
                &resize,
                json!({"width": 1792, "height": 1280, "windows": []}),
            )
            .await;
        // Applied: the presentation settles and the stream's viewport is
        // the window, in CSS pixels.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while s
                .presentation
                .pending(&format!("{}:7", s.display.identity()), true, false)
                .is_some()
            {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the layout settles");
        // The follower checks once more on its own completion; the window
        // already holds, so it does not resize again.
        s.helper.info((1792, 1280), (1560, 1200)).await;
        assert_eq!(s.display.window(), (1560, 1200));
        assert_eq!(
            (s.display.surface().width, s.display.surface().height),
            (1792, 1280)
        );
        assert_eq!(
            s.display.surface().generation,
            generation,
            "a resize keeps the window's generation"
        );
        assert_eq!(
            (
                *s.viewport.width.lock().await,
                *s.viewport.height.lock().await
            ),
            (780, 600)
        );
        let _ = shutdown.send(true);
        task.await.unwrap();
        drop(display_sender);
    }

    /// Without the helper's size class, or while a viewer draws the whole
    /// frame, the framebuffer is the window.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_framebuffer_is_the_window_unless_the_helper_and_every_viewer_crop() {
        for (features, crops) in [(&["layoutGate"][..], true), (&["sizeClass"][..], false)] {
            let mut s = setup(features, crops);
            let session = async {
                follow_once(&s.presentation, &s.display, &s.media, false, &s.viewport).await
            };
            let helper = async {
                s.helper.info((2560, 1440), (2560, 1440)).await;
                let resize = s.helper.request().await;
                assert!(resize.get("sizeClass").is_none(), "{resize}");
                s.helper
                    .answer(
                        &resize,
                        json!({"width": 1560, "height": 1200, "windows": []}),
                    )
                    .await;
            };
            tokio::join!(session, helper);
            assert_eq!(s.display.window(), (1560, 1200));
        }
    }

    /// While a person controls input, an older presenter's layout waits: its
    /// controller resizes through `viewport` input. A cropping presenter's
    /// layout applies at once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn under_control_only_a_cropping_presenter_resizes_the_window() {
        let mut older = setup(&["sizeClass"], false);
        let session = async {
            follow_once(
                &older.presentation,
                &older.display,
                &older.media,
                true,
                &older.viewport,
            )
            .await
        };
        let helper = async { older.helper.info((2560, 1440), (2560, 1440)).await };
        tokio::join!(session, helper);
        let mut line = String::new();
        let quiet = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            older.helper.0.read_line(&mut line),
        )
        .await;
        assert!(quiet.is_err(), "no resize under control: {line}");

        let mut cropping = setup(&["sizeClass"], true);
        let session = async {
            follow_once(
                &cropping.presentation,
                &cropping.display,
                &cropping.media,
                true,
                &cropping.viewport,
            )
            .await
        };
        let helper = async {
            cropping.helper.info((2560, 1440), (2560, 1440)).await;
            let resize = cropping.helper.request().await;
            assert_eq!(resize["op"], "resize");
            cropping
                .helper
                .answer(
                    &resize,
                    json!({"width": 1792, "height": 1280, "windows": []}),
                )
                .await;
        };
        tokio::join!(session, helper);
        assert_eq!(cropping.display.window(), (1560, 1200));
    }

    /// A viewer that draws whole frames never sees the part of a size-class
    /// framebuffer outside the window, even with no presenter to lay it
    /// out; while every viewer crops, nothing is laid out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_viewer_of_whole_frames_returns_the_framebuffer_to_the_window() {
        let (display, control, _frames) = DisplayClient::test_channel();
        display.advertise(&["sizeClass"]);
        let mut helper = Helper(BufReader::new(control), vec!["sizeClass".into()]);
        // A cropping presenter left the framebuffer at a size class.
        let grown = async { apply(&display, 780, 600, true).await.unwrap() };
        let answered = async {
            helper.info((2560, 1440), (2560, 1440)).await;
            let resize = helper.request().await;
            assert_eq!(resize["sizeClass"], true);
            helper
                .answer(
                    &resize,
                    json!({"width": 1792, "height": 1280, "windows": []}),
                )
                .await;
        };
        tokio::join!(grown, answered);
        let presentation = Presentation::new();
        let viewport = Viewport {
            width: Arc::new(Mutex::new(780)),
            height: Arc::new(Mutex::new(600)),
            changed: Arc::new(Notify::new()),
        };

        let cropping = StreamMedia::new(Default::default());
        cropping.viewer_joined(true, true);
        follow_once(&presentation, &display, &cropping, false, &viewport).await;
        let mut line = String::new();
        let quiet = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            helper.0.read_line(&mut line),
        )
        .await;
        assert!(quiet.is_err(), "nothing to lay out: {line}");

        let whole = StreamMedia::new(Default::default());
        whole.viewer_joined(true, true);
        whole.viewer_joined(false, false);
        let pass = follow_once(&presentation, &display, &whole, false, &viewport);
        let answered = async {
            helper.info((1792, 1280), (1560, 1200)).await;
            let resize = helper.request().await;
            assert!(resize.get("sizeClass").is_none(), "{resize}");
            assert_eq!(
                (resize["width"].as_u64(), resize["height"].as_u64()),
                (Some(1560), Some(1200))
            );
            helper
                .answer(
                    &resize,
                    json!({"width": 1560, "height": 1200, "windows": []}),
                )
                .await;
        };
        tokio::join!(pass, answered);
        let surface = display.surface();
        assert_eq!((surface.width, surface.height), (1560, 1200));
        assert_eq!(display.window(), (1560, 1200));
    }

    /// A window that already has the requested size is not laid out again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unchanged_window_is_not_laid_out_again() {
        let mut s = setup(&["sizeClass"], true);
        let applied = async { apply(&s.display, 1280, 720, true).await };
        let helper = async { s.helper.info((2560, 1440), (2560, 1440)).await };
        let (applied, _) = tokio::join!(applied, helper);
        let applied = applied.unwrap();
        assert_eq!(applied.window, (2560, 1440));
        let mut line = String::new();
        let quiet = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            s.helper.0.read_line(&mut line),
        )
        .await;
        assert!(quiet.is_err(), "no resize: {line}");
    }
}
