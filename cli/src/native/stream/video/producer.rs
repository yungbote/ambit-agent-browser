//! The session's video producer: one per display, shared by its video
//! viewers, with one encoding per codec they use. Pictures never carry the
//! native pointer: video viewers draw it themselves (`cursor=viewer`).
//!
//! Threads, none of them the async runtime's:
//! - the capture thread asks the display helper for pictures when an
//!   encoding is due (paced by its fastest viewer's rate) and has a path with
//!   room (a capture is skipped, never an encoded unit), and converts each
//!   picture's changed rows into a free picture buffer of every encoding the
//!   picture is for;
//! - one encode thread per encoding encodes those pictures at motion
//!   quality, re-encodes the last one as a key unit when a viewer needs one,
//!   and refines it once the screen is still, then hands every unit to the
//!   encoding's subscribers.
//!
//! Each encoding has two picture buffers. A buffer is owned by exactly one
//! side at a time (moved through the mailbox), so capture and encode overlap
//! without sharing memory; rows changed while a buffer was away are
//! remembered and converted when it comes back. An encoder slower than the
//! rate therefore lowers the rate: the capture waits for a buffer.
//!
//! Lock order: the producer's state, then an encoding's mailbox, then its
//! subscribers or the display's surface state; never the reverse.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use super::policy::{
    period, rate, Band, CodedSize, LinkRate, Pace, Quality, Refinement, RefinementCredit,
    RefinementSweep, EXACT_ROWS,
};
use super::stages::{self, Stages};
use super::subscription::{Delivery, Subscriber, Unit};
use crate::native::display::pictures::{PictureReply, PictureRequest};
use crate::native::display::{DisplayClient, Rect, Surface};
use crate::native::stream::cursor_identity::CursorIdentities;
use crate::native::stream::StreamMedia;
use crate::native::video::convert::Planar;
use crate::native::video::{
    self as codec, EncodeRequest, EncoderRate, VideoCodec, VideoEncoder, VideoError,
};

/// The longest a picture request waits in the helper for damage. The helper
/// serves one request at a time, so a wait holds back every encoding:
/// `decide` ends it when another encoding will owe a picture.
const PICTURE_WAIT_MS: u32 = 100;
/// Encoder threads per stream, measured on the production node
/// (encoder-decision.md): full motion is as fast at 2 as at 4, but 4 cut
/// interactive pictures from 11.8 to 9.2 ms and a page change from 298 to
/// 221 ms; 8 doubled the CPU for 7%.
pub(super) const ENCODER_THREADS: u32 = 4;

/// What the producer needs from its stream.
pub(crate) struct Source {
    pub display: Arc<DisplayClient>,
    pub media: Arc<StreamMedia>,
    pub cursors: CursorIdentities,
    /// The stream's runtime: cursor identities are refined on it.
    pub runtime: tokio::runtime::Handle,
}

pub(crate) struct Producer {
    inner: Arc<Inner>,
}

struct Inner {
    source: Source,
    state: Mutex<State>,
    /// Wakes the capture thread: a subscriber, a returned buffer, a path
    /// with room again, or stopping.
    wake: Condvar,
    stopping: AtomicBool,
}

#[derive(Default)]
struct State {
    encodings: Vec<Arc<Encoding>>,
    snapshots: super::snapshot::Requests,
    /// The failure that ended the producer, if one did.
    failed: Option<String>,
}

/// One codec's stream.
struct Encoding {
    codec: VideoCodec,
    subscribers: Mutex<Vec<Arc<Subscriber>>>,
    mailbox: Mutex<Mailbox>,
    /// Wakes the encode thread: a job, a key request, a path with room
    /// again, or stopping.
    changed: Condvar,
    motion_bytes: AtomicU32,
}

struct Mailbox {
    /// Buffers the capture side may convert into.
    free: Vec<Buffer>,
    /// A converted picture for the encoder, at most one.
    job: Option<Job>,
    /// Rows changed since each buffer was last converted into.
    stale: [Vec<bool>; 2],
    coded: CodedSize,
    /// When this encoding may take its next picture; none before its first,
    /// which is then captured whole.
    pace: Pace,
    visible: Option<Rect>,
    /// The screen changed since this encoding's latest picture. The helper
    /// reports each change once, to whichever picture came next, so an
    /// encoding that picture was not for catches up from the slot.
    behind: bool,
    /// The rows that changed since the last picture handed to the encoder.
    changed: Option<Band>,
    /// The rows that changed since this encoding's last unit, picture or
    /// exact.
    unsent: Option<Band>,
    /// The coded size of the picture the encoder holds, while that picture
    /// is final and the encoder has nothing else to do: only then may small
    /// damage leave as an exact unit (contract browser-presentation-units),
    /// so no refinement of an older capture ever follows one.
    settled: Option<(u32, u32)>,
    /// An exact unit left after the held picture: a key unit is then of a
    /// fresh capture, never of the held one.
    exact_since_held: bool,
    key_requested: bool,
    stop: bool,
}

struct Buffer {
    id: usize,
    picture: Planar,
    coded: (u32, u32),
    /// The framebuffer the picture was last converted from.
    source: Option<(u32, u32, Rect)>,
}

struct Job {
    buffer: Buffer,
    capture: Capture,
    /// The rows that differ from the picture the encoder coded before it.
    changed: Band,
}

