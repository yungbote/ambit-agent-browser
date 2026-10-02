use super::*;
use crate::native::video::convert::{to_rgb, Colour, Planar};
use std::mem::{offset_of, size_of};

#[test]
fn active_map_layout_and_region_bounds_match_the_coded_picture() {
    assert_eq!(size_of::<ActiveMap>(), 16);
    assert_eq!(offset_of!(ActiveMap, cells), 0);
    assert_eq!(offset_of!(ActiveMap, rows), 8);
    assert_eq!(offset_of!(ActiveMap, columns), 12);
    let mut encoder = AomEncoder::new(VideoCodec::Av1Full, 64, 64, 1).unwrap();
    let valid = EncoderRegion {
        x: 16,
        y: 16,
        width: 16,
        height: 16,
    };
    let actual = encoder.set_refinement_region(Some(valid)).unwrap().unwrap();
    assert_eq!(
        actual,
        EncoderRegion {
            x: 0,
            y: 0,
            width: 32,
            height: 32
        }
    );
    encoder.configure_region(Some(actual)).unwrap();
    assert_eq!(
        encoder
            .active_cells
            .iter()
            .filter(|cell| **cell == 1)
            .count(),
        4
    );
    assert_eq!(encoder.active_cells[5], 1);
    for region in [
        EncoderRegion {
            x: 0,
            y: 0,
            width: 0,
            height: 16,
        },
        EncoderRegion {
            x: 0,
            y: 0,
            width: 16,
            height: 0,
        },
        EncoderRegion {
            x: 63,
            y: 0,
            width: 2,
            height: 16,
        },
        EncoderRegion {
            x: 0,
            y: 63,
            width: 16,
            height: 2,
        },
        EncoderRegion {
            x: u32::MAX,
            y: 0,
            width: 1,
            height: 16,
        },
    ] {
        assert!(encoder.set_refinement_region(Some(region)).is_err());
        assert_eq!(encoder.region, Some(actual));
    }
    encoder.set_refinement_region(None).unwrap();
    encoder.configure_region(None).unwrap();
    assert_eq!(encoder.region, None);
    assert!(encoder.active_map.cells.is_null());
}

#[test]
fn a_regional_refinement_preserves_unmodified_reference_pixels() {
    let mut source = text_fixture(LIGHT);
    let mut planar = Planar::new(Chroma::Full, FIXTURE.0 as u32, FIXTURE.1 as u32);
    assert!(planar.convert(&source, FIXTURE.0 * 4, FIXTURE, (0, FIXTURE.1)));
    let mut encoder =
        AomEncoder::new(VideoCodec::Av1Full, FIXTURE.0 as u32, FIXTURE.1 as u32, 2).unwrap();
    let mut decoder = Decoder::new();
    let motion = encoder
        .encode(
            &planar.picture(),
            EncodeRequest {
                key: true,
                quantizer: 48,
                refine: false,
            },
        )
        .unwrap();
    decoder.decode(&motion.data);
    for step in 0..4 {
        source = text_fixture(LIGHT);
        for y in 32 + step * 16..64 + step * 16 {
            for x in 200..232 {
                let at = (y * FIXTURE.0 + x) * 4;
                source[at..at + 4].copy_from_slice(&[255, 0, 0, 0]);
            }
        }
        assert!(planar.convert(&source, FIXTURE.0 * 4, FIXTURE, (0, FIXTURE.1)));
        let motion = encoder
            .encode(
                &planar.picture(),
                EncodeRequest {
                    key: false,
                    quantizer: 48,
                    refine: false,
                },
            )
            .unwrap();
        decoder.decode(&motion.data);
    }
    let coarse = decoder.decode(
        &encoder
            .encode(
                &planar.picture(),
                EncodeRequest {
                    key: false,
                    quantizer: 48,
                    refine: false,
                },
            )
            .unwrap()
            .data,
    );
    let before = coarse.rgb();
    let preview = unsafe {
        (encoder.api.preview_frame)(&mut *encoder.context)
            .as_ref()
            .unwrap()
    };
    let preview_errors: Vec<_> = (0..3)
        .map(|plane| {
            let mut changed = 0;
            let mut maximum = 0;
            for y in 0..FIXTURE.1 {
                for x in 0..FIXTURE.0 {
                    let error = unsafe {
                        *preview.planes[plane].add(y * preview.strides[plane] as usize + x)
                    }
                    .abs_diff(coarse.planes[plane][y * FIXTURE.0 + x]);
                    changed += usize::from(error != 0);
                    maximum = maximum.max(error);
                }
            }
            (changed, maximum)
        })
        .collect();
    encoder
        .set_rate(Some(EncoderRate {
            bits_per_second: 500_000,
            pictures_per_second: 60,
        }))
        .unwrap();
    encoder
        .set_refinement_region(Some(EncoderRegion {
            x: 0,
            y: 0,
            width: 128,
            height: 128,
        }))
        .unwrap();
    let refined = encoder
        .encode(
            &planar.picture(),
            EncodeRequest {
                key: false,
                quantizer: 8,
                refine: true,
            },
        )
        .unwrap();
    assert!(!refined.key);
    let after = decoder.decode(&refined.data).rgb();
    let mut outside_max = 0u8;
    let mut outside_changed = 0usize;
    let mut inside_before = 0u64;
    let mut inside_after = 0u64;
    let mut first_changes = Vec::new();
    for y in 0..FIXTURE.1 {
        for x in 0..FIXTURE.0 {
            let at = (y * FIXTURE.0 + x) * 4;
            if x < 128 && y < 128 {
                for (rgb, bgr) in [(0, 2), (1, 1), (2, 0)] {
                    inside_before += u64::from(source[at + bgr].abs_diff(before[at + rgb])).pow(2);
                    inside_after += u64::from(source[at + bgr].abs_diff(after[at + rgb])).pow(2);
                }
            }
            if x < 128 && y < 128 {
                continue;
            }
            let error = (0..3)
                .map(|channel| before[at + channel].abs_diff(after[at + channel]))
                .max()
                .unwrap();
            outside_max = outside_max.max(error);
            outside_changed += usize::from(error != 0);
            if error != 0 && first_changes.len() < 10 {
                first_changes.push((x, y, error));
            }
        }
    }
    println!(
        "REGION_SCOPE {}",
        serde_json::json!({"outsideMaxRgbDelta":outside_max,"outsideChangedPixels":outside_changed,"unitBytes":refined.data.len(),"insideErrorBefore":inside_before,"insideErrorAfter":inside_after,"previewErrors":preview_errors,"firstChanges":first_changes})
    );
    assert_eq!(
        outside_max, 0,
        "inactive-reference gate remains exact until scope is understood"
    );
    assert!(
        inside_after < inside_before,
        "a refinement must improve its active region"
    );
    encoder.set_refinement_region(None).unwrap();
    let motion = encoder
        .encode(
            &planar.picture(),
            EncodeRequest {
                key: false,
                quantizer: 32,
                refine: false,
            },
        )
        .unwrap();
    assert!(!motion.key);
    decoder.decode(&motion.data);
}

