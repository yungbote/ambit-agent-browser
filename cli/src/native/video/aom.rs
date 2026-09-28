//! libaom (the image's `libaom.so.3`) through a minimal hand-written FFI:
//! only the functions and struct fields this encoder uses. Every offset it
//! relies on is pinned to the values measured from the 3.12.1 headers with
//! `offsetof` (the layout test below). libaom checks the encoder interface
//! version itself at init, so a library of another interface is refused,
//! never misread; the structs carry slack beyond the measured sizes so a
//! call that writes a whole struct cannot overrun them.
//!
//! One encoder is one stream: real-time usage, no lag, no frame reordering,
//! so every `encode` call returns exactly the unit of the picture it was
//! given (checked, not assumed).

use std::ffi::{c_char, c_int, c_long, c_uint, c_ulong, c_void, CStr};
use std::sync::OnceLock;

use super::library::Library;
use super::{Chroma, EncodeRequest, EncodedUnit, Picture, VideoCodec, VideoEncoder, VideoError};

const SONAME: &CStr = c"libaom.so.3";
/// `AOM_ENCODER_ABI_VERSION` of the headers the layouts below come from.
const ENCODER_ABI_VERSION: c_int = 29;

const CODEC_OK: c_int = 0;
const USAGE_REALTIME: c_uint = 1;
const IMG_FMT_I420: c_uint = 258;
const IMG_FMT_I444: c_uint = 262;
const CX_FRAME_PKT: c_uint = 0;
const FRAME_IS_KEY: u32 = 1;
const EFLAG_FORCE_KF: c_long = 1;
const RC_ONE_PASS: c_uint = 0;
const Q: c_uint = 3;
const KF_DISABLED: c_uint = 0;
const BITS_8: c_uint = 8;

/// CICP code points the sequence header carries: BT.709 primaries, the sRGB
/// transfer the screen was drawn in, the BT.709 matrix and full range. The
/// pictures are converted with exactly this matrix and range (`convert`).
const CICP_PRIMARIES_BT709: c_int = 1;
const CICP_TRANSFER_SRGB: c_int = 13;
const CICP_MATRIX_BT709: c_int = 1;
const COLOR_RANGE_FULL: c_int = 1;
const CONTENT_SCREEN: c_int = 1;
const SUPERBLOCK_128: c_int = 1;
/// The cost tables' update frequency "off": each superblock row reuses them.
const COST_UPDATE_OFF: c_int = 3;

/// Encoder controls (`aomcx.h` ids) this encoder sets.
mod control {
    use std::ffi::c_int;
    pub(super) const CPU_USED: c_int = 13;
    pub(super) const ROW_MT: c_int = 32;
    pub(super) const TILE_COLUMNS: c_int = 33;
    pub(super) const TILE_ROWS: c_int = 34;
    pub(super) const ENABLE_TPL_MODEL: c_int = 35;
    pub(super) const AQ_MODE: c_int = 40;
    pub(super) const TUNE_CONTENT: c_int = 43;
    pub(super) const COLOR_PRIMARIES: c_int = 45;
    pub(super) const TRANSFER_CHARACTERISTICS: c_int = 46;
    pub(super) const MATRIX_COEFFICIENTS: c_int = 47;
    pub(super) const COLOR_RANGE: c_int = 52;
    pub(super) const TARGET_SEQ_LEVEL_IDX: c_int = 54;
    pub(super) const SUPERBLOCK_SIZE: c_int = 56;
    pub(super) const ENABLE_CDEF: c_int = 58;
    pub(super) const ENABLE_ORDER_HINT: c_int = 79;
    pub(super) const ENABLE_PALETTE: c_int = 104;
    pub(super) const DELTAQ_MODE: c_int = 107;
    pub(super) const COEFF_COST_UPD_FREQ: c_int = 126;
    pub(super) const MODE_COST_UPD_FREQ: c_int = 127;
    pub(super) const MV_COST_UPD_FREQ: c_int = 128;
    pub(super) const QUANTIZER_ONE_PASS: c_int = 159;
}

