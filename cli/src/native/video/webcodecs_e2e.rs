//! Chrome paints what the encoder signals. The text fixtures on a white and
//! on a black page over colour bars are each encoded as one key unit at the
//! still target, then decoded and drawn by a real Chrome the way the viewer
//! does: a WebCodecs `VideoDecoder` configured from the key unit, the frame
//! drawn 1:1 on an opaque 2D canvas and read back. Chrome must decode the
//! very samples libaom decodes, read the colour the stream signals (BT.709
//! at full range), and paint the fixture as it was drawn (`check_painted`,
//! the checks libaom's own decode through the exact inverse passes). Prints
//! `COLOUR <json>`; with `$AMBIT_VIDEO_PROOF` set to a directory, writes the
//! numbers and each drawn and painted picture there.
//!
//! `cargo test --profile ci colour_in_chrome -- --ignored` with
//! `AGENT_BROWSER_EXECUTABLE_PATH` set.

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

use super::aom::tests::{check_painted, still_unit, text_fixture, Decoder, DARK, FIXTURE, LIGHT};
use super::{Chroma, VideoCodec};
use crate::native::actions::{execute_command, DaemonState};

/// Decodes one key unit as the viewer configures its decoder and draws the
/// frame 1:1 on an opaque 2D canvas. Answers the frame's colour space, its
/// decoded samples with their layout, and the canvas pixels (RGBA), the
/// bytes in base64; or what failed.
const PAINT: &str = r#"async (unit, codec, width, height) => {
  const base64 = (bytes) => {
    let binary = "";
    for (let at = 0; at < bytes.length; at += 0x8000) {
      binary += String.fromCharCode(...bytes.subarray(at, at + 0x8000));
    }
    return btoa(binary);
  };
  const data = Uint8Array.from(atob(unit), (c) => c.charCodeAt(0));
  const frames = [];
  let failure = null;
  const decoder = new VideoDecoder({
    output: (frame) => frames.push(frame),
    error: (error) => { failure = String(error); },
  });
  decoder.configure({ codec, codedWidth: width, codedHeight: height,
    optimizeForLatency: true, hardwareAcceleration: "no-preference" });
  decoder.decode(new EncodedVideoChunk({ type: "key", timestamp: 0, data }));
  await decoder.flush().catch((error) => { failure ??= String(error); });
  decoder.close();
  if (failure || frames.length !== 1) {
    frames.forEach((frame) => frame.close());
    return { error: failure ?? `${frames.length} frames` };
  }
  const [frame] = frames;
  const samples = new Uint8Array(frame.allocationSize());
  const layout = await frame.copyTo(samples);
  const canvas = Object.assign(document.createElement("canvas"),
    { width: frame.displayWidth, height: frame.displayHeight });
  const context = canvas.getContext("2d", { alpha: false });
  context.drawImage(frame, 0, 0);
  const answer = { colorSpace: frame.colorSpace.toJSON(), format: frame.format, layout };
  frame.close();
  const pixels = context.getImageData(0, 0, canvas.width, canvas.height).data;
  return { ...answer, samples: base64(samples), pixels: base64(pixels) };
}"#;

/// An empty page on the loopback address, a secure context where WebCodecs
/// is exposed. Answers its URL; the server lives as long as the runtime.
async fn secure_page() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = [0; 4096];
                let _ = socket.read(&mut request).await;
                let body = "<!doctype html><title>Colour</title>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("http://127.0.0.1:{port}/")
}

async fn command(state: &mut DaemonState, command: Value) -> Value {
    let response = Box::pin(execute_command(&command, state)).await;
    assert_eq!(
        response["success"], true,
        "{}: {}",
        command["action"], response["error"]
    );
    response["data"].clone()
}

fn bytes(answer: &Value, field: &str) -> Vec<u8> {
    STANDARD.decode(answer[field].as_str().unwrap()).unwrap()
}