/// Challenge the native dock dimensions and valid high-entropy screen content;
/// every unit decodes before its measured size is compared with the contract.
#[test]
fn regional_steps_preserve_other_reference_pixels_including_already_exact_regions() {
    let (width, height) = (512u32, 256u32);
    let fixture = text_fixture(LIGHT);
    let mut source = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height as usize {
        for x in 0..width as usize {
            let at = ((y % FIXTURE.1) * FIXTURE.0 + x % FIXTURE.0) * 4;
            source.extend_from_slice(&[fixture[at], fixture[at + 1], fixture[at + 2], 0]);
        }
    }
    let mut planar = Planar::new(Chroma::Full, width, height);
    assert!(planar.convert(
        &source,
        width as usize * 4,
        (width as usize, height as usize),
        (0, height as usize)
    ));
    let mut encoder = AomEncoder::new(VideoCodec::Av1Full, width, height, 4).unwrap();
    let mut decoder = Decoder::new();
    let key = encoder
        .encode(
            &planar.picture(),
            EncodeRequest {
                key: true,
                quantizer: 48,
                refine: false,
            },
        )
        .unwrap();
    let mut before = decoder.decode(&key.data).rgb();
    // The first key can code flat text exactly even at a coarse target. Give
    // the regions a normal moving history so each owes actual codec error.
    for step in 0..4 {
        for y in 0..height as usize {
            for x in 0..width as usize {
                let at = (y * width as usize + x) * 4;
                let from = ((y % FIXTURE.1) * FIXTURE.0 + (x + step * 3) % FIXTURE.0) * 4;
                source[at..at + 4].copy_from_slice(&fixture[from..from + 4]);
            }
        }
        assert!(planar.convert(
            &source,
            width as usize * 4,
            (width as usize, height as usize),
            (0, height as usize)
        ));
        let motion = encoder
            .encode(
                &planar.picture(),
                EncodeRequest {
                    key: false,
                    quantizer: 48,
                    refine: false,
                },
            )
            .unwrap();
        before = decoder.decode(&motion.data).rgb();
    }
    encoder
        .set_rate(Some(EncoderRate {
            bits_per_second: 500_000,
            pictures_per_second: 60,
        }))
        .unwrap();
    for requested in [
        EncoderRegion {
            x: 0,
            y: 0,
            width: 64,
            height: 64,
        },
        EncoderRegion {
            x: 128,
            y: 0,
            width: 64,
            height: 64,
        },
        EncoderRegion {
            x: 448,
            y: 192,
            width: 64,
            height: 64,
        },
        EncoderRegion {
            x: 200,
            y: 70,
            width: 30,
            height: 45,
        },
    ] {
        let region = encoder
            .set_refinement_region(Some(requested))
            .unwrap()
            .unwrap();
        let unit = encoder
            .encode(
                &planar.picture(),
                EncodeRequest {
                    key: false,
                    quantizer: 8,
                    refine: true,
                },
            )
            .unwrap();
        assert!(!unit.key);
        let after = decoder.decode(&unit.data).rgb();
        let mut error_before = 0u64;
        let mut error_after = 0u64;
        for y in 0..height {
            for x in 0..width {
                let at = (y as usize * width as usize + x as usize) * 4;
                if (region.x..region.x + region.width).contains(&x)
                    && (region.y..region.y + region.height).contains(&y)
                {
                    for (rgb, bgr) in [(0, 2), (1, 1), (2, 0)] {
                        error_before +=
                            u64::from(source[at + bgr].abs_diff(before[at + rgb])).pow(2);
                        error_after += u64::from(source[at + bgr].abs_diff(after[at + rgb])).pow(2);
                    }
                } else {
                    assert_eq!(
                        after[at..at + 3],
                        before[at..at + 3],
                        "outside region{region:?} at{x},{y}"
                    );
                }
            }
        }
        println!(
            "REGION_STEP {}",
            serde_json::json!({"region":[region.x,region.y,region.width,region.height],"bytes":unit.data.len(),"insideBefore":error_before,"insideAfter":error_after})
        );
        let pixels = f64::from(region.width) * f64::from(region.height) * 3.0;
        let already_sharp =
            error_after as f64 / pixels <= 255.0f64.powi(2) / 10.0f64.powf(49.0 / 10.0);
        assert!(
            error_after <= error_before && (error_after < error_before || already_sharp),
            "refinement must not degrade{region:?} and must improve it unless it already meets the measured49dB text target"
        );
        assert!(
            unit.data.len() <= 500_000 / 320,
            "region exceeded its25ms byte budget"
        );
        before = after;
    }
    // Subsequent motion returns to ordinary CBR with a complete active map;
    // the previously improved blocks still belong to the same reference chain.
    source[0..4].copy_from_slice(&[0, 0, 255, 0]);
    assert!(planar.convert(
        &source,
        width as usize * 4,
        (width as usize, height as usize),
        (0, 1)
    ));
    let motion = encoder
        .encode(
            &planar.picture(),
            EncodeRequest {
                key: false,
                quantizer: 32,
                refine: false,
            },
        )
        .unwrap();
    assert!(!motion.key);
    decoder.decode(&motion.data);
    assert_eq!(encoder.region, None);
    assert!(encoder.active_map.cells.is_null());
}