/// What a unit says about the picture it encodes.
#[derive(Clone)]
struct Capture {
    ts: u64,
    /// When the helper read the screen: the rate is kept between reads, so
    /// the helper's own latency never lengthens the period.
    read: Instant,
    visible: Rect,
    surface: Surface,
    input_seq: Option<u64>,
    pointer: Option<(i32, i32)>,
    /// With diagnostics on, when it passed each stage so far.
    stages: Option<Stages>,
}

/// One capture: the request, and the encodings its picture is for.
struct Plan {
    request: PictureRequest,
    encodings: Vec<Arc<Encoding>>,
}

/// When an encoding may take its next picture, and what it needs of it.
struct Due {
    at: Instant,
    /// Its fastest ready viewer's period.
    period: Duration,
    /// It holds no picture yet: the helper writes every row.
    whole: bool,
    /// It has not seen the screen: the helper need not wait for damage.
    behind: bool,
    /// Its encoder holds both buffers: it takes a picture once one comes
    /// back, which wakes the capture thread.
    busy: bool,
}

impl Due {
    /// Whether the encoding takes a picture at `now`.
    fn takes(&self, now: Instant) -> bool {
        !self.busy && self.at <= now
    }

    /// The latest a wait for damage may end without holding back a picture
    /// this encoding owes; none once it has seen the screen. A busy encoding
    /// owes one once a buffer comes back, which the capture thread cannot
    /// hear while the helper waits, so after its due time it is looked at
    /// again every period.
    fn owed(&self, now: Instant) -> Option<Instant> {
        self.behind.then(|| {
            if self.busy && self.at <= now {
                now + self.period
            } else {
                self.at
            }
        })
    }
}

/// What the encode thread does next.
enum Work {
    /// A new picture, and whether a viewer asked for a key unit.
    Picture(Box<Job>, bool),
    /// The held picture again, as a key unit.
    Key,
    /// The held picture again, at the still target.
    Refine,
    /// A fresh capture for a key unit: the capture thread is woken.
    Recapture,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

impl Producer {
    /// Demand on the existing capture thread; no second picture reader.
    pub(crate) fn snapshot(
        &self,
        after_us: u64,
    ) -> Result<
        tokio::sync::oneshot::Receiver<Result<Arc<super::snapshot::Snapshot>, String>>,
        VideoError,
    > {
        let receive = {
            let mut state = lock(&self.inner.state);
            if self.inner.stopping.load(Ordering::Acquire) {
                return Err(VideoError::Unavailable(
                    "the picture producer has stopped".into(),
                ));
            }
            state.snapshots.request(after_us)
        };
        self.inner.nudge();
        Ok(receive)
    }

    /// A producer of `source`'s pictures; it captures once it has a
    /// subscriber and stops when the last reference goes.
    pub(crate) fn start(source: Source) -> Result<Arc<Self>, VideoError> {
        if source.display.pictures().is_none() {
            return Err(VideoError::Unavailable(
                "the display helper serves no pictures".into(),
            ));
        }
        let inner = Arc::new(Inner {
            source,
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            stopping: AtomicBool::new(false),
        });
        let capture = Arc::downgrade(&inner);
        std::thread::Builder::new()
            .name("video-capture".into())
            .spawn(move || capture_loop(capture))
            .map_err(|error| VideoError::Unavailable(error.to_string()))?;
        Ok(Arc::new(Self { inner }))
    }

    /// Whether this producer captures `display` and can still serve.
    pub(crate) fn serves(&self, display: &Arc<DisplayClient>) -> bool {
        Arc::ptr_eq(&self.inner.source.display, display)
            && !self.inner.stopping.load(Ordering::Acquire)
    }

    /// A new subscription to the stream of `codec` pictures. Its first unit
    /// is a key unit.
    pub(crate) fn subscribe(
        self: &Arc<Self>,
        codec: VideoCodec,
        requested_rate: u32,
        exact: bool,
    ) -> Result<Subscription, VideoError> {
        let mut state = lock(&self.inner.state);
        if let Some(failure) = &state.failed {
            return Err(VideoError::Unavailable(failure.clone()));
        }
        let encoding = match state
            .encodings
            .iter()
            .find(|encoding| encoding.codec == codec)
        {
            Some(encoding) => encoding.clone(),
            None => {
                if !codec::encodes(codec) {
                    return Err(VideoError::Unavailable(format!(
                        "no encoder for {}",
                        codec.token()
                    )));
                }
                let encoding = Arc::new(Encoding::new(codec));
                let worker = (Arc::downgrade(&self.inner), encoding.clone());
                std::thread::Builder::new()
                    .name(format!("video-encode-{}", codec.token()))
                    .spawn(move || encode_loop(worker.0, worker.1))
                    .map_err(|error| VideoError::Unavailable(error.to_string()))?;
                state.encodings.push(encoding.clone());
                encoding
            }
        };
        let subscriber = Arc::new(Subscriber::new(rate(requested_rate), exact));
        lock(&encoding.subscribers).push(subscriber.clone());
        drop(state);
        encoding.request_key();
        self.inner.nudge();
        Ok(Subscription {
            producer: self.clone(),
            encoding,
            subscriber,
        })
    }
}

impl Drop for Producer {
    fn drop(&mut self) {
        self.inner.stop();
    }
}

impl Inner {
    /// Wakes the capture thread. The state lock is taken first, so the wake
    /// cannot fall between its check and its wait.
    fn nudge(&self) {
        drop(lock(&self.state));
        self.wake.notify_all();
    }

    fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
        let encodings = {
            let mut state = lock(&self.state);
            state.snapshots.fail("the picture producer has stopped");
            std::mem::take(&mut state.encodings)
        };
        for encoding in encodings {
            encoding.stop();
        }
        self.nudge();
    }

    /// The producer cannot continue: every subscriber learns why.
    fn fail(&self, reason: String) {
        self.stopping.store(true, Ordering::Release);
        let encodings = {
            let mut state = lock(&self.state);
            state.failed.get_or_insert(reason.clone());
            state.snapshots.fail(&reason);
            std::mem::take(&mut state.encodings)
        };
        for encoding in encodings {
            encoding.fail(&reason);
        }
        self.nudge();
    }
}

impl Encoding {
    fn new(codec: VideoCodec) -> Self {
        let chroma = codec.chroma();
        let buffers = (0..2)
            .map(|id| Buffer {
                id,
                picture: Planar::new(chroma, 2, 2),
                coded: (0, 0),
                source: None,
            })
            .collect();
        Self {
            codec,
            subscribers: Mutex::new(Vec::new()),
            mailbox: Mutex::new(Mailbox {
                free: buffers,
                job: None,
                stale: [Vec::new(), Vec::new()],
                coded: CodedSize::new(),
                pace: Pace::default(),
                visible: None,
                behind: true,
                changed: None,
                unsent: None,
                settled: None,
                exact_since_held: false,
                key_requested: false,
                stop: false,
            }),
            changed: Condvar::new(),
            motion_bytes: AtomicU32::new(1),
        }
    }

    /// Wakes the encode thread, after the mailbox lock (see `Inner::nudge`).
    fn nudge(&self) {
        drop(lock(&self.mailbox));
        self.changed.notify_all();
    }

    fn request_key(&self) {
        lock(&self.mailbox).key_requested = true;
        self.changed.notify_all();
    }

    fn stop(&self) {
        lock(&self.mailbox).stop = true;
        self.changed.notify_all();
    }

    fn fail(&self, reason: &str) {
        let subscribers = std::mem::take(&mut *lock(&self.subscribers));
        for subscriber in subscribers {
            subscriber.end(reason);
        }
        self.stop();
    }

    /// The fastest rate among the viewers whose path has room; none when no
    /// path has room, which skips this encoding's captures.
    fn rate(&self) -> Option<u32> {
        let requested = lock(&self.subscribers)
            .iter()
            .filter(|subscriber| subscriber.ready())
            .map(|subscriber| subscriber.rate())
            .max()?;
        Some(self.link_rate().map_or(requested, |rate| {
            rate.capture_rate(requested, self.motion_bytes.load(Ordering::Acquire))
        }))
    }

    /// Several viewers share one encoding; the lowest path budget governs it.
    fn link_rate(&self) -> Option<LinkRate> {
        lock(&self.subscribers)
            .iter()
            .filter_map(|subscriber| subscriber.link_rate())
            .reduce(|left, right| LinkRate {
                bits_per_second: left.bits_per_second.min(right.bits_per_second),
                burst_bytes: left.burst_bytes.min(right.burst_bytes),
            })
    }

    /// When this encoding may take its next picture; none once it stops or
    /// while no path has room.
    fn due(&self, now: Instant) -> Option<Due> {
        let mailbox = lock(&self.mailbox);
        if mailbox.stop {
            return None;
        }
        let period = period(self.rate()?);
        Some(Due {
            at: mailbox.pace.next().unwrap_or(now),
            period,
            whole: mailbox.pace.next().is_none(),
            behind: mailbox.behind,
            busy: mailbox.free.is_empty(),
        })
    }

    /// Remembers the rows a picture wrote, for both buffers and for the
    /// encoder: a window that moved changed every row.
    fn mark(&self, reply: &PictureReply) {
        let mut mailbox = lock(&self.mailbox);
        let moved = mailbox.visible != Some(reply.window());
        mailbox.visible = Some(reply.window());
        if !reply.rows.is_empty() || moved {
            mailbox.behind = true;
        }
        let wrote = if moved {
            Some(Band::whole(reply.height))
        } else {
            reply
                .rows
                .iter()
                .map(|[top, bottom]| Band {
                    top: *top,
                    bottom: *bottom,
                })
                .reduce(Band::union)
        };
        if let Some(wrote) = wrote {
            mailbox.changed = Some(
                mailbox
                    .changed
                    .map_or(wrote, |changed| changed.union(wrote)),
            );
            mailbox.unsent = Some(mailbox.unsent.map_or(wrote, |unsent| unsent.union(wrote)));
        }
        let height = reply.height as usize;
        for rows in mailbox.stale.iter_mut() {
            if rows.len() != height || moved {
                *rows = vec![true; height];
                continue;
            }
            for [top, bottom] in &reply.rows {
                rows[*top as usize..*bottom as usize].fill(true);
            }
        }
    }

    /// Where small damage leaves as an exact unit, with the coded size of the
    /// picture under it: only while every viewer draws exact units, the held
    /// picture is final and nothing else is pending, and the rows changed
    /// since this encoding's last unit are at most `EXACT_ROWS` of an
    /// unmoved window (a moved window changed every row).
    fn exact(&self, mailbox: &Mailbox, capture: &Capture) -> Option<(Rect, (u32, u32))> {
        let coded = mailbox.settled?;
        if mailbox.job.is_some() || mailbox.key_requested || !self.exact_viewers() {
            return None;
        }
        let rows = mailbox.unsent?.within(capture.visible)?;
        (rows.height <= EXACT_ROWS).then(|| {
            let rect = Rect {
                x: rows.x as i32,
                y: rows.y as i32,
                width: rows.width,
                height: rows.height,
            };
            (rect, coded)
        })
    }