#[repr(C)]
struct Rational {
    numerator: c_int,
    denominator: c_int,
}

#[repr(C)]
struct FixedBuffer {
    data: *mut c_void,
    size: usize,
}

/// `aom_codec_ctx_t` (56 bytes).
#[repr(C)]
struct Context {
    name: *const c_char,
    iface: *const c_void,
    error: c_int,
    error_detail: *const c_char,
    init_flags: c_long,
    config: *const c_void,
    private: *mut c_void,
}

impl Context {
    fn new() -> Box<Self> {
        Box::new(Self {
            name: std::ptr::null(),
            iface: std::ptr::null(),
            error: 0,
            error_detail: std::ptr::null(),
            init_flags: 0,
            config: std::ptr::null(),
            private: std::ptr::null_mut(),
        })
    }
}

/// `aom_codec_enc_cfg_t` (904 bytes in 3.12.1): the fields up to the last
/// one this encoder sets, then the rest of the library's struct and slack,
/// which only libaom reads or writes.
#[repr(C)]
struct EncoderConfig {
    usage: c_uint,
    threads: c_uint,
    profile: c_uint,
    width: c_uint,
    height: c_uint,
    limit: c_uint,
    forced_max_frame_width: c_uint,
    forced_max_frame_height: c_uint,
    bit_depth: c_uint,
    input_bit_depth: c_uint,
    timebase: Rational,
    error_resilient: u32,
    pass: c_uint,
    lag_in_frames: c_uint,
    dropframe_threshold: c_uint,
    resize_mode: c_uint,
    resize_denominator: c_uint,
    resize_key_denominator: c_uint,
    superres_mode: c_uint,
    superres_denominator: c_uint,
    superres_key_denominator: c_uint,
    superres_threshold: c_uint,
    superres_key_threshold: c_uint,
    end_usage: c_uint,
    twopass_stats: FixedBuffer,
    firstpass_stats: FixedBuffer,
    target_bitrate: c_uint,
    min_quantizer: c_uint,
    max_quantizer: c_uint,
    undershoot_pct: c_uint,
    overshoot_pct: c_uint,
    buffer_size: c_uint,
    buffer_initial_size: c_uint,
    buffer_optimal_size: c_uint,
    vbr_bias_pct: c_uint,
    vbr_minsection_pct: c_uint,
    vbr_maxsection_pct: c_uint,
    forward_keyframes: c_int,
    keyframe_mode: c_uint,
    keyframe_min_distance: c_uint,
    keyframe_max_distance: c_uint,
    rest: [u8; CONFIG_BYTES - 196],
}
const CONFIG_BYTES: usize = 4096;

/// `aom_image_t` (168 bytes in 3.12.1): the fields up to the plane strides,
/// then the rest and slack.
#[repr(C)]
struct Image {
    format: c_uint,
    primaries: c_uint,
    transfer: c_uint,
    matrix: c_uint,
    monochrome: c_int,
    sample_position: c_uint,
    range: c_uint,
    width: c_uint,
    height: c_uint,
    bit_depth: c_uint,
    display_width: c_uint,
    display_height: c_uint,
    render_width: c_uint,
    render_height: c_uint,
    x_chroma_shift: c_uint,
    y_chroma_shift: c_uint,
    planes: [*mut u8; 3],
    strides: [c_int; 3],
    rest: [u8; IMAGE_BYTES - 100],
}
const IMAGE_BYTES: usize = 512;

impl Image {
    fn zeroed() -> Box<Self> {
        // SAFETY: every field is an integer, a raw pointer or bytes: all-zero
        // is a valid value of each.
        Box::new(unsafe { std::mem::zeroed() })
    }
}

