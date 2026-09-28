use super::*;
use crate::native::video::convert::{to_rgb, Colour, Planar};
use std::mem::{offset_of, size_of};

/// The layout this FFI relies on, against the values `offsetof` measured
/// from the libaom 3.12.1 headers (the image's libaom3 3.12.1-1+deb13u1).
/// A field declared at a wrong place fails here, not in memory.
#[test]
fn every_field_sits_where_the_pinned_headers_put_it() {
    assert_eq!(size_of::<Context>(), 56);
    assert_eq!(offset_of!(Context, error), 16);
    assert_eq!(offset_of!(Context, error_detail), 24);
    for (offset, expected) in [
        (offset_of!(EncoderConfig, usage), 0),
        (offset_of!(EncoderConfig, threads), 4),
        (offset_of!(EncoderConfig, profile), 8),
        (offset_of!(EncoderConfig, width), 12),
        (offset_of!(EncoderConfig, height), 16),
        (offset_of!(EncoderConfig, forced_max_frame_width), 24),
        (offset_of!(EncoderConfig, bit_depth), 32),
        (offset_of!(EncoderConfig, input_bit_depth), 36),
        (offset_of!(EncoderConfig, timebase), 40),
        (offset_of!(EncoderConfig, error_resilient), 48),
        (offset_of!(EncoderConfig, pass), 52),
        (offset_of!(EncoderConfig, lag_in_frames), 56),
        (offset_of!(EncoderConfig, dropframe_threshold), 60),
        (offset_of!(EncoderConfig, resize_mode), 64),
        (offset_of!(EncoderConfig, superres_mode), 76),
        (offset_of!(EncoderConfig, end_usage), 96),
        (offset_of!(EncoderConfig, twopass_stats), 104),
        (offset_of!(EncoderConfig, target_bitrate), 136),
        (offset_of!(EncoderConfig, min_quantizer), 140),
        (offset_of!(EncoderConfig, max_quantizer), 144),
        (offset_of!(EncoderConfig, undershoot_pct), 148),
        (offset_of!(EncoderConfig, buffer_size), 156),
        (offset_of!(EncoderConfig, buffer_optimal_size), 164),
        (offset_of!(EncoderConfig, keyframe_mode), 184),
        (offset_of!(EncoderConfig, keyframe_min_distance), 188),
        (offset_of!(EncoderConfig, keyframe_max_distance), 192),
        (offset_of!(Image, format), 0),
        (offset_of!(Image, primaries), 4),
        (offset_of!(Image, transfer), 8),
        (offset_of!(Image, matrix), 12),
        (offset_of!(Image, range), 24),
        (offset_of!(Image, width), 28),
        (offset_of!(Image, display_width), 40),
        (offset_of!(Image, display_height), 44),
        (offset_of!(Image, x_chroma_shift), 56),
        (offset_of!(Image, planes), 64),
        (offset_of!(Image, strides), 88),
        (offset_of!(Packet, kind), 0),
        (offset_of!(Packet, data), 8),
        (offset_of!(Packet, size), 16),
        (offset_of!(Packet, pts), 24),
        (offset_of!(Packet, flags), 40),
    ] {
        assert_eq!(offset, expected);
    }
    // Room for everything libaom writes into a whole struct, with slack.
    assert!(size_of::<EncoderConfig>() >= 904 && size_of::<Image>() >= 168);
}

#[test]
fn the_level_is_the_smallest_that_holds_the_stream_at_sixty_pictures() {
    assert_eq!(level(64, 64), 8);
    assert_eq!(level(1280, 720), 8);
    assert_eq!(
        level(1920, 1080),
        9,
        "124.4 M samples/s exceed 4.0's 70.8 M"
    );
    assert_eq!(level(2048, 2048), 12);
    assert_eq!(level(4096, 2176), 13);
    assert_eq!(level(4096, 4096), 16);
}