/// BGRX or RGBA rows of the fixture's size as a PNG in `directory`.
fn save(directory: &str, name: &str, pixels: &[u8], bgrx: bool) {
    let (width, height) = FIXTURE;
    let image = image::RgbImage::from_fn(width as u32, height as u32, |x, y| {
        let at = (y as usize * width + x as usize) * 4;
        let [first, second, third] = [pixels[at], pixels[at + 1], pixels[at + 2]];
        image::Rgb(if bgrx {
            [third, second, first]
        } else {
            [first, second, third]
        })
    });
    image.save(format!("{directory}/{name}.png")).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn e2e_colour_in_chrome_is_what_the_stream_signals() {
    let output = std::env::var("AMBIT_VIDEO_PROOF").ok();
    let mut state = DaemonState::new();
    command(
        &mut state,
        json!({"id":"1","action":"launch","headless":true}),
    )
    .await;
    let page = secure_page().await;
    command(&mut state, json!({"action":"navigate","url":page})).await;
    let chrome = command(
        &mut state,
        json!({"action":"evaluate","script":"navigator.userAgent"}),
    )
    .await["result"]
        .clone();
    let (width, height) = FIXTURE;
    let mut cases = Vec::new();
    for (page, scheme) in [("white", LIGHT), ("black", DARK)] {
        let source = text_fixture(scheme);
        if let Some(directory) = &output {
            save(directory, &format!("colour-drawn-{page}"), &source, true);
        }
        for codec in [VideoCodec::Av1Full, VideoCodec::Av1] {
            let case = format!("{} on the {page} page", codec.token());
            let (_, unit, codec_string) = still_unit(codec, &source);
            let script = format!(
                "({PAINT})({}, {}, {width}, {height})",
                json!(STANDARD.encode(&unit.data)),
                json!(codec_string)
            );
            let answer = command(&mut state, json!({"action":"evaluate","script":script})).await
                ["result"]
                .clone();
            assert!(answer.get("error").is_none(), "{case}: {answer}");
            assert_eq!(
                answer["colorSpace"],
                json!({"primaries":"bt709","transfer":"bt709","matrix":"bt709","fullRange":true}),
                "{case}: Chrome reads the signalled colour"
            );

            // Chrome's decoder returns exactly the samples libaom decodes.
            let reference = Decoder::new().decode(&unit.data);
            let (format, chroma_size) = match codec.chroma() {
                Chroma::Full => ("I444", (width, height)),
                Chroma::Subsampled => ("I420", (width / 2, height / 2)),
            };
            assert_eq!(answer["format"], format, "{case}");
            let samples = bytes(&answer, "samples");
            for (plane, (plane_width, plane_height)) in [(width, height), chroma_size, chroma_size]
                .into_iter()
                .enumerate()
            {
                let layout = &answer["layout"][plane];
                let offset = layout["offset"].as_u64().unwrap() as usize;
                let stride = layout["stride"].as_u64().unwrap() as usize;
                for row in 0..plane_height {
                    assert_eq!(
                        &samples[offset + row * stride..][..plane_width],
                        &reference.planes[plane][row * plane_width..][..plane_width],
                        "{case}: plane {plane} row {row}"
                    );
                }
            }

            let painted = bytes(&answer, "pixels");
            assert_eq!(painted.len(), width * height * 4, "{case}");
            let measured = check_painted(&case, scheme, codec.chroma(), &source, &painted);
            // Chrome's painter against the exact inverse of the same samples.
            let exact = reference.rgb();
            let inverse = check_painted(
                &format!("{case}, exact inverse"),
                scheme,
                codec.chroma(),
                &source,
                &exact,
            );
            let (mut differing, mut largest) = (0usize, [0u8; 3]);
            for (chrome, exact) in painted.chunks_exact(4).zip(exact.chunks_exact(4)) {
                let mut differs = false;
                for channel in 0..3 {
                    let difference = chrome[channel].abs_diff(exact[channel]);
                    largest[channel] = largest[channel].max(difference);
                    differs |= difference > 0;
                }
                differing += usize::from(differs);
            }
            if let Some(directory) = &output {
                let name = format!("colour-chrome-{}-{page}", codec.token());
                save(directory, &name, &painted, false);
            }
            cases.push(json!({
                "case": case,
                "codecString": codec_string,
                "unitBytes": unit.data.len(),
                "colorSpace": answer["colorSpace"],
                "samplesEqualLibaom": true,
                "textPsnrDb": (measured.psnr * 100.0).round() / 100.0,
                "barLargestError": measured.bars,
                "exactInverseTextPsnrDb": (inverse.psnr * 100.0).round() / 100.0,
                "exactInverseBarLargestError": inverse.bars,
                "pixelsDifferingFromExactInverse": differing,
                "largestDifferenceFromExactInverseRgb": largest,
            }));
        }
    }
    command(&mut state, json!({"action":"close"})).await;
    let report = json!({
        "chrome": chrome,
        "fixture": {"width": width, "height": height},
        "bars": ["white", "yellow", "cyan", "green", "magenta", "red", "blue", "black"],
        "cases": cases,
    });
    println!("COLOUR {report}");
    if let Some(directory) = &output {
        std::fs::write(
            format!("{directory}/colour-in-chrome.json"),
            serde_json::to_string_pretty(&report).unwrap(),
        )
        .unwrap();
    }
}
