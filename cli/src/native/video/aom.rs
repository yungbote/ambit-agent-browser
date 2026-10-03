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

use super::convert::COLOUR;
use super::library::Library;
use super::{
    Chroma, EncodeRequest, EncodedUnit, EncoderRate, EncoderRegion, Picture, VideoCodec,
    VideoEncoder, VideoError,
};

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
const CBR: c_uint = 1;
const KF_DISABLED: c_uint = 0;
const BITS_8: c_uint = 8;

const CONTENT_SCREEN: c_int = 1;
const SUPERBLOCK_128: c_int = 1;
/// The cost tables' update frequency "off": each superblock row reuses them.
const COST_UPDATE_OFF: c_int = 3;
/// Encoder speed for pictures of motion: measured on the node, 9, 10 and 11
/// cost the same (encoder-decision.md).
const MOTION_SPEED: c_int = 10;
/// Encoder speed for a refinement. At speed 10 a refinement after a scroll
/// of more than a few pictures stops near 44.5 dB RGB PSNR on dense text
/// whatever the quantizer (a second pass adds nothing); at 8 it reaches
/// 49.1-49.4 dB in one pass for about the same bytes and time
/// (media-producer/refinement-speed.md).
const REFINE_SPEED: c_int = 8;

/// Encoder controls (`aomcx.h` ids) this encoder sets.
mod control {
    use std::ffi::c_int;
    pub(super) const ACTIVE_MAP: c_int = 9;
    pub(super) const CPU_USED: c_int = 13;
    pub(super) const ROW_MT: c_int = 32;
    pub(super) const MAX_INTER_BITRATE_PCT: c_int = 28;
    pub(super) const TILE_COLUMNS: c_int = 33;
    pub(super) const TILE_ROWS: c_int = 34;
    pub(super) const ENABLE_TPL_MODEL: c_int = 35;
    pub(super) const AQ_MODE: c_int = 40;
    pub(super) const TUNE_CONTENT: c_int = 43;
    pub(super) const COLOR_PRIMARIES: c_int = 45;
    pub(super) const TRANSFER_CHARACTERISTICS: c_int = 46;
    pub(super) const MATRIX_COEFFICIENTS: c_int = 47;
    pub(super) const COLOR_RANGE: c_int = 52;
    pub(super) const SUPERBLOCK_SIZE: c_int = 56;
    pub(super) const ENABLE_CDEF: c_int = 58;
    pub(super) const ENABLE_ORDER_HINT: c_int = 79;
    pub(super) const ENABLE_PALETTE: c_int = 104;
    pub(super) const DELTAQ_MODE: c_int = 107;
    pub(super) const COEFF_COST_UPD_FREQ: c_int = 126;
    pub(super) const MODE_COST_UPD_FREQ: c_int = 127;
    pub(super) const MV_COST_UPD_FREQ: c_int = 128;
    pub(super) const QUANTIZER_ONE_PASS: c_int = 159;
    pub(super) const LOOPFILTER_CONTROL: c_int = 149;
}