#[test]
#[ignore]
fn aligned_regional_updates_bound_noisy_steps_on_the_measured_consumer_link() {
    let (width, height) = (1536u32, 2048u32);
    let mut random = 0x1234_5678u32;
    let mut source = Vec::with_capacity(width as usize * height as usize * 4);
    for _ in 0..width * height {
        for _ in 0..3 {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            source.push(random as u8);
        }
        source.push(0);
    }
    let mut planar = Planar::new(Chroma::Full, width, height);
    assert!(planar.convert(
        &source,
        width as usize * 4,
        (width as usize, height as usize),
        (0, height as usize)
    ));
    let mut encoder = AomEncoder::new(VideoCodec::Av1Full, width, height, 4).unwrap();
    let mut decoder = Decoder::new();
    let key = encoder
        .encode(
            &planar.picture(),
            EncodeRequest {
                key: true,
                quantizer: 48,
                refine: false,
            },
        )
        .unwrap();
    let mut before = decoder.decode(&key.data).rgb();
    encoder
        .set_rate(Some(EncoderRate {
            bits_per_second: 3_000_000,
            pictures_per_second: 60,
        }))
        .unwrap();
    for region in [
        EncoderRegion {
            x: 0,
            y: 0,
            width: 32,
            height: 32,
        },
        EncoderRegion {
            x: 736,
            y: 992,
            width: 32,
            height: 32,
        },
        EncoderRegion {
            x: 1504,
            y: 2016,
            width: 32,
            height: 32,
        },
    ] {
        encoder.set_refinement_region(Some(region)).unwrap();
        let began = std::time::Instant::now();
        let unit = encoder
            .encode(
                &planar.picture(),
                EncodeRequest {
                    key: false,
                    quantizer: 8,
                    refine: true,
                },
            )
            .unwrap();
        let elapsed = began.elapsed();
        let after = decoder.decode(&unit.data).rgb();
        let mut old_error = 0u64;
        let mut new_error = 0u64;
        let mut changed = 0usize;
        let mut first_changes = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let at = (y as usize * width as usize + x as usize) * 4;
                if (region.x..region.x + region.width).contains(&x)
                    && (region.y..region.y + region.height).contains(&y)
                {
                    for (rgb, bgr) in [(0, 2), (1, 1), (2, 0)] {
                        old_error += u64::from(source[at + bgr].abs_diff(before[at + rgb])).pow(2);
                        new_error += u64::from(source[at + bgr].abs_diff(after[at + rgb])).pow(2);
                    }
                } else {
                    if before[at..at + 3] != after[at..at + 3] {
                        changed += 1;
                        if first_changes.len() < 20 {
                            first_changes.push((x, y));
                        }
                    }
                }
            }
        }
        println!(
            "MINIMUM_REGION {}",
            serde_json::json!({"region":[region.x,region.y,region.width,region.height],"bytes":unit.data.len(),"budget":3_000_000/320,"encodeMs":elapsed.as_millis(),"outsideChanged":changed,"insideBefore":old_error,"insideAfter":new_error,"firstChanges":first_changes})
        );
        assert_eq!(
            changed, 0,
            "regional step modified inactive reference pixels"
        );
        assert!(
            new_error < old_error,
            "regional step failed to sharpen noisy pixels"
        );
        assert!(
            unit.data.len() <= 3_000_000 / 320,
            "even the smallest native block exceeds the link step budget"
        );
        before = after;
    }
}

