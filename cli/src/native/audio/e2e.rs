//! Explicit task-owned runtime qualification; no host audio server or microphone.
use super::*;
use crate::native::actions::{execute_command, DaemonState};
use crate::native::sign_in_e2e_tests::{acquire, control, sign_in};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn command(state: &mut DaemonState, value: Value) -> Value {
    let reply = Box::pin(execute_command(&value, state)).await;
    assert_eq!(reply["success"], true, "{reply}");
    reply["data"].clone()
}

async fn wait_for_report(reports: &Mutex<Vec<String>>, report: &str, count: usize) -> bool {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if reports
                .lock()
                .unwrap()
                .iter()
                .filter(|row| row.as_str() == report)
                .count()
                >= count
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

async fn screenshot(state: &DaemonState, name: &str) {
    use crate::native::display::CaptureRequest;
    use base64::Engine;
    let Some(directory) = std::env::var_os("AUDIO_EVIDENCE_DIR") else {
        return;
    };
    let (capture, _) = state
        .window_display()
        .unwrap()
        .capture(CaptureRequest {
            cursor: false,
            budget_bytes: 3_000_000,
            force: true,
            patches: false,
            wait_ms: 0,
            cursor_identity: false,
        })
        .await
        .unwrap()
        .frame
        .unwrap();
    std::fs::write(
        std::path::Path::new(&directory).join(format!("{name}.{}", capture.encoding)),
        base64::engine::general_purpose::STANDARD
            .decode(capture.data.unwrap())
            .unwrap(),
    )
    .unwrap();
}

async fn wait_for_paint(state: &DaemonState, point: (f64, f64), rgb: [u8; 3]) {
    use crate::native::display::CaptureRequest;
    use base64::Engine;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let (capture, _) = state
                .window_display()
                .unwrap()
                .capture(CaptureRequest {
                    force: true,
                    budget_bytes: 3_000_000,
                    ..Default::default()
                })
                .await
                .unwrap()
                .frame
                .unwrap();
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(capture.data.unwrap())
                .unwrap();
            let frame = image::load_from_memory(&bytes).unwrap().to_rgb8();
            let pixel = frame.get_pixel(point.0 as u32, point.1 as u32);
            if pixel
                .0
                .iter()
                .zip(rgb)
                .all(|(observed, expected)| observed.abs_diff(expected) < 5)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the person sees the painted button before clicking it");
}

fn save_wave(name: &str, pcm: &[i16]) {
    let Some(directory) = std::env::var_os("AUDIO_EVIDENCE_DIR") else {
        return;
    };
    let data_bytes = (pcm.len() * 2) as u32;
    let mut wave = Vec::with_capacity(44 + data_bytes as usize);
    wave.extend(b"RIFF");
    wave.extend((36 + data_bytes).to_le_bytes());
    wave.extend(b"WAVEfmt ");
    wave.extend(16u32.to_le_bytes());
    wave.extend(1u16.to_le_bytes());
    wave.extend(2u16.to_le_bytes());
    wave.extend(SAMPLE_RATE.to_le_bytes());
    wave.extend((SAMPLE_RATE * 4).to_le_bytes());
    wave.extend(4u16.to_le_bytes());
    wave.extend(16u16.to_le_bytes());
    wave.extend(b"data");
    wave.extend(data_bytes.to_le_bytes());
    wave.extend(pcm.iter().flat_map(|sample| sample.to_le_bytes()));
    std::fs::write(
        std::path::Path::new(&directory).join(format!("{name}.wav")),
        wave,
    )
    .unwrap();
}

fn process_cpu_us() -> u64 {
    // CPU of this source/test process, including the test's reference decoder; not Chrome CPU.
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
        0
    );
    let usage = unsafe { usage.assume_init() };
    ((usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) * 1_000_000
        + usage.ru_utime.tv_usec
        + usage.ru_stime.tv_usec) as u64
}

fn power(samples: &[i16], frequency: f64) -> f64 {
    let n = samples.len() / 2;
    let (mut real, mut imaginary) = (0.0, 0.0);
    for (i, pair) in samples.chunks_exact(2).enumerate() {
        let phase = std::f64::consts::TAU * frequency * i as f64 / SAMPLE_RATE as f64;
        real += pair[0] as f64 * phase.cos();
        imaginary += pair[0] as f64 * phase.sin();
    }
    (real * real + imaginary * imaginary).sqrt() / n as f64
}