#[test]
fn another_interface_version_and_impossible_sizes_are_refused() {
    let mismatch = AomEncoder::open(VideoCodec::Av1, 64, 64, 1, ENCODER_ABI_VERSION + 1)
        .err()
        .unwrap();
    assert!(
        matches!(&mismatch, VideoError::Unavailable(detail) if detail.to_lowercase().contains("abi")),
        "{mismatch}"
    );
    for (codec, width, height) in [
        (VideoCodec::Av1, 0, 64),
        (VideoCodec::Av1, 64, 4097),
        (VideoCodec::Av1, 63, 64),
        (VideoCodec::Vp9, 64, 64),
    ] {
        assert!(
            matches!(
                AomEncoder::new(codec, width, height, 1),
                Err(VideoError::Unavailable(_))
            ),
            "{codec:?} {width}x{height}"
        );
    }
    // A 4:4:4 stream may have odd dimensions.
    assert!(AomEncoder::new(VideoCodec::Av1Full, 63, 65, 1).is_ok());
}

#[test]
fn a_picture_of_another_size_chroma_or_quantizer_fails_that_picture_only() {
    let mut encoder = AomEncoder::new(VideoCodec::Av1Full, 64, 64, 1).unwrap();
    let wrong_size = Planar::new(Chroma::Full, 64, 32);
    let wrong_chroma = Planar::new(Chroma::Subsampled, 64, 64);
    let right = Planar::new(Chroma::Full, 64, 64);
    for picture in [wrong_size.picture(), wrong_chroma.picture()] {
        assert!(matches!(
            encoder.encode(
                &picture,
                EncodeRequest {
                    key: true,
                    quantizer: 20,
                    ..Default::default()
                }
            ),
            Err(VideoError::Failed(_))
        ));
    }
    assert!(matches!(
        encoder.encode(
            &right.picture(),
            EncodeRequest {
                key: true,
                quantizer: 64,
                ..Default::default()
            }
        ),
        Err(VideoError::Failed(_))
    ));
    // The encoder is intact: the refused pictures never reached libaom.
    let unit = encoder
        .encode(
            &right.picture(),
            EncodeRequest {
                key: true,
                quantizer: 20,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(unit.key);
}

/// Every picture comes back as its own unit from its own call: no lookahead,
/// no reordering, no frame delay. The first is a key unit, later ones are
/// not unless asked for.
#[test]
fn one_picture_in_one_unit_out_with_key_units_only_where_asked() {
    let mut encoder = AomEncoder::new(VideoCodec::Av1, 256, 128, 4).unwrap();
    let mut picture = Planar::new(Chroma::Subsampled, 256, 128);
    let mut keys = Vec::new();
    for index in 0..12u8 {
        let row: Vec<u8> = (0..256usize)
            .flat_map(|x| {
                let value = (x as u8).wrapping_add(index.wrapping_mul(9));
                [value, value / 2, 255 - value, 0]
            })
            .collect();
        let source: Vec<u8> = (0..128).flat_map(|_| row.clone()).collect();
        assert!(picture.convert(&source, 1024, (256, 128), (0, 128)));
        let key = index == 7;
        let unit = encoder
            .encode(
                &picture.picture(),
                EncodeRequest {
                    key,
                    quantizer: 30,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(!unit.data.is_empty());
        keys.push(unit.key);
    }
    let expected: Vec<bool> = (0..12).map(|index| index == 0 || index == 7).collect();
    assert_eq!(keys, expected);
}

/// The key unit's sequence header says what the codec string and the
/// pictures claim: profile, level, and the contract's colour (section 2, rev
/// 4), BT.709 primaries (1), transfer (1) and matrix (1) at full range.
#[test]
fn the_sequence_header_signals_the_codec_string_and_colour_description() {
    for (codec, width, height) in [
        (VideoCodec::Av1Full, 2048, 2048),
        (VideoCodec::Av1, 1280, 720),
    ] {
        let mut encoder = AomEncoder::new(codec, width, height, 2).unwrap();
        let picture = Planar::new(codec.chroma(), width, height);
        let unit = encoder
            .encode(
                &picture.picture(),
                EncodeRequest {
                    key: true,
                    quantizer: 40,
                    ..Default::default()
                },
            )
            .unwrap();
        let header = sequence_header(&unit.data).expect("a key unit carries its sequence header");
        let profile = u8::from(codec.chroma() == Chroma::Full);
        assert_eq!(header.profile, profile);
        assert_eq!(
            encoder.codec_string(),
            format!("av01.{profile}.{:02}M.08", header.level)
        );
        assert_eq!(header.level, level(width, height));
        assert_eq!(header.tier, 0);
        assert_eq!(
            (
                header.primaries,
                header.transfer,
                header.matrix,
                header.full_range
            ),
            (Some(1), Some(1), Some(1), true)
        );
        assert_eq!(
            header.subsampling,
            match codec.chroma() {
                Chroma::Full => (0, 0),
                Chroma::Subsampled => (1, 1),
            }
        );
    }
    let encoder = AomEncoder::new(VideoCodec::Av1Full, 2048, 2048, 1).unwrap();
    assert_eq!(
        encoder.codec_string(),
        "av01.1.12M.08",
        "the contract's probe string"
    );
}

/// A screen of text (1 px strokes, grey antialiasing, coloured links) on a
/// white page and on a black one, with colour bars, goes through the
/// conversion, the encoder at the still target, a real AV1 decoder
/// (libaom's own) that reads the colour the bitstream signals, and the
/// inverse at that range, as the viewer's decoder paints it. The decoder
/// reads the colour `convert` produced, inside the bars it returns the very
/// samples the encoder was given, and the picture is painted as it was
/// drawn (`check_painted`). A wrong stride, matrix or range fails this by
/// tens of dB, a tint, or a page that is not white or black.
#[test]
fn text_and_colour_bars_survive_convert_encode_decode_and_the_inverse_matrix() {
    let (width, height) = FIXTURE;
    for scheme in [LIGHT, DARK] {
        let source = text_fixture(scheme);
        for codec in [VideoCodec::Av1Full, VideoCodec::Av1] {
            let case = format!("{codec:?} on {:?}", scheme.page);
            let (picture, unit, _) = still_unit(codec, &source);
            let decoded = Decoder::new().decode(&unit.data);
            assert_eq!(
                (decoded.width, decoded.height, decoded.chroma),
                (width as u32, height as u32, codec.chroma())
            );
            assert_eq!(decoded.colour, COLOUR, "{case}: the signalled colour");
            let given = picture.picture().planes().unwrap();
            for (x, y) in bar_insides() {
                let chroma = match codec.chroma() {
                    Chroma::Full => y * width + x,
                    Chroma::Subsampled => (y / 2) * given[1].1 + x / 2,
                };
                for (plane, at) in [y * width + x, chroma, chroma].into_iter().enumerate() {
                    assert_eq!(
                        decoded.planes[plane][at], given[plane].0[at],
                        "{case}: bar sample {plane} at {x},{y}"
                    );
                }
            }
            check_painted(&case, scheme, codec.chroma(), &source, &decoded.rgb());
        }
    }
}

/// A refinement is encoded at the refinement speed, and the next picture of
/// motion at the motion speed again; the speed is set only when it changes.
#[test]
fn a_refinement_takes_its_own_speed_and_motion_returns_to_its_own() {
    let mut encoder = AomEncoder::new(VideoCodec::Av1Full, 64, 64, 1).unwrap();
    let picture = Planar::new(Chroma::Full, 64, 64);
    let mut speeds = Vec::new();
    for (key, quantizer, refine) in [
        (true, 32, false),
        (false, 8, true),
        (false, 8, true),
        (false, 32, false),
    ] {
        let request = EncodeRequest {
            key,
            quantizer,
            refine,
        };
        encoder.encode(&picture.picture(), request).unwrap();
        speeds.push(encoder.speed);
    }
    assert_eq!(
        speeds,
        [MOTION_SPEED, REFINE_SPEED, REFINE_SPEED, MOTION_SPEED]
    );
}

// ---------------------------------------------------------------------------
// Fixtures: text pages over colour bars and what they must look like once
// painted, a real decoder and a sequence header reader.

/// The text fixture's size: text inside a margin of bare page, over colour
/// bars.
pub(crate) const FIXTURE: (usize, usize) = (320, 192);
/// The bare page around the text, as a page's margins.
const MARGIN: usize = 32;
/// The colour bars along the bottom: white, the six saturated primaries and
/// secondaries, and black.
const BARS: [[u8; 3]; 8] = [
    [255, 255, 255],
    [255, 255, 0],
    [0, 255, 255],
    [0, 255, 0],
    [255, 0, 255],
    [255, 0, 0],
    [0, 0, 255],
    [0, 0, 0],
];
const BARS_HEIGHT: usize = 32;
/// The distance from any edge beyond which the page must be exact: more
/// than twice as far as the codec's ringing reaches at the still target
/// (measured: 3 px).
const RINGING: usize = 8;

/// A page's colours: its background, its text and its links.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Scheme {
    pub page: [u8; 3],
    pub ink: [u8; 3],
    pub link: [u8; 3],
}

pub(crate) const LIGHT: Scheme = Scheme {
    page: [255; 3],
    ink: [29, 29, 31],
    link: [10, 102, 194],
};

pub(crate) const DARK: Scheme = Scheme {
    page: [0; 3],
    ink: [232, 234, 237],
    link: [138, 180, 248],
};

/// The text fixture on the page of `scheme`, as BGRX rows: glyph cells of
/// 1 px strokes with antialiased edges and link runs inside a margin of bare
/// page, then the colour bars.
pub(crate) fn text_fixture(scheme: Scheme) -> Vec<u8> {
    let (width, height) = FIXTURE;
    let mut seed = 0x2545_f491u32;
    let mut random = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    let [red, green, blue] = scheme.page;
    let mut pixels: Vec<u8> = (0..width * height)
        .flat_map(|_| [blue, green, red, 0])
        .collect();
    let text_rows = height - BARS_HEIGHT;
    for cell_y in (MARGIN..text_rows - MARGIN).step_by(16) {
        for cell_x in (MARGIN..width - MARGIN).step_by(8) {
            let link = cell_x % 96 < 32 && cell_y % 48 == 16;
            let ink = if link { scheme.link } else { scheme.ink };
            for stroke in 0..4 {
                let horizontal = random() % 2 == 0;
                let at = random() as usize;
                for step in 0..6 {
                    let (x, y) = if horizontal {
                        (cell_x + 1 + step, cell_y + 3 + at % 10)
                    } else {
                        (cell_x + 1 + at % 6, cell_y + 3 + step + stroke)
                    };
                    let offset = (y * width + x) * 4;
                    let weight = if step == 0 || step == 5 { 128u16 } else { 255 };
                    for (channel, value) in [(2, ink[0]), (1, ink[1]), (0, ink[2])] {
                        let background = u16::from(pixels[offset + channel]);
                        pixels[offset + channel] =
                            ((u16::from(value) * weight + background * (255 - weight)) / 255) as u8;
                    }
                }
            }
        }
    }
    for y in text_rows..height {
        for x in 0..width {
            let [r, g, b] = BARS[x * BARS.len() / width];
            let offset = (y * width + x) * 4;
            pixels[offset..offset + 4].copy_from_slice(&[b, g, r, 0]);
        }
    }
    pixels
}

/// A fixture converted for `codec` and encoded as one key unit at the still
/// target (quantizer 8), with the stream's codec string.
pub(crate) fn still_unit(codec: VideoCodec, source: &[u8]) -> (Planar, EncodedUnit, String) {
    let (width, height) = FIXTURE;
    let mut picture = Planar::new(codec.chroma(), width as u32, height as u32);
    assert!(picture.convert(source, width * 4, FIXTURE, (0, height)));
    let mut encoder = AomEncoder::new(codec, width as u32, height as u32, 2).unwrap();
    let request = EncodeRequest {
        key: true,
        quantizer: 8,
        ..Default::default()
    };
    let unit = encoder.encode(&picture.picture(), request).unwrap();
    (picture, unit, encoder.codec_string())
}

/// The pixels inside the bars, 4 px from their sides and 8 px from their
/// top and bottom: flat colour with no edge near for the codec to ring from.
fn bar_insides() -> impl Iterator<Item = (usize, usize)> {
    let (width, height) = FIXTURE;
    let bar = width / BARS.len();
    (height - BARS_HEIGHT + 8..height - 8).flat_map(move |y| {
        (0..width)
            .filter(move |x| (4..bar - 4).contains(&(x % bar)))
            .map(move |x| (x, y))
    })
}

/// How a painted fixture measured against what was drawn.
#[derive(Debug)]
pub(crate) struct Painted {
    /// RGB PSNR over the text block.
    pub psnr: f64,
    /// Each bar's largest channel error inside it.
    pub bars: [u8; 8],
}

/// Checks a painted fixture (`painted`: four bytes a pixel, red first)
/// against the drawn one in `scheme` (`source`, BGRX): the bare page exactly
/// white or black, the white and black bars exact, every other bar within
/// one code value (the rounding of 8-bit Y′CbCr), and the text block at
/// least 44 dB RGB PSNR at 4:4:4 and 30 at 4:2:0, where a wrong stride,
/// matrix or range costs tens of dB.
pub(crate) fn check_painted(
    case: &str,
    scheme: Scheme,
    chroma: Chroma,
    source: &[u8],
    painted: &[u8],
) -> Painted {
    let (width, height) = FIXTURE;
    let drawn_at = |x: usize, y: usize| {
        let at = (y * width + x) * 4;
        [source[at + 2], source[at + 1], source[at]]
    };
    let painted_at = |x: usize, y: usize| {
        let at = (y * width + x) * 4;
        [painted[at], painted[at + 1], painted[at + 2]]
    };
    let text_rows = height - BARS_HEIGHT;
    let (right, bottom) = (width - MARGIN, text_rows - MARGIN);
    for y in 0..=text_rows - RINGING {
        for x in 0..width {
            let dx = MARGIN.saturating_sub(x).max((x + 1).saturating_sub(right));
            let dy = MARGIN.saturating_sub(y).max((y + 1).saturating_sub(bottom));
            if dx.max(dy) >= RINGING {
                assert_eq!(painted_at(x, y), scheme.page, "{case}: page at {x},{y}");
            }
        }
    }
    let mut bars = [0u8; 8];
    for (x, y) in bar_insides() {
        let (drawn, got) = (drawn_at(x, y), painted_at(x, y));
        let error = (0..3)
            .map(|channel| got[channel].abs_diff(drawn[channel]))
            .max()
            .unwrap();
        let allowed = u8::from(drawn != [255; 3] && drawn != [0; 3]);
        assert!(error <= allowed, "{case}: bar {drawn:?} painted {got:?}");
        let bar = &mut bars[x * BARS.len() / width];
        *bar = (*bar).max(error);
    }
    let (mut error, mut samples) = (0f64, 0f64);
    for (x, y) in (MARGIN..bottom).flat_map(|y| (MARGIN..right).map(move |x| (x, y))) {
        let (drawn, got) = (drawn_at(x, y), painted_at(x, y));
        for channel in 0..3 {
            error += (f64::from(drawn[channel]) - f64::from(got[channel])).powi(2);
            samples += 1.0;
        }
    }
    let psnr = 10.0 * (255.0 * 255.0 / (error / samples).max(1e-9)).log10();
    let floor = match chroma {
        Chroma::Full => 44.0,
        Chroma::Subsampled => 30.0,
    };
    assert!(
        psnr >= floor,
        "{case}: text RGB PSNR {psnr:.2} dB below {floor}"
    );
    Painted { psnr, bars }
}

pub(crate) struct Decoded {
    pub chroma: Chroma,
    /// The colour the bitstream signals, as the decoder read it.
    pub colour: Colour,
    pub width: u32,
    pub height: u32,
    pub planes: [Vec<u8>; 3],
}

impl Decoded {
    /// RGB, four bytes per pixel (the fourth unused), through the exact
    /// inverse of the matrix and range the bitstream signals, as a decoder
    /// that honours the signal paints it.
    pub(crate) fn rgb(&self) -> Vec<u8> {
        let (width, height) = (self.width as usize, self.height as usize);
        let chroma_width = match self.chroma {
            Chroma::Full => width,
            Chroma::Subsampled => width / 2,
        };
        let mut rgb = vec![0u8; width * height * 4];
        for y in 0..height {
            for x in 0..width {
                let (cx, cy) = match self.chroma {
                    Chroma::Full => (x, y),
                    Chroma::Subsampled => (x / 2, y / 2),
                };
                let [r, g, b] = to_rgb(
                    self.colour,
                    self.planes[0][y * width + x],
                    self.planes[1][cy * chroma_width + cx],
                    self.planes[2][cy * chroma_width + cx],
                );
                rgb[(y * width + x) * 4..(y * width + x) * 4 + 3].copy_from_slice(&[r, g, b]);
            }
        }
        rgb
    }
}

/// libaom's own AV1 decoder, through the same library, for proofs only.
pub(crate) struct Decoder {
    _library: Library,
    context: Box<Context>,
    decode: unsafe extern "C" fn(*mut Context, *const u8, usize, *mut c_void) -> c_int,
    frame: unsafe extern "C" fn(*mut Context, *mut *const c_void) -> *const Image,
    destroy: Destroy,
}

impl Decoder {
    pub(crate) fn new() -> Self {
        #[repr(C)]
        struct DecoderConfig {
            threads: c_uint,
            width: c_uint,
            height: c_uint,
            allow_low_bitdepth: c_uint,
        }
        type DecoderInit = unsafe extern "C" fn(
            *mut Context,
            *const c_void,
            *const DecoderConfig,
            c_long,
            c_int,
        ) -> c_int;
        let library = Library::open(SONAME).unwrap();
        // SAFETY: the declarations of the pinned aom decoder headers.
        unsafe {
            let interface: Interface = library.function(c"aom_codec_av1_dx").unwrap();
            let init: DecoderInit = library.function(c"aom_codec_dec_init_ver").unwrap();
            let mut context = Context::new();
            let config = DecoderConfig {
                threads: 1,
                width: 0,
                height: 0,
                allow_low_bitdepth: 1,
            };
            // AOM_DECODER_ABI_VERSION of the pinned headers.
            assert_eq!(init(&mut *context, interface(), &config, 0, 22), CODEC_OK);
            Self {
                decode: library.function(c"aom_codec_decode").unwrap(),
                frame: library.function(c"aom_codec_get_frame").unwrap(),
                destroy: library.function(c"aom_codec_destroy").unwrap(),
                context,
                _library: library,
            }
        }
    }

    pub(crate) fn decode(&mut self, data: &[u8]) -> Decoded {
        // SAFETY: an initialized decoder and a unit it may read.
        let status = unsafe {
            (self.decode)(
                &mut *self.context,
                data.as_ptr(),
                data.len(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(status, CODEC_OK);
        let mut iterator: *const c_void = std::ptr::null();
        // SAFETY: the decoder owns the returned image until the next call.
        let image = unsafe { (self.frame)(&mut *self.context, &mut iterator).as_ref() }
            .expect("one decoded picture");
        let chroma = match image.format {
            IMG_FMT_I444 => Chroma::Full,
            IMG_FMT_I420 => Chroma::Subsampled,
            other => panic!("decoded format {other}"),
        };
        let (width, height) = (image.display_width, image.display_height);
        let plane = |index: usize, plane_width: usize, plane_height: usize| {
            let stride = image.strides[index] as usize;
            (0..plane_height)
                .flat_map(|row| {
                    // SAFETY: the decoder's plane holds plane_height rows of
                    // at least plane_width bytes, stride bytes apart.
                    unsafe {
                        std::slice::from_raw_parts(
                            image.planes[index].add(row * stride),
                            plane_width,
                        )
                    }
                    .to_vec()
                })
                .collect::<Vec<u8>>()
        };
        let (chroma_width, chroma_height) = match chroma {
            Chroma::Full => (width as usize, height as usize),
            Chroma::Subsampled => (width as usize / 2, height as usize / 2),
        };
        let code = |value: c_uint| u8::try_from(value).expect("an H.273 code point");
        Decoded {
            chroma,
            colour: Colour {
                primaries: code(image.primaries),
                transfer: code(image.transfer),
                matrix: code(image.matrix),
                full_range: image.range == 1,
            },
            width,
            height,
            planes: [
                plane(0, width as usize, height as usize),
                plane(1, chroma_width, chroma_height),
                plane(2, chroma_width, chroma_height),
            ],
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: initialized in `new`, destroyed once.
        unsafe { (self.destroy)(&mut *self.context) };
    }
}

#[derive(Debug)]
struct SequenceHeader {
    profile: u8,
    level: u8,
    tier: u8,
    primaries: Option<u8>,
    transfer: Option<u8>,
    matrix: Option<u8>,
    full_range: bool,
    subsampling: (u8, u8),
}

struct Bits<'a> {
    data: &'a [u8],
    position: usize,
}

impl Bits<'_> {
    fn read(&mut self, count: usize) -> u32 {
        let mut value = 0;
        for _ in 0..count {
            let byte = self.data[self.position / 8];
            value = (value << 1) | u32::from((byte >> (7 - self.position % 8)) & 1);
            self.position += 1;
        }
        value
    }
}

fn leb128(data: &[u8], at: &mut usize) -> usize {
    let mut value = 0usize;
    for index in 0..8 {
        let byte = data[*at];
        *at += 1;
        value |= usize::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            break;
        }
    }
    value
}

/// The first sequence header OBU of a temporal unit (AV1 specification
/// 5.5), read as far as its colour configuration.
fn sequence_header(unit: &[u8]) -> Option<SequenceHeader> {
    let mut at = 0;
    while at < unit.len() {
        let header = unit[at];
        at += 1;
        let kind = (header >> 3) & 0x0f;
        if header & 0x04 != 0 {
            at += 1;
        }
        let size = if header & 0x02 != 0 {
            leb128(unit, &mut at)
        } else {
            unit.len() - at
        };
        let payload = &unit[at..at + size];
        at += size;
        if kind != 1 {
            continue;
        }
        let mut bits = Bits {
            data: payload,
            position: 0,
        };
        let profile = bits.read(3) as u8;
        let _still = bits.read(1);
        let reduced = bits.read(1) == 1;
        let (level, tier);
        if reduced {
            level = bits.read(5) as u8;
            tier = 0;
        } else {
            let timing = bits.read(1) == 1;
            let mut decoder_model = false;
            let mut buffer_delay_length = 0;
            if timing {
                bits.read(32);
                bits.read(32);
                if bits.read(1) == 1 {
                    let mut zeros = 0;
                    while bits.read(1) == 0 {
                        zeros += 1;
                    }
                    bits.read(zeros);
                }
                decoder_model = bits.read(1) == 1;
                if decoder_model {
                    buffer_delay_length = bits.read(5) as usize + 1;
                    bits.read(32);
                    bits.read(5);
                    bits.read(5);
                }
            }
            let initial_display_delay = bits.read(1) == 1;
            let points = bits.read(5) + 1;
            let mut first = None;
            for _ in 0..points {
                bits.read(12);
                let point_level = bits.read(5) as u8;
                let point_tier = if point_level > 7 {
                    bits.read(1) as u8
                } else {
                    0
                };
                if decoder_model && bits.read(1) == 1 {
                    bits.read(buffer_delay_length);
                    bits.read(buffer_delay_length);
                    bits.read(1);
                }
                if initial_display_delay && bits.read(1) == 1 {
                    bits.read(4);
                }
                first.get_or_insert((point_level, point_tier));
            }
            (level, tier) = first?;
        }
        let width_bits = bits.read(4) as usize + 1;
        let height_bits = bits.read(4) as usize + 1;
        bits.read(width_bits);
        bits.read(height_bits);
        let frame_ids = !reduced && bits.read(1) == 1;
        if frame_ids {
            bits.read(4);
            bits.read(3);
        }
        bits.read(3); // 128x128 superblocks, filter intra, intra edge filter
        if !reduced {
            bits.read(4); // interintra, masked compound, warped motion, dual filter
            let order_hint = bits.read(1) == 1;
            if order_hint {
                bits.read(2);
            }
            let screen_tools = if bits.read(1) == 1 { 2 } else { bits.read(1) };
            if screen_tools > 0 && bits.read(1) == 0 {
                bits.read(1);
            }
            if order_hint {
                bits.read(3);
            }
        }
        bits.read(3); // superres, cdef, restoration
        let high_bitdepth = bits.read(1) == 1;
        if profile == 2 && high_bitdepth {
            bits.read(1);
        }
        let monochrome = profile != 1 && bits.read(1) == 1;
        let described = bits.read(1) == 1;
        let (primaries, transfer, matrix) = if described {
            (
                Some(bits.read(8) as u8),
                Some(bits.read(8) as u8),
                Some(bits.read(8) as u8),
            )
        } else {
            (None, None, None)
        };
        let full_range;
        let subsampling;
        if monochrome {
            full_range = bits.read(1) == 1;
            subsampling = (1, 1);
        } else if (primaries, transfer, matrix) == (Some(1), Some(13), Some(0)) {
            full_range = true;
            subsampling = (0, 0);
        } else {
            full_range = bits.read(1) == 1;
            subsampling = match profile {
                0 => (1, 1),
                1 => (0, 0),
                _ => (bits.read(1) as u8, 0),
            };
        }
        return Some(SequenceHeader {
            profile,
            level,
            tier,
            primaries,
            transfer,
            matrix,
            full_range,
            subsampling,
        });
    }
    None
}
