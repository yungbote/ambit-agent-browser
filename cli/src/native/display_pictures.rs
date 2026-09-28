//! The helper's picture channel: the video producer's captures. The helper
//! copies the framebuffer rows that changed since the previous picture into
//! shared memory the driver created, and names them; the producer converts
//! and encodes those rows before it asks again. Pixels never cross a socket
//! and nothing is encoded in the helper.
//!
//! Only the producer's capture thread uses the channel, with blocking I/O: no
//! picture ever waits on the async runtime, and no input waits on a picture.

use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use crate::native::display::{DisplayError, Rect, MAX_DISPLAY_SIZE};

/// The slot holds the largest framebuffer, 32-bit pixels.
pub(crate) const SLOT_BYTES: usize = (MAX_DISPLAY_SIZE as usize) * (MAX_DISPLAY_SIZE as usize) * 4;
/// Where the helper finds the channel and the slot (see `DisplayProcess`).
pub(crate) const CHANNEL_FD: i32 = 4;
pub(crate) const PIXELS_FD: i32 = 5;
const REPLY_BYTES: u64 = 64 * 1024;
/// Beyond a picture's own wait: the helper is gone or stuck.
const REPLY_MARGIN: Duration = Duration::from_secs(5);

/// One picture request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PictureRequest {
    /// Composite the cursor into the picture.
    pub cursor: bool,
    /// Write every row, not only the changed ones.
    pub force: bool,
    /// Hold an unchanged answer this long for damage (at most 250).
    pub wait_ms: u32,
    /// Report the displayed cursor's identity when it changed.
    pub cursor_identity: bool,
}

/// A picture as the helper answered it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PictureReply {
    /// The framebuffer's size; the slot holds its rows packed at `stride`.
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// The row ranges this picture wrote, ascending and disjoint; every other
    /// row is as the previous picture left it.
    pub rows: Vec<[u32; 2]>,
    pub cursor_included: bool,
    /// The browser window inside a larger framebuffer; absent: all of it.
    #[serde(default)]
    pub visible: Option<Rect>,
    #[serde(default)]
    pub timings: Option<Value>,
}

impl PictureReply {
    /// Microseconds the helper waited before reading this picture.
    pub(crate) fn wait_us(&self) -> u64 {
        self.timings
            .as_ref()
            .and_then(|timings| timings["waitUs"].as_u64())
            .unwrap_or(0)
    }

    /// The window, or the whole framebuffer.
    pub(crate) fn window(&self) -> Rect {
        self.visible.unwrap_or(Rect {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        })
    }

    fn coherent(&self, request: PictureRequest) -> bool {
        let rows_ordered = self.rows.windows(2).all(|pair| pair[0][1] <= pair[1][0]);
        (1..=MAX_DISPLAY_SIZE).contains(&self.width)
            && (1..=MAX_DISPLAY_SIZE).contains(&self.height)
            && self.stride == self.width * 4
            && (self.stride as usize) * (self.height as usize) <= SLOT_BYTES
            && self.cursor_included == request.cursor
            && rows_ordered
            && self
                .rows
                .iter()
                .all(|[top, bottom]| top < bottom && *bottom <= self.height)
            && (!request.force || self.rows == [[0, self.height]])
            && self.visible.is_none_or(|visible| {
                visible.x == 0
                    && visible.y == 0
                    && visible.width > 0
                    && visible.height > 0
                    && visible.width <= self.width
                    && visible.height <= self.height
            })
    }
}

/// What one request returned: the picture, when the screen changed (seen
/// through the caller's `read`), and the cursor identity, when it changed.
pub(crate) struct Answer<T> {
    pub picture: Option<T>,
    pub cursor: Option<Value>,
}

/// A read-only view of the slot, unmapped on drop.
struct Mapping {
    address: NonNull<u8>,
    length: usize,
}

// SAFETY: the mapping is plain shared memory the helper writes only while the
// channel's lock is held by a request; reads happen under the same lock.
unsafe impl Send for Mapping {}
// SAFETY: as above.
unsafe impl Sync for Mapping {}