async fn collect(
    subscription: &mut AudioSubscription,
    millis: u64,
) -> (Vec<i16>, Vec<(u64, u64, usize)>, u32) {
    let mut samples = Vec::new();
    let mut packets = Vec::new();
    let mut priming = 0;
    let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).unwrap();
    tokio::time::timeout(Duration::from_secs(8), async {
        while samples.len() < millis as usize * 48 * 2 {
            let packet = subscription.recv().await.expect("live audio packet");
            priming = packet.format.priming_samples;
            assert_eq!(packet.format.sample_rate, SAMPLE_RATE);
            assert_eq!(packet.format.frame_samples, FRAME_SAMPLES);
            if let Some((sequence, ts, _)) = packets.last() {
                assert_eq!(packet.seq, sequence + 1);
                assert_eq!(packet.ts, ts + 10_000);
            }
            let now = crate::native::stream::monotonic_us();
            assert!(
                packet.ts.abs_diff(now) < 250_000,
                "source ts={}, receive={}, offset={}",
                packet.ts,
                now,
                packet.ts as i128 - now as i128
            );
            packets.push((packet.seq, packet.ts, packet.data.len()));
            if packet.format.codec == AudioCodec::Opus {
                let mut pcm = [0; FRAME_SAMPLES * 2];
                assert_eq!(
                    decoder.decode(&packet.data, &mut pcm, false).unwrap(),
                    FRAME_SAMPLES
                );
                samples.extend(pcm);
            } else {
                samples.extend(
                    packet
                        .data
                        .chunks_exact(2)
                        .map(|b| i16::from_le_bytes([b[0], b[1]])),
                );
            }
        }
    })
    .await
    .expect("bounded capture");
    (samples, packets, priming)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the task-owned Pulse/Chrome/display candidate image"]
