//! The video producer on a real Chrome, display helper and X server: a viewer
//! subscribes to video, the page scrolls dense text at its own frame rate,
//! comes to rest and is refined, a dock drag resizes the window, and the
//! pointer moves over hover targets. Prints `VIDEO <json>`; with
//! `$AMBIT_VIDEO_PROOF` set to a directory, writes the numbers and the decoded
//! and true pictures there. Timings are this machine's measurements: the
//! assertions hold the contract, not a latency guarantee.
//!
//! Run optimized, as production is:
//! `cargo test --profile ci --features browser-audio e2e_native_video -- --ignored`
//! with `AGENT_BROWSER_EXECUTABLE_PATH` and `AGENT_BROWSER_DISPLAY_HELPER` set.

use std::collections::HashMap;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

use super::monotonic_us;
use super::video::measured;
use crate::native::actions::{execute_command, DaemonState};
use crate::native::video::{Decoded, Decoder};
use crate::test_utils::EnvGuard;

/// Dense text, like a documentation page: small type, links, inline code and
/// headings, generated from a fixed seed; a row of narrow hover targets at the
/// top, each crossed by one pointer step.
const PAGE: &str = r#"<!doctype html><meta charset=utf-8><style>
body{margin:0;padding:20px 28px;font:13px/1.45 system-ui,sans-serif;color:#1d1d1f;background:#fff}
h2{font-size:17px;margin:18px 0 6px;color:#111}
a{color:#0a58ca}b{color:#8a1c1c}
code{font:12px ui-monospace,monospace;background:#f1f3f5;color:#c7254e;padding:0 3px}
#targets{display:flex;gap:4px;height:40px}
.hover{width:36px;background:#e9ecef}.hover:hover{background:#ff8c00}
</style><div id=targets></div><main id=doc></main><script>
let seed=7;const rnd=()=>(seed=(seed*1103515245+12345)%2147483648)/2147483648;
const words='the of and to in is that for it as was with be by on not he this are or his from at which but have an they you were her she there one all we their has would when been if more will who no out so can said what up its about into them than some could time these two may then do first any my now such like other how our over also back after use well way even new want because day most us service window pointer video frame picture encoder stream viewer display render layout scroll'.split(' ');
const para=n=>Array.from({length:n},(_,i)=>{const w=words[Math.floor(rnd()*words.length)];const r=rnd();return r<.05?`<a href=#>${w}</a>`:r<.08?`<b>${w}</b>`:r<.1?`<code>${w}_${i}</code>`:w}).join(' ');
let html='';for(let s=0;s<60;s++){html+=`<h2>Section ${s+1}: ${para(5)}</h2>`;for(let p=0;p<6;p++)html+=`<p>${para(70+Math.floor(rnd()*50))}</p>`}
doc.innerHTML=html;
targets.innerHTML='<span class=hover></span>'.repeat(40);
</script>"#;

/// What the viewer received, stamped at arrival on the media clock.
#[derive(Debug)]
enum Seen {
    Record(Value, u64),
    Unit(Value, Vec<u8>, u64),
    Frame(Value, u64),
}

impl Seen {
    fn at(&self) -> u64 {
        match self {
            Self::Record(_, at) | Self::Unit(_, _, at) | Self::Frame(_, at) => *at,
        }
    }
}

/// The viewer: every video unit is acknowledged as painted on arrival, so
/// the producer's pacing, not this client, sets the rate.
fn viewer(
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> (mpsc::UnboundedSender<Value>, mpsc::UnboundedReceiver<Seen>) {
    let (outgoing, mut to_send) = mpsc::unbounded_channel::<Value>();
    let (seen, received) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let (mut sink, mut stream) = socket.split();
        loop {
            tokio::select! {
                message = to_send.recv() => {
                    let Some(message) = message else { return };
                    if sink.send(Message::Text(message.to_string())).await.is_err() { return; }
                }
                message = stream.next() => {
                    let Some(Ok(message)) = message else { return };
                    let at = monotonic_us();
                    match message {
                        Message::Text(text) => {
                            let _ = seen.send(Seen::Record(serde_json::from_str(&text).unwrap(), at));
                        }
                        Message::Binary(bytes) => {
                            let end = 4 + u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
                            let header: Value = serde_json::from_slice(&bytes[4..end]).unwrap();
                            if header["track"] == "video" {
                                let ack = json!({"type":"ack","track":"video",
                                    "streamId":header["streamId"],"seq":header["seq"]});
                                if sink.send(Message::Text(ack.to_string())).await.is_err() { return; }
                                let _ = seen.send(Seen::Unit(header, bytes[end..].to_vec(), at));
                            } else {
                                let _ = seen.send(Seen::Frame(header, at));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    });
    (outgoing, received)
}

/// Everything seen within `window`.
async fn gather(received: &mut mpsc::UnboundedReceiver<Seen>, window: Duration) -> Vec<Seen> {
    let deadline = tokio::time::Instant::now() + window;
    let mut all = Vec::new();
    while let Ok(Some(seen)) = tokio::time::timeout_at(deadline, received.recv()).await {
        all.push(seen);
    }
    all
}

/// Seen things until one matches, within five seconds.
async fn until(
    received: &mut mpsc::UnboundedReceiver<Seen>,
    all: &mut Vec<Seen>,
    matches: impl Fn(&Seen) -> bool,
) {
    let from = all.len();
    let found = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let seen = received.recv().await.expect("the viewer is open");
            let found = matches(&seen);
            all.push(seen);
            if found {
                return;
            }
        }
    })
    .await;
    if found.is_err() {
        let seen: Vec<String> = all[from..]
            .iter()
            .map(|seen| match seen {
                Seen::Record(record, _) => record.to_string(),
                Seen::Unit(header, _, _) | Seen::Frame(header, _) => {
                    let mut header = header.clone();
                    header.as_object_mut().unwrap().remove("surface");
                    header.to_string()
                }
            })
            .collect();
        panic!("the awaited message did not come; seen: {seen:#?}");
    }
}

fn is_final(seen: &Seen) -> bool {
    matches!(seen, Seen::Unit(header, _, _) if header["quality"] == "final")
}

/// Takes what comes until the picture is at rest: the newest unit is the
/// refinement, and nothing has followed it for 300 ms.
async fn rest(received: &mut mpsc::UnboundedReceiver<Seen>, all: &mut Vec<Seen>) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut more = gather(received, Duration::from_millis(300)).await;
            let quiet = !more.iter().any(|seen| matches!(seen, Seen::Unit(..)));
            all.append(&mut more);
            let refined = all
                .iter()
                .rev()
                .find(|seen| matches!(seen, Seen::Unit(..)))
                .is_some_and(is_final);
            if quiet && refined {
                return;
            }
        }
    })
    .await
    .expect("the picture comes to rest");
}

fn stats(values: &[f64]) -> Value {
    if values.is_empty() {
        return json!({"n": 0});
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let at = |share: usize| sorted[(sorted.len() * share / 100).min(sorted.len() - 1)];
    json!({"n": sorted.len(), "min": sorted[0], "p50": at(50), "p95": at(95),
        "max": sorted[sorted.len() - 1],
        "mean": sorted.iter().sum::<f64>() / sorted.len() as f64})
}

fn ms(micros: u64) -> f64 {
    micros as f64 / 1000.0
}

/// RGB PSNR of `decoded` (the window at its top-left) against `truth`
/// (RGBA rows), over `truth`'s area placed at `origin` in the window.
fn psnr(decoded: &[u8], width: usize, truth: &image::RgbaImage, origin: (usize, usize)) -> f64 {
    let mut error = 0f64;
    let mut samples = 0f64;
    for (x, y, pixel) in truth.enumerate_pixels() {
        let at = ((origin.1 + y as usize) * width + origin.0 + x as usize) * 4;
        for channel in 0..3 {
            let difference = f64::from(decoded[at + channel]) - f64::from(pixel.0[channel]);
            error += difference * difference;
            samples += 1.0;
        }
    }
    10.0 * (255.0 * 255.0 / (error / samples).max(1e-9)).log10()
}

/// `width` x `height` of a decoded picture (RGB-and-padding rows), from
/// `origin`, as an image.
fn crop(rgb: &[u8], stride: usize, origin: (usize, usize), size: (u32, u32)) -> image::RgbImage {
    image::RgbImage::from_fn(size.0, size.1, |x, y| {
        let at = ((origin.1 + y as usize) * stride + origin.0 + x as usize) * 4;
        image::Rgb([rgb[at], rgb[at + 1], rgb[at + 2]])
    })
}

/// The contract every hop checks, over one run of units: per stream id, the
/// sequence starts at 1 with a key unit and never skips, `ts` never
/// decreases, the coded size changes only on a key unit, and only key units
/// name the codec string; the window lies inside the coded picture and the
/// pointer is never drawn in.
fn assert_contract(units: &[&Value]) -> usize {
    let mut streams: HashMap<String, (u64, u64, Value)> = HashMap::new();
    for header in units {
        let id = header["streamId"].as_str().unwrap().to_string();
        let (seq, ts, key) = (
            header["seq"].as_u64().unwrap(),
            header["ts"].as_u64().unwrap(),
            header["key"].as_bool().unwrap(),
        );
        assert_eq!(key, header.get("codecString").is_some(), "{header}");
        assert_eq!(header["surface"]["cursorIncluded"], false, "{header}");
        let (coded, visible) = (&header["coded"], &header["visible"]);
        assert!(
            visible["width"].as_u64() <= coded["width"].as_u64()
                && visible["height"].as_u64() <= coded["height"].as_u64(),
            "{header}"
        );
        match streams.get_mut(&id) {
            None => {
                assert!(
                    seq == 1 && key,
                    "a stream begins at 1 with a key unit: {header}"
                );
                streams.insert(id, (seq, ts, coded.clone()));
            }
            Some((last, last_ts, last_coded)) => {
                assert_eq!(seq, *last + 1, "no gap: {header}");
                assert!(ts >= *last_ts, "ts never decreases: {header}");
                assert!(
                    key || coded == last_coded,
                    "coded changes on a key: {header}"
                );
                (*last, *last_ts, *last_coded) = (seq, ts, coded.clone());
            }
        }
    }
    streams.len()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn e2e_native_video_producer_proof() {
    let env = EnvGuard::new(&["AGENT_BROWSER_WINDOW_STREAM", "DISPLAY"]);
    env.set("AGENT_BROWSER_WINDOW_STREAM", "1");
    env.set("DISPLAY", "");
    let output = std::env::var("AMBIT_VIDEO_PROOF").ok();
    let mut state = DaemonState::new();
    async fn command(state: &mut DaemonState, command: Value) -> Value {
        let response = Box::pin(execute_command(&command, state)).await;
        assert_eq!(response["success"], true, "{command}: {response}");
        response["data"].clone()
    }
    let port = command(&mut state, json!({"action":"stream_enable","port":0})).await["port"]
        .as_u64()
        .unwrap();
    command(
        &mut state,
        json!({"action":"navigate","url":format!("data:text/html,{}",urlencoding::encode(PAGE))}),
    )
    .await;
    let mut request = format!(
        "ws://127.0.0.1:{port}/?frames=binary&patches=1&cursor=viewer&visible=crop\
         &video=av1-444,av1&width=920&height=944"
    )
    .into_client_request()
    .unwrap();
    request.headers_mut().insert(
        "X-Ambit-Browser-Viewer",
        uuid::Uuid::new_v4().to_string().parse().unwrap(),
    );
    let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let (send, mut received) = viewer(socket);
    let mut all = Vec::new();

    // Video is offered before any frame; then the presenter's window is
    // laid out.
    until(&mut received, &mut all, |seen| {
        matches!(seen, Seen::Record(record, _) if record["type"] == "video" && record["state"] == "available")
    })
    .await;
    until(
        &mut received,
        &mut all,
        |seen| matches!(seen, Seen::Frame(header, _) if header["visible"]["width"] == 1840),
    )
    .await;
    measured::take();

    // Subscribe: `started`, a key unit, then the still picture refined.
    let subscribed = monotonic_us();
    send.send(json!({"type":"video","enabled":true,"generation":1}))
        .unwrap();
    until(&mut received, &mut all, is_final).await;
    rest(&mut received, &mut all).await;
    let started = all
        .iter()
        .find_map(|seen| match seen {
            Seen::Record(record, at) if record["state"] == "started" => Some((record.clone(), *at)),
            _ => None,
        })
        .expect("started");
    let key = all
        .iter()
        .find_map(|seen| match seen {
            Seen::Unit(header, data, at) => Some((header.clone(), data.len(), *at)),
            _ => None,
        })
        .unwrap();
    assert_eq!(started.0["codec"], "av1-444");
    assert_eq!(key.0["key"], true);
    let first_final = all.iter().rev().find(|seen| is_final(seen)).unwrap().at();
    let (encoded, _) = measured::take();
    let subscription = json!({
        "startedMs": ms(started.1 - subscribed),
        "firstUnitMs": ms(key.2 - subscribed),
        "keyBytes": key.1,
        "keyEncodeMs": encoded.iter().find(|each| each.key).map(|each| each.encode.as_secs_f64() * 1000.0),
        "stillRefinedMs": ms(first_final - key.0["ts"].as_u64().unwrap()),
        "coded": key.0["coded"],
        "visible": key.0["visible"],
        "codecString": key.0["codecString"],
    });

    // Scroll dense text at the page's own frame rate: 40 px a frame, 3 s.
    let scroll_from = monotonic_us();
    command(&mut state, json!({"action":"evaluate","script":
        "window.__scroll=new Promise(done=>{const t0=performance.now();let frames=0;\
         (function step(t){scrollBy(0,40);frames++;if(t-t0<3000)requestAnimationFrame(step);else done(frames)})(t0)});true"}))
    .await;
    let mut scrolled = gather(&mut received, Duration::from_millis(3300)).await;
    let page_frames = command(
        &mut state,
        json!({"action":"evaluate","script":"window.__scroll"}),
    )
    .await["result"]
        .as_u64()
        .unwrap();
    let scroll_to = monotonic_us();
    rest(&mut received, &mut scrolled).await;
    let (encoded, converted) = measured::take();
    let motion: Vec<(&Value, usize, u64)> = scrolled
        .iter()
        .filter_map(|seen| match seen {
            Seen::Unit(header, data, at) if header["quality"] == "motion" => {
                Some((header, data.len(), *at))
            }
            _ => None,
        })
        .filter(|(header, _, _)| (scroll_from..scroll_to).contains(&header["ts"].as_u64().unwrap()))
        .collect();
    let span =
        ms(motion.last().unwrap().0["ts"].as_u64().unwrap() - motion[0].0["ts"].as_u64().unwrap())
            / 1000.0;
    // The still picture is the last one before the refinement: the scroll's
    // last frame, or whatever the page drew after it.
    let (mut last_motion, mut refined) = (0, None);
    for seen in &scrolled {
        if let Seen::Unit(header, data, at) = seen {
            match header["quality"].as_str() {
                Some("motion") => last_motion = header["ts"].as_u64().unwrap(),
                Some("final") => refined = Some((header, data.len(), *at)),
                _ => {}
            }
        }
    }
    let (refined_header, refined_bytes, refined_at) = refined.unwrap();
    assert_eq!(
        refined_header["ts"].as_u64(),
        Some(last_motion),
        "the last picture refined"
    );
    assert_eq!(refined_header["key"], false);
    let encode_ms = |quality: &str| -> Vec<f64> {
        encoded
            .iter()
            .filter(|each| each.quality == quality && !each.key)
            .map(|each| each.encode.as_secs_f64() * 1000.0)
            .collect()
    };
    let helper_us = |field: &str| -> Vec<f64> {
        converted
            .iter()
            .filter_map(|each| each.helper.as_ref()?.get(field)?.as_f64())
            .map(|micros| micros / 1000.0)
            .collect()
    };
    let scroll = json!({
        "pageFramesPerSecond": page_frames as f64 / 3.0,
        "pictures": motion.len(),
        "picturesPerSecond": motion.len() as f64 / span,
        "bytesPerSecond": motion.iter().map(|(_, bytes, _)| *bytes).sum::<usize>() as f64 / span,
        "bytesPerPicture": stats(&motion.iter().map(|(_, bytes, _)| *bytes as f64).collect::<Vec<_>>()),
        "encodeMs": stats(&encode_ms("motion")),
        "convertMs": stats(&converted.iter().map(|each| each.convert.as_secs_f64() * 1000.0).collect::<Vec<_>>()),
        "helperFetchMs": stats(&helper_us("fetchUs")),
        "helperCopyMs": stats(&helper_us("copyUs")),
        "captureToArrivalMs": stats(&motion.iter().map(|(header, _, at)| ms(at - header["ts"].as_u64().unwrap())).collect::<Vec<_>>()),
    });
    let refinement = json!({
        "lastDamageToFinalArrivalMs": ms(refined_at - last_motion),
        "finalBytes": refined_bytes,
        "finalEncodeMs": stats(&encode_ms("final")),
    });
    all.append(&mut scrolled);

    // The still, decoded, against the true pixels: the framebuffer itself,
    // read through the picture channel while the page is at rest.
    let units: Vec<&Value> = all
        .iter()
        .filter_map(|seen| match seen {
            Seen::Unit(header, _, _) => Some(header),
            _ => None,
        })
        .collect();
    assert_contract(&units);
    let mut decoder = Decoder::new();
    let (mut motion_still, mut final_still): (Option<Decoded>, Option<Decoded>) = (None, None);
    for seen in &all {
        if let Seen::Unit(header, data, _) = seen {
            let decoded = decoder.decode(data);
            if header["ts"].as_u64() == Some(last_motion) {
                if header["quality"] == "motion" {
                    motion_still = Some(decoded);
                } else {
                    final_still = Some(decoded);
                }
            }
        }
    }
    let (motion_still, final_still) = (motion_still.unwrap(), final_still.unwrap());
    let display = state.browser.as_ref().unwrap().display_client().unwrap();
    let whole = crate::native::display::pictures::PictureRequest {
        cursor: false,
        force: true,
        wait_ms: 0,
        cursor_identity: false,
    };
    let (framebuffer, pixels) = display
        .pictures()
        .expect("the helper serves pictures")
        .picture(whole, |reply, pixels| (reply.clone(), pixels.to_vec()))
        .unwrap()
        .picture
        .unwrap();
    let window = framebuffer.window();
    let (window_width, window_height) = (window.width as usize, window.height as usize);
    let truth = image::RgbaImage::from_fn(window.width, window.height, |x, y| {
        let at = y as usize * framebuffer.stride as usize + x as usize * 4;
        image::Rgba([pixels[at + 2], pixels[at + 1], pixels[at], 255])
    });
    let width = final_still.width as usize;
    let (final_rgb, motion_rgb) = (final_still.rgb(), motion_still.rgb());
    // Dense text in the page, below the browser's own toolbar.
    let text = (80, 600);
    let text_truth = image::imageops::crop_imm(&truth, 80, 600, 640, 360).to_image();
    let final_text = psnr(&final_rgb, width, &text_truth, text);
    assert!(
        final_text >= 47.0,
        "the still target: dense text at {final_text:.1} dB RGB PSNR"
    );
    let still = json!({
        "window": [window_width, window_height],
        "finalRgbPsnrDb": psnr(&final_rgb, width, &truth, (0, 0)),
        "motionRgbPsnrDb": psnr(&motion_rgb, width, &truth, (0, 0)),
        "finalTextRgbPsnrDb": final_text,
        "motionTextRgbPsnrDb": psnr(&motion_rgb, width, &text_truth, text),
        "chroma": format!("{:?}", final_still.chroma),
    });
    if let Some(directory) = &output {
        let directory = std::path::Path::new(directory);
        std::fs::create_dir_all(directory).unwrap();
        crop(&final_rgb, width, (0, 0), (window.width, window.height))
            .save(directory.join("decoded-final-window.png"))
            .unwrap();
        truth.save(directory.join("true-window.png")).unwrap();
        crop(&final_rgb, width, text, (640, 360))
            .save(directory.join("text-decoded-final.png"))
            .unwrap();
        crop(&motion_rgb, width, text, (640, 360))
            .save(directory.join("text-decoded-motion.png"))
            .unwrap();
        text_truth.save(directory.join("text-true.png")).unwrap();
    }

    // A dock drag: 300 CSS px out and back, one step a frame, three times.
    command(
        &mut state,
        json!({"action":"evaluate","script":"scrollTo(0,0);true"}),
    )
    .await;
    let mut settled = gather(&mut received, Duration::from_millis(500)).await;
    all.append(&mut settled);
    measured::take();
    let dragged_from = monotonic_us();
    let mut steps = Vec::new();
    for pass in 0..6 {
        for step in 0..=30u64 {
            let width = if pass % 2 == 0 {
                920 + step * 10
            } else {
                1220 - step * 10
            };
            send.send(json!({"type":"presentation","width":width,"height":944}))
                .unwrap();
            steps.push((monotonic_us(), width * 2));
            tokio::time::sleep(Duration::from_millis(16)).await;
        }
    }
    let released = monotonic_us();
    let mut drag = gather(&mut received, Duration::from_millis(1500)).await;
    let (drag_encoded, _) = measured::take();
    let drag_units: Vec<(&Value, u64)> = drag
        .iter()
        .filter_map(|seen| match seen {
            Seen::Unit(header, _, at) => Some((header, *at)),
            _ => None,
        })
        .collect();
    let during: Vec<&(&Value, u64)> = drag_units
        .iter()
        .filter(|(header, _)| {
            (dragged_from..released).contains(&header["ts"].as_u64().unwrap())
                && header["quality"] == "motion"
        })
        .collect();
    // Each size the pictures showed, from the newest presentation of it
    // sent before the picture was read.
    let mut shown_after = Vec::new();
    let mut previous = 1840;
    for (header, at) in &drag_units {
        let (width, ts) = (
            header["visible"]["width"].as_u64().unwrap(),
            header["ts"].as_u64().unwrap(),
        );
        if width != previous {
            previous = width;
            if let Some((sent, _)) = steps
                .iter()
                .rev()
                .find(|(sent, asked)| *asked == width && *sent < ts)
            {
                shown_after.push(ms(at - sent));
            }
        }
    }
    let final_size = drag_units
        .iter()
        .find(|(header, at)| *at > released && header["visible"]["width"] == 1840);
    let drag_json = json!({
        "presentations": steps.len(),
        "keyUnits": drag_units.iter().filter(|(header, _)| header["key"] == true).count(),
        "keyUnitCodedSizes": drag_units.iter().filter(|(header, _)| header["key"] == true)
            .map(|(header, _)| header["coded"].clone()).collect::<Vec<_>>(),
        "pictures": during.len(),
        "picturesPerSecond": during.len() as f64 / ((released - dragged_from) as f64 / 1e6),
        "sizesShown": shown_after.len(),
        "presentationToPictureMs": stats(&shown_after),
        "releaseToFinalSizeMs": final_size.map(|(_, at)| ms(at - released)),
        "encodeMs": stats(&drag_encoded.iter().filter(|each| each.quality == "motion").map(|each| each.encode.as_secs_f64() * 1000.0).collect::<Vec<_>>()),
    });
    all.append(&mut drag);

    // The agent's pointer over the hover targets: one step a frame, each
    // into the next target, through the helper as a glide's samples are.
    let (x0, y0, _) = crate::native::e2e_tests::window_point(&state, 30.0, 40.0).await;
    let mut settled = gather(&mut received, Duration::from_millis(500)).await;
    all.append(&mut settled);
    measured::take();
    let moving_from = monotonic_us();
    let mut rpc = Vec::new();
    for step in 0..60u32 {
        // Twenty targets out, twenty back, twenty out.
        let target = if (step / 20) % 2 == 0 {
            step % 20
        } else {
            20 - step % 20
        };
        let started = std::time::Instant::now();
        display
            .input(&[json!({"type":"input_mouse","eventType":"mouseMoved",
                "x": x0 + f64::from(target) * 80.0, "y": y0, "modifiers": 0})])
            .await
            .unwrap();
        rpc.push(started.elapsed().as_secs_f64() * 1000.0);
        tokio::time::sleep(Duration::from_millis(16)).await;
    }
    let moved_until = monotonic_us();
    let mut hover = gather(&mut received, Duration::from_millis(500)).await;
    let (hover_encoded, _) = measured::take();
    let hover_pictures = hover
        .iter()
        .filter(|seen| {
            matches!(seen, Seen::Unit(header, _, _) if header["quality"] == "motion"
                && (moving_from..moved_until).contains(&header["ts"].as_u64().unwrap()))
        })
        .count();
    let motion_json = json!({
        "moves": rpc.len(),
        "helperInputRpcMs": stats(&rpc),
        "pictures": hover_pictures,
        "picturesPerSecond": hover_pictures as f64 / ((moved_until - moving_from) as f64 / 1e6),
        "encodeMs": stats(&hover_encoded.iter().filter(|each| each.quality == "motion").map(|each| each.encode.as_secs_f64() * 1000.0).collect::<Vec<_>>()),
    });
    all.append(&mut hover);

    // A person takes control: the window is the one the viewer paints, so
    // its generation stays and nothing is sent again for it.
    rest(&mut received, &mut all).await;
    let painted = all
        .iter()
        .rev()
        .find_map(|seen| match seen {
            Seen::Unit(header, _, _) => Some(header["surface"]["generation"].clone()),
            _ => None,
        })
        .unwrap();
    let controller = uuid::Uuid::new_v4().to_string();
    let expires = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 30_000;
    let acquired = command(
        &mut state,
        json!({"action":"ambit_browser_control","op":"acquire","controllerId":controller,"expiresAt":expires}),
    )
    .await;
    let mut controlled = gather(&mut received, Duration::from_millis(500)).await;
    let after_acquire = controlled
        .iter()
        .filter(|seen| matches!(seen, Seen::Unit(..) | Seen::Frame(..)))
        .count();
    assert_eq!(
        acquired["surface"]["generation"], painted,
        "the generation stays"
    );
    assert_eq!(after_acquire, 0, "taking control sends no picture again");
    command(
        &mut state,
        json!({"action":"ambit_browser_control","op":"release","controllerId":controller}),
    )
    .await;
    let take_control = json!({
        "generationKept": true,
        "picturesWithin500Ms": after_acquire,
    });
    all.append(&mut controlled);

    let units: Vec<&Value> = all
        .iter()
        .filter_map(|seen| match seen {
            Seen::Unit(header, _, _) => Some(header),
            _ => None,
        })
        .collect();
    let streams = assert_contract(&units);
    assert_eq!(streams, 1, "one epoch throughout: no resynchronization");
    let frames_while_video = all
        .iter()
        .filter(|seen| matches!(seen, Seen::Frame(_, at) if *at > key.2))
        .count();
    assert_eq!(frames_while_video, 0, "no frames while served video");
    let report = json!({
        "subscription": subscription,
        "scroll": scroll,
        "refinement": refinement,
        "still": still,
        "drag": drag_json,
        "motion": motion_json,
        "takeControl": take_control,
        "units": units.len(),
    });
    println!("VIDEO {report}");
    if let Some(directory) = &output {
        std::fs::write(
            std::path::Path::new(directory).join("e2e-video-proof.json"),
            serde_json::to_string_pretty(&report).unwrap(),
        )
        .unwrap();
    }
    drop(send);
    command(&mut state, json!({"action":"close"})).await;
}
