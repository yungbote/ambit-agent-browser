//! Measurement harness for the encoder as the producer runs it (ignored;
//! run by hand on the machine being measured):
//!
//! ```sh
//! VIDEO_BENCH_PAGES=<dir with pageA.png pageB.png> VIDEO_BENCH_OUT=<file.json> \
//!   cargo test --release --bin agent-browser -- --ignored --nocapture native::video::bench
//! ```
//!
//! Sequences follow the design memo's bench at the probe's surface
//! (1840x1888): a dense article scrolled 40 px per picture, a 40x40 change
//! moving 4 px per picture, and two pages alternating every 30 pictures.
//! Every picture goes through the production conversion (`convert`) and the
//! production encoder (`aom`); quality is measured on RGB against the source
//! pixels through libaom's own decoder and the inverse matrix.

use super::aom::AomEncoder;
use super::convert::{to_rgb, Planar, COLOUR};
use super::{open, Chroma, EncodeRequest, VideoCodec, VideoEncoder};
use serde_json::json;
use std::time::Instant;

const WIDTH: usize = 1840;
const HEIGHT: usize = 1888;
const STEP: usize = 40;

struct Page {
    width: usize,
    height: usize,
    bgrx: Vec<u8>,
}

impl Page {
    fn load(path: &std::path::Path) -> Page {
        let image = image::open(path).unwrap().to_rgba8();
        let (width, height) = (image.width() as usize, image.height() as usize);
        let bgrx = image
            .pixels()
            .flat_map(|pixel| [pixel[2], pixel[1], pixel[0], 0])
            .collect();
        Page {
            width,
            height,
            bgrx,
        }
    }

    /// A WIDTH x HEIGHT window of the page at `top`, as BGRX rows.
    fn window(&self, top: usize) -> Vec<u8> {
        assert!(
            top + HEIGHT <= self.height && WIDTH <= self.width,
            "page too small"
        );
        let stride = self.width * 4;
        (top..top + HEIGHT)
            .flat_map(|row| {
                self.bgrx[row * stride..row * stride + WIDTH * 4]
                    .iter()
                    .copied()
            })
            .collect()
    }
}

fn cpu_seconds() -> f64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: getrusage fills one rusage.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    let seconds = |time: libc::timeval| time.tv_sec as f64 + time.tv_usec as f64 / 1e6;
    seconds(usage.ru_utime) + seconds(usage.ru_stime)
}

fn percentile(values: &[f64], quantile: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[((sorted.len() - 1) as f64 * quantile).round() as usize]
}

fn rgb_psnr(source: &[u8], picture: &Planar) -> f64 {
    let planes = picture.picture().planes().unwrap();
    let chroma_width = planes[1].1;
    let subsampled = picture.picture().chroma == Chroma::Subsampled;
    let mut error = 0f64;
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let (cx, cy) = if subsampled { (x / 2, y / 2) } else { (x, y) };
            let rgb = to_rgb(
                COLOUR,
                planes[0].0[y * WIDTH + x],
                planes[1].0[cy * chroma_width + cx],
                planes[2].0[cy * chroma_width + cx],
            );
            let at = (y * WIDTH + x) * 4;
            for (channel, value) in [(2, rgb[0]), (1, rgb[1]), (0, rgb[2])] {
                let difference = f64::from(source[at + channel]) - f64::from(value);
                error += difference * difference;
            }
        }
    }
    let mse = error / (WIDTH * HEIGHT * 3) as f64;
    10.0 * (65025.0 / mse.max(1e-9)).log10()
}

/// A 640x400 region of dense text and links, for looking at.
const CROP: (usize, usize, usize, usize) = (40, 120, 640, 400);

fn crop(picture: &Planar, path: &std::path::Path) {
    let planes = picture.picture().planes().unwrap();
    let chroma_width = planes[1].1;
    let subsampled = picture.picture().chroma == Chroma::Subsampled;
    let (left, top, width, height) = CROP;
    let mut image = image::RgbImage::new(width as u32, height as u32);
    for y in 0..height {
        for x in 0..width {
            let (px, py) = (left + x, top + y);
            let (cx, cy) = if subsampled {
                (px / 2, py / 2)
            } else {
                (px, py)
            };
            let rgb = to_rgb(
                COLOUR,
                planes[0].0[py * WIDTH + px],
                planes[1].0[cy * chroma_width + cx],
                planes[2].0[cy * chroma_width + cx],
            );
            image.put_pixel(x as u32, y as u32, image::Rgb(rgb));
        }
    }
    image.save(path).unwrap();
}