/// The leading part of `aom_codec_cx_pkt_t` a frame packet is read through.
#[repr(C)]
struct Packet {
    kind: c_uint,
    data: *const u8,
    size: usize,
    pts: i64,
    duration: c_ulong,
    flags: u32,
}

type Interface = unsafe extern "C" fn() -> *const c_void;
type ConfigDefault = unsafe extern "C" fn(*const c_void, *mut EncoderConfig, c_uint) -> c_int;
type EncoderInit =
    unsafe extern "C" fn(*mut Context, *const c_void, *const EncoderConfig, c_long, c_int) -> c_int;
type Control = unsafe extern "C" fn(*mut Context, c_int, ...) -> c_int;
type Encode = unsafe extern "C" fn(*mut Context, *const Image, i64, c_ulong, c_long) -> c_int;
type GetPacket = unsafe extern "C" fn(*mut Context, *mut *const c_void) -> *const Packet;
type Destroy = unsafe extern "C" fn(*mut Context) -> c_int;
type Describe = unsafe extern "C" fn(*const Context) -> *const c_char;
type ImageWrap = unsafe extern "C" fn(*mut Image, c_uint, c_uint, c_uint, c_uint, *mut u8) -> *mut Image;

/// The functions this encoder calls, resolved once per process.
struct Api {
    _library: Library,
    av1_cx: Interface,
    config_default: ConfigDefault,
    encoder_init: EncoderInit,
    control: Control,
    encode: Encode,
    get_packet: GetPacket,
    destroy: Destroy,
    error: Describe,
    error_detail: Describe,
    image_wrap: ImageWrap,
}

impl Api {
    fn load() -> Result<Self, String> {
        let library = Library::open(SONAME)?;
        // SAFETY: each type is the declaration in the pinned aom headers.
        unsafe {
            Ok(Self {
                av1_cx: library.function(c"aom_codec_av1_cx")?,
                config_default: library.function(c"aom_codec_enc_config_default")?,
                encoder_init: library.function(c"aom_codec_enc_init_ver")?,
                control: library.function(c"aom_codec_control")?,
                encode: library.function(c"aom_codec_encode")?,
                get_packet: library.function(c"aom_codec_get_cx_data")?,
                destroy: library.function(c"aom_codec_destroy")?,
                error: library.function(c"aom_codec_error")?,
                error_detail: library.function(c"aom_codec_error_detail")?,
                image_wrap: library.function(c"aom_img_wrap")?,
                _library: library,
            })
        }
    }

    fn describe(&self, context: &Context) -> String {
        let text = |pointer: *const c_char| {
            (!pointer.is_null())
                // SAFETY: libaom returns NUL-terminated static or
                // context-owned strings, copied at once.
                .then(|| unsafe { CStr::from_ptr(pointer) }.to_string_lossy().into_owned())
        };
        // SAFETY: the context is initialized or zeroed; both accept these.
        let (error, detail) = unsafe { ((self.error)(context), (self.error_detail)(context)) };
        match (text(error), text(detail)) {
            (Some(error), Some(detail)) => format!("{error}: {detail}"),
            (Some(error), None) => error,
            _ => "libaom error".into(),
        }
    }
}

fn api() -> Result<&'static Api, VideoError> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(Api::load)
        .as_ref()
        .map_err(|error| VideoError::Unavailable(error.clone()))
}

/// Whether libaom is present and accepts this interface: probed once with
/// a small real-time encoder of each chroma.
pub(super) fn available() -> bool {
    static PROBE: OnceLock<bool> = OnceLock::new();
    *PROBE.get_or_init(|| {
        [VideoCodec::Av1, VideoCodec::Av1Full]
            .into_iter()
            .all(|codec| AomEncoder::new(codec, 64, 64, 1).is_ok())
    })
}

