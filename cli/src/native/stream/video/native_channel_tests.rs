//! Actual StreamServer/WS, native codec and shared-memory boundary. The helper
//! is a protocol fixture; these tests do not claim real X11/browser acceptance.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use crate::native::display::DisplayClient;
use crate::native::stream::{IdleActivity, StreamServer};

fn noise(width: u32, height: u32) -> Vec<u8> {
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
    let mut random = 0x01234567u32;
    for _ in 0..width * height {
        for _ in 0..3 {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            pixels.push(random as u8);
        }
        pixels.push(0);
    }
    pixels
}

type Viewer =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn next(viewer: &mut Viewer) -> (Value, Vec<u8>) {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), viewer.next())
            .await
            .expect("native writer progress")
            .expect("open WS")
            .expect("valid WS");
        match message {
            Message::Text(text) => return (serde_json::from_str(&text).unwrap(), Vec::new()),
            Message::Binary(bytes) => {
                let end = 4 + u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
                return (
                    serde_json::from_slice(&bytes[4..end]).unwrap(),
                    bytes[end..].to_vec(),
                );
            }
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn presentation_retires_a_native_partial_picture_before_any_paint_or_byte_progress() {
    assert!(crate::native::video::encodes(
        crate::native::video::VideoCodec::Av1Full
    ));
    let crate::native::display::TestPictures {
        display,
        control,
        frames,
        mut helper,
    } = DisplayClient::test_pictures();
    let geometry = Arc::new(Mutex::new((640u32, 480u32)));
    let controlled = geometry.clone();
    let control_task = tokio::spawn(async move {
        let mut peer = BufReader::new(control);
        let mut line = String::new();
        while peer.read_line(&mut line).await.unwrap() > 0 {
            let request: Value = serde_json::from_str(&line).unwrap();
            line.clear();
            if request["op"] == "resize" {
                *controlled.lock().unwrap() = (
                    request["width"].as_u64().unwrap() as u32,
                    request["height"].as_u64().unwrap() as u32,
                );
            }
            let (width, height) = *controlled.lock().unwrap();
            let data = json!({"width":width,"height":height,"focusWindow":7,
                "features":["captureWait","cursorIdentity","pictures"],
                "windows":[{"id":7,"pid":1,"x":0,"y":0,"width":width,"height":height,
                    "mapped":true,"focused":true,"overrideRedirect":false,"windowType":"normal"}]});
            let reply = json!({"id":request["id"],"success":true,"data":data});
            if peer
                .get_mut()
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let frame_task = tokio::spawn(async move {
        let mut peer = BufReader::new(frames);
        let mut line = String::new();
        while peer.read_line(&mut line).await.unwrap() > 0 {
            let request: Value = serde_json::from_str(&line).unwrap();
            line.clear();
            let reply = json!({"id":request["id"],"success":true,"data":{
                "changed":true,"width":640,"height":480,"encoding":"jpeg","data":"AA==",
                "cursorIncluded":request["cursor"],"quality":85}});
            if peer
                .get_mut()
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let picture_geometry = geometry.clone();
    let picture_thread = std::thread::spawn(move || {
        let mut previous = None;
        while let Some(request) = helper.next_request() {
            let window = *picture_geometry.lock().unwrap();
            if previous != Some(window) || request["force"] == true {
                let pixels = noise(window.0, window.1);
                helper.paint_image(window.0, window.1, &pixels);
                helper.answer(
                    &request,
                    json!({"changed":true,"width":window.0,"height":window.1,
                    "stride":window.0*4,"rows":[[0,window.1]],"cursorIncluded":false}),
                );
                previous = Some(window);
            } else {
                helper.answer(&request, json!({"changed":false}));
            }
        }
    });
    {
        let layout = display.layout().await;
        display
            .resize(&layout, 640, 480, Some(7), false)
            .await
            .unwrap();
    }
    let (server, _) = StreamServer::start_without_client(
        0,
        "chunks-layout".into(),
        true,
        Arc::new(IdleActivity::new()),
    )
    .await
    .unwrap();
    server.set_display(Some(display.clone())).await;
    let viewer_id = uuid::Uuid::new_v4();
    let target = format!("ws://127.0.0.1:{}/?frames=binary&cursor=viewer&visible=crop&video=av1-444&videoCapacity=coded&videoFraming=chunks&width=320&height=240", server.port());
    let mut request = target.into_client_request().unwrap();
    request.headers_mut().insert(
        "x-ambit-browser-viewer",
        viewer_id.to_string().parse().unwrap(),
    );
    let (mut viewer, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    loop {
        let (record, _) = next(&mut viewer).await;
        if record["type"] == "video" && record["state"] == "available" {
            break;
        }
    }
    viewer
        .send(Message::Text(
            json!({"type":"video","enabled":true,"generation":1}).to_string(),
        ))
        .await
        .unwrap();
    let old_stream = loop {
        let (record, body) = next(&mut viewer).await;
        if record["type"] == "media" && record["track"] == "video" {
            assert_eq!(record["offset"], 0);
            assert!(record["byteLength"].as_u64().unwrap() > 3072);
            assert!(!body.is_empty() && body.len() <= 1024);
            break record["streamId"].as_str().unwrap().to_owned();
        }
    };
    // A secondary B viewer declares no presenter identity. It still follows
    // the same applied window and must retire its own held first picture.
    let target = format!("ws://127.0.0.1:{}/?frames=binary&cursor=viewer&visible=crop&video=av1-444&videoCapacity=coded&videoFraming=chunks", server.port());
    let (mut secondary, _) = tokio_tungstenite::connect_async(target).await.unwrap();
    loop {
        let (record, _) = next(&mut secondary).await;
        if record["type"] == "video" && record["state"] == "available" {
            break;
        }
    }
    secondary
        .send(Message::Text(
            json!({"type":"video","enabled":true,"generation":1}).to_string(),
        ))
        .await
        .unwrap();
    let old_secondary = loop {
        let (record, _) = next(&mut secondary).await;
        if record["type"] == "media" && record["track"] == "video" {
            assert_eq!(record["offset"], 0);
            break record["streamId"].as_str().unwrap().to_owned();
        }
    };
    // No received-prefix or paint ACK exists. The live native writer must
    // still publish records and apply the real in-band presentation request.
    server
        .frame_tx
        .send(json!({"type":"activity","sample":"during-partial"}).to_string())
        .unwrap();
    loop {
        let (record, _) = next(&mut viewer).await;
        if record["type"] == "activity" {
            assert_eq!(record["sample"], "during-partial");
            break;
        }
    }
    let presentation_at = std::time::Instant::now();
    viewer
        .send(Message::Text(
            json!({"type":"presentation","width":160,"height":120}).to_string(),
        ))
        .await
        .unwrap();
    let mut successor_part_ms = None;
    let result = tokio::time::timeout(Duration::from_secs(2), async {
        let mut applied = false;
        let mut started = None;
        loop {
            let (record, body) = next(&mut viewer).await;
            if record["type"] == "presentation" && record["applied"]["width"] == 320 {
                applied = true;
            }
            if record["type"] == "video"
                && record["state"] == "started"
                && record["streamId"] != old_stream
            {
                assert!(applied);
                assert_eq!(record["generation"], 1);
                started = Some(record["streamId"].clone());
            }
            if record["type"] == "media"
                && record["track"] == "video"
                && record["streamId"] != old_stream
            {
                assert!(applied, "applied geometry precedes successor pixels");
                assert_eq!(started.as_ref(), Some(&record["streamId"]));
                assert_eq!(record["seq"], 1);
                assert_eq!(record["key"], true);
                assert_eq!(record["offset"], 0);
                assert_eq!(record["visible"]["width"], 320);
                assert_eq!(record["visible"]["height"], 240);
                assert!(!body.is_empty());
                successor_part_ms = Some(presentation_at.elapsed().as_secs_f64() * 1000.0);
                return;
            }
        }
    })
    .await;
    let secondary_result = tokio::time::timeout(Duration::from_secs(2), async {
        let mut started = None;
        loop {
            let (record, _) = next(&mut secondary).await;
            if record["type"] == "video"
                && record["state"] == "started"
                && record["streamId"] != old_secondary
            {
                assert_eq!(record["generation"], 1);
                started = Some(record["streamId"].clone());
            }
            if record["type"] == "media"
                && record["track"] == "video"
                && record["streamId"] != old_secondary
            {
                assert_eq!(started.as_ref(), Some(&record["streamId"]));
                assert_eq!(record["seq"], 1);
                assert_eq!(record["offset"], 0);
                assert_eq!(record["visible"]["width"], 320);
                assert_eq!(record["visible"]["height"], 240);
                return;
            }
        }
    })
    .await;
    let applied_window = display.window();
    println!(
        "NATIVE_B_PRESENTATION {}",
        json!({"appliedWindow":applied_window,
        "layoutEpoch":display.layout_epoch(),"successorBeforePaint":result.is_ok(),
        "secondarySuccessorBeforePaint":secondary_result.is_ok(),"successorFirstPartMs":successor_part_ms})
    );
    let _ = viewer.close(None).await;
    let _ = secondary.close(None).await;
    server.shutdown().await;
    drop(server);
    drop(display);
    control_task.abort();
    frame_task.abort();
    let _ = control_task.await;
    let _ = frame_task.await;
    picture_thread.join().unwrap();
    assert_eq!(
        applied_window,
        (320, 240),
        "the native layout itself applied"
    );
    assert!(
        result.is_ok(),
        "applied presentation did not retire held partial before paint"
    );
    assert!(
        secondary_result.is_ok(),
        "nonpresenting B viewer also retires obsolete partial"
    );
}
