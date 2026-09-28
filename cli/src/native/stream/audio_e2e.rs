//! Real private Pulse -> native viewer socket qualification, without a relay mock.
use super::{IdleActivity, StreamServer};
use crate::native::audio::{AudioCodec, RetainedAudio, FRAME_SAMPLES, SAMPLE_RATE};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::{atomic::AtomicBool, Arc};
use std::time::Duration;
use tokio_tungstenite::{tungstenite::Message, MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(port: u16, query: &str) -> Socket {
    tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/?{query}"))
        .await
        .unwrap()
        .0
}
async fn next(socket: &mut Socket) -> (Value, Vec<u8>) {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(3), socket.next())
            .await
            .expect("bounded media delivery")
            .expect("live viewer")
            .unwrap();
        match message {
            Message::Text(text) => return (serde_json::from_str(&text).unwrap(), Vec::new()),
            Message::Binary(bytes) => {
                assert!(bytes.len() >= 4);
                let end = 4 + u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
                assert!(end <= bytes.len());
                let header: Value = serde_json::from_slice(&bytes[4..end]).unwrap();
                if header["track"] == "audio" {
                    assert!(end <= 1028 && bytes.len() - end <= 4096);
                    assert_eq!(
                        header["byteLength"].as_u64(),
                        Some((bytes.len() - end) as u64)
                    );
                    assert_eq!(header["samples"], 480);
                }
                return (header, bytes[end..].to_vec());
            }
            Message::Ping(_) | Message::Pong(_) => {}
            other => panic!("unexpected viewer message: {other:?}"),
        }
    }
}
async fn state(socket: &mut Socket, expected: &str, generation: Option<u64>) -> Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let (header, _) = next(socket).await;
            if header["type"] == "audio"
                && header["state"] == expected
                && generation.is_none_or(|generation| header["generation"] == generation)
            {
                return header;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("missing audio {expected} generation {generation:?}"))
}
async fn request(socket: &mut Socket, enabled: bool, generation: u64) {
    socket
        .send(Message::Text(
            json!({"type":"audio","enabled":enabled,"generation":generation}).to_string(),
        ))
        .await
        .unwrap();
}
async fn packet(socket: &mut Socket, stream_id: &Value) -> (Value, Vec<u8>) {
    loop {
        let (header, bytes) = next(socket).await;
        if header["type"] == "media" && header["track"] == "audio" {
            assert_eq!(
                &header["streamId"], stream_id,
                "no retired epoch crosses a new started record"
            );
            return (header, bytes);
        }
    }
}