    /// Whether this encoding has viewers and every one draws exact units.
    fn exact_viewers(&self) -> bool {
        let subscribers = lock(&self.subscribers);
        !subscribers.is_empty() && subscribers.iter().all(|subscriber| subscriber.exact())
    }

    /// Publishes the capture's pixels in `rect` as an exact unit over the
    /// held picture of size `coded`. Units leave in capture order: while an
    /// exact unit is newer than the held picture, the encoder answers a key
    /// request with a fresh capture (`next_work`), and a settled picture
    /// owes no refinement.
    fn publish_exact(
        &self,
        capture: &Capture,
        reply: &PictureReply,
        pixels: &[u8],
        rect: Rect,
        coded: (u32, u32),
    ) {
        let started = crate::native::stream::monotonic_us();
        let area = (
            rect.x as usize,
            rect.y as usize,
            rect.width as usize,
            rect.height as usize,
        );
        let Some(data) = crate::native::video::exact::png(pixels, reply.stride as usize, area)
        else {
            self.fail("an exact unit's rectangle is outside its picture");
            return;
        };
        let mut unit = Unit {
            data,
            wire_bytes: 0,
            key: false,
            ts: capture.ts,
            coded,
            visible: capture.visible,
            surface: capture.surface.clone(),
            input_seq: capture.input_seq,
            quality: Quality::Final,
            codec_string: None,
            exact: Some(rect),
            stages: capture.stages.map(|stages| Stages {
                converted: started,
                encode_started: started,
                encoded: crate::native::stream::monotonic_us(),
                ..stages
            }),
        };
        let Some(wire_bytes) = crate::native::stream::wire::video_budget_bytes(&unit, self.codec)
        else {
            self.fail("an exact unit is outside the video wire contract");
            return;
        };
        unit.wire_bytes = wire_bytes;
        self.publish(Arc::new(unit));
    }

    /// The encoder published its picture at the still target: small damage
    /// may now leave as exact units over it.
    fn settle(&self, coded: (u32, u32)) {
        lock(&self.mailbox).settled = Some(coded);
    }

    /// Hands an admitted capture to the encoder only while some subscriber's
    /// path still has room. Readiness can change after `decide` selected this
    /// encoding and while the helper was waiting for damage; marking above
    /// retains the changed rows so the next ready capture catches up.
    fn take_if_ready(&self, capture: &Capture, reply: &PictureReply, pixels: &[u8]) {
        if self.rate().is_some() {
            self.take(capture, reply, pixels);
        }
    }

    /// Converts the screen into a free buffer and hands it to the encoder,
    /// when this encoding has not seen it yet; an unencoded picture it
    /// supersedes goes back to the free buffers, and its changed rows go
    /// with the picture that replaces it.
    fn take(&self, capture: &Capture, reply: &PictureReply, pixels: &[u8]) {
        let period = period(self.rate().unwrap_or(super::policy::MAX_RATE));
        let mut mailbox = lock(&self.mailbox);
        if !mailbox.behind {
            return;
        }
        if let Some((rect, coded)) = self.exact(&mailbox, capture) {
            mailbox.pace.took(capture.read, period);
            mailbox.behind = false;
            mailbox.unsent = None;
            mailbox.exact_since_held = true;
            drop(mailbox);
            self.publish_exact(capture, reply, pixels, rect, coded);
            return;
        }
        let Some(mut buffer) = mailbox.free.pop() else {
            return;
        };
        // Admission checked these global device extents inside the source.
        // Cropping must never discard the right/bottom edge of an offset window.
        let window = (
            capture.visible.x as u32 + capture.visible.width,
            capture.visible.y as u32 + capture.visible.height,
        );
        let coded = mailbox.coded.fit(window, capture.read);
        mailbox.pace.took(capture.read, period);
        mailbox.behind = false;
        mailbox.unsent = None;
        let changed = mailbox
            .changed
            .take()
            .unwrap_or_else(|| Band::whole(reply.height));
        // Only the capture thread marks or converts, so the rows can leave
        // the mailbox while the encoder keeps using it.
        let mut stale = std::mem::take(&mut mailbox.stale[buffer.id]);
        drop(mailbox);
        let mut capture = capture.clone();
        #[cfg(test)]
        let converting = Instant::now();
        convert(
            &mut buffer,
            &mut stale,
            reply,
            pixels,
            coded,
            self.codec.chroma(),
        );
        #[cfg(test)]
        measured::converted(capture.ts, converting.elapsed(), reply.timings.clone());
        if let Some(stages) = capture.stages.as_mut() {
            stages.converted = crate::native::stream::monotonic_us();
        }
        let mut mailbox = lock(&self.mailbox);
        mailbox.stale[buffer.id] = stale;
        let mut job = Job {
            buffer,
            capture,
            changed,
        };
        if let Some(superseded) = mailbox.job.take() {
            job.changed = job.changed.union(superseded.changed);
            mailbox.free.push(superseded.buffer);
        }
        mailbox.job = Some(job);
        drop(mailbox);
        self.changed.notify_all();
    }