/// The AV1 level of a stream of `width` × `height` at up to 60 pictures per
/// second: the smallest whose picture size, dimensions and display rate
/// hold it (AV1 specification, annex A.3).
fn level(width: u32, height: u32) -> u8 {
    let samples = u64::from(width) * u64::from(height);
    let rate = samples * 60;
    // (seq_level_idx, max picture size, max width, max height, max display rate)
    const LEVELS: [(u8, u64, u32, u32, u64); 6] = [
        (8, 2_228_224, 4096, 2176, 70_778_880),
        (9, 2_228_224, 4096, 2176, 141_557_760),
        (12, 8_912_896, 8192, 4352, 267_386_880),
        (13, 8_912_896, 8192, 4352, 534_773_760),
        (14, 8_912_896, 8192, 4352, 1_069_547_520),
        (16, 35_651_584, 16384, 8704, 1_069_547_520),
    ];
    LEVELS
        .iter()
        .find(|(_, size, max_width, max_height, display)| {
            samples <= *size && width <= *max_width && height <= *max_height && rate <= *display
        })
        .map_or(16, |level| level.0)
}

/// Tile columns (log2) for `threads`: one tile per thread, at most four.
/// Measured on the production node: eight threads over eight tiles cut a
/// full-motion frame's latency by 7% at twice the CPU of four.
fn tile_columns_log2(threads: u32) -> c_int {
    threads.clamp(1, 4).next_power_of_two().trailing_zeros() as c_int
}

pub(super) struct AomEncoder {
    api: &'static Api,
    context: Box<Context>,
    image: Box<Image>,
    codec: VideoCodec,
    width: u32,
    height: u32,
    level: u8,
    /// The quantizer the encoder holds; set only when a request differs.
    quantizer: Option<u8>,
    pictures: i64,
}

// SAFETY: the context and its image are owned by this value and used by one
// thread at a time (`&mut self`); libaom keeps no thread affinity.
unsafe impl Send for AomEncoder {}

impl AomEncoder {
    pub(super) fn new(
        codec: VideoCodec,
        width: u32,
        height: u32,
        threads: u32,
    ) -> Result<Self, VideoError> {
        Self::open(codec, width, height, threads, ENCODER_ABI_VERSION)
    }