async fn e2e_audio_two_sessions_page_output_and_sign_in() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let reports = Arc::new(Mutex::new(Vec::new()));
    let recorded = reports.clone();
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let recorded = recorded.clone();
            tokio::spawn(async move {
                let mut bytes = [0; 4096];
                let count = socket.read(&mut bytes).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&bytes[..count]);
                let path = text.split_whitespace().nth(1).unwrap_or("/");
                recorded.lock().unwrap().push(path.to_string());
                if path.starts_with("/report/") {
                    let _ = socket
                        .write_all(b"HTTP/1.1 204 No Content\r\nCache-Control: no-store\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    return;
                }
                let frequency = if text.starts_with("GET /b") { 880 } else { 440 };
                let body =
                    include_str!("fixture.html").replace("__FREQUENCY__", &frequency.to_string());
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    let mut a = DaemonState::new();
    let mut b = DaemonState::new();
    let started = Instant::now();
    command(
        &mut a,
        json!({"action":"navigate","url":format!("http://127.0.0.1:{port}/a")}),
    )
    .await;
    let launch_a_ms = started.elapsed().as_millis();
    command(
        &mut b,
        json!({"action":"navigate","url":format!("http://127.0.0.1:{port}/b")}),
    )
    .await;
    let source_a = a
        .browser
        .as_ref()
        .unwrap()
        .audio_source()
        .expect("qualified audio");
    let source_b = b
        .browser
        .as_ref()
        .unwrap()
        .audio_source()
        .expect("qualified audio");
    assert!(source_a.observe().ready && source_b.observe().ready);
    command(&mut a, json!({"action":"click","selector":"#play"})).await;
    command(&mut b, json!({"action":"click","selector":"#play"})).await;
    assert!(wait_for_report(&reports, "/report/playing/440", 1).await);
    assert!(wait_for_report(&reports, "/report/playing/880", 1).await);
    let (x, y, _) = crate::native::e2e_tests::window_point(&a, 320.0, 240.0).await;
    screenshot(&a, "automation-playing").await;
    let mut sub_a = source_a.subscribe(AudioCodec::Opus).unwrap();
    let mut sub_b = source_b.subscribe(AudioCodec::PcmS16le).unwrap();
    let capture_started = Instant::now();
    let capture_cpu = process_cpu_us();
    let (a_pcm, b_pcm) = tokio::join!(collect(&mut sub_a, 1200), collect(&mut sub_b, 1200));
    let capture_cpu = process_cpu_us() - capture_cpu;
    let capture_wall = capture_started.elapsed().as_micros();
    save_wave("automation-a-opus-decoded", &a_pcm.0);
    save_wave("automation-b-pcm", &b_pcm.0);
    let (a_own, a_other, b_own, b_other) = (
        power(&a_pcm.0, 440.0),
        power(&a_pcm.0, 880.0),
        power(&b_pcm.0, 880.0),
        power(&b_pcm.0, 440.0),
    );
    assert!(
        a_own > 1000.0 && a_other < a_own / 50.0,
        "session A {a_own}/{a_other}"
    );
    assert!(
        b_own > 1000.0 && b_other < b_own / 50.0,
        "session B {b_own}/{b_other}"
    );
    drop(sub_a);
    drop(sub_b);
    assert_eq!(source_a.observe().subscribers, 0);
    let mut slow = source_a.subscribe(AudioCodec::Opus).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(slow.recv().await.unwrap_err(), AudioError::Overrun);
    drop(slow);
    let mut late = source_a.subscribe(AudioCodec::Opus).unwrap();
    let resumed = collect(&mut late, 200).await;
    assert_eq!(resumed.1[0].0, 1);
    drop(late);
    let controller = acquire(&mut a).await;
    let (response, transition) = sign_in(&mut a, &controller, 1, 600_000).await;
    assert_eq!(response["success"], true, "{response}");
    screenshot(&a, "sign-in-restored").await;
    let restored = wait_for_report(&reports, "/report/loaded/440", 2).await;
    screenshot(&a, "sign-in-loaded-or-timeout").await;
    assert!(
        restored,
        "restored page did not load: {:?}",
        reports.lock().unwrap()
    );
    wait_for_paint(&a, (x, y), [34, 34, 51]).await;
    let signing_in = a.window_audio().unwrap();
    assert_eq!(
        signing_in.observe().startup_us,
        source_a.observe().startup_us
    );
    // Calibrated before handover; this actual person's input has no DevTools channel.
    let mut click = control("input", &controller);
    click["sequence"] = json!(2);
    click["expectedSurfaceGeneration"] = response["data"]["surface"]["generation"].clone();
    click["events"] = json!([
        {"type":"input_mouse","eventType":"mouseMoved","x":x,"y":y,"button":"none","buttons":0},
        {"type":"input_mouse","eventType":"mousePressed","x":x,"y":y,"button":"left","buttons":1,"clickCount":1},
        {"type":"input_mouse","eventType":"mouseReleased","x":x,"y":y,"button":"left","buttons":0,"clickCount":1}
    ]);
    command(&mut a, click).await;
    assert!(wait_for_report(&reports, "/report/playing/440", 2).await);
    wait_for_paint(&a, (x, y), [255, 255, 255]).await;
    screenshot(&a, "sign-in-after-gesture").await;
    let mut signed = signing_in.subscribe(AudioCodec::Opus).unwrap();
    let signed_pcm = collect(&mut signed, 1000).await;
    save_wave("sign-in-opus-decoded", &signed_pcm.0);
    assert!(
        power(&signed_pcm.0, 440.0) > 1000.0,
        "sign-in output must reach the retained sink: power={}",
        power(&signed_pcm.0, 440.0)
    );
    drop(signed);
    command(&mut a, control("release", &controller)).await;
    assert!(
        a.browser
            .as_ref()
            .unwrap()
            .audio_source()
            .unwrap()
            .observe()
            .ready
    );
    let observation = source_a.observe();
    command(&mut a, json!({"action":"close"})).await;
    command(&mut b, json!({"action":"close"})).await;
    assert!(!source_a.observe().ready && !source_b.observe().ready);
    server.abort();
    println!(
        "AUDIO_PROOF {}",
        json!({"status":"passed","pulse":observation,"launchAMs":launch_a_ms,"signInMs":transition.as_millis(),"aTone":a_own,"aForeignTone":a_other,"bTone":b_own,"bForeignTone":b_other,"opusPrimingSamples":a_pcm.2,"packets":a_pcm.1.len(),"maxOpusBytes":a_pcm.1.iter().map(|p|p.2).max(),"captureAndReferenceDecoderCpuUs":capture_cpu,"captureWallUs":capture_wall,"slowSubscriber":"overrun_closed","lateSubscriber":"fresh_sequence","signIn":"retained_output","shutdown":"retired"})
    );
}