/// A native key's bytes are observed independently of replenishment credit.
/// Native quality stays unchanged; legacy and coded-capacity admission are
/// distinct from the first key's physical serialization time.
#[test]
#[ignore = "qualification: wide high-entropy native keys"]
fn wide_fixed_quality_keys_preserve_native_pixels_and_report_the_wire_boundary() {
    for (width, height, bits_per_second, former_level) in [
        (320u32, 192u32, 20_000u32, 8),
        (1536, 2048, 3_000_000, 12),
        (4096, 4096, 500_000, 16),
    ] {
        let mut source = Vec::with_capacity(width as usize * height as usize * 4);
        let mut random = 0x01234567u32;
        for _ in 0..width * height {
            for _ in 0..3 {
                random ^= random << 13;
                random ^= random >> 17;
                random ^= random << 5;
                source.push(random as u8);
            }
            source.push(0);
        }
        let mut planar = Planar::new(Chroma::Full, width, height);
        assert!(planar.convert(
            &source,
            width as usize * 4,
            (width as usize, height as usize),
            (0, height as usize)
        ));
        let request = EncodeRequest {
            key: true,
            quantizer: 32,
            refine: false,
        };
        let mut baseline = AomEncoder::new(VideoCodec::Av1Full, width, height, 4).unwrap();
        baseline.tune(54, former_level).unwrap();
        let before = baseline.encode(&planar.picture(), request).unwrap();
        let mut candidate = AomEncoder::new(VideoCodec::Av1Full, width, height, 4).unwrap();
        candidate
            .set_rate(Some(EncoderRate {
                bits_per_second,
                pictures_per_second: 60,
            }))
            .unwrap();
        let key = candidate.encode(&planar.picture(), request).unwrap();
        assert_eq!(
            frame_obus(&before.data),
            frame_obus(&key.data),
            "native coded-frame byte parity at{width}x{height}"
        );
        let mut decoder = Decoder::new();
        let after = decoder.decode(&key.data);
        assert_eq!(after.rgb(), Decoder::new().decode(&before.data).rgb());
        let bytes = key.data.len();
        let capacity = VideoCodec::Av1Full.coded_capacity(width, height).unwrap();
        assert!(bytes <= capacity);
        if let Ok(directory) = std::env::var("AMBIT_NATIVE_KEY_EVIDENCE") {
            let directory = std::path::Path::new(&directory);
            std::fs::create_dir_all(directory).unwrap();
            std::fs::write(
                directory.join(format!("native-q32-noise-{width}x{height}.av1")),
                &key.data,
            )
            .unwrap();
        }
        println!(
            "NATIVE_WIDE_KEY {}",
            serde_json::json!({"width":width,"height":height,"rate":bits_per_second,"codecString":key.codec_string,"keyBytes":bytes,"fitsLegacyEnvelope":bytes <= 4*1024*1024,"codedCapacity":capacity,"fitsCodedCapacity":true,"serializationFloorMs":bytes as f64*8000.0/f64::from(bits_per_second),"codedFrameBytesEqual":true,"pixelsEqual":true})
        );
        // Transport admission does not poison native state: a valid dependent
        // unit still decodes. The track owns legacy refusal and retirement.
        let delta = candidate
            .encode(
                &planar.picture(),
                EncodeRequest {
                    key: false,
                    quantizer: 32,
                    refine: false,
                },
            )
            .unwrap();
        assert!(!delta.key && delta.codec_string.is_none());
        let decoded = decoder.decode(&delta.data);
        assert_eq!(
            (decoded.width, decoded.height, decoded.colour),
            (width, height, COLOUR)
        );
    }
}

