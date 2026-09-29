//! A display helper the tests play: a screen of coloured rows behind the
//! picture channel's fake, which answers pictures as the real helper does.

use serde_json::{json, Value};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::native::display::pictures::fake::FakeHelper;

pub(crate) const GREY: [u8; 4] = [128, 128, 128, 0];
pub(crate) const RED: [u8; 4] = [0, 0, 255, 0];
pub(crate) const BLUE: [u8; 4] = [255, 0, 0, 0];
pub(crate) const GREEN: [u8; 4] = [0, 255, 0, 0];

/// What the screen does next.
pub(crate) enum Change {
    /// Rows `[top, bottom)` now show one colour (BGRX).
    Paint {
        top: u32,
        bottom: u32,
        colour: [u8; 4],
    },
    /// The window is laid out anew inside a framebuffer of `framebuffer`,
    /// every row repainted.
    Layout {
        framebuffer: (u32, u32),
        window: (u32, u32),
        colour: [u8; 4],
    },
    /// The next answer contradicts itself.
    Break,
}

struct Screen {
    framebuffer: (u32, u32),
    window: (u32, u32),
    rows: Vec<[u8; 4]>,
    /// Rows changed since the previous picture.
    dirty: Vec<bool>,
    broken: bool,
}

impl Screen {
    fn apply(&mut self, change: Change) {
        match change {
            Change::Paint {
                top,
                bottom,
                colour,
            } => {
                for row in top..bottom {
                    self.rows[row as usize] = colour;
                    self.dirty[row as usize] = true;
                }
            }
            Change::Layout {
                framebuffer,
                window,
                colour,
            } => {
                self.framebuffer = framebuffer;
                self.window = window;
                self.rows = vec![colour; framebuffer.1 as usize];
                self.dirty = vec![true; framebuffer.1 as usize];
            }
            Change::Break => self.broken = true,
        }
    }
}

/// The helper: each picture writes the rows painted since the previous one
/// (every row when forced) into the slot and names them; an unchanged
/// answer is held for the request's wait unless the screen changes, and a
/// picture says how long it was held (`waitUs`), as the real helper's does.
fn serve(
    mut helper: FakeHelper,
    mut screen: Screen,
    changes: mpsc::Receiver<Change>,
    requests: mpsc::Sender<Value>,
) {
    while let Some(request) = helper.next_request() {
        let _ = requests.send(request.clone());
        let asked = Instant::now();
        let forced = request["force"] == true;
        if !forced && !screen.dirty.contains(&true) {
            let wait = Duration::from_millis(request["waitMs"].as_u64().unwrap_or(0));
            if let Ok(change) = changes.recv_timeout(wait) {
                screen.apply(change);
            }
        }
        let waited = u64::try_from(asked.elapsed().as_micros()).unwrap_or(u64::MAX);
        while let Ok(change) = changes.try_recv() {
            screen.apply(change);
        }
        let (width, height) = screen.framebuffer;
        if screen.broken {
            helper.answer(
                &request,
                json!({"changed":true,"width":width,"height":height,"stride":width * 4 + 1,
                    "rows":[],"cursorIncluded":request["cursor"]}),
            );
            continue;
        }
        let mut runs: Vec<[u32; 2]> = Vec::new();
        for row in 0..height {
            if forced || screen.dirty[row as usize] {
                helper.paint(width, row, row + 1, screen.rows[row as usize]);
                match runs.last_mut() {
                    Some(run) if run[1] == row => run[1] = row + 1,
                    _ => runs.push([row, row + 1]),
                }
            }
        }
        screen.dirty.fill(false);
        if runs.is_empty() {
            helper.answer(&request, json!({"changed": false}));
            continue;
        }
        let mut data = json!({"changed":true,"width":width,"height":height,"stride":width * 4,
            "rows":runs,"cursorIncluded":request["cursor"],
            "timings":{"waitUs":waited}});
        if screen.window != screen.framebuffer {
            data["visible"] = json!({"x":0,"y":0,"width":screen.window.0,"height":screen.window.1});
        }
        helper.answer(&request, data);
    }
}

/// The screen's side, for a test: what it paints and what it was asked.
pub(crate) struct FakeScreen {
    pub changes: mpsc::Sender<Change>,
    pub requests: mpsc::Receiver<Value>,
}

impl FakeScreen {
    /// A 640x480 grey window behind `helper`.
    pub(crate) fn new(helper: FakeHelper) -> Self {
        let (changes, changed) = mpsc::channel();
        let (requested, requests) = mpsc::channel();
        let screen = Screen {
            framebuffer: (640, 480),
            window: (640, 480),
            rows: vec![GREY; 480],
            dirty: vec![false; 480],
            broken: false,
        };
        std::thread::spawn(move || serve(helper, screen, changed, requested));
        Self { changes, requests }
    }

    pub(crate) fn paint(&self, top: u32, bottom: u32, colour: [u8; 4]) {
        self.changes
            .send(Change::Paint {
                top,
                bottom,
                colour,
            })
            .unwrap();
    }

    /// The requests the helper received so far.
    pub(crate) fn requested(&self) -> Vec<Value> {
        self.requests.try_iter().collect()
    }
}
