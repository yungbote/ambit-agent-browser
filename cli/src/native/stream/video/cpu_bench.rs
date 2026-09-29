//! What one dock's video costs the CPU, at the dock sizes a viewer opens:
//! the producer's conversion and encoder (its size class, quantizers,
//! speeds and 4 threads) on a page of dense text, for full-motion scrolling
//! (40 px a picture), small damage (a 40 px square moving 4 px a picture)
//! and the one refinement a still picture gets. Encode CPU is the process's
//! (every encoder thread) across each call; conversion runs on one thread.
//!
//! `VIDEO_BENCH_PAGE=<a PNG at least 1840 px wide and 5,000 tall>`
//! `VIDEO_BENCH_OUT=<file.json>`, run alone:
//! `cargo test --profile ci producer_cpu_at_the_dock_sizes -- --ignored --test-threads=1`
//!
//! `key_units_by_encoder_threads` measures what a key unit costs when the
//! encoder spends more threads and tile columns on it than on the motion
//! around it.

use std::ffi::c_int;
use std::time::Instant;

use serde_json::{json, Value};

use super::policy::{CodedSize, Quality};
use super::producer::ENCODER_THREADS;
use crate::native::video::convert::Planar;
use crate::native::video::{
    open, tile_columns_log2, AomEncoder, EncodeRequest, VideoCodec, VideoEncoder,
};

const PICTURES: usize = 240;
/// The default dock and the wide dock, in device pixels (DPR 2).
const DOCKS: [(&str, (usize, usize)); 2] = [("default", (988, 1888)), ("wide", (1840, 1888))];

/// Process CPU time (every thread), in milliseconds.
fn cpu_ms() -> f64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: getrusage fills one rusage.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    let ms = |time: libc::timeval| time.tv_sec as f64 * 1000.0 + time.tv_usec as f64 / 1000.0;
    ms(usage.ru_utime) + ms(usage.ru_stime)
}

fn stats(values: &[f64]) -> Value {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let at = |share: f64| sorted[((sorted.len() - 1) as f64 * share).round() as usize];
    let round = |value: f64| (value * 100.0).round() / 100.0;
    json!({"p50": round(at(0.5)), "p95": round(at(0.95)), "max": round(sorted[sorted.len() - 1]),
        "mean": round(sorted.iter().sum::<f64>() / sorted.len() as f64)})
}

/// The page as BGRX rows, as the display helper hands a window over.
struct Page {
    width: usize,
    height: usize,
    bgrx: Vec<u8>,
}

impl Page {
    fn load(path: &std::path::Path) -> Self {
        let image = image::open(path).unwrap().to_rgba8();
        let (width, height) = (image.width() as usize, image.height() as usize);
        let bgrx = image
            .pixels()
            .flat_map(|pixel| [pixel.0[2], pixel.0[1], pixel.0[0], 0])
            .collect();
        Self {
            width,
            height,
            bgrx,
        }
    }

    /// `width` x `height` BGRX rows from `top`; past the page's width it
    /// continues with the page 2,000 rows further down, as dense.
    fn window(&self, top: usize, width: usize, height: usize) -> Vec<u8> {
        const SIDEWAYS: usize = 2000;
        let mut rows = Vec::with_capacity(width * height * 4);
        for row in top..top + height {
            let mut x = 0;
            while x < width {
                let band = x / self.width;
                let at = ((row + band * SIDEWAYS) % self.height) * self.width * 4;
                let run = (width - x).min(self.width);
                rows.extend_from_slice(&self.bgrx[at..at + run * 4]);
                x += run;
            }
        }
        rows
    }
}

/// One sequence's pictures through the conversion and the encoder: the
/// first is a key unit and is not counted.
struct Run {
    convert_ms: Vec<f64>,
    encode_ms: Vec<f64>,
    cpu_ms: Vec<f64>,
    bytes: Vec<f64>,
}