/// aom_active_map_t: one flag per 16x16 coded block. Both pointer and backing storage remain owned.
#[repr(C)]
struct ActiveMap {
    cells: *mut u8,
    rows: c_uint,
    columns: c_uint,
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
type ImageWrap =
    unsafe extern "C" fn(*mut Image, c_uint, c_uint, c_uint, c_uint, *mut u8) -> *mut Image;
type ConfigSet = unsafe extern "C" fn(*mut Context, *const EncoderConfig) -> c_int;
type PreviewFrame = unsafe extern "C" fn(*mut Context) -> *const Image;

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
    config_set: ConfigSet,
    preview_frame: PreviewFrame,
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
                config_set: library.function(c"aom_codec_enc_config_set")?,
                preview_frame: library.function(c"aom_codec_get_preview_frame")?,
                _library: library,
            })
        }
    }

    fn describe(&self, context: &Context) -> String {
        let text = |pointer: *const c_char| {
            (!pointer.is_null())
                // SAFETY: libaom returns NUL-terminated static or
                // context-owned strings, copied at once.
                .then(|| {
                    unsafe { CStr::from_ptr(pointer) }
                        .to_string_lossy()
                        .into_owned()
                })
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

/// Read only the configuration fields needed by the codec parameter string
/// from operating point zero of the key's sequence header. Bounds-checked
/// reads keep a malformed library result a stream failure, never a panic.
fn key_codec_string(unit: &[u8]) -> Option<String> {
    struct Bits<'a> {
        data: &'a [u8],
        position: usize,
    }
    impl Bits<'_> {
        fn read(&mut self, count: usize) -> Option<u32> {
            if count > 32 || self.position.checked_add(count)? > self.data.len().checked_mul(8)? {
                return None;
            }
            let mut value = 0;
            for _ in 0..count {
                value = (value << 1)
                    | u32::from((self.data[self.position / 8] >> (7 - self.position % 8)) & 1);
                self.position += 1;
            }
            Some(value)
        }
    }
    let mut at = 0usize;
    while let Some(&header) = unit.get(at) {
        at += 1;
        if header & 0x81 != 0 {
            return None;
        }
        if header & 4 != 0 {
            unit.get(at)?;
            at += 1;
        }
        let size = if header & 2 != 0 {
            let mut value = 0usize;
            let mut ended = false;
            for shift in (0..56).step_by(7) {
                let byte = *unit.get(at)?;
                at += 1;
                value = value.checked_add(usize::from(byte & 127).checked_shl(shift)?)?;
                if byte & 128 == 0 {
                    ended = true;
                    break;
                }
            }
            if !ended {
                return None;
            }
            value
        } else {
            unit.len().checked_sub(at)?
        };
        let end = at.checked_add(size)?;
        let payload = unit.get(at..end)?;
        at = end;
        if (header >> 3) & 15 != 1 {
            continue;
        }
        let mut bits = Bits {
            data: payload,
            position: 0,
        };
        let profile = bits.read(3)?;
        if profile > 2 {
            return None;
        }
        let still = bits.read(1)? != 0;
        let reduced = bits.read(1)? != 0;
        let (level, tier) = if reduced {
            if !still {
                return None;
            }
            (bits.read(5)?, 0)
        } else {
            if bits.read(1)? != 0 {
                bits.read(32)?;
                bits.read(32)?;
                if bits.read(1)? != 0 {
                    let mut zeros = 0usize;
                    while bits.read(1)? == 0 {
                        zeros += 1;
                        if zeros == 32 {
                            break;
                        }
                    }
                    if zeros < 32 {
                        bits.read(zeros)?;
                    }
                }
                if bits.read(1)? != 0 {
                    bits.read(5)?;
                    bits.read(32)?;
                    bits.read(5)?;
                    bits.read(5)?;
                }
            }
            bits.read(1)?; // initial_display_delay_present_flag
            bits.read(5)?; // operating_points_cnt_minus_1
            bits.read(12)?; // operating_point_idc[0]
            let level = bits.read(5)?;
            let tier = if level > 7 { bits.read(1)? } else { 0 };
            (level, tier)
        };
        return Some(format!(
            "av01.{profile}.{level:02}{}.08",
            if tier == 0 { 'M' } else { 'H' }
        ));
    }
    None
}