impl Mapping {
    fn new(memory: &OwnedFd, length: usize) -> std::io::Result<Self> {
        // SAFETY: a fresh read-only shared mapping of a descriptor we own.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ,
                libc::MAP_SHARED,
                memory.as_raw_fd(),
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self {
            address: NonNull::new(address.cast()).expect("mmap never maps address 0"),
            length,
        })
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: `length` bytes are mapped for the life of `self`; the
        // helper does not write them while the channel lock is held.
        unsafe { std::slice::from_raw_parts(self.address.as_ptr(), self.length) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: unmaps exactly what `new` mapped.
        unsafe { libc::munmap(self.address.as_ptr().cast(), self.length) };
    }
}

struct Wire {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

pub(crate) struct PictureChannel {
    wire: Mutex<Wire>,
    /// Shuts the socket down from another thread (an aborted display).
    abort: UnixStream,
    pixels: Mapping,
    failed: AtomicBool,
    next_id: AtomicU64,
}

/// The driver's ends of a new channel and the descriptors the helper gets.
pub(crate) struct Handover {
    pub channel: PictureChannel,
    pub helper_socket: OwnedFd,
    pub helper_pixels: OwnedFd,
}

impl PictureChannel {
    /// A socket pair and a slot of `SLOT_BYTES`: the channel, and what the
    /// helper inherits.
    pub(crate) fn create() -> std::io::Result<Handover> {
        let (ours, theirs) = UnixStream::pair()?;
        // SAFETY: memfd_create takes a NUL-terminated name and flags.
        let memory = unsafe { libc::memfd_create(c"ambit-pictures".as_ptr(), libc::MFD_CLOEXEC) };
        if memory < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: memfd_create returned a descriptor we now own.
        let memory = unsafe { OwnedFd::from_raw_fd(memory) };
        // SAFETY: a valid descriptor; the size is a constant.
        if unsafe { libc::ftruncate(memory.as_raw_fd(), SLOT_BYTES as libc::off_t) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let pixels = Mapping::new(&memory, SLOT_BYTES)?;
        Ok(Handover {
            channel: Self {
                wire: Mutex::new(Wire {
                    reader: BufReader::new(ours.try_clone()?),
                    writer: ours.try_clone()?,
                }),
                abort: ours,
                pixels,
                failed: AtomicBool::new(false),
                next_id: AtomicU64::new(1),
            },
            helper_socket: theirs.into(),
            helper_pixels: memory,
        })
    }

    /// Ends the channel: a picture in flight fails at once, and every later
    /// one is refused.
    pub(crate) fn abort(&self) {
        self.failed.store(true, Ordering::Release);
        let _ = self.abort.shutdown(std::net::Shutdown::Both);
    }

    pub(crate) fn available(&self) -> bool {
        !self.failed.load(Ordering::Acquire)
    }

    /// One picture. `read` sees the reply and the slot while no other
    /// request can rewrite it, and runs only when the screen changed. Any
    /// broken or incoherent answer ends the channel: the helper is not
    /// trusted to be half-working.
    pub(crate) fn picture<T>(
        &self,
        request: PictureRequest,
        read: impl FnOnce(&PictureReply, &[u8]) -> T,
    ) -> Result<Answer<T>, DisplayError> {
        let mut wire = self.wire.lock().unwrap_or_else(|error| error.into_inner());
        if !self.available() {
            return Err(DisplayError::unavailable());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut command = json!({"id": id, "op": "picture", "cursor": request.cursor});
        if request.force {
            command["force"] = json!(true);
        }
        if request.wait_ms > 0 {
            command["waitMs"] = json!(request.wait_ms.min(250));
        }
        if request.cursor_identity {
            command["cursorIdentity"] = json!(true);
        }
        let response = self.exchange(&mut wire, &command, request.wait_ms);
        let Ok(mut response) = response else {
            self.abort();
            return Err(DisplayError::unavailable());
        };
        if response["id"].as_u64() != Some(id) || !response["success"].is_boolean() {
            self.abort();
            return Err(DisplayError::unavailable());
        }
        if response["success"] != true {
            // The helper refused without breaking: no pictures here.
            return Err(serde_json::from_value(response["error"].take())
                .unwrap_or_else(|_| DisplayError::unavailable()));
        }
        let mut data = response["data"].take();
        let cursor = data
            .as_object_mut()
            .and_then(|data| data.remove("cursor"))
            .filter(Value::is_object);
        if data["changed"] == false {
            return Ok(Answer {
                picture: None,
                cursor,
            });
        }
        let reply: PictureReply = match serde_json::from_value(data) {
            Ok(reply) if PictureReply::coherent(&reply, request) => reply,
            _ => {
                self.abort();
                return Err(DisplayError::unavailable());
            }
        };
        let pixels = &self.pixels.bytes()[..reply.stride as usize * reply.height as usize];
        Ok(Answer {
            picture: Some(read(&reply, pixels)),
            cursor,
        })
    }

    fn exchange(&self, wire: &mut Wire, command: &Value, wait_ms: u32) -> std::io::Result<Value> {
        let timeout = Duration::from_millis(u64::from(wait_ms.min(250))) + REPLY_MARGIN;
        wire.writer.set_write_timeout(Some(timeout))?;
        wire.reader.get_ref().set_read_timeout(Some(timeout))?;
        let mut line = serde_json::to_vec(command)?;
        line.push(b'\n');
        wire.writer.write_all(&line)?;
        let mut response = Vec::new();
        std::io::Read::take(&mut wire.reader, REPLY_BYTES + 1).read_until(b'\n', &mut response)?;
        if response.len() as u64 > REPLY_BYTES || response.last() != Some(&b'\n') {
            return Err(std::io::Error::other("invalid picture response boundary"));
        }
        Ok(serde_json::from_slice(&response)?)
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A helper's side of the picture channel, for tests.
    use super::*;

    pub(crate) struct FakeHelper {
        pub socket: BufReader<UnixStream>,
        pixels: NonNull<u8>,
    }

    // SAFETY: the test owns the mapping for its lifetime.
    unsafe impl Send for FakeHelper {}

    impl FakeHelper {
        pub(crate) fn from(handover: &Handover) -> Self {
            let socket = UnixStream::from(handover.helper_socket.try_clone().unwrap());
            // SAFETY: a writable shared mapping of the test's own memfd.
            let address = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    SLOT_BYTES,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    handover.helper_pixels.as_raw_fd(),
                    0,
                )
            };
            assert_ne!(address, libc::MAP_FAILED);
            Self {
                socket: BufReader::new(socket),
                pixels: NonNull::new(address.cast()).unwrap(),
            }
        }

        /// The next request.
        pub(crate) fn request(&mut self) -> Value {
            let mut line = String::new();
            self.socket.read_line(&mut line).unwrap();
            serde_json::from_str(&line).unwrap()
        }

        /// Paints rows `[top, bottom)` of a `width`-wide framebuffer one
        /// BGRX colour, as the helper's copy would.
        pub(crate) fn paint(&mut self, width: u32, top: u32, bottom: u32, bgrx: [u8; 4]) {
            let stride = width as usize * 4;
            // SAFETY: SLOT_BYTES are mapped and the rows lie inside them.
            let slot = unsafe { std::slice::from_raw_parts_mut(self.pixels.as_ptr(), SLOT_BYTES) };
            for pixel in slot[top as usize * stride..bottom as usize * stride].chunks_exact_mut(4) {
                pixel.copy_from_slice(&bgrx);
            }
        }

        pub(crate) fn answer(&mut self, request: &Value, data: Value) {
            let reply = json!({"id": request["id"], "success": true, "data": data});
            self.socket
                .get_mut()
                .write_all(format!("{reply}\n").as_bytes())
                .unwrap();
        }
    }

    impl Drop for FakeHelper {
        fn drop(&mut self) {
            // SAFETY: unmaps what `from` mapped.
            unsafe { libc::munmap(self.pixels.as_ptr().cast(), SLOT_BYTES) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeHelper;
    use super::*;

    fn request() -> PictureRequest {
        PictureRequest {
            cursor: false,
            force: false,
            wait_ms: 100,
            cursor_identity: true,
        }
    }

    /// A picture's reader sees exactly the rows the helper wrote, and an
    /// unchanged answer still carries a new cursor identity.
    #[test]
    fn a_picture_reads_the_shared_rows_the_helper_named() {
        let handover = PictureChannel::create().unwrap();
        let mut helper = FakeHelper::from(&handover);
        let channel = handover.channel;
        let peer = std::thread::spawn(move || {
            let first = helper.request();
            assert_eq!(
                first,
                json!({"id":1,"op":"picture","cursor":false,"waitMs":100,"cursorIdentity":true})
            );
            helper.paint(8, 2, 4, [1, 2, 3, 0]);
            helper.answer(
                &first,
                json!({"changed":true,"width":8,"height":6,"stride":32,"rows":[[2,4]],
                    "cursorIncluded":false,"timings":{"waitUs":42},"cursor":{"serial":3,"css":"text"}}),
            );
            let second = helper.request();
            helper.answer(&second, json!({"changed":false}));
            helper
        });
        let answer = channel
            .picture(request(), |reply, pixels| {
                assert_eq!(reply.rows, vec![[2, 4]]);
                assert_eq!(reply.wait_us(), 42);
                assert_eq!(reply.window(), Rect { x: 0, y: 0, width: 8, height: 6 });
                pixels[2 * 32..4 * 32].to_vec()
            })
            .unwrap();
        assert_eq!(answer.picture.unwrap(), [1, 2, 3, 0].repeat(16));
        assert_eq!(answer.cursor.unwrap()["css"], "text");
        let unchanged = channel.picture(request(), |_, _| ()).unwrap();
        assert!(unchanged.picture.is_none());
        peer.join().unwrap();
    }

    /// An answer that contradicts itself or the request ends the channel,
    /// and so does a helper that is gone.
    #[test]
    fn an_incoherent_or_missing_helper_ends_the_channel() {
        for data in [
            json!({"changed":true,"width":8,"height":6,"stride":31,"rows":[[0,6]],"cursorIncluded":false}),
            json!({"changed":true,"width":8,"height":6,"stride":32,"rows":[[4,7]],"cursorIncluded":false}),
            json!({"changed":true,"width":8,"height":6,"stride":32,"rows":[[2,4],[0,1]],"cursorIncluded":false}),
            json!({"changed":true,"width":8,"height":6,"stride":32,"rows":[],"cursorIncluded":true}),
            json!({"changed":true,"width":8,"height":6,"stride":32,"rows":[],"cursorIncluded":false,
                "visible":{"x":1,"y":0,"width":4,"height":4}}),
            json!({"changed":true,"width":5000,"height":6,"stride":20000,"rows":[],"cursorIncluded":false}),
        ] {
            let handover = PictureChannel::create().unwrap();
            let mut helper = FakeHelper::from(&handover);
            let channel = handover.channel;
            let peer = std::thread::spawn(move || {
                let request = helper.request();
                helper.answer(&request, data);
                helper
            });
            assert!(channel.picture(request(), |_, _| ()).is_err());
            assert!(!channel.available());
            assert!(channel.picture(request(), |_, _| ()).is_err());
            drop(peer.join().unwrap());
        }
        let handover = PictureChannel::create().unwrap();
        drop(FakeHelper::from(&handover));
        drop(handover.helper_socket);
        assert!(handover.channel.picture(request(), |_, _| ()).is_err());
    }

    /// A forced picture must have written every row.
    #[test]
    fn a_forced_picture_names_every_row() {
        let forced = PictureRequest {
            force: true,
            ..request()
        };
        let reply = |rows: Value| -> PictureReply {
            serde_json::from_value(json!({"width":8,"height":6,"stride":32,"rows":rows,"cursorIncluded":false}))
                .unwrap()
        };
        assert!(reply(json!([[0, 6]])).coherent(forced));
        assert!(!reply(json!([[0, 4]])).coherent(forced));
        assert!(reply(json!([[0, 4]])).coherent(request()));
    }
}