/// Used by the actual page-tone proof to cover DaemonState's production source binding.
pub(crate) async fn capture(port: u16, millis: usize) -> Vec<i16> {
    let mut socket = connect(port, "frames=binary&audio=opus&pacing=ack&frameWindow=1").await;
    state(&mut socket, "available", None).await;
    request(&mut socket, true, 1).await;
    let started = state(&mut socket, "started", Some(1)).await;
    let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).unwrap();
    let mut samples = Vec::new();
    let mut previous = 0;
    while samples.len() < millis * 48 * 2 {
        let (header, bytes) = packet(&mut socket, &started["streamId"]).await;
        assert_eq!(header["seq"], previous + 1);
        previous += 1;
        let mut pcm = [0; FRAME_SAMPLES * 2];
        assert_eq!(
            decoder.decode(&bytes, &mut pcm, false).unwrap(),
            FRAME_SAMPLES
        );
        samples.extend(pcm);
    }
    request(&mut socket, false, 2).await;
    state(&mut socket, "stopped", Some(2)).await;
    socket.close(None).await.unwrap();
    samples
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the task-owned Pulse runtime candidate"]
async fn e2e_audio_generation_and_independent_image_flow() {
    let owner = RetainedAudio::start(&AtomicBool::new(false)).unwrap();
    let source = owner.source();
    let (server, _) = StreamServer::start_without_client(
        0,
        "audio-protocol".into(),
        true,
        Arc::new(IdleActivity::new()),
    )
    .await
    .unwrap();
    server.set_audio(Some(source.clone()));
    server.broadcast_frame(r#"{"type":"frame","seq":1,"data":"/9j/2Q=="}"#);

    let mut legacy = connect(server.port(), "").await;
    let mut initial_legacy_frame = false;
    while !initial_legacy_frame {
        let (header, _) = next(&mut legacy).await;
        assert_ne!(header["type"], "audio");
        initial_legacy_frame = header["type"] == "frame";
    }
    assert_eq!(source.observe().subscribers, 0);
    let mut completed = Vec::new();
    for (codec, name) in [
        (AudioCodec::Opus, "opus"),
        (AudioCodec::PcmS16le, "pcm-s16le"),
    ] {
        let mut socket = connect(
            server.port(),
            &format!("frames=binary&audio={name}&pacing=ack&frameWindow=1"),
        )
        .await;
        let offered = state(&mut socket, "available", None).await;
        assert_eq!(offered["codec"], name);
        assert!(offered.get("generation").is_none());
        assert_eq!(
            source.observe().subscribers,
            0,
            "an offer cannot start capture"
        );
        request(&mut socket, true, 1).await;
        let first = state(&mut socket, "started", Some(1)).await;
        assert_eq!(first["sampleRate"], 48_000);
        assert_eq!(first["channels"], 2);
        assert_eq!(first["frameSamples"], 480);
        let (first_packet, payload) = packet(&mut socket, &first["streamId"]).await;
        assert_eq!(first_packet["seq"], 1);
        if codec == AudioCodec::Opus {
            let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).unwrap();
            let mut pcm = [0; FRAME_SAMPLES * 2];
            assert_eq!(decoder.decode(&payload, &mut pcm, false).unwrap(), 480);
        } else {
            assert_eq!(first["primingSamples"], 0);
            assert_eq!(payload.len(), 1920);
        }
        // Neither the opening image nor the next image is acknowledged. Audio keeps moving.
        server.broadcast_frame(r#"{"type":"frame","seq":2,"data":"/9j/2Q=="}"#);
        let mut sequence = 1;
        let mut timestamp = first_packet["ts"].as_u64().unwrap();
        for _ in 0..10 {
            let (header, _) = packet(&mut socket, &first["streamId"]).await;
            sequence += 1;
            assert_eq!(header["seq"], sequence);
            assert_eq!(header["ts"].as_u64().unwrap(), timestamp + 10_000);
            timestamp += 10_000;
        }
        request(&mut socket, false, 2).await;
        state(&mut socket, "stopped", Some(2)).await;
        assert_eq!(source.observe().subscribers, 0);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), next(&mut socket))
                .await
                .is_err(),
            "mute leaves no queued audio after stopped"
        );
        request(&mut socket, true, 3).await;
        let second = state(&mut socket, "started", Some(3)).await;
        assert_ne!(second["streamId"], first["streamId"]);
        assert_eq!(packet(&mut socket, &second["streamId"]).await.0["seq"], 1);
        request(&mut socket, false, 2).await;
        assert_eq!(
            packet(&mut socket, &second["streamId"]).await.0["seq"],
            2,
            "stale mute cannot reset the current epoch"
        );
        request(&mut socket, false, 4).await;
        request(&mut socket, true, 5).await;
        request(&mut socket, true, 6).await;
        let latest = state(&mut socket, "started", Some(6)).await;
        assert_ne!(latest["streamId"], second["streamId"]);
        assert_eq!(packet(&mut socket, &latest["streamId"]).await.0["seq"], 1);
        request(&mut socket, true, 6).await;
        assert_eq!(
            packet(&mut socket, &latest["streamId"]).await.0["seq"],
            2,
            "duplicate generation is inert"
        );
        request(&mut socket, true, 7).await;
        let renewed = state(&mut socket, "started", Some(7)).await;
        assert_ne!(renewed["streamId"], latest["streamId"]);
        assert_eq!(packet(&mut socket, &renewed["streamId"]).await.0["seq"], 1);
        server.set_audio(None);
        server.set_audio(Some(source.clone()));
        let rebound = state(&mut socket, "started", Some(7)).await;
        assert_ne!(
            rebound["streamId"], renewed["streamId"],
            "coalesced source removal still retires the epoch"
        );
        assert_eq!(packet(&mut socket, &rebound["streamId"]).await.0["seq"], 1);
        owner.discontinue();
        let continued = state(&mut socket, "started", Some(7)).await;
        assert_ne!(continued["streamId"], rebound["streamId"]);
        assert_eq!(
            packet(&mut socket, &continued["streamId"]).await.0["seq"],
            1
        );
        socket.close(None).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while source.observe().subscribers != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("disconnect drops capture independently of JPEG ACKs");
        completed.push(name);
    }
    // Both codec cases published an image. Legacy peers may receive either or both;
    // the invariant is absence of audio, not absence of ordinary image traffic.
    let _ = tokio::time::timeout(Duration::from_millis(50), async {
        loop {
            let (header, _) = next(&mut legacy).await;
            assert_ne!(header["type"], "audio");
            assert_ne!(header["track"], "audio");
        }
    })
    .await;
    legacy.close(None).await.unwrap();

    let mut failure = connect(server.port(), "frames=binary&audio=opus&pacing=ack").await;
    state(&mut failure, "available", None).await;
    request(&mut failure, true, 1).await;
    state(&mut failure, "started", Some(1)).await;
    drop(owner);
    state(&mut failure, "unavailable", Some(1)).await;
    failure
        .send(Message::Text(json!({"type":"ack","seq":2}).to_string()))
        .await
        .unwrap();
    server.broadcast_frame(r#"{"type":"frame","seq":3,"data":"/9j/2Q=="}"#);
    loop {
        let (header, _) = next(&mut failure).await;
        if header["type"] == "frame" && header["seq"] == 3 {
            break;
        }
    }
    failure.close(None).await.unwrap();
    server.shutdown().await;
    println!(
        "AUDIO_CHANNEL_PROOF {}",
        json!({"status":"passed","codecs":completed,"audioGeneration":"strict_increase_fresh_epoch","jpegAck":"independent","mute":"queue_retired","sourceRebind":"fresh_epoch","sourceFailure":"audio_unavailable_images_continue","legacy":"unchanged","disconnect":"capture_dropped"})
    );
}