#[test]
fn rate_control_uses_real_picture_periods_and_preserves_the_reference_chain() {
    let source = text_fixture(LIGHT);
    let mut planar = Planar::new(Chroma::Full, FIXTURE.0 as u32, FIXTURE.1 as u32);
    assert!(planar.convert(&source, FIXTURE.0 * 4, FIXTURE, (0, FIXTURE.1)));
    let mut encoder =
        AomEncoder::new(VideoCodec::Av1Full, FIXTURE.0 as u32, FIXTURE.1 as u32, 2).unwrap();
    let mut decoder = Decoder::new();
    let rate = EncoderRate {
        bits_per_second: 500_000,
        pictures_per_second: 60,
    };
    encoder.set_rate(Some(rate)).unwrap();
    assert_eq!(
        (
            encoder.config.end_usage,
            encoder.config.min_quantizer,
            encoder.config.max_quantizer
        ),
        (CBR, 20, 48)
    );
    let mut bytes = Vec::new();
    for frame in 0..60 {
        let unit = encoder
            .encode(
                &planar.picture(),
                EncodeRequest {
                    key: frame == 0,
                    quantizer: 32,
                    refine: false,
                },
            )
            .unwrap();
        assert_eq!(unit.key, frame == 0);
        let decoded = decoder.decode(&unit.data);
        assert_eq!(
            (decoded.width, decoded.height),
            (FIXTURE.0 as u32, FIXTURE.1 as u32)
        );
        bytes.push(unit.data.len());
    }
    assert_eq!(
        encoder.pictures, 60,
        "the encoder clock counts frames rather than mixing frame and microsecond units"
    );
    assert!(
        bytes[0] <= 4 * 1024 * 1024,
        "key exceeds the existing wire bound"
    );
    assert!(bytes[1..]
        .iter()
        .all(|bytes| *bytes <= (rate.bits_per_second as usize / 8 / 60) * 115 / 100));
    println!(
        "RATE_FIXTURE {}",
        serde_json::json!({"codec":"av1-444","width":FIXTURE.0,"height":FIXTURE.1,"bitsPerSecond":rate.bits_per_second,"picturesPerSecond":rate.pictures_per_second,"unitBytes":bytes})
    );
    // A refinement is fixed-quality against the same references; motion returns to CBR without a key unit.
    let refined = encoder
        .encode(
            &planar.picture(),
            EncodeRequest {
                key: false,
                quantizer: 8,
                refine: true,
            },
        )
        .unwrap();
    assert!(!refined.key);
    decoder.decode(&refined.data);
    let motion = encoder
        .encode(
            &planar.picture(),
            EncodeRequest {
                key: false,
                quantizer: 32,
                refine: false,
            },
        )
        .unwrap();
    assert!(!motion.key);
    decoder.decode(&motion.data);
    assert_eq!(encoder.config.end_usage, CBR);
    encoder.set_rate(None).unwrap();
    let fixed = encoder
        .encode(
            &planar.picture(),
            EncodeRequest {
                key: false,
                quantizer: 8,
                refine: false,
            },
        )
        .unwrap();
    assert!(!fixed.key);
    decoder.decode(&fixed.data);
    assert_eq!((encoder.config.end_usage, encoder.quantizer), (Q, Some(8)));
}

#[test]
fn first_and_reset_keys_preserve_native_fixed_quality_across_rate_and_refinement_modes() {
    for codec in [VideoCodec::Av1, VideoCodec::Av1Full] {
        let source = text_fixture(LIGHT);
        let mut picture = Planar::new(codec.chroma(), FIXTURE.0 as u32, FIXTURE.1 as u32);
        assert!(picture.convert(&source, FIXTURE.0 * 4, FIXTURE, (0, FIXTURE.1)));
        let key_request = EncodeRequest {
            key: true,
            quantizer: 32,
            refine: false,
        };
        let mut native = AomEncoder::new(codec, FIXTURE.0 as u32, FIXTURE.1 as u32, 2).unwrap();
        // Independent fixed-Q baseline retains the former explicit target
        // level. Automatic metadata may change its sequence-header bytes;
        // frame payload and reconstructed pixels must retain native quality.
        native.tune(54, 8).unwrap();
        let native_key = native.encode(&picture.picture(), key_request).unwrap();
        let mut encoder = AomEncoder::new(codec, FIXTURE.0 as u32, FIXTURE.1 as u32, 2).unwrap();
        encoder
            .set_rate(Some(EncoderRate {
                bits_per_second: 3_000_000,
                pictures_per_second: 60,
            }))
            .unwrap();
        let first = encoder.encode(&picture.picture(), key_request).unwrap();
        assert_eq!(
            frame_obus(&first.data),
            frame_obus(&native_key.data),
            "first-key coded-frame parity with the native fixed-quality encoder"
        );
        assert_eq!(encoder.config.end_usage, Q);
        let mut decoder = Decoder::new();
        let first_pixels = decoder.decode(&first.data).rgb();
        let native_pixels = Decoder::new().decode(&native_key.data).rgb();
        assert_eq!(first_pixels, native_pixels);
        let motion = encoder
            .encode(
                &picture.picture(),
                EncodeRequest {
                    key: false,
                    quantizer: 32,
                    refine: false,
                },
            )
            .unwrap();
        assert!(!motion.key);
        decoder.decode(&motion.data);
        assert_eq!(encoder.config.end_usage, CBR);
        let actual = encoder
            .set_refinement_region(Some(EncoderRegion {
                x: 10,
                y: 10,
                width: 20,
                height: 20,
            }))
            .unwrap()
            .unwrap();
        assert_eq!(
            actual,
            EncoderRegion {
                x: 0,
                y: 0,
                width: 32,
                height: 32
            }
        );
        let refined = encoder
            .encode(
                &picture.picture(),
                EncodeRequest {
                    key: false,
                    quantizer: 8,
                    refine: true,
                },
            )
            .unwrap();
        assert!(!refined.key);
        decoder.decode(&refined.data);
        let reset = encoder.encode(&picture.picture(), key_request).unwrap();
        assert!(reset.key);
        assert_eq!(encoder.config.end_usage, Q);
        assert_eq!(encoder.region, None);
        assert!(encoder.active_map.cells.is_null());
        let reset_pixels = decoder.decode(&reset.data);
        assert_eq!(reset_pixels.colour, COLOUR);
        assert_eq!(
            (reset_pixels.width, reset_pixels.height),
            (FIXTURE.0 as u32, FIXTURE.1 as u32)
        );
    }
}

