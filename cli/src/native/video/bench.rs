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
use super::convert::{to_rgb, Planar};
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
                times.push((started.elapsed().as_secs_f64() * 1000.0, unit.data.len()));
            }
            println!(
                "PAGE_CHANGE q{quantizer} controls[{}] key {:.0}ms {}KB, changes {:?}",
                std::env::var("VIDEO_BENCH_CONTROLS").unwrap_or_default(),
                times[0].0,
                times[0].1 / 1024,
                times[1..]
                    .iter()
                    .map(|(ms, bytes)| format!("{ms:.0}ms {}KB", bytes / 1024))
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
