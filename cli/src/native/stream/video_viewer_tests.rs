//! A viewer on the stream's WebSocket negotiates video, is sent units instead
//! of frames while subscribed, and returns to frames, whole, when it stops.
//! The display is played by the test: frames on its frame channel, pictures
//! through the picture channel's fake with the real encoder.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_tungstenite::tungstenite::Message;

use super::video::testing::{FakeScreen, RED};
use super::{IdleActivity, StreamServer};
use crate::native::display::{DisplayClient, TestPictures};
use crate::native::video::{encodes, VideoCodec};

type Viewer =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// What arrives on the viewer's socket.
#[derive(Debug)]
enum Received {
    Record(Value),
    /// A binary message's header.
    Binary(Value),
}

async fn receive(viewer: &mut Viewer) -> Received {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), viewer.next())
            .await
            .expect("a message")
            .expect("an open socket")
            .unwrap();
        match message {
            Message::Text(text) => return Received::Record(serde_json::from_str(&text).unwrap()),
            Message::Binary(bytes) => {
                let end = 4 + u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
                let header: Value = serde_json::from_slice(&bytes[4..end]).unwrap();
                if header["track"] == "video" {
                    // This connection negotiates whole legacy units. The
                    // transport fixture consumes a complete body; real
                    // decoder/paint acknowledgement is qualified separately.
                    assert_eq!(
                        header["byteLength"].as_u64(),
                        Some((bytes.len() - end) as u64)
                    );
                }
                return Received::Binary(header);
            }
            _ => continue,
        }
    }
}

/// The next video record, skipping everything else.
async fn video_record(viewer: &mut Viewer) -> Value {
    loop {
        if let Received::Record(record) = receive(viewer).await {
            if record["type"] == "video" {
                return record;
            }
        }
    }
}

/// The next video unit's header; a frame arriving first is a failure.
async fn video_unit(viewer: &mut Viewer) -> Value {
    loop {
        match receive(viewer).await {
            Received::Binary(header) if header["track"] == "video" => {
                send(viewer, json!({"type":"ack","track":"video","streamId":header["streamId"],"seq":header["seq"]})).await;
                return header;
            }
            Received::Binary(header) => panic!("a frame while served video: {header}"),
            Received::Record(_) => continue,
        }
    }
}

async fn send(viewer: &mut Viewer, message: Value) {
    viewer
        .send(Message::Text(message.to_string()))
        .await
        .unwrap();
}