    /// The encode thread's next work, waiting for it; none once stopped. A
    /// held picture owes its refinement only while a path has room for it,
    /// and only once the window's geometry (`geometry`: since when it has
    /// held) is still too.
    fn next_work(
        &self,
        holds: bool,
        refinement: &Refinement,
        credit: &mut RefinementCredit,
        geometry: &dyn Fn() -> Instant,
    ) -> Option<Work> {
        let mut mailbox = lock(&self.mailbox);
        loop {
            if mailbox.stop {
                return None;
            }
            if let Some(job) = mailbox.job.take() {
                let key = std::mem::take(&mut mailbox.key_requested);
                mailbox.settled = None;
                mailbox.exact_since_held = false;
                return Some(Work::Picture(Box::new(job), key));
            }
            if holds && mailbox.key_requested && mailbox.exact_since_held {
                // The held picture is older than an exact unit sent after
                // it: the key is of a fresh capture, which the job carries.
                if !mailbox.behind {
                    mailbox.behind = true;
                    return Some(Work::Recapture);
                }
            } else if holds && std::mem::take(&mut mailbox.key_requested) {
                mailbox.settled = None;
                return Some(Work::Key);
            }
            let due = refinement
                .due(geometry())
                .filter(|_| holds && !mailbox.behind && self.rate().is_some())
                .map(|at| {
                    self.link_rate().map_or(at, |path| {
                        at.max(credit.due(path.bits_per_second, Instant::now()))
                    })
                });
            mailbox = match due {
                Some(at) => {
                    let now = Instant::now();
                    if at <= now {
                        return Some(Work::Refine);
                    }
                    self.changed
                        .wait_timeout(mailbox, at - now)
                        .unwrap_or_else(|error| error.into_inner())
                        .0
                }
                None => self
                    .changed
                    .wait(mailbox)
                    .unwrap_or_else(|error| error.into_inner()),
            };
        }
    }

    /// Hands a unit to every subscriber; one that needs a key unit asks.
    fn publish(&self, unit: Arc<Unit>) {
        let subscribers = lock(&self.subscribers).clone();
        let wants_key = subscribers
            .iter()
            .fold(false, |wants, subscriber| subscriber.push(&unit) | wants);
        if wants_key {
            self.request_key();
        }
    }
}

/// Converts into `buffer` every row that changed since it was last
/// converted into (`stale`, which this picture's rows are already in).
fn convert(
    buffer: &mut Buffer,
    stale: &mut Vec<bool>,
    reply: &PictureReply,
    pixels: &[u8],
    coded: (u32, u32),
    chroma: codec::Chroma,
) {
    let window = reply.window();
    let source = (reply.width, reply.height, window);
    let height = reply.height as usize;
    let mut whole = false;
    if buffer.coded != coded {
        buffer.picture = Planar::new(chroma, coded.0, coded.1);
        buffer.coded = coded;
        whole = true;
    }
    if buffer.source != Some(source) {
        // A new aperture may expose old gutter rows without pixel damage.
        // Clear stale padding and reconvert the complete coherent slot.
        buffer.picture.clear_outside(0, 0);
        buffer.source = Some(source);
        whole = true;
    }
    if whole || stale.len() != height {
        *stale = vec![true; height];
    }
    let mut row = 0;
    while row < height {
        if !stale[row] {
            row += 1;
            continue;
        }
        let top = row;
        while row < height && stale[row] {
            stale[row] = false;
            row += 1;
        }
        buffer.picture.convert_visible(
            pixels,
            reply.stride as usize,
            (reply.width as usize, height),
            codec::EncoderRegion {
                x: window.x as u32,
                y: window.y as u32,
                width: window.width,
                height: window.height,
            },
            (top, row),
        );
    }
}

/// Pictures until the producer stops or fails.
fn capture_loop(inner: Weak<Inner>) {
    loop {
        let Some(inner) = inner.upgrade() else {
            return;
        };
        let _runtime = inner.source.runtime.enter();
        let Some(plan) = plan(&inner) else {
            return;
        };
        let Some(pictures) = inner.source.display.pictures() else {
            inner.fail("the display helper serves no pictures".into());
            return;
        };
        let requested = crate::native::stream::monotonic_us();
        let asked = Instant::now();
        let encodings = lock(&inner.state).encodings.clone();
        let answer = pictures.picture(plan.request, |reply, pixels| {
            let received = crate::native::stream::monotonic_us();
            let ts = requested + reply.wait_us();
            let applied = inner.source.media.applied_input(ts);
            let mut surface = inner.source.display.surface();
            surface.width = reply.width;
            surface.height = reply.height;
            surface.cursor_included = reply.cursor_included;
            let capture = Capture {
                ts,
                read: asked + std::time::Duration::from_micros(reply.wait_us()),
                visible: reply.window(),
                surface,
                input_seq: applied.map(|(sequence, _)| sequence),
                pointer: reply.pointer.map(|point| (point.x, point.y)),
                stages: stages::enabled().then(|| Stages {
                    input: applied,
                    requested,
                    read: ts,
                    waited_us: reply.wait_us(),
                    received,
                    rows: reply.rows.iter().map(|[top, bottom]| bottom - top).sum(),
                    ..Stages::default()
                }),
            };
            // Remember every encoding's damage before a snapshot wakes its
            // caller; a blocked viewer must already owe this same picture.
            for encoding in &encodings {
                encoding.mark(reply);
            }
            lock(&inner.state).snapshots.captured(
                reply,
                pixels,
                capture.surface.clone(),
                inner.source.display.layout_epoch(),
                super::snapshot::CaptureBounds {
                    requested_us: requested,
                    received_us: crate::native::stream::monotonic_us(),
                    picture_us: ts,
                },
                capture.input_seq,
            );
            for encoding in &plan.encodings {
                encoding.take_if_ready(&capture, reply, pixels);
            }
            ts
        });
        match answer {
            Ok(answer) => {
                let at = answer
                    .picture
                    .unwrap_or_else(crate::native::stream::monotonic_us);
                if let Some(cursor) = answer.cursor {
                    inner.source.cursors.observe(cursor, at);
                }
            }
            Err(error) => {
                inner.fail(format!("the display stopped serving pictures: {error}"));
                return;
            }
        }
    }
}