fn crop_source(source: &[u8], path: &std::path::Path) {
    let (left, top, width, height) = CROP;
    let mut image = image::RgbImage::new(width as u32, height as u32);
    for y in 0..height {
        for x in 0..width {
            let at = ((top + y) * WIDTH + left + x) * 4;
            image.put_pixel(
                x as u32,
                y as u32,
                image::Rgb([source[at + 2], source[at + 1], source[at]]),
            );
        }
    }
    image.save(path).unwrap();
}

/// The decoded picture of a unit, as a `Planar` of the stream's chroma.
fn decode(decoder: &mut super::aom::tests::Decoder, data: &[u8], chroma: Chroma) -> Planar {
    let decoded = decoder.decode(data);
    let mut planar = Planar::new(chroma, decoded.width, decoded.height);
    planar.replace(&decoded.planes);
    planar
}

/// The production encoder plus `VIDEO_BENCH_CONTROLS` (`id=value,...`).
fn tuned(codec: VideoCodec, threads: u32) -> AomEncoder {
    let mut encoder = AomEncoder::new(codec, WIDTH as u32, HEIGHT as u32, threads).unwrap();
    for pair in std::env::var("VIDEO_BENCH_CONTROLS")
        .unwrap_or_default()
        .split(',')
        .filter(|pair| !pair.is_empty())
    {
        let (id, value) = pair.split_once('=').unwrap();
        encoder
            .tune(id.parse().unwrap(), value.parse().unwrap())
            .unwrap();
    }
    encoder
}

fn sequences() -> Vec<String> {
    std::env::var("VIDEO_BENCH_SEQUENCES")
        .unwrap_or_else(|_| "scroll,small,navigate,still".into())
        .split(',')
        .map(String::from)
        .collect()
}

/// What a whole-page change costs: page A's text replaced by page B, as a
/// P-frame and as a key unit, at `VIDEO_BENCH_CONTROLS` and quantizers.
#[test]
#[ignore = "measurement harness: needs the rendered pages (VIDEO_BENCH_PAGES)"]
fn a_whole_page_change() {
    let pages = std::path::PathBuf::from(std::env::var_os("VIDEO_BENCH_PAGES").unwrap());
    let page_a = Page::load(&pages.join("pageA.png"));
    let page_b = Page::load(&pages.join("pageB.png"));
    for codec in [VideoCodec::Av1Full] {
        for quantizer in [32u8, 40, 48, 56] {
            let mut encoder = tuned(codec, 4);
            let mut decoder = super::aom::tests::Decoder::new();
            let mut picture = Planar::new(codec.chroma(), WIDTH as u32, HEIGHT as u32);
            let mut times = Vec::new();
            for (index, page) in [&page_b, &page_a, &page_b, &page_a, &page_b]
                .into_iter()
                .enumerate()
            {
                assert!(picture.convert(&page.window(0), WIDTH * 4, (WIDTH, HEIGHT), (0, HEIGHT)));
                let started = Instant::now();
                let unit = encoder
                    .encode(
                        &picture.picture(),
                        EncodeRequest {
                            key: index == 0,
                            quantizer,
                            ..Default::default()
                        },
                    )
                    .unwrap();
                let elapsed = started.elapsed().as_secs_f64() * 1000.0;
                let decoded = decode(&mut decoder, &unit.data, codec.chroma());
                times.push((
                    elapsed,
                    unit.data.len(),
                    rgb_psnr(&page.window(0), &decoded),
                ));
            }
            println!(
                "PAGE_CHANGE q{quantizer} controls[{}] key {:.0}ms {}KB {:.2}dB, changes {:?}",
                std::env::var("VIDEO_BENCH_CONTROLS").unwrap_or_default(),
                times[0].0,
                times[0].1 / 1024,
                times[0].2,
                times[1..]
                    .iter()
                    .map(|(ms, bytes, psnr)| format!("{ms:.0}ms {}KB {psnr:.2}dB", bytes / 1024))
                    .collect::<Vec<_>>()
            );
        }
    }
}

