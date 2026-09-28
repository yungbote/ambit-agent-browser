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
//! subscribers; never the reverse.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::Instant;

use super::policy::{period, rate, CodedSize, Quality, Refinement};
use super::subscription::{Delivery, Subscriber, Unit};
use crate::native::display::pictures::{PictureReply, PictureRequest};
use crate::native::display::{DisplayClient, Rect, Surface};
use crate::native::stream::cursor_identity::CursorIdentities;
use crate::native::stream::StreamMedia;
use crate::native::video::convert::Planar;
use crate::native::video::{self as codec, EncodeRequest, VideoCodec, VideoEncoder, VideoError};

/// How long a picture request waits in the helper for damage.
const PICTURE_WAIT_MS: u32 = 100;
/// Encoder threads per stream, measured on the production node
/// (encoder-decision.md): full motion is as fast at 2 as at 4, but 4 cut
/// interactive pictures from 11.8 to 9.2 ms and a page change from 298 to
/// 221 ms; 8 doubled the CPU for 7%.
const ENCODER_THREADS: u32 = 4;

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
}

struct Mailbox {
    /// Buffers the capture side may convert into.
    free: Vec<Buffer>,
    /// A converted picture for the encoder, at most one.
    job: Option<Job>,
    /// Rows changed since each buffer was last converted into.
    stale: [Vec<bool>; 2],
    coded: CodedSize,
    /// When a picture was last converted for this encoding; none before its
    /// first, which is then captured whole.
    pictured: Option<Instant>,
    /// The screen changed since this encoding's latest picture. The helper
    /// reports each change once, to whichever picture came next, so an
    /// encoding that picture was not for catches up from the slot.
    behind: bool,
    key_requested: bool,
    stop: bool,
}

struct Buffer {
    id: usize,
    picture: Planar,
    coded: (u32, u32),
    /// The framebuffer the picture was last converted from.
    framebuffer: (u32, u32),
}

struct Job {
    buffer: Buffer,
    capture: Capture,
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
}

/// One capture: the request, and the encodings its picture is for.
struct Plan {
    request: PictureRequest,
    encodings: Vec<Arc<Encoding>>,
}

/// When an encoding may take its next picture, and what it needs of it.
struct Due {
    at: Instant,
    /// It holds no picture yet: the helper writes every row.
    whole: bool,
    /// It has not seen the screen: the helper need not wait for damage.
    behind: bool,
}