#[test]
fn malformed_rate_budgets_do_not_mutate_the_encoder() {
    let mut encoder = AomEncoder::new(VideoCodec::Av1Full, 64, 64, 1).unwrap();
    for rate in [
        EncoderRate {
            bits_per_second: 999,
            pictures_per_second: 60,
        },
        EncoderRate {
            bits_per_second: 1_000_000,
            pictures_per_second: 0,
        },
        EncoderRate {
            bits_per_second: 1_000_000,
            pictures_per_second: 61,
        },
    ] {
        assert!(encoder.set_rate(Some(rate)).is_err());
        assert_eq!(encoder.rate, None);
        assert_eq!(encoder.config.end_usage, Q);
    }
}

#[test]
fn changing_picture_rates_keeps_one_decodable_native_reference_chain() {
    let source = text_fixture(LIGHT);
    let mut planar = Planar::new(Chroma::Full, FIXTURE.0 as u32, FIXTURE.1 as u32);
    assert!(planar.convert(&source, FIXTURE.0 * 4, FIXTURE, (0, FIXTURE.1)));
    let mut encoder =
        AomEncoder::new(VideoCodec::Av1Full, FIXTURE.0 as u32, FIXTURE.1 as u32, 2).unwrap();
    let mut decoder = Decoder::new();
    let mut frame = 0;
    for pictures_per_second in [60, 10, 1, 30, 60] {
        encoder
            .set_rate(Some(EncoderRate {
                bits_per_second: 500_000,
                pictures_per_second,
            }))
            .unwrap();
        for _ in 0..8 {
            let unit = encoder
                .encode(
                    &planar.picture(),
                    EncodeRequest {
                        key: frame == 0,
                        quantizer: 32,
                        refine: false,
                    },
                )
                .unwrap();
            assert_eq!(unit.key, frame == 0);
            let decoded = decoder.decode(&unit.data);
            assert_eq!(
                (decoded.width, decoded.height, decoded.colour),
                (FIXTURE.0 as u32, FIXTURE.1 as u32, COLOUR)
            );
            frame += 1;
        }
    }
    assert_eq!(encoder.pictures, frame);
}

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
fn malformed_sequence_metadata_is_refused_without_panicking() {
    for bytes in [
        vec![],
        vec![0x0a],
        vec![0x0a, 255],
        vec![0x0a, 1, 0],
        vec![0x0a, 3, 0],
        vec![0x0e],
        vec![0x8a, 0],
        vec![0x0b, 0],
        vec![0x0a, 255, 255, 255, 255, 255, 255, 255, 255],
    ] {
        assert!(key_codec_string(&bytes).is_none(), "{bytes:?}");
    }
    let mut noise = 0xabcdef01u32;
    for length in 0..128 {
        let bytes: Vec<u8> = (0..length)
            .map(|_| {
                noise ^= noise << 13;
                noise ^= noise >> 17;
                noise ^= noise << 5;
                noise as u8
            })
            .collect();
        let _ = key_codec_string(&bytes);
    }
}