    /// `new` against the encoder interface version `abi`: libaom refuses any
    /// version other than its own.
    fn open(
        codec: VideoCodec,
        width: u32,
        height: u32,
        threads: u32,
        abi: c_int,
    ) -> Result<Self, VideoError> {
        let api = api()?;
        let chroma = codec.chroma();
        if !matches!(codec, VideoCodec::Av1 | VideoCodec::Av1Full)
            || width == 0
            || height == 0
            || width > 4096
            || height > 4096
            || (chroma == Chroma::Subsampled && (width % 2 != 0 || height % 2 != 0))
        {
            return Err(VideoError::Unavailable(format!(
                "no AV1 encoder for {} at {width}x{height}",
                codec.token()
            )));
        }
        // SAFETY: an all-zero config is valid input; config_default fills
        // at most the library's 904 bytes of this 4096-byte struct.
        let mut config: Box<EncoderConfig> = Box::new(unsafe { std::mem::zeroed() });
        // SAFETY: the interface function has no preconditions.
        let interface = unsafe { (api.av1_cx)() };
        // SAFETY: interface is libaom's AV1 encoder; config is large enough.
        if unsafe { (api.config_default)(interface, &mut *config, USAGE_REALTIME) } != CODEC_OK {
            return Err(VideoError::Unavailable(
                "libaom refused its real-time defaults".into(),
            ));
        }
        config.threads = threads.clamp(1, 64);
        config.profile = match chroma {
            Chroma::Subsampled => 0,
            Chroma::Full => 1,
        };
        config.width = width;
        config.height = height;
        config.bit_depth = BITS_8;
        config.input_bit_depth = BITS_8;
        config.timebase = Rational {
            numerator: 1,
            denominator: 1_000_000,
        };
        config.error_resilient = 0;
        config.pass = RC_ONE_PASS;
        config.lag_in_frames = 0;
        config.dropframe_threshold = 0;
        config.end_usage = Q;
        config.min_quantizer = 0;
        config.max_quantizer = 63;
        config.keyframe_mode = KF_DISABLED;
        config.keyframe_max_distance = u32::MAX;
        let mut context = Context::new();
        // SAFETY: context is zeroed and boxed (stable address); libaom copies
        // the config at init. The interface version is libaom's to check.
        let status = unsafe { (api.encoder_init)(&mut *context, interface, &*config, 0, abi) };
        if status != CODEC_OK {
            return Err(VideoError::Unavailable(api.describe(&context)));
        }
        let mut encoder = Self {
            api,
            context,
            image: Image::zeroed(),
            codec,
            width,
            height,
            level: level(width, height),
            quantizer: None,
            pictures: 0,
        };
        // Measured on screen content at the probe's surface (evidence:
        // media-producer/encoder-decision.md): screen tuning and the palette
        // make scrolled text 29x and key units 31% smaller; 128x128
        // superblocks, no CDEF and frozen cost tables cut 10-20% of the CPU
        // for 1% more bytes. Speeds 9, 10 and 11 cost the same here.
        for (id, value) in [
            (control::CPU_USED, 10),
            (control::TUNE_CONTENT, CONTENT_SCREEN),
            (control::ENABLE_PALETTE, 1),
            (control::SUPERBLOCK_SIZE, SUPERBLOCK_128),
            (control::ENABLE_CDEF, 0),
            (control::COEFF_COST_UPD_FREQ, COST_UPDATE_OFF),
            (control::MODE_COST_UPD_FREQ, COST_UPDATE_OFF),
            (control::MV_COST_UPD_FREQ, COST_UPDATE_OFF),
            (control::ROW_MT, 1),
            (control::TILE_COLUMNS, tile_columns_log2(threads)),
            (control::TILE_ROWS, 0),
            (control::ENABLE_TPL_MODEL, 0),
            (control::DELTAQ_MODE, 0),
            (control::ENABLE_ORDER_HINT, 0),
            (control::AQ_MODE, 0),
            (control::COLOR_PRIMARIES, CICP_PRIMARIES_BT709),
            (control::TRANSFER_CHARACTERISTICS, CICP_TRANSFER_SRGB),
            (control::MATRIX_COEFFICIENTS, CICP_MATRIX_BT709),
            (control::COLOR_RANGE, COLOR_RANGE_FULL),
            (control::TARGET_SEQ_LEVEL_IDX, c_int::from(encoder.level)),
        ] {
            encoder
                .control(id, value)
                .map_err(|error| VideoError::Unavailable(format!("control {id}: {error}")))?;
        }
        Ok(encoder)
    }

    /// One more control, for the measurement harness only.
    #[cfg(test)]
    pub(super) fn tune(&mut self, id: c_int, value: c_int) -> Result<(), String> {
        self.control(id, value)
    }

    fn control(&mut self, id: c_int, value: c_int) -> Result<(), String> {
        // SAFETY: an initialized context; every control set here takes one
        // int (or unsigned int, passed identically through varargs).
        if unsafe { (self.api.control)(&mut *self.context, id, value) } == CODEC_OK {
            Ok(())
        } else {
            Err(self.api.describe(&self.context))
        }
    }
}

impl Drop for AomEncoder {
    fn drop(&mut self) {
        // SAFETY: initialized in `new` and destroyed exactly once.
        unsafe { (self.api.destroy)(&mut *self.context) };
    }
}

impl VideoEncoder for AomEncoder {
    fn codec(&self) -> VideoCodec {
        self.codec
    }