/// The helper's frame channel: a whole frame when forced, otherwise nothing
/// changed after the capture's wait. Counts the captures asked for.
fn serve_frames(frames: tokio::net::UnixStream, asked: Arc<AtomicUsize>) {
    tokio::spawn(async move {
        let mut frames = BufReader::new(frames);
        let mut line = String::new();
        while frames.read_line(&mut line).await.is_ok_and(|read| read > 0) {
            let request: Value = serde_json::from_str(&line).unwrap();
            line.clear();
            asked.fetch_add(1, Ordering::SeqCst);
            let data = if request["force"] == true {
                json!({"changed":true,"width":2560,"height":1440,"encoding":"jpeg","data":"AA==",
                    "cursorIncluded":false,"quality":85,"timings":{"waitUs":0}})
            } else {
                let wait = request["waitMs"].as_u64().unwrap_or(0).min(100);
                tokio::time::sleep(Duration::from_millis(wait)).await;
                json!({"changed": false})
            };
            let reply = json!({"id": request["id"], "success": true, "data": data});
            if frames
                .get_mut()
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .is_err()
            {
                return;
            }
        }
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_video_viewer_is_sent_units_instead_of_frames_and_returns_to_frames_whole() {
    if !encodes(VideoCodec::Av1Full) {
        eprintln!("libaom.so.3 is absent: no encoder to serve video with");
        return;
    }
    let TestPictures {
        display,
        control: _control,
        frames,
        helper,
    } = DisplayClient::test_pictures();
    let screen = FakeScreen::new(helper);
    let asked = Arc::new(AtomicUsize::new(0));
    serve_frames(frames, asked.clone());
    let (server, _slot) = StreamServer::start_without_client(
        0,
        "video-viewer".into(),
        true,
        Arc::new(IdleActivity::new()),
    )
    .await
    .unwrap();
    server.set_display(Some(display)).await;
    let (mut viewer, _) = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{}/?frames=binary&patches=1&cursor=viewer&visible=crop&video=av1,av1-444",
        server.port()
    ))
    .await
    .unwrap();
    assert_eq!(
        video_record(&mut viewer).await,
        json!({"type":"video","state":"available","codec":"av1-444"})
    );
    // Frames flow until the viewer subscribes.
    loop {
        if let Received::Binary(header) = receive(&mut viewer).await {
            assert_eq!(header["type"], "frame");
            break;
        }
    }

    send(
        &mut viewer,
        json!({"type":"video","enabled":true,"generation":1}),
    )
    .await;
    let started = video_record(&mut viewer).await;
    assert_eq!(started["state"], "started");
    assert_eq!(started["generation"], 1);
    let first = video_unit(&mut viewer).await;
    assert_eq!(first["streamId"], started["streamId"]);
    assert_eq!(first["codecString"], started["codecString"]);
    assert_eq!(
        (first["seq"].as_u64(), first["key"].as_bool()),
        (Some(1), Some(true))
    );
    assert_eq!(first["codec"], "av1-444");
    assert_eq!(first["surface"]["cursorIncluded"], false);

    // With every viewer on video, the window's frames stop being captured.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let stopped_at = asked.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        asked.load(Ordering::SeqCst),
        stopped_at,
        "no frame captures while on video"
    );

    // Video consumption is not frame credit. A faster encoder can already
    // have queued refinement of the first picture; consume it before the
    // picture captured after this damage, retaining gap-free stream order.
    let stream = first["streamId"].clone();
    let initial_picture = first["ts"].as_u64().unwrap();
    screen.paint(100, 140, RED);
    let mut previous = first["seq"].as_u64().unwrap();
    let mut last = loop {
        let unit = video_unit(&mut viewer).await;
        assert_eq!(unit["seq"], previous + 1, "no gap");
        assert_eq!(unit["streamId"], stream);
        previous += 1;
        if unit["ts"].as_u64().unwrap() > initial_picture {
            break unit;
        }
    };

    // A key request is answered with a key unit, in the same epoch.
    send(
        &mut viewer,
        json!({"type":"video","keyframe":true,"generation":1}),
    )
    .await;
    loop {
        let unit = video_unit(&mut viewer).await;
        assert_eq!(unit["seq"], last["seq"].as_u64().unwrap() + 1, "no gap");
        last = unit;
        if last["key"] == true {
            break;
        }
    }
    assert_eq!(last["streamId"], stream);

    // Stopping is answered `stopped`, and frames resume with a whole one.
    send(
        &mut viewer,
        json!({"type":"video","enabled":false,"generation":2}),
    )
    .await;
    let mut stopped = false;
    let whole = loop {
        match receive(&mut viewer).await {
            Received::Record(record) if record["type"] == "video" => {
                assert_eq!(
                    record,
                    json!({"type":"video","state":"stopped","codec":"av1-444","generation":2})
                );
                stopped = true;
            }
            Received::Binary(header) if header["type"] == "frame" => break header,
            Received::Binary(header) => {
                assert!(!stopped, "a unit after stopped: {header}");
            }
            Received::Record(_) => {}
        }
    };
    assert!(stopped, "stopped precedes the frames");
    assert!(
        whole.get("patches").is_none() && whole.get("baseSeq").is_none(),
        "{whole}"
    );
    assert!(
        asked.load(Ordering::SeqCst) > stopped_at,
        "frames are captured again"
    );
    server.shutdown().await;
}