#[test]
fn automatic_level_repeated_regional_sequence_decodes_beyond_the_former_native_abort() {
    let codec = VideoCodec::Av1Full;
    let mut encoder = AomEncoder::new(codec, 1024, 768, 2).unwrap();
    if std::env::var_os("AMBIT_TEST_FORMER_FORCED_LEVEL").is_some() {
        encoder.tune(54, 8).unwrap();
    }
    encoder
        .set_rate(Some(EncoderRate {
            bits_per_second: 20_000,
            pictures_per_second: 60,
        }))
        .unwrap();
    let mut source = vec![0u8; 1024 * 768 * 4];
    for y in 0..768usize {
        for x in 0..1024usize {
            source[(y * 1024 + x) * 4..(y * 1024 + x) * 4 + 4].copy_from_slice(&[
                (y * 17) as u8,
                (y * 71) as u8,
                (y * 113) as u8,
                0,
            ]);
        }
    }
    let mut planar = Planar::new(Chroma::Full, 1024, 768);
    assert!(planar.convert(&source, 4096, (1024, 768), (0, 768)));
    let mut decoder = Decoder::new();
    for frame in 0..80 {
        encoder
            .set_refinement_region((frame > 0).then_some(EncoderRegion {
                x: (frame % 20) * 32,
                y: (frame / 20) * 32,
                width: 32,
                height: 32,
            }))
            .unwrap();
        let unit = encoder
            .encode(
                &planar.picture(),
                EncodeRequest {
                    key: frame == 0,
                    quantizer: if frame == 0 { 32 } else { 8 },
                    refine: frame > 0,
                },
            )
            .unwrap();
        let decoded = decoder.decode(&unit.data);
        assert_eq!(
            (
                decoded.width,
                decoded.height,
                decoded.colour,
                decoded.chroma
            ),
            (1024, 768, COLOUR, Chroma::Full)
        );
        if frame == 0 {
            let header = sequence_header(&unit.data).unwrap();
            assert_eq!(
                unit.codec_string.as_deref(),
                Some(format!("av01.{}.{:02}M.08", header.profile, header.level).as_str())
            );
            assert!(unit.data.len() <= 4 * 1024 * 1024);
            println!(
                "AUTOMATIC_KEY {}",
                serde_json::json!({"codecString":unit.codec_string,"bytes":unit.data.len(),"level":header.level,"profile":header.profile})
            );
        } else {
            assert!(unit.codec_string.is_none());
        }
    }
}

/// Native first/reset keys at the accepted dimensional and rate boundaries,
/// exported for the real Chromium decoder, never substituted by ffmpeg.
#[test]
#[ignore = "qualification matrix: AMBIT_AUTOMATIC_LEVEL_FIXTURE output path"]
fn export_automatic_level_dimension_rate_reset_matrix() {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let mut cases = Vec::new();
    for (codec, width, height, bits_per_second) in [
        (VideoCodec::Av1Full, 64, 64, 1000),
        (VideoCodec::Av1, 64, 64, 20_000),
        (VideoCodec::Av1Full, 1280, 720, 500_000),
        (VideoCodec::Av1, 1280, 720, 3_000_000),
        (VideoCodec::Av1Full, 1536, 2048, 20_000),
        (VideoCodec::Av1, 1536, 2048, 500_000),
        (VideoCodec::Av1Full, 2048, 2048, 3_000_000),
        (VideoCodec::Av1, 2048, 2048, 20_000),
        (VideoCodec::Av1Full, 4096, 4096, 500_000),
        (VideoCodec::Av1, 4096, 4096, 3_000_000),
    ] {
        let mut source = vec![0u8; width as usize * height as usize * 4];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let rgb = match x * 4 / width as usize {
                    0 => [255, 255, 255, 0],
                    1 => [48, 48, 48, 0],
                    2 => [0, 0, 255, 0],
                    _ => [218, 105, 9, 0],
                };
                source[(y * width as usize + x) * 4..(y * width as usize + x) * 4 + 4]
                    .copy_from_slice(&rgb);
            }
        }
        let mut planar = Planar::new(codec.chroma(), width, height);
        assert!(planar.convert(
            &source,
            width as usize * 4,
            (width as usize, height as usize),
            (0, height as usize)
        ));
        let mut encoder = AomEncoder::new(codec, width, height, 4).unwrap();
        encoder
            .set_rate(Some(EncoderRate {
                bits_per_second,
                pictures_per_second: 60,
            }))
            .unwrap();
        let mut decoder = Decoder::new();
        let mut units = Vec::new();
        for frame in 0..3 {
            let key = frame != 1;
            let unit = encoder
                .encode(
                    &planar.picture(),
                    EncodeRequest {
                        key,
                        quantizer: 32,
                        refine: false,
                    },
                )
                .unwrap();
            assert_eq!(unit.key, key);
            assert!(unit.data.len() <= 4 * 1024 * 1024);
            if key {
                let header = sequence_header(&unit.data).unwrap();
                assert_eq!(
                    unit.codec_string.as_deref(),
                    Some(format!("av01.{}.{:02}M.08", header.profile, header.level).as_str())
                );
                assert_eq!(
                    (
                        header.primaries,
                        header.transfer,
                        header.matrix,
                        header.full_range
                    ),
                    (Some(1), Some(1), Some(1), true)
                );
            } else {
                assert!(unit.codec_string.is_none());
            }
            let decoded = decoder.decode(&unit.data);
            assert_eq!(
                (
                    decoded.width,
                    decoded.height,
                    decoded.chroma,
                    decoded.colour
                ),
                (width, height, codec.chroma(), COLOUR)
            );
            let rgb = decoded.rgb();
            let samples: Vec<_> = (0..4u32)
                .map(|bar| {
                    let (x, y) = ((bar * 2 + 1) * width / 8, height / 2);
                    let at = (y as usize * width as usize + x as usize) * 4;
                    serde_json::json!({"x":x,"y":y,"rgb":rgb[at..at+3]})
                })
                .collect();
            units.push(serde_json::json!({"key":key,"codecString":unit.codec_string,"bytes":unit.data.len(),"data":STANDARD.encode(&unit.data),"samples":samples}));
        }
        println!(
            "NATIVE_CONFIGURATION {}",
            serde_json::json!({"codec":codec.token(),"width":width,"height":height,"rate":bits_per_second,"keys":units.iter().filter(|unit|unit["key"]==true).map(|unit|(&unit["codecString"],&unit["bytes"])).collect::<Vec<_>>()})
        );
        cases.push(serde_json::json!({"codec":codec.token(),"width":width,"height":height,"rate":bits_per_second,"units":units}));
    }
    std::fs::write(
        std::env::var("AMBIT_AUTOMATIC_LEVEL_FIXTURE").unwrap(),
        serde_json::to_vec(&cases).unwrap(),
    )
    .unwrap();
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
            unit.codec_string.as_deref(),
            Some(format!("av01.{profile}.{:02}M.08", header.level).as_str())
        );
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