/// Waits until some encoding is due for a picture and says how to take it;
/// none when stopping.
fn plan(inner: &Inner) -> Option<Plan> {
    let mut state = lock(&inner.state);
    loop {
        if inner.stopping.load(Ordering::Acquire) {
            return None;
        }
        let now = Instant::now();
        let snapshot = state.snapshots.pending();
        // Ready video receives the same picture at its ordinary cadence. With
        // no scheduled capture, snapshot demand needs neither encoder nor a second reader.
        state = match decide(&state.encodings, now) {
            Decision::Capture(mut plan) => {
                if snapshot {
                    plan.request.force = true;
                    plan.request.wait_ms = 0;
                }
                return Some(plan);
            }
            Decision::Wait(None) if snapshot => {
                return Some(Plan {
                    request: PictureRequest {
                        cursor: false,
                        force: true,
                        wait_ms: 0,
                        cursor_identity: false,
                    },
                    encodings: Vec::new(),
                })
            }
            Decision::Wait(Some(at)) => {
                inner
                    .wake
                    .wait_timeout(state, at.saturating_duration_since(now))
                    .unwrap_or_else(|error| error.into_inner())
                    .0
            }
            Decision::Wait(None) => inner
                .wake
                .wait(state)
                .unwrap_or_else(|error| error.into_inner()),
        };
    }
}

/// What the capture thread does at a moment.
enum Decision {
    /// Takes a picture for the encodings that are due.
    Capture(Plan),
    /// Waits until the next encoding is due, if one will be.
    Wait(Option<Instant>),
}

/// The capture decision at `now`. An encoding without a picture yet gets a
/// whole one; one that has not seen the screen does not wait for more
/// damage, since an unchanged answer means the slot already holds the
/// screen. The helper serves one request at a time, so a wait for damage
/// ends when another encoding will owe a picture (`Due::owed`); one that
/// begins to owe during the wait, such as a new codec's first viewer, waits
/// for it to end.
fn decide(encodings: &[Arc<Encoding>], now: Instant) -> Decision {
    let (taking, waiting): (Vec<_>, Vec<_>) = encodings
        .iter()
        .filter_map(|encoding| Some((encoding, encoding.due(now)?)))
        .partition(|(_, due)| due.takes(now));
    if taking.is_empty() {
        // A busy encoding's encoder wakes the thread when a buffer comes back.
        return Decision::Wait(
            waiting
                .iter()
                .filter(|(_, due)| !due.busy)
                .map(|(_, due)| due.at)
                .min(),
        );
    }
    let whole = taking.iter().any(|(_, due)| due.whole);
    let wait_ms = if whole {
        // A whole picture waits only for a layout in progress; the helper
        // answers it at once otherwise.
        PICTURE_WAIT_MS
    } else if taking.iter().any(|(_, due)| due.behind) {
        0
    } else {
        damage_wait(
            now,
            waiting.iter().filter_map(|(_, due)| due.owed(now)).min(),
        )
    };
    Decision::Capture(Plan {
        request: PictureRequest {
            cursor: false,
            force: whole,
            wait_ms,
            cursor_identity: true,
        },
        encodings: taking
            .into_iter()
            .map(|(encoding, _)| encoding.clone())
            .collect(),
    })
}

/// How long the helper may wait for damage: `PICTURE_WAIT_MS`, and no
/// longer than until `owed`, to the wire's millisecond.
fn damage_wait(now: Instant, owed: Option<Instant>) -> u32 {
    owed.map_or(PICTURE_WAIT_MS, |owed| {
        let until = owed
            .saturating_duration_since(now)
            .as_micros()
            .div_ceil(1000);
        u32::try_from(until).map_or(PICTURE_WAIT_MS, |until| until.min(PICTURE_WAIT_MS))
    })
}