#[test]
#[ignore = "measurement harness: needs the rendered pages (VIDEO_BENCH_PAGES)"]
fn encoder_on_screen_content_at_the_probe_surface() {
    let pages = std::path::PathBuf::from(std::env::var_os("VIDEO_BENCH_PAGES").unwrap());
    let page_a = Page::load(&pages.join("pageA.png"));
    let page_b = Page::load(&pages.join("pageB.png"));
    let frames: usize = std::env::var("VIDEO_BENCH_FRAMES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(240);
    let threads: Vec<u32> = std::env::var("VIDEO_BENCH_THREADS")
        .unwrap_or_else(|_| "1,2,4".into())
        .split(',')
        .map(|value| value.parse().unwrap())
        .collect();
    let motion_quantizer: u8 = 32;
    let mut results = Vec::new();
    let codecs: Vec<VideoCodec> = std::env::var("VIDEO_BENCH_CODECS")
        .unwrap_or_else(|_| "av1-444,av1".into())
        .split(',')
        .map(|token| VideoCodec::parse(token).unwrap())
        .collect();
    for codec in codecs {
        let chroma = codec.chroma();
        for sequence in ["scroll", "small", "navigate"] {
            let pictures: Vec<Vec<u8>> = (0..frames)
                .map(|index| match sequence {
                    "scroll" => page_a.window(index * STEP),
                    "small" => {
                        let mut window = page_a.window(1200);
                        let left = 200 + (index * 4) % 800;
                        for y in 400..440 {
                            for x in left..left + 40 {
                                let at = (y * WIDTH + x) * 4;
                                window[at..at + 4].copy_from_slice(&[0x20, 0x20, 0xd0, 0]);
                            }
                        }
                        window
                    }
                    _ if index % 60 < 30 => page_b.window(0),
                    _ => page_a.window(0),
                })
                .collect();
            if !sequences().iter().any(|name| name == sequence) {
                continue;
            }
            for &count in &threads {
                let mut encoder = tuned(codec, count);
                let mut planar = Planar::new(chroma, WIDTH as u32, HEIGHT as u32);
                let (mut encode_ms, mut convert_ms, mut cpu_ms, mut bytes) =
                    (Vec::new(), Vec::new(), Vec::new(), Vec::new());
                let mut key_bytes = 0;
                for (index, source) in pictures.iter().enumerate() {
                    let started = Instant::now();
                    assert!(planar.convert(source, WIDTH * 4, (WIDTH, HEIGHT), (0, HEIGHT)));
                    convert_ms.push(started.elapsed().as_secs_f64() * 1000.0);
                    let cpu = cpu_seconds();
                    let started = Instant::now();
                    let unit = encoder
                        .encode(
                            &planar.picture(),
                            EncodeRequest {
                                key: index == 0,
                                quantizer: motion_quantizer,
                                ..Default::default()
                            },
                        )
                        .unwrap();
                    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
                    if index == 0 {
                        key_bytes = unit.data.len();
                    } else if index >= 5 {
                        encode_ms.push(elapsed);
                        cpu_ms.push((cpu_seconds() - cpu) * 1000.0);
                        bytes.push(unit.data.len() as f64);
                    }
                }
                let entry = json!({"codec": codec.token(), "sequence": sequence, "threads": count,
                    "frames": frames, "quantizer": motion_quantizer,
                    "encodeMsP50": percentile(&encode_ms, 0.5), "encodeMsP95": percentile(&encode_ms, 0.95),
                    "encodeMsMax": percentile(&encode_ms, 1.0), "cpuMsP50": percentile(&cpu_ms, 0.5),
                    "cpuMsMean": cpu_ms.iter().sum::<f64>() / cpu_ms.len() as f64,
                    "convertFullFrameMsP50": percentile(&convert_ms, 0.5),
                    "keyBytes": key_bytes, "bytesMean": bytes.iter().sum::<f64>() / bytes.len() as f64,
                    "bytesP95": percentile(&bytes, 0.95), "bytesMax": percentile(&bytes, 1.0)});
                println!("VIDEO_BENCH {entry}");
                results.push(entry);
            }
        }
        if !sequences().iter().any(|sequence| sequence == "still") {
            continue;
        }
        // The still target: a picture that stopped moving, first as the key
        // unit a new viewer gets, then as a motion picture refined in place.
        let still = page_a.window(2000);
        if let Some(directory) = std::env::var_os("VIDEO_BENCH_CROPS") {
            std::fs::create_dir_all(&directory).unwrap();
            crop_source(&still, &std::path::Path::new(&directory).join("source.png"));
        }
        let mut planar = Planar::new(chroma, WIDTH as u32, HEIGHT as u32);
        assert!(planar.convert(&still, WIDTH * 4, (WIDTH, HEIGHT), (0, HEIGHT)));
        let mut decoder = super::aom::tests::Decoder::new();
        for quantizer in [16u8, 24, 32, 40, 48] {
            let mut encoder = open(codec, WIDTH as u32, HEIGHT as u32, 4).unwrap();
            let unit = encoder
                .encode(
                    &planar.picture(),
                    EncodeRequest {
                        key: true,
                        quantizer,
                        ..Default::default()
                    },
                )
                .unwrap();
            let decoded = decode(&mut decoder, &unit.data, chroma);
            let entry = json!({"codec": codec.token(), "kind": "key", "quantizer": quantizer,
                "bytes": unit.data.len(), "psnrRgb": rgb_psnr(&still, &decoded)});
            println!("VIDEO_BENCH {entry}");
            results.push(entry);
        }
        // Motion: the previous scroll position, then the still picture at the
        // motion quantizer, then refinements of the same capture.
        let ladders: [&[u8]; 6] = [
            &[32, 20, 12],
            &[32, 16, 8],
            &[32, 12, 4],
            &[40, 16, 8],
            &[32, 8],
            &[32, 8, 8],
        ];
        for ladder in ladders {
            let mut decoder = super::aom::tests::Decoder::new();
            let mut encoder = open(codec, WIDTH as u32, HEIGHT as u32, 4).unwrap();
            let mut before = Planar::new(chroma, WIDTH as u32, HEIGHT as u32);
            assert!(before.convert(
                &page_a.window(2000 - STEP),
                WIDTH * 4,
                (WIDTH, HEIGHT),
                (0, HEIGHT)
            ));
            let unit = encoder
                .encode(
                    &before.picture(),
                    EncodeRequest {
                        key: true,
                        quantizer: ladder[0],
                        ..Default::default()
                    },
                )
                .unwrap();
            decoder.decode(&unit.data);
            let mut steps = Vec::new();
            for (step, quantizer) in ladder.iter().enumerate() {
                let started = Instant::now();
                let unit = encoder
                    .encode(
                        &planar.picture(),
                        EncodeRequest {
                            key: false,
                            quantizer: *quantizer,
                            ..Default::default()
                        },
                    )
                    .unwrap();
                let encode_ms = started.elapsed().as_secs_f64() * 1000.0;
                let decoded = decode(&mut decoder, &unit.data, chroma);
                steps.push(
                    json!({"step": step, "quantizer": quantizer, "bytes": unit.data.len(),
                    "encodeMs": encode_ms, "psnrRgb": rgb_psnr(&still, &decoded)}),
                );
                if let Some(directory) = std::env::var_os("VIDEO_BENCH_CROPS") {
                    crop(
                        &decoded,
                        &std::path::Path::new(&directory).join(format!(
                            "{}-ladder{}-q{quantizer}.png",
                            codec.token(),
                            ladder
                                .iter()
                                .map(|q| q.to_string())
                                .collect::<Vec<_>>()
                                .join("-")
                        )),
                    );
                }
            }
            let entry =
                json!({"codec": codec.token(), "kind": "refine", "ladder": ladder, "steps": steps});
            println!("VIDEO_BENCH {entry}");
            results.push(entry);
        }
    }
    if let Some(out) = std::env::var_os("VIDEO_BENCH_OUT") {
        std::fs::write(out, serde_json::to_vec_pretty(&results).unwrap()).unwrap();
    }
}

/// The refinement of one captured window (`VIDEO_BENCH_STILL`, a 1840x1888
/// PNG of the screen) after `VIDEO_BENCH_HISTORY` pictures of motion scrolled
/// 40 px apart (a list, default `1,30,120`), all at the motion speed; the
/// refinements at the motion speed and at the refinement speed, one pass and
/// a second.
#[test]
#[ignore = "measurement harness: needs a captured window (VIDEO_BENCH_STILL)"]
fn a_refinement_after_a_scroll() {
    let still = Page::load(std::path::Path::new(
        &std::env::var_os("VIDEO_BENCH_STILL").unwrap(),
    ));
    let source = still.window(0);
    let shifted = |by: usize| -> Vec<u8> {
        let (stride, by) = (WIDTH * 4, by % HEIGHT);
        let mut shifted = vec![255u8; source.len()];
        shifted[..(HEIGHT - by) * stride].copy_from_slice(&source[by * stride..]);
        shifted
    };
    let codec = VideoCodec::Av1Full;
    let mut results = Vec::new();
    for history in std::env::var("VIDEO_BENCH_HISTORY")
        .unwrap_or_else(|_| "1,30,120".into())
        .split(',')
        .map(|value| value.parse::<usize>().unwrap())
    {
        for refine in [false, true] {
            let mut decoder = super::aom::tests::Decoder::new();
            let mut encoder = tuned(codec, 4);
            let mut picture = Planar::new(codec.chroma(), WIDTH as u32, HEIGHT as u32);
            for step in (0..=history).rev() {
                let window = if step == 0 {
                    source.clone()
                } else {
                    shifted(step * STEP)
                };
                assert!(picture.convert(&window, WIDTH * 4, (WIDTH, HEIGHT), (0, HEIGHT)));
                let request = EncodeRequest {
                    key: step == history,
                    quantizer: 32,
                    ..Default::default()
                };
                decoder.decode(&encoder.encode(&picture.picture(), request).unwrap().data);
            }
            let mut passes = Vec::new();
            for _ in 0..2 {
                let started = Instant::now();
                let request = EncodeRequest {
                    quantizer: 8,
                    refine,
                    ..Default::default()
                };
                let unit = encoder.encode(&picture.picture(), request).unwrap();
                let encode_ms = started.elapsed().as_secs_f64() * 1000.0;
                let decoded = decode(&mut decoder, &unit.data, codec.chroma());
                passes.push(json!({"bytes": unit.data.len(), "encodeMs": encode_ms,
                    "psnrRgb": rgb_psnr(&source, &decoded)}));
            }
            let entry = json!({"history": history, "refinementSpeed": refine, "passes": passes});
            println!("VIDEO_BENCH {entry}");
            results.push(entry);
        }
    }
    if let Some(out) = std::env::var_os("VIDEO_BENCH_OUT") {
        std::fs::write(out, serde_json::to_vec_pretty(&results).unwrap()).unwrap();
    }
}

/// A synthetic page of text: dark glyph strokes in lines on white, BGRX.
fn text_page(width: usize, height: usize) -> Vec<u8> {
    let mut source = vec![255u8; width * height * 4];
    let mut seed = 0x2545_f491u32;
    for line in (140..height.saturating_sub(40)).step_by(44) {
        for cell in (48..width.saturating_sub(64)).step_by(18) {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            if seed.is_multiple_of(7) {
                continue;
            }
            for y in line..line + 30 {
                for x in cell..cell + 14 {
                    if (seed >> ((x - cell + y - line) % 24)) & 1 == 1 {
                        source[(y * width + x) * 4..][..3].fill(32);
                    }
                }
            }
        }
    }
    source
}

/// One typed key's encode cost by the coded picture's size, whether the
/// unit codes only the changed rows, and the encoder's threads, on a
/// synthetic page of text: what a unit of motion costs that does not scale
/// with what changed. `VIDEO_BENCH_OUT` names a file for the results.
#[test]
#[ignore = "measurement harness"]
fn a_typed_key_by_coded_size_region_and_threads() {
    let mut results = Vec::new();
    for (width, height) in [(2048usize, 2048usize), (1856, 1920), (2816, 2048)] {
        for threads in [4u32, 8] {
            for regional in [false, true] {
                let mut encoder =
                    AomEncoder::new(VideoCodec::Av1Full, width as u32, height as u32, threads)
                        .unwrap();
                let mut source = text_page(width, height);
                let mut planar = Planar::new(Chroma::Full, width as u32, height as u32);
                assert!(planar.convert(&source, width * 4, (width, height), (0, height)));
                let request = |key| EncodeRequest {
                    key,
                    quantizer: 32,
                    refine: false,
                };
                encoder.encode(&planar.picture(), request(true)).unwrap();
                let (top, bottom) = (16usize, 112usize);
                let (mut wall, cpu_before) = (Vec::new(), cpu_seconds());
                for key in 0..40usize {
                    let left = 48 + key * 40;
                    for y in 24..104 {
                        for x in left..left + 28 {
                            source[(y * width + x) * 4..][..3].fill(if (x + y) % 5 == 0 {
                                0
                            } else {
                                40
                            });
                        }
                    }
                    assert!(planar.convert(&source, width * 4, (width, height), (top, bottom)));
                    encoder
                        .set_region(regional.then_some(super::EncoderRegion {
                            x: 0,
                            y: top as u32,
                            width: width as u32,
                            height: (bottom - top) as u32,
                        }))
                        .unwrap();
                    let started = Instant::now();
                    encoder.encode(&planar.picture(), request(false)).unwrap();
                    wall.push(started.elapsed().as_secs_f64() * 1000.0);
                }
                let result = json!({
                    "coded": [width, height], "threads": threads, "regional": regional,
                    "p50Ms": percentile(&wall, 0.5), "p95Ms": percentile(&wall, 0.95),
                    "meanMs": wall.iter().sum::<f64>() / wall.len() as f64,
                    "cpuMsPerUnit": (cpu_seconds() - cpu_before) * 1000.0 / wall.len() as f64,
                });
                println!("TYPED_KEY {result}");
                results.push(result);
            }
        }
    }
    if let Some(out) = std::env::var_os("VIDEO_BENCH_OUT") {
        std::fs::write(out, serde_json::to_vec_pretty(&results).unwrap()).unwrap();
    }
}

/// A key unit's time and size by coded size and intra tools, on a synthetic
/// page of text: the encoder's own configuration, every intra predictor it
/// leaves out searched again (`aomcx.h` 98-101, 106, 141), and without the
/// palette (104) or intra block copy (105). Speed 11 and the quantizer
/// changed it little. `VIDEO_BENCH_OUT` names a file for the results.
#[test]
#[ignore = "measurement harness"]
fn a_key_unit_by_coded_size_and_intra_tools() {
    let mut results = Vec::new();
    let every_predictor: &[(i32, i32)] =
        &[(98, 1), (99, 1), (100, 1), (101, 1), (106, 1), (141, 1)];
    for (width, height) in [(2048usize, 2048usize), (2816, 2048)] {
        for (name, controls) in [
            ("encoder", &[][..]),
            ("everyPredictor", every_predictor),
            ("noPalette", &[(104, 0)][..]),
            ("noIntraBlockCopy", &[(105, 0)][..]),
        ] {
            let source = text_page(width, height);
            let mut planar = Planar::new(Chroma::Full, width as u32, height as u32);
            assert!(planar.convert(&source, width * 4, (width, height), (0, height)));
            let (mut wall, mut bytes) = (Vec::new(), 0);
            for _ in 0..2 {
                let mut encoder =
                    AomEncoder::new(VideoCodec::Av1Full, width as u32, height as u32, 4).unwrap();
                for (id, value) in controls {
                    encoder.tune(*id, *value).unwrap();
                }
                let started = Instant::now();
                let unit = encoder
                    .encode(
                        &planar.picture(),
                        EncodeRequest {
                            key: true,
                            quantizer: 32,
                            refine: false,
                        },
                    )
                    .unwrap();
                wall.push(started.elapsed().as_secs_f64() * 1000.0);
                bytes = unit.data.len();
            }
            let result = json!({
                "coded": [width, height], "tools": name, "bytes": bytes,
                "p50Ms": percentile(&wall, 0.5), "minMs": percentile(&wall, 0.0),
            });
            println!("KEY_UNIT {result}");
            results.push(result);
        }
    }
    if let Some(out) = std::env::var_os("VIDEO_BENCH_OUT") {
        std::fs::write(out, serde_json::to_vec_pretty(&results).unwrap()).unwrap();
    }
}

/// One typed key on the rendered app page, coded as the lead's exact-rect
/// candidates: the rectangle of changed pixels (found by comparing the
/// damaged rows) as a lossless PNG, the whole damaged band as a PNG, and the
/// band and the rectangle as key pictures of a small second AV1 encoder at
/// the motion and still quantizers. The glyph is the most inked 40 by 68
/// block of the dense text page (one 34 px character at device pixel ratio
/// 2). Median time of 20 and bytes; `VIDEO_BENCH_OUT` names a results file.
#[test]
#[ignore = "measurement harness: needs the rendered pages (VIDEO_BENCH_PAGES)"]
fn a_typed_key_as_an_exact_rectangle() {
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    use image::{ExtendedColorType, ImageEncoder};
    let pages = std::path::PathBuf::from(std::env::var_os("VIDEO_BENCH_PAGES").unwrap());
    let (text, app) = (
        Page::load(&pages.join("pageA.png")),
        Page::load(&pages.join("pageB.png")),
    );
    let (glyph_width, glyph_height) = (40usize, 68usize);
    let ink = |page: &Page, x: usize, y: usize| {
        (y..y + glyph_height)
            .flat_map(|row| (x..x + glyph_width).map(move |column| (row, column)))
            .filter(|(row, column)| page.bgrx[(row * page.width + column) * 4] < 128)
            .count()
    };
    let (source_x, source_y) = (0..HEIGHT - glyph_height)
        .step_by(4)
        .flat_map(|y| (0..WIDTH - glyph_width).step_by(8).map(move |x| (x, y)))
        .max_by_key(|(x, y)| ink(&text, *x, *y))
        .unwrap();
    let before = app.window(0);
    let mut after = before.clone();
    let (left, top) = (88usize, 30usize);
    for row in 0..glyph_height {
        let from = ((source_y + row) * text.width + source_x) * 4;
        let to = ((top + row) * WIDTH + left) * 4;
        after[to..to + glyph_width * 4].copy_from_slice(&text.bgrx[from..from + glyph_width * 4]);
    }
    let band = (16usize, 112usize);
    let median = |mut runs: Vec<f64>| {
        runs.sort_by(f64::total_cmp);
        runs[runs.len() / 2]
    };
    let timed = |work: &mut dyn FnMut() -> usize| {
        let mut bytes = 0;
        let runs = (0..20)
            .map(|_| {
                let started = Instant::now();
                bytes = work();
                started.elapsed().as_secs_f64() * 1000.0
            })
            .collect::<Vec<_>>();
        (median(runs), bytes)
    };
    // The exact rectangle: the bounds of every pixel that differs in the band.
    let find = || {
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
        for y in band.0..band.1 {
            let row = y * WIDTH * 4;
            for x in 0..WIDTH {
                let at = row + x * 4;
                if before[at..at + 3] != after[at..at + 3] {
                    (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1));
                }
            }
        }
        (x0, y0, x1 - x0, y1 - y0)
    };
    let (search_ms, rect) = {
        let mut rect = (0, 0, 0, 0);
        let (ms, _) = timed(&mut || {
            rect = find();
            0
        });
        (ms, rect)
    };
    let rgb = |(x, y, width, height): (usize, usize, usize, usize)| {
        (y..y + height)
            .flat_map(|row| {
                after[(row * WIDTH + x) * 4..(row * WIDTH + x + width) * 4]
                    .chunks_exact(4)
                    .flat_map(|pixel| [pixel[2], pixel[1], pixel[0]])
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<u8>>()
    };
    let png = |area: (usize, usize, usize, usize), filter: FilterType| {
        let mut out = Vec::new();
        PngEncoder::new_with_quality(&mut out, CompressionType::Fast, filter)
            .write_image(&rgb(area), area.2 as u32, area.3 as u32, ExtendedColorType::Rgb8)
            .unwrap();
        out.len()
    };
    let whole_band = (0, band.0, WIDTH, band.1 - band.0);
    let mut results = vec![json!({"rect": [rect.0, rect.1, rect.2, rect.3], "searchMs": search_ms})];
    for (name, area) in [("rect", rect), ("band", whole_band)] {
        for (filter_name, filter) in [("sub", FilterType::Sub), ("adaptive", FilterType::Adaptive)] {
            let (ms, bytes) = timed(&mut || png(area, filter));
            results.push(json!({"coding": format!("png-{filter_name}"), "area": name, "ms": ms, "bytes": bytes}));
        }
        let (width, height) = (area.2.next_multiple_of(2), area.3.next_multiple_of(2));
        let mut padded = vec![255u8; width * height * 4];
        for row in 0..area.3 {
            let from = ((area.1 + row) * WIDTH + area.0) * 4;
            padded[row * width * 4..row * width * 4 + area.2 * 4]
                .copy_from_slice(&after[from..from + area.2 * 4]);
        }
        let mut planar = Planar::new(Chroma::Full, width as u32, height as u32);
        assert!(planar.convert(&padded, width * 4, (width, height), (0, height)));
        let mut encoder =
            AomEncoder::new(VideoCodec::Av1Full, width as u32, height as u32, 2).unwrap();
        for quantizer in [32u8, 8, 0] {
            let (ms, bytes) = timed(&mut || {
                encoder
                    .encode(
                        &planar.picture(),
                        EncodeRequest {
                            key: true,
                            quantizer,
                            refine: false,
                        },
                    )
                    .unwrap()
                    .data
                    .len()
            });
            results.push(json!({"coding": format!("av1-key-q{quantizer}"), "area": name, "ms": ms, "bytes": bytes}));
        }
    }
    for result in &results {
        println!("EXACT_RECT {result}");
    }
    if let Some(out) = std::env::var_os("VIDEO_BENCH_OUT") {
        std::fs::write(out, serde_json::to_vec_pretty(&results).unwrap()).unwrap();
    }
}

/// A scroll of the dense text page (40 device px a picture) coded at the
/// window's own size rounded to 64 px and at its size class, interleaved:
/// what a coded size nearer the window saves on large damage.
/// `VIDEO_BENCH_OUT` names a results file.
#[test]
#[ignore = "measurement harness: needs the rendered pages (VIDEO_BENCH_PAGES)"]
fn a_scroll_by_coded_size() {
    let pages = std::path::PathBuf::from(std::env::var_os("VIDEO_BENCH_PAGES").unwrap());
    let text = Page::load(&pages.join("pageA.png"));
    let mut results = Vec::new();
    for round in 0..2 {
        for (width, height) in [(1856usize, 1920usize), (2048, 2048)] {
            let mut encoder =
                AomEncoder::new(VideoCodec::Av1Full, width as u32, height as u32, 4).unwrap();
            let mut planar = Planar::new(Chroma::Full, width as u32, height as u32);
            let mut wall = Vec::new();
            for picture in 0..31usize {
                let window = text.window(picture * 40);
                assert!(planar.convert_visible(
                    &window,
                    WIDTH * 4,
                    (WIDTH, HEIGHT),
                    super::EncoderRegion {
                        x: 0,
                        y: 0,
                        width: WIDTH as u32,
                        height: HEIGHT as u32
                    },
                    (0, HEIGHT)
                ));
                let started = Instant::now();
                encoder
                    .encode(
                        &planar.picture(),
                        EncodeRequest {
                            key: picture == 0,
                            quantizer: 32,
                            refine: false,
                        },
                    )
                    .unwrap();
                if picture > 0 {
                    wall.push(started.elapsed().as_secs_f64() * 1000.0);
                }
            }
            let result = json!({"round": round, "coded": [width, height],
                "p50Ms": percentile(&wall, 0.5), "p95Ms": percentile(&wall, 0.95)});
            println!("SCROLL {result}");
            results.push(result);
        }
    }
    if let Some(out) = std::env::var_os("VIDEO_BENCH_OUT") {
        std::fs::write(out, serde_json::to_vec_pretty(&results).unwrap()).unwrap();
    }
}