/// What the encode thread does next.
enum Work {
    /// A new picture, and whether a viewer asked for a key unit.
    Picture(Box<Job>, bool),
    /// The held picture again, as a key unit.
    Key,
    /// The held picture again, at the still target.
    Refine,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

impl Producer {
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
        let subscriber = Arc::new(Subscriber::new(rate(requested_rate)));
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
        let encodings = std::mem::take(&mut lock(&self.state).encodings);
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
                framebuffer: (0, 0),
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
                pictured: None,
                behind: true,
                key_requested: false,
                stop: false,
            }),
            changed: Condvar::new(),
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
        lock(&self.subscribers)
            .iter()
            .filter(|subscriber| subscriber.ready())
            .map(|subscriber| subscriber.rate())
            .max()
    }

    /// When this encoding may take its next picture, if a buffer is free
    /// and a path has room.
    fn due(&self, now: Instant) -> Option<Due> {
        let mailbox = lock(&self.mailbox);
        if mailbox.stop || mailbox.free.is_empty() {
            return None;
        }
        let rate = self.rate()?;
        Some(Due {
            at: mailbox.pictured.map_or(now, |last| last + period(rate)),
            whole: mailbox.pictured.is_none(),
            behind: mailbox.behind,
        })
    }

    /// Remembers the rows a picture wrote, for both buffers.
    fn mark(&self, reply: &PictureReply) {
        let mut mailbox = lock(&self.mailbox);
        if !reply.rows.is_empty() {
            mailbox.behind = true;
        }
        let height = reply.height as usize;
        for rows in mailbox.stale.iter_mut() {
            if rows.len() != height {
                *rows = vec![true; height];
                continue;
            }
            for [top, bottom] in &reply.rows {
                rows[*top as usize..*bottom as usize].fill(true);
            }
        }
    }

    /// Converts the screen into a free buffer and hands it to the encoder,
    /// when this encoding has not seen it yet; an unencoded picture it
    /// supersedes goes back to the free buffers.
    fn take(&self, capture: &Capture, reply: &PictureReply, pixels: &[u8]) {
        let mut mailbox = lock(&self.mailbox);
        if !mailbox.behind {
            return;
        }
        let Some(mut buffer) = mailbox.free.pop() else {
            return;
        };
        let window = (capture.visible.width, capture.visible.height);
        let coded = mailbox.coded.fit(window, capture.read);
        mailbox.pictured = Some(capture.read);
        mailbox.behind = false;
        // Only the capture thread marks or converts, so the rows can leave
        // the mailbox while the encoder keeps using it.
        let mut stale = std::mem::take(&mut mailbox.stale[buffer.id]);
        drop(mailbox);
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
        let mut mailbox = lock(&self.mailbox);
        mailbox.stale[buffer.id] = stale;
        let job = Job {
            buffer,
            capture: capture.clone(),
        };
        if let Some(superseded) = mailbox.job.replace(job) {
            mailbox.free.push(superseded.buffer);
        }
        drop(mailbox);
        self.changed.notify_all();
    }

    /// The encode thread's next work, waiting for it; none once stopped. A
    /// held picture owes its refinement only while a path has room for it.
    fn next_work(&self, holds: bool, refinement: &Refinement) -> Option<Work> {
        let mut mailbox = lock(&self.mailbox);
        loop {
            if mailbox.stop {
                return None;
            }
            if let Some(job) = mailbox.job.take() {
                let key = std::mem::take(&mut mailbox.key_requested);
                return Some(Work::Picture(Box::new(job), key));
            }
            if holds && std::mem::take(&mut mailbox.key_requested) {
                return Some(Work::Key);
            }
            let due = refinement.due().filter(|_| holds && self.rate().is_some());
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
    let framebuffer = (reply.width, reply.height);
    let height = reply.height as usize;
    let mut whole = false;
    if buffer.coded != coded {
        buffer.picture = Planar::new(chroma, coded.0, coded.1);
        buffer.coded = coded;
        whole = true;
    }
    if buffer.framebuffer != framebuffer {
        buffer.picture.clear_outside(reply.width as usize, height);
        buffer.framebuffer = framebuffer;
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
        buffer.picture.convert(
            pixels,
            reply.stride as usize,
            (reply.width as usize, height),
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
            let ts = requested + reply.wait_us();
            let mut surface = inner.source.display.surface();
            surface.width = reply.width;
            surface.height = reply.height;
            surface.cursor_included = reply.cursor_included;
            let capture = Capture {
                ts,
                read: asked + std::time::Duration::from_micros(reply.wait_us()),
                visible: reply.window(),
                surface,
                input_seq: inner.source.media.applied_input_at(ts),
            };
            for encoding in &encodings {
                encoding.mark(reply);
            }
            for encoding in &plan.encodings {
                encoding.take(&capture, reply, pixels);
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
/// none when stopping. An encoding without a picture yet gets a whole one;
/// one that has not seen the screen does not wait for more damage: an
/// unchanged answer means the slot already holds the screen.
fn plan(inner: &Inner) -> Option<Plan> {
    let mut state = lock(&inner.state);
    loop {
        if inner.stopping.load(Ordering::Acquire) {
            return None;
        }
        let now = Instant::now();
        let mut encodings = Vec::new();
        let (mut whole, mut behind) = (false, false);
        let mut next: Option<Instant> = None;
        for encoding in &state.encodings {
            match encoding.due(now) {
                Some(due) if due.at <= now => {
                    whole |= due.whole;
                    behind |= due.behind;
                    encodings.push(encoding.clone());
                }
                Some(due) => next = Some(next.map_or(due.at, |next| next.min(due.at))),
                None => {}
            }
        }
        if !encodings.is_empty() {
            return Some(Plan {
                request: PictureRequest {
                    cursor: false,
                    force: whole,
                    // A whole picture waits only for a layout in progress;
                    // the helper answers it at once otherwise.
                    wait_ms: if behind && !whole { 0 } else { PICTURE_WAIT_MS },
                    cursor_identity: true,
                },
                encodings,
            });
        }
        state = match next {
            Some(at) => {
                inner
                    .wake
                    .wait_timeout(state, at.saturating_duration_since(now))
                    .unwrap_or_else(|error| error.into_inner())
                    .0
            }
            None => inner
                .wake
                .wait(state)
                .unwrap_or_else(|error| error.into_inner()),
        };
    }
}

/// Encodes one encoding's pictures until it stops or fails.
fn encode_loop(producer: Weak<Inner>, encoding: Arc<Encoding>) {
    let mut encoder: Option<Box<dyn VideoEncoder>> = None;
    let mut held: Option<(Buffer, Capture)> = None;
    let mut refinement = Refinement::default();
    loop {
        let Some(work) = encoding.next_work(held.is_some(), &refinement) else {
            return;
        };
        let (quality, asked) = match work {
            Work::Picture(job, asked) => {
                if let Some((previous, _)) = held.replace((job.buffer, job.capture)) {
                    lock(&encoding.mailbox).free.push(previous);
                    if let Some(producer) = producer.upgrade() {
                        producer.nudge();
                    }
                }
                (Quality::Motion, asked)
            }
            Work::Key => (Quality::Motion, true),
            Work::Refine => (Quality::Final, false),
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
        let request = EncodeRequest {
            key: asked || fresh,
            quantizer: quality.quantizer(),
            refine: quality == Quality::Final,
        };
        #[cfg(test)]
        let encoding_started = Instant::now();
        let unit = match encoder.encode(&buffer.picture.picture(), request) {
            Ok(unit) => unit,
            Err(error) => {
                fail(&producer, &encoding, error);
                return;
            }
        };
        #[cfg(test)]
        measured::encoded(measured::Encoded {
            ts: capture.ts,
            key: unit.key,
            quality: quality.label(),
            bytes: unit.data.len(),
            encode: encoding_started.elapsed(),
            coded: buffer.coded,
        });
        match quality {
            Quality::Motion => refinement.moved(capture.read),
            Quality::Final => refinement.refined(),
        }
        encoding.publish(Arc::new(Unit {
            data: unit.data,
            key: unit.key,
            ts: capture.ts,
            coded: buffer.coded,
            visible: capture.visible,
            surface: capture.surface.clone(),
            input_seq: capture.input_seq,
            quality,
            codec_string: unit.key.then(|| encoder.codec_string()),
        }));
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
            self.encoding.nudge();
            self.producer.inner.nudge();
        }
    }

    /// The viewer's rate changed (0: its display's).
    pub(crate) fn set_rate(&self, requested: u32) {
        self.subscriber.set_rate(rate(requested));
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