/// Encodes one encoding's pictures until it stops or fails.
fn encode_loop(producer: Weak<Inner>, encoding: Arc<Encoding>) {
    let Some(display) = producer.upgrade().map(|inner| inner.source.display.clone()) else {
        return;
    };
    let geometry = || display.geometry_since();
    let mut encoder: Option<Box<dyn VideoEncoder>> = None;
    let mut held: Option<(Buffer, Capture)> = None;
    let mut refinement = Refinement::default();
    let mut credit = RefinementCredit::default();
    let mut sweep: Option<RefinementSweep> = None;
    let mut native_key_bytes = 0usize;
    // The rows the stream shows below the still target: each unit of motion
    // adds the rows it coded, a key unit all of them, and a refinement codes
    // only these.
    let mut unrefined: Option<Band> = None;
    loop {
        let Some(work) = encoding.next_work(held.is_some(), &refinement, &mut credit, &geometry)
        else {
            return;
        };
        let (mut quality, asked, changed) = match work {
            Work::Picture(job, asked) => {
                sweep = None;
                if let Some((previous, _)) = held.replace((job.buffer, job.capture)) {
                    lock(&encoding.mailbox).free.push(previous);
                    if let Some(producer) = producer.upgrade() {
                        producer.nudge();
                    }
                }
                (Quality::Motion, asked, Some(job.changed))
            }
            Work::Key => {
                sweep = None;
                (Quality::Motion, true, None)
            }
            Work::Refine => (Quality::Final, false, None),
            Work::Recapture => {
                if let Some(producer) = producer.upgrade() {
                    producer.nudge();
                }
                continue;
            }
        };
        let (buffer, capture) = held.as_ref().expect("work needs a held picture");
        // A new coded size needs a new encoder, whose first unit is a key
        // unit: the stream's size never changes on a dependent unit.
        let fresh = encoder
            .as_ref()
            .is_none_or(|encoder| encoder.coded() != buffer.coded);
        if fresh {
            match codec::open(
                encoding.codec,
                buffer.coded.0,
                buffer.coded.1,
                ENCODER_THREADS,
            ) {
                Ok(opened) => encoder = Some(opened),
                Err(error) => {
                    fail(&producer, &encoding, error);
                    return;
                }
            }
        }
        let encoder = encoder.as_mut().expect("an encoder is open");
        let path = encoding.link_rate();
        if let Err(error) = encoder.set_rate(path.map(|path| EncoderRate {
            bits_per_second: path.bits_per_second,
            pictures_per_second: encoding.rate().unwrap_or(10),
        })) {
            fail(&producer, &encoding, error);
            return;
        }
        let key = asked || fresh;
        let visible = crate::native::video::EncoderRegion {
            // PictureChannel has validated this rectangle inside its
            // framebuffer. Keep its signed device origin in the same space
            // as the helper's fresh pointer.
            x: capture.visible.x as u32,
            y: capture.visible.y as u32,
            width: capture.visible.width,
            height: capture.visible.height,
        };
        let whole = Band::whole(capture.surface.height);
        // A unit of motion codes only the rows that changed since the last
        // one; a key unit codes every row.
        let region = match (quality, changed) {
            (Quality::Motion, _) if key => {
                unrefined = Some(whole);
                None
            }
            (Quality::Motion, changed) => {
                let changed = changed.unwrap_or(whole);
                unrefined = Some(unrefined.map_or(changed, |rows| rows.union(changed)));
                changed.within(capture.visible)
            }
            // A refinement codes the rows below the still target. Native key
            // size is an observed entropy estimate, not a packet-size proof:
            // when refining them would exceed a path's step budget, they are
            // covered in finite aligned regions instead.
            _ => {
                let scope = unrefined.unwrap_or(whole).within(capture.visible);
                let target = scope.unwrap_or(visible);
                let affordable = path.map(|path| {
                    let pixels = u64::from(visible.width) * u64::from(visible.height);
                    pixels.saturating_mul(u64::from(path.bits_per_second) / 320)
                        / native_key_bytes.max(1) as u64
                });
                match affordable.filter(|affordable| {
                    u64::from(target.width) * u64::from(target.height) > *affordable
                }) {
                    Some(affordable) => {
                        let plan = sweep.get_or_insert_with(|| {
                            let mut edge = 32u32;
                            while edge < 1024 && u64::from(edge * 2).pow(2) <= affordable {
                                edge *= 2;
                            }
                            RefinementSweep::new(target, edge, capture.pointer)
                        });
                        plan.next()
                    }
                    None => {
                        sweep = None;
                        scope
                    }
                }
            }
        };
        let actual = match encoder.set_region(region) {
            Ok(actual) => actual,
            Err(error) => {
                fail(&producer, &encoding, error);
                return;
            }
        };
        if let (Some(plan), Some(actual)) = (sweep.as_mut(), actual) {
            if !plan.covered(actual) {
                quality = Quality::Refine;
            }
        }
        let request = EncodeRequest {
            key,
            quantizer: quality.quantizer(),
            refine: matches!(quality, Quality::Refine | Quality::Final),
        };
        #[cfg(test)]
        let encoding_started = Instant::now();
        let encode_started = crate::native::stream::monotonic_us();
        let unit = match encoder.encode(&buffer.picture.picture(), request) {
            Ok(unit) => unit,
            Err(error) => {
                fail(&producer, &encoding, error);
                return;
            }
        };
        if unit.key {
            native_key_bytes = unit.data.len();
        }
        if quality == Quality::Motion && !unit.key {
            let previous = encoding.motion_bytes.load(Ordering::Acquire);
            encoding.motion_bytes.store(
                ((u64::from(previous) * 3 + unit.data.len() as u64) / 4).min(u64::from(u32::MAX))
                    as u32,
                Ordering::Release,
            );
        }
        #[cfg(test)]
        measured::encoded(measured::Encoded {
            ts: capture.ts,
            key: unit.key,
            quality: quality.label(),
            bytes: unit.data.len(),
            encode: encoding_started.elapsed(),
            coded: buffer.coded,
            region: actual,
        });
        match quality {
            Quality::Motion => refinement.moved(capture.read),
            Quality::Refine => {}
            Quality::Final => {
                refinement.refined();
                unrefined = None;
            }
        }
        let mut unit = Unit {
            data: unit.data,
            wire_bytes: 0,
            key: unit.key,
            ts: capture.ts,
            coded: buffer.coded,
            visible: capture.visible,
            surface: capture.surface.clone(),
            input_seq: capture.input_seq,
            quality,
            codec_string: unit.codec_string,
            exact: None,
            stages: capture.stages.map(|stages| Stages {
                encode_started,
                encoded: crate::native::stream::monotonic_us(),
                ..stages
            }),
        };
        let Some(wire_bytes) =
            crate::native::stream::wire::video_budget_bytes(&unit, encoding.codec)
        else {
            fail(
                &producer,
                &encoding,
                VideoError::Failed("encoded picture is outside the video wire contract".into()),
            );
            return;
        };
        unit.wire_bytes = wire_bytes;
        if matches!(quality, Quality::Refine | Quality::Final) {
            if let Some(path) = path {
                credit.spent(path.bits_per_second, wire_bytes, Instant::now());
            }
        }
        encoding.publish(Arc::new(unit));
        if quality == Quality::Final {
            encoding.settle(buffer.coded);
        }
    }
}