    fn coded(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn codec_string(&self) -> String {
        let profile = match self.codec.chroma() {
            Chroma::Subsampled => 0,
            Chroma::Full => 1,
        };
        format!("av01.{profile}.{:02}M.08", self.level)
    }

    fn encode(
        &mut self,
        picture: &Picture<'_>,
        request: EncodeRequest,
    ) -> Result<EncodedUnit, VideoError> {
        let planes = picture
            .planes()
            .filter(|_| {
                picture.chroma == self.codec.chroma()
                    && (picture.width, picture.height) == (self.width, self.height)
            })
            .ok_or_else(|| {
                VideoError::Failed(format!(
                    "a {:?} {}x{} picture for a {}x{} {} stream",
                    picture.chroma,
                    picture.width,
                    picture.height,
                    self.width,
                    self.height,
                    self.codec.token()
                ))
            })?;
        if request.quantizer > 63 {
            return Err(VideoError::Failed(format!(
                "quantizer {} is outside 0..=63",
                request.quantizer
            )));
        }
        if self.quantizer != Some(request.quantizer) {
            self.control(control::QUANTIZER_ONE_PASS, c_int::from(request.quantizer))
                .map_err(VideoError::Failed)?;
            self.quantizer = Some(request.quantizer);
        }
        let format = match self.codec.chroma() {
            Chroma::Subsampled => IMG_FMT_I420,
            Chroma::Full => IMG_FMT_I444,
        };
        // SAFETY: the image struct outlasts the call; the data pointer is
        // only read by libaom, and the planes are set exactly below.
        let wrapped = unsafe {
            (self.api.image_wrap)(
                &mut *self.image,
                format,
                self.width,
                self.height,
                1,
                picture.data.as_ptr().cast_mut(),
            )
        };
        if wrapped.is_null() {
            return Err(VideoError::Failed("libaom refused the picture".into()));
        }
        for (index, (plane, stride)) in planes.iter().enumerate() {
            self.image.planes[index] = plane.as_ptr().cast_mut();
            self.image.strides[index] = *stride as c_int;
        }
        self.image.primaries = CICP_PRIMARIES_BT709 as c_uint;
        self.image.transfer = CICP_TRANSFER_SRGB as c_uint;
        self.image.matrix = CICP_MATRIX_BT709 as c_uint;
        self.image.range = COLOR_RANGE_FULL as c_uint;
        let flags = if request.key { EFLAG_FORCE_KF } else { 0 };
        let pts = self.pictures;
        self.pictures += 1;
        // SAFETY: an initialized context and an image whose planes borrow
        // `picture` for this call; libaom copies the source before returning.
        let status = unsafe { (self.api.encode)(&mut *self.context, &*self.image, pts, 1, flags) };
        if status != CODEC_OK {
            return Err(VideoError::Failed(self.api.describe(&self.context)));
        }
        let mut unit: Option<EncodedUnit> = None;
        let mut iterator: *const c_void = std::ptr::null();
        loop {
            // SAFETY: the iterator starts null and is advanced only by libaom.
            let packet = unsafe { (self.api.get_packet)(&mut *self.context, &mut iterator) };
            // SAFETY: a non-null packet is valid until the next encoder call.
            let Some(packet) = (unsafe { packet.as_ref() }) else {
                break;
            };
            if packet.kind != CX_FRAME_PKT {
                continue;
            }
            if unit.is_some() || packet.data.is_null() || packet.size == 0 {
                return Err(VideoError::Failed(
                    "libaom returned other than one picture for one picture".into(),
                ));
            }
            // SAFETY: libaom owns `size` bytes at `data` until the next call.
            let data = unsafe { std::slice::from_raw_parts(packet.data, packet.size) }.to_vec();
            unit = Some(EncodedUnit {
                data,
                key: packet.flags & FRAME_IS_KEY != 0,
            });
        }
        let unit = unit.ok_or_else(|| {
            VideoError::Failed("libaom held a picture back: the stream would lag".into())
        })?;
        if request.key && !unit.key {
            return Err(VideoError::Failed(
                "libaom did not encode the requested key unit".into(),
            ));
        }
        Ok(unit)
    }
}

#[cfg(test)]
#[path = "aom_tests.rs"]
pub(super) mod tests;