/// A stream may change its threads and tile columns between pictures, as a
/// key unit encoded on more of both than the motion around it would be:
/// every unit still decodes to its picture, and the first picture after a
/// change sets its quantizer again (libaom takes a whole configuration back,
/// which resets the quantizer's range), so a key unit after a change is the
/// key unit a stream opened with those threads makes.
#[test]
fn a_stream_changes_threads_and_tiles_between_pictures() {
    // Eight tile columns need eight 128 px superblock columns.
    let (width, height) = (1024u32, 128u32);
    let codec = VideoCodec::Av1Full;
    let pictures: Vec<Planar> = (0..3usize)
        .map(|index| {
            let source: Vec<u8> = (0..height as usize)
                .flat_map(|y| {
                    (0..width as usize).flat_map(move |x| {
                        let value = ((x * 7 + y * 13 + index * 29) ^ (x >> 3)) as u8;
                        [value, value.wrapping_mul(3), 255 - value, 0]
                    })
                })
                .collect();
            let mut picture = Planar::new(Chroma::Full, width, height);
            assert!(picture.convert(
                &source,
                width as usize * 4,
                (width as usize, height as usize),
                (0, height as usize)
            ));
            picture
        })
        .collect();
    let request = |key| EncodeRequest {
        key,
        quantizer: 30,
        ..Default::default()
    };
    let mut opened_wide = AomEncoder::new(codec, width, height, 8).unwrap();
    opened_wide.set_threads(8, 3).unwrap();
    let wide_key = opened_wide
        .encode(&pictures[2].picture(), request(true))
        .unwrap();

    let mut encoder = AomEncoder::new(codec, width, height, 4).unwrap();
    let mut decoder = Decoder::new();
    let mut layout = (4, 2);
    // (threads, tile columns log2, key, picture)
    for (step, (threads, tiles, key, picture)) in [
        (4, 2, true, 0),
        (4, 2, false, 1),
        (8, 3, true, 2),
        (4, 2, false, 0),
        (8, 3, true, 1),
        (4, 2, false, 2),
    ]
    .into_iter()
    .enumerate()
    {
        if (threads, tiles) != layout {
            encoder.set_threads(threads, tiles).unwrap();
            assert_eq!(
                encoder.quantizer, None,
                "step {step}: the quantizer is set again"
            );
            layout = (threads, tiles);
        }
        let unit = encoder
            .encode(&pictures[picture].picture(), request(key))
            .unwrap();
        assert_eq!(unit.key, key, "step {step}");
        if step == 2 {
            let (after, opened) = (unit.data.len() as f64, wide_key.data.len() as f64);
            assert!(
                (after / opened - 1.0).abs() < 0.05,
                "step {step}: a key unit after the change is {after} bytes, one from a stream opened there {opened}"
            );
        }
        let decoded = decoder.decode(&unit.data);
        assert_eq!((decoded.width, decoded.height), (width, height));
        let given = pictures[picture].picture().planes().unwrap();
        let error: f64 = given[0]
            .0
            .iter()
            .zip(&decoded.planes[0])
            .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
            .sum::<f64>()
            / f64::from(width * height);
        let psnr = 10.0 * (65025.0 / error.max(1e-9)).log10();
        assert!(psnr > 30.0, "step {step}: luma PSNR {psnr:.1} dB");
    }
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
    let codec_string = unit.codec_string.clone().unwrap();
    (picture, unit, codec_string)
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

/// Trusted native output, independent of the production configuration parser.
/// Only the sequence header may differ when libaom selects its own level.
fn frame_obus(unit: &[u8]) -> Vec<Vec<u8>> {
    let mut at = 0;
    let mut frames = Vec::new();
    while at < unit.len() {
        let begin = at;
        let header = unit[at];
        at += 1 + usize::from(header & 4 != 0);
        let size = if header & 2 != 0 {
            leb128(unit, &mut at)
        } else {
            unit.len() - at
        };
        at += size;
        if (header >> 3) & 15 != 1 {
            frames.push(unit[begin..at].to_vec());
        }
    }
    frames
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