/// An encoding failed: its viewers learn why, and the producer no longer
/// offers it.
fn fail(producer: &Weak<Inner>, encoding: &Encoding, error: VideoError) {
    encoding.fail(&error.to_string());
    if let Some(producer) = producer.upgrade() {
        lock(&producer.state)
            .encodings
            .retain(|each| !std::ptr::eq(Arc::as_ptr(each), encoding));
        producer.nudge();
    }
}

/// A viewer's subscription. Dropping it leaves the stream; the last one to
/// leave an encoding stops its encoder, and the last reference to the
/// producer stops it.
pub(crate) struct Subscription {
    producer: Arc<Producer>,
    encoding: Arc<Encoding>,
    subscriber: Arc<Subscriber>,
}

impl Subscription {
    pub(crate) async fn next(&self) -> Delivery {
        self.subscriber.next().await
    }

    /// The viewer reset its decoder or found a gap: the next unit is a key
    /// unit.
    pub(crate) fn keyframe(&self) {
        self.encoding.request_key();
    }

    /// Whether the viewer's path has room for another picture. Room again
    /// wakes both a skipped capture and a deferred refinement.
    pub(crate) fn set_ready(&self, ready: bool) {
        if self.subscriber.set_ready(ready) && ready {
            if self.subscriber.needs_key() {
                self.encoding.request_key();
            }
            self.encoding.nudge();
            self.producer.inner.nudge();
        }
    }

    /// The viewer's rate changed (0: its display's).
    pub(crate) fn set_rate(&self, requested: u32) {
        self.subscriber.set_rate(rate(requested));
        self.producer.inner.nudge();
    }

    pub(crate) fn set_link_rate(&self, rate: LinkRate) {
        self.subscriber.set_link_rate(rate);
        self.encoding.nudge();
        self.producer.inner.nudge();
    }

    pub(crate) fn queued_bytes(&self) -> usize {
        self.subscriber.queued_bytes()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut subscribers = lock(&self.encoding.subscribers);
        subscribers.retain(|subscriber| !Arc::ptr_eq(subscriber, &self.subscriber));
        let empty = subscribers.is_empty();
        drop(subscribers);
        if empty {
            lock(&self.producer.inner.state)
                .encodings
                .retain(|encoding| !Arc::ptr_eq(encoding, &self.encoding));
            self.encoding.stop();
        }
        self.producer.inner.nudge();
    }
}

/// What the producer measured, for the proofs that run it in process: every
/// picture's conversion and every unit's encode, newest last.
#[cfg(test)]
pub(crate) mod measured {
    use std::sync::Mutex;
    use std::time::Duration;

    #[derive(Clone, Debug)]
    pub(crate) struct Encoded {
        pub ts: u64,
        pub key: bool,
        pub quality: &'static str,
        pub bytes: usize,
        pub encode: Duration,
        pub coded: (u32, u32),
        /// The region the unit coded; none for the whole picture.
        pub region: Option<crate::native::video::EncoderRegion>,
    }

    /// One picture converted for an encoding, with the helper's own timings
    /// of its capture (its wait, fetch and copy).
    #[derive(Clone, Debug)]
    pub(crate) struct Converted {
        pub ts: u64,
        pub convert: Duration,
        pub helper: Option<serde_json::Value>,
    }

    const KEPT: usize = 100_000;
    static ENCODED: Mutex<Vec<Encoded>> = Mutex::new(Vec::new());
    static CONVERTED: Mutex<Vec<Converted>> = Mutex::new(Vec::new());

    fn keep<T>(samples: &Mutex<Vec<T>>, sample: T) {
        let mut samples = samples.lock().unwrap_or_else(|error| error.into_inner());
        if samples.len() < KEPT {
            samples.push(sample);
        }
    }

    pub(super) fn encoded(sample: Encoded) {
        keep(&ENCODED, sample);
    }

    pub(super) fn converted(ts: u64, convert: Duration, helper: Option<serde_json::Value>) {
        keep(
            &CONVERTED,
            Converted {
                ts,
                convert,
                helper,
            },
        );
    }

    fn drain<T>(samples: &Mutex<Vec<T>>) -> Vec<T> {
        std::mem::take(&mut *samples.lock().unwrap_or_else(|error| error.into_inner()))
    }

    /// Everything measured since the last call.
    pub(crate) fn take() -> (Vec<Encoded>, Vec<Converted>) {
        (drain(&ENCODED), drain(&CONVERTED))
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "producer_tests.rs"]
mod tests;