/// libaom's configuration of one stream: its real-time defaults, then every
/// field this encoder sets. libaom copies it at init.
fn configuration(
    api: &Api,
    codec: VideoCodec,
    width: u32,
    height: u32,
    threads: u32,
) -> Result<Box<EncoderConfig>, VideoError> {
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
    config.profile = match codec.chroma() {
        Chroma::Subsampled => 0,
        Chroma::Full => 1,
    };
    config.width = width;
    config.height = height;
    config.bit_depth = BITS_8;
    config.input_bit_depth = BITS_8;
    config.timebase = Rational {
        numerator: 1,
        denominator: 60,
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
    Ok(config)
}

/// Tile columns (log2) for `threads`: one tile per thread, at most four.
/// Measured on the production node: eight threads over eight tiles cut a
/// full-motion frame's latency by 7% at twice the CPU of four.
pub(crate) fn tile_columns_log2(threads: u32) -> c_int {
    threads.clamp(1, 4).next_power_of_two().trailing_zeros() as c_int
}

pub(crate) struct AomEncoder {
    api: &'static Api,
    context: Box<Context>,
    image: Box<Image>,
    codec: VideoCodec,
    width: u32,
    height: u32,
    config: Box<EncoderConfig>,
    rate: Option<EncoderRate>,
    region: Option<EncoderRegion>,
    active_cells: Vec<u8>,
    active_map: Box<ActiveMap>,
    region_data: Vec<u8>,
    /// The quantizer and speed the encoder holds; set only when a request
    /// differs.
    quantizer: Option<u8>,
    speed: c_int,
    pictures: i64,
}

// SAFETY: the context and its image are owned by this value and used by one
// thread at a time (`&mut self`); libaom keeps no thread affinity.
unsafe impl Send for AomEncoder {}

impl AomEncoder {
    pub(crate) fn new(
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
            || (chroma == Chroma::Subsampled
                && (!width.is_multiple_of(2) || !height.is_multiple_of(2)))
        {
            return Err(VideoError::Unavailable(format!(
                "no AV1 encoder for {} at {width}x{height}",
                codec.token()
            )));
        }
        let config = configuration(api, codec, width, height, threads)?;
        // SAFETY: the interface function has no preconditions.
        let interface = unsafe { (api.av1_cx)() };
        let mut context = Context::new();
        // SAFETY: context is zeroed and boxed (stable address); libaom copies
        // the config at init. The interface version is libaom's to check.
        let status = unsafe { (api.encoder_init)(&mut *context, interface, &*config, 0, abi) };
        if status != CODEC_OK {
            return Err(VideoError::Unavailable(api.describe(&context)));
        }
        let rows = height.div_ceil(16);
        let columns = width.div_ceil(16);
        let active_cells = vec![1; (rows * columns) as usize];
        let active_map = Box::new(ActiveMap {
            cells: std::ptr::null_mut(),
            rows,
            columns,
        });
        let mut encoder = Self {
            api,
            context,
            image: Image::zeroed(),
            codec,
            width,
            height,
            config,
            rate: None,
            region: None,
            active_cells,
            active_map,
            region_data: Vec::new(),
            quantizer: None,
            speed: MOTION_SPEED,
            pictures: 0,
        };
        // Measured on screen content at the probe's surface (evidence:
        // media-producer/encoder-decision.md): screen tuning and the palette
        // make scrolled text 29x and key units 31% smaller; 128x128
        // superblocks, no CDEF and frozen cost tables cut 10-20% of the CPU
        // for 1% more bytes. Speeds 9, 10 and 11 cost the same here.
        for (id, value) in [
            (control::CPU_USED, MOTION_SPEED),
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
            // The sequence header says what the pictures are (`COLOUR`);
            // libaom's range is 0 for limited, 1 for full.
            (control::COLOR_PRIMARIES, c_int::from(COLOUR.primaries)),
            (
                control::TRANSFER_CHARACTERISTICS,
                c_int::from(COLOUR.transfer),
            ),
            (control::MATRIX_COEFFICIENTS, c_int::from(COLOUR.matrix)),
            (control::COLOR_RANGE, c_int::from(COLOUR.full_range)),
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

    /// Threads and tile columns (log2) for the pictures that follow, for the
    /// measurement harnesses only. libaom sizes its workers per picture and
    /// grows its pool when a picture needs more; it takes a whole
    /// configuration back, which also resets the quantizer's range, so the
    /// next picture sets its quantizer again.
    #[cfg(test)]
    pub(crate) fn set_threads(&mut self, threads: u32, tile_columns: c_int) -> Result<(), String> {
        let config = configuration(self.api, self.codec, self.width, self.height, threads)
            .map_err(|error| error.to_string())?;
        // SAFETY: an initialized context; libaom copies the configuration.
        if unsafe { (self.api.config_set)(&mut *self.context, &*config) } != CODEC_OK {
            return Err(self.api.describe(&self.context));
        }
        self.quantizer = None;
        self.config = config;
        self.control(control::TILE_COLUMNS, tile_columns)
    }

    fn rate_configuration(&mut self, refine: bool, key: bool) -> Result<(), VideoError> {
        // Initial/reference-reset pictures retain the native fixed-quality
        // policy. CBR governs dependent motion, never a fictional key bound.
        let end_usage = if self.rate.is_some() && !key { CBR } else { Q };
        let bitrate = self.rate.map_or(self.config.target_bitrate, |rate| {
            rate.bits_per_second.div_ceil(1000)
        });
        let picture_rate = self.rate.map_or(60, |rate| rate.pictures_per_second) as c_int;
        let minimum = if end_usage == CBR {
            if refine {
                8
            } else {
                20
            }
        } else {
            0
        };
        let maximum = if end_usage == CBR {
            if refine {
                8
            } else {
                48
            }
        } else {
            63
        };
        if self.config.end_usage == end_usage
            && self.config.target_bitrate == bitrate
            && self.config.min_quantizer == minimum
            && self.config.max_quantizer == maximum
            && self.config.timebase.denominator == picture_rate
        {
            return Ok(());
        }
        self.config.end_usage = end_usage;
        self.config.target_bitrate = bitrate;
        self.config.min_quantizer = minimum;
        self.config.max_quantizer = maximum;
        self.config.timebase = Rational {
            numerator: 1,
            denominator: picture_rate,
        };
        self.config.buffer_size = 300;
        self.config.buffer_initial_size = 150;
        self.config.buffer_optimal_size = 150;
        // SAFETY: initialized context and the same owned configuration used at open.
        if unsafe { (self.api.config_set)(&mut *self.context, &*self.config) } != CODEC_OK {
            return Err(VideoError::Failed(self.api.describe(&self.context)));
        }
        self.quantizer = None;
        Ok(())
    }

    /// A regional unit reuses reconstructed pixels outside its region; source pixels there would invite a different prediction.
    fn prepare_region(
        &mut self,
        picture: &Picture<'_>,
        region: EncoderRegion,
    ) -> Result<(), VideoError> {
        // SAFETY: initialized encoder; preview remains owned by it until the next codec call and is copied here.
        let preview = unsafe { (self.api.preview_frame)(&mut *self.context).as_ref() }
            .ok_or_else(|| VideoError::Failed("no reconstructed picture for a regional unit".into()))?;
        let format = match self.codec.chroma() {
            Chroma::Full => IMG_FMT_I444,
            Chroma::Subsampled => IMG_FMT_I420,
        };
        if preview.format != format
            || preview.display_width != self.width
            || preview.display_height != self.height
        {
            return Err(VideoError::Failed(
                "reconstructed regional geometry differs from its stream".into(),
            ));
        }
        let chroma = self.codec.chroma();
        self.region_data.resize(
            chroma.picture_bytes(self.width as usize, self.height as usize),
            0,
        );
        let source = picture
            .planes()
            .ok_or_else(|| VideoError::Failed("invalid regional source".into()))?;
        let mut offset = 0usize;
        for (index, (capture, capture_stride)) in source.iter().enumerate() {
            let scale = if index > 0 && chroma == Chroma::Subsampled {
                2
            } else {
                1
            };
            let width = self.width as usize / scale;
            let height = self.height as usize / scale;
            let stride = preview.strides[index];
            if preview.planes[index].is_null() || stride < width as c_int {
                return Err(VideoError::Failed(
                    "invalid reconstructed regional plane".into(),
                ));
            }
            let target = &mut self.region_data[offset..offset + width * height];
            for row in 0..height {
                // SAFETY: validated coded plane dimensions/stride; the preview owns every referenced row for this call.
                let pixels = unsafe {
                    std::slice::from_raw_parts(
                        preview.planes[index].add(row * stride as usize),
                        width,
                    )
                };
                target[row * width..(row + 1) * width].copy_from_slice(pixels);
            }
            let left = region.x as usize / scale;
            let right = (region.x + region.width).div_ceil(scale as u32) as usize;
            let top = region.y as usize / scale;
            let bottom = (region.y + region.height).div_ceil(scale as u32) as usize;
            for row in top..bottom {
                target[row * width + left..row * width + right].copy_from_slice(
                    &capture[row * capture_stride + left..row * capture_stride + right],
                );
            }
            offset += width * height;
        }
        Ok(())
    }

    /// Active blocks are applied after rate/configuration changes. Filtering a regional update would also alter inactive references.
    fn configure_region(&mut self, region: Option<EncoderRegion>) -> Result<(), VideoError> {
        if let Some(region) = region {
            self.active_cells.fill(0);
            for row in region.y / 16..(region.y + region.height).div_ceil(16) {
                for column in region.x / 16..(region.x + region.width).div_ceil(16) {
                    self.active_cells[(row * self.active_map.columns + column) as usize] = 1;
                }
            }
            self.active_map.cells = self.active_cells.as_mut_ptr();
        } else if self.active_map.cells.is_null() {
            return Ok(());
        } else {
            self.active_map.cells = std::ptr::null_mut();
        }
        // SAFETY: pinned aom_active_map_t layout; descriptor and fixed-size cells outlive the encoder call.
        if unsafe {
            (self.api.control)(
                &mut *self.context,
                control::ACTIVE_MAP,
                &mut *self.active_map,
            )
        } != CODEC_OK
        {
            return Err(VideoError::Failed(self.api.describe(&self.context)));
        }
        self.control(control::LOOPFILTER_CONTROL, i32::from(region.is_none()))
            .map_err(VideoError::Failed)
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
    fn coded(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn set_rate(&mut self, rate: Option<EncoderRate>) -> Result<(), VideoError> {
        if rate.is_some_and(|rate| {
            rate.bits_per_second < 1000
                || rate.pictures_per_second == 0
                || rate.pictures_per_second > 60
        }) {
            return Err(VideoError::Failed("invalid video rate budget".into()));
        }
        if self.rate == rate {
            return Ok(());
        }
        self.rate = rate;
        self.rate_configuration(false, false)?;
        self.control(
            control::MAX_INTER_BITRATE_PCT,
            if rate.is_some() { 115 } else { 0 },
        )
        .map_err(VideoError::Failed)
    }

    fn set_region(
        &mut self,
        region: Option<EncoderRegion>,
    ) -> Result<Option<EncoderRegion>, VideoError> {
        if let Some(region) = region {
            if region.width == 0
                || region.height == 0
                || region
                    .x
                    .checked_add(region.width)
                    .is_none_or(|end| end > self.width)
                || region
                    .y
                    .checked_add(region.height)
                    .is_none_or(|end| end > self.height)
            {
                return Err(VideoError::Failed(
                    "region is outside the coded picture".into(),
                ));
            }
        }
        // This realtime preset codes mixed active-map cells in 32 px blocks.
        // Return its actual update scope instead of pretending 16 px map cells
        // are independent coding units. Source pixels outside it stay exact.
        self.region = region.map(|region| {
            let x = region.x / 32 * 32;
            let y = region.y / 32 * 32;
            let right = (region.x + region.width).div_ceil(32) * 32;
            let bottom = (region.y + region.height).div_ceil(32) * 32;
            EncoderRegion {
                x,
                y,
                width: right.min(self.width) - x,
                height: bottom.min(self.height) - y,
            }
        });
        Ok(self.region)
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
        if request.key {
            self.region = None;
        }
        let region = self.region;
        let regional = region.is_some();
        if let Some(region) = region {
            self.prepare_region(picture, region)?;
        }
        self.rate_configuration(request.refine, request.key)?;
        if (self.rate.is_none() || request.refine || request.key)
            && self.quantizer != Some(request.quantizer)
        {
            self.control(control::QUANTIZER_ONE_PASS, c_int::from(request.quantizer))
                .map_err(VideoError::Failed)?;
            self.quantizer = Some(request.quantizer);
        }
        let speed = if request.refine {
            REFINE_SPEED
        } else {
            MOTION_SPEED
        };
        if self.speed != speed {
            self.control(control::CPU_USED, speed)
                .map_err(VideoError::Failed)?;
            self.speed = speed;
        }
        self.configure_region(region)?;
        let format = match self.codec.chroma() {
            Chroma::Subsampled => IMG_FMT_I420,
            Chroma::Full => IMG_FMT_I444,
        };
        let refined;
        let (picture, planes) = if regional {
            refined = Picture {
                chroma: self.codec.chroma(),
                width: self.width,
                height: self.height,
                data: &self.region_data,
            };
            (
                &refined,
                refined
                    .planes()
                    .expect("the copied refinement has exact coded planes"),
            )
        } else {
            (picture, planes)
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
        self.image.primaries = c_uint::from(COLOUR.primaries);
        self.image.transfer = c_uint::from(COLOUR.transfer);
        self.image.matrix = c_uint::from(COLOUR.matrix);
        self.image.range = c_uint::from(COLOUR.full_range);
        let flags = if request.key { EFLAG_FORCE_KF } else { 0 };
        let pts = self.pictures;
        // The codec clock is frame ticks at its configured rate. Sandbox
        // capture timestamps remain in the unit envelope, never this clock.
        let duration = 1u64;
        self.pictures += 1;
        #[cfg(test)]
        if std::env::var_os("AMBIT_CODEC_FRAME_TRACE").is_some() {
            eprintln!(
                "CODEC_FRAME {}",
                serde_json::json!({"picture":pts,"key":request.key,"refine":request.refine,"rate":self.rate.map(|rate|rate.bits_per_second),"timebase":[self.config.timebase.numerator,self.config.timebase.denominator],"mode":self.config.end_usage,"region":self.region.map(|region|[region.x,region.y,region.width,region.height])})
            );
        }
        // SAFETY: an initialized context and an image whose planes borrow
        // `picture` for this call; libaom copies the source before returning.
        let status = unsafe {
            (self.api.encode)(
                &mut *self.context,
                &*self.image,
                pts,
                duration as c_ulong,
                flags,
            )
        };
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
            if packet.size
                > self
                    .codec
                    .coded_capacity(self.width, self.height)
                    .expect("validated native dimensions")
            {
                return Err(VideoError::Failed(
                    "libaom exceeded its dimensional output storage".into(),
                ));
            }
            // SAFETY: libaom owns `size` bytes at `data` until the next call.
            let data = unsafe { std::slice::from_raw_parts(packet.data, packet.size) }.to_vec();
            let key = packet.flags & FRAME_IS_KEY != 0;
            let codec_string = if key {
                Some(key_codec_string(&data).ok_or_else(|| {
                    VideoError::Failed("libaom key lacks a valid AV1 sequence header".into())
                })?)
            } else {
                None
            };
            unit = Some(EncodedUnit {
                data,
                key,
                codec_string,
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