impl Run {
    fn report(&self) -> Value {
        // What a second of pictures at the encoder's pace costs: pictures
        // come at most 60 a second, and no faster than encodes end.
        let encode_p50 = {
            let mut sorted = self.encode_ms.clone();
            sorted.sort_by(f64::total_cmp);
            sorted[sorted.len() / 2]
        };
        let rate = (1000.0 / encode_p50).min(60.0);
        let mean = |values: &[f64]| values.iter().sum::<f64>() / values.len() as f64;
        let cores = rate * (mean(&self.cpu_ms) + mean(&self.convert_ms)) / 1000.0;
        json!({"encodeMs": stats(&self.encode_ms), "encodeCpuMs": stats(&self.cpu_ms),
            "convertMs": stats(&self.convert_ms), "kib": stats(&self.bytes.iter().map(|b| b / 1024.0).collect::<Vec<_>>()),
            "picturesPerSecond": (rate * 10.0).round() / 10.0, "cores": (cores * 100.0).round() / 100.0})
    }
}

#[test]
#[ignore = "measurement harness: needs a page render (VIDEO_BENCH_PAGE)"]
fn producer_cpu_at_the_dock_sizes() {
    let page = Page::load(std::path::Path::new(
        &std::env::var_os("VIDEO_BENCH_PAGE").unwrap(),
    ));
    let codec = VideoCodec::Av1Full;
    let mut docks = serde_json::Map::new();
    for (name, (width, height)) in DOCKS {
        assert!(page.width >= width && page.height >= height + PICTURES * 40);
        let coded = CodedSize::new().fit((width as u32, height as u32), Instant::now());
        let stride = page.width * 4;
        let window = |top: usize| &page.bgrx[top * stride..(top + height) * stride];
        let run = |pictures: &mut dyn FnMut(usize) -> Vec<u8>| {
            let mut encoder = open(codec, coded.0, coded.1, ENCODER_THREADS).unwrap();
            let mut planar = Planar::new(codec.chroma(), coded.0, coded.1);
            let mut run = Run {
                convert_ms: Vec::new(),
                encode_ms: Vec::new(),
                cpu_ms: Vec::new(),
                bytes: Vec::new(),
            };
            for index in 0..=PICTURES {
                let source = pictures(index);
                let started = Instant::now();
                assert!(planar.convert(&source, stride, (width, height), (0, height)));
                let converted = started.elapsed();
                let (cpu, started) = (cpu_ms(), Instant::now());
                let unit = encoder
                    .encode(
                        &planar.picture(),
                        EncodeRequest {
                            key: index == 0,
                            quantizer: Quality::Motion.quantizer(),
                            refine: false,
                        },
                    )
                    .unwrap();
                if index > 0 {
                    run.encode_ms.push(started.elapsed().as_secs_f64() * 1000.0);
                    run.cpu_ms.push(cpu_ms() - cpu);
                    run.convert_ms.push(converted.as_secs_f64() * 1000.0);
                    run.bytes.push(unit.data.len() as f64);
                }
            }
            // The page stops: one refinement to the still target.
            let (cpu, started) = (cpu_ms(), Instant::now());
            let refined = encoder
                .encode(
                    &planar.picture(),
                    EncodeRequest {
                        key: false,
                        quantizer: Quality::Final.quantizer(),
                        refine: true,
                    },
                )
                .unwrap();
            let refinement = json!({"encodeMs": (started.elapsed().as_secs_f64() * 10_000.0).round() / 10.0,
                "cpuMs": ((cpu_ms() - cpu) * 10.0).round() / 10.0, "kib": refined.data.len() / 1024});
            (run, refinement)
        };
        let (scroll, refinement) = run(&mut |index| window(index * 40).to_vec());
        // A 40 px square of the page moving 4 px a picture over a still page.
        let still = window(0).to_vec();
        let (small, _) = run(&mut |index| {
            let mut picture = still.clone();
            let (x, y) = (200 + (4 * index) % (width - 300), 600);
            for row in y..y + 40 {
                for pixel in &mut picture[row * stride + x * 4..row * stride + (x + 40) * 4] {
                    *pixel = 255 - *pixel;
                }
            }
            picture
        });
        docks.insert(
            name.into(),
            json!({"window": [width, height], "coded": [coded.0, coded.1],
                "scroll": scroll.report(), "small": small.report(), "refinementAfterScroll": refinement}),
        );
    }
    let threads = std::thread::available_parallelism().map_or(0, |n| n.get());
    let report = json!({"codec": codec.token(), "encoderThreads": ENCODER_THREADS, "availableCpus": threads, "docks": docks});
    println!("CPU {report}");
    if let Some(out) = std::env::var_os("VIDEO_BENCH_OUT") {
        std::fs::write(out, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    }
}

/// The docks `key_units_by_encoder_threads` encodes key units for: the
/// default and wide docks, and the widest the resize measurement drags to
/// (1220 CSS px), in device pixels.
const KEY_DOCKS: [(&str, (usize, usize)); 3] = [
    ("default", (988, 1888)),
    ("wide", (1840, 1888)),
    ("widest", (2440, 1888)),
];

/// A comma-separated list of numbers from the environment, or `default`.
fn numbers(name: &str, default: &str) -> Vec<u32> {
    std::env::var(name)
        .unwrap_or_else(|_| default.into())
        .split(',')
        .map(|value| value.trim().parse().unwrap())
        .collect()
}

/// What a key unit costs when the encoder spends more threads and tile
/// columns on it than on the motion around it, at the coded sizes the
/// producer gives the docks, on a page of dense text:
///
/// - `fresh`: a new stream's first unit (a new viewer, a new size class),
///   from an encoder opened at those threads and tiles;
/// - `inStream`: a key unit inside a stream of motion at the producer's
///   threads and tiles, switched to the key unit's and back, with the time
///   of both switches and the encode time of the motion pictures before and
///   after them (a switch must leave motion as it was).
///
/// Latency is wall time around the encode call; CPU is the process's,
/// every thread, across it. `VIDEO_BENCH_PAGE=<a PNG at least 1840 px
/// wide>` (continued sideways past its width), `VIDEO_BENCH_KEY_THREADS`
/// (default 4,6,8,12,16), `VIDEO_BENCH_KEY_TILES` (tile columns, default
/// 4,8), `VIDEO_BENCH_KEY_REPEATS` (key units per case, default 5),
/// `VIDEO_BENCH_OUT=<file.json>`; run alone:
/// `<unit-test binary> --ignored --exact native::stream::video::cpu_bench::key_units_by_encoder_threads --nocapture --test-threads=1`
#[test]
#[ignore = "measurement harness: needs a page render (VIDEO_BENCH_PAGE)"]
fn key_units_by_encoder_threads() {
    let page = Page::load(std::path::Path::new(
        &std::env::var_os("VIDEO_BENCH_PAGE").unwrap(),
    ));
    let threads = numbers("VIDEO_BENCH_KEY_THREADS", "4,6,8,12,16");
    let tiles = numbers("VIDEO_BENCH_KEY_TILES", "4,8");
    let repeats = numbers("VIDEO_BENCH_KEY_REPEATS", "5")[0] as usize;
    let codec = VideoCodec::Av1Full;
    let motion_tiles = tile_columns_log2(ENCODER_THREADS);
    let motion = EncodeRequest {
        key: false,
        quantizer: Quality::Motion.quantizer(),
        refine: false,
    };
    let key = EncodeRequest {
        key: true,
        ..motion
    };
    let ms = |started: Instant| started.elapsed().as_secs_f64() * 1000.0;
    let mut cases = Vec::new();
    for (name, (width, height)) in KEY_DOCKS {
        let coded = CodedSize::new().fit((width as u32, height as u32), Instant::now());
        let converted = |top: usize| {
            let mut picture = Planar::new(codec.chroma(), coded.0, coded.1);
            let source = page.window(top, width, height);
            assert!(picture.convert(&source, width * 4, (width, height), (0, height)));
            picture
        };
        // A scroll of 40 px a picture, and the screen a key unit shows.
        let scroll: Vec<Planar> = (0..30).map(|index| converted(index * 40)).collect();
        let screen = converted(0);
        for &count in &threads {
            for &columns in &tiles {
                assert!(columns.is_power_of_two(), "tile columns {columns}");
                let layout = columns.trailing_zeros() as c_int;
                let (mut fresh_ms, mut fresh_cpu, mut fresh_bytes) = (Vec::new(), Vec::new(), 0);
                for _ in 0..repeats {
                    let mut encoder = AomEncoder::new(codec, coded.0, coded.1, count).unwrap();
                    encoder.set_threads(count, layout).unwrap();
                    let (cpu, started) = (cpu_ms(), Instant::now());
                    let unit = encoder.encode(&screen.picture(), key).unwrap();
                    fresh_ms.push(ms(started));
                    fresh_cpu.push(cpu_ms() - cpu);
                    fresh_bytes = unit.data.len();
                }
                let mut encoder =
                    AomEncoder::new(codec, coded.0, coded.1, ENCODER_THREADS).unwrap();
                encoder.encode(&scroll[0].picture(), key).unwrap();
                let mut before = Vec::new();
                for picture in &scroll[1..] {
                    let started = Instant::now();
                    encoder.encode(&picture.picture(), motion).unwrap();
                    before.push(ms(started));
                }
                let (mut key_ms, mut key_cpu, mut up, mut down, mut after) =
                    (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
                let mut key_bytes = 0;
                for repeat in 0..repeats {
                    let started = Instant::now();
                    encoder.set_threads(count, layout).unwrap();
                    up.push(ms(started));
                    let (cpu, started) = (cpu_ms(), Instant::now());
                    let unit = encoder.encode(&screen.picture(), key).unwrap();
                    key_ms.push(ms(started));
                    key_cpu.push(cpu_ms() - cpu);
                    key_bytes = unit.data.len();
                    let started = Instant::now();
                    encoder.set_threads(ENCODER_THREADS, motion_tiles).unwrap();
                    down.push(ms(started));
                    for step in 0..5 {
                        let picture = &scroll[1 + (repeat * 5 + step) % (scroll.len() - 1)];
                        let started = Instant::now();
                        encoder.encode(&picture.picture(), motion).unwrap();
                        after.push(ms(started));
                    }
                }
                let kib = |bytes: usize| (bytes as f64 / 102.4).round() / 10.0;
                let case = json!({"dock": name, "window": [width, height], "coded": [coded.0, coded.1],
                    "keyThreads": count, "keyTileColumns": columns,
                    "fresh": {"keyMs": stats(&fresh_ms), "keyCpuMs": stats(&fresh_cpu), "kib": kib(fresh_bytes)},
                    "inStream": {"keyMs": stats(&key_ms), "keyCpuMs": stats(&key_cpu), "kib": kib(key_bytes),
                        "switchToKeyMs": stats(&up), "switchBackMs": stats(&down),
                        "motionMsBefore": stats(&before), "motionMsAfter": stats(&after)}});
                println!("KEY {case}");
                cases.push(case);
            }
        }
    }
    let cpu_model = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|info| {
            info.lines().find_map(|line| {
                line.strip_prefix("model name")
                    .map(|rest| rest.trim_start_matches([' ', '\t', ':']).to_string())
            })
        })
        .unwrap_or_default();
    let available = std::thread::available_parallelism().map_or(0, |n| n.get());
    let report = json!({"codec": codec.token(), "motionThreads": ENCODER_THREADS,
        "motionTileColumns": 1 << motion_tiles, "availableCpus": available, "cpuModel": cpu_model, "cases": cases});
    if let Some(out) = std::env::var_os("VIDEO_BENCH_OUT") {
        std::fs::write(out, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    }
}
