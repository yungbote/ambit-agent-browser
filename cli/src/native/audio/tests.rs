use super::*;

fn packet(seq: u64) -> AudioPacket {
    AudioPacket {
        format: Arc::new(AudioFormat {
            stream_id: "test".into(),
            codec: AudioCodec::PcmS16le,
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            frame_samples: FRAME_SAMPLES,
            priming_samples: 0,
        }),
        seq,
        ts: seq * 10_000,
        data: vec![0; FRAME_BYTES],
    }
}
#[tokio::test]
async fn a_slow_subscriber_discards_its_entire_old_epoch_without_blocking() {
    let queue = AudioQueue::default();
    for seq in 0..QUEUE_FRAMES {
        assert!(queue.push(packet(seq as u64)));
    }
    assert!(!queue.push(packet(7)));
    assert_eq!(queue.recv().await.unwrap_err(), AudioError::Overrun);
    assert!(queue.state.lock().unwrap().packets.is_empty());
}
#[tokio::test]
async fn retiring_wakes_an_idle_reader_and_forgets_queued_sound() {
    let queue = Arc::new(AudioQueue::default());
    let read = {
        let queue = queue.clone();
        tokio::spawn(async move { queue.recv().await })
    };
    queue.end(AudioError::Retired);
    assert_eq!(read.await.unwrap().unwrap_err(), AudioError::Retired);
}
#[cfg(all(target_os = "linux", feature = "browser-audio"))]
#[test]
fn opus_is_ten_ms_stereo_with_measured_priming_and_exact_pcm_alternative() {
    let samples: Vec<_> = (0..FRAME_SAMPLES * 2)
        .map(|i| ((i / 2) as f32 * 0.2).sin().mul_add(10000.0, 0.0) as i16)
        .collect();
    let mut encoder = encoder::Encoder::new(AudioCodec::Opus).unwrap();
    let packet = encoder.encode(&samples, 20_000).unwrap();
    assert_eq!(packet.ts, 20_000);
    assert!(packet.format.priming_samples > 0);
    let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).unwrap();
    let mut output = [0; FRAME_SAMPLES * 2];
    assert_eq!(
        decoder.decode(&packet.data, &mut output, false).unwrap(),
        FRAME_SAMPLES
    );
    let raw = encoder::Encoder::new(AudioCodec::PcmS16le)
        .unwrap()
        .encode(&samples, 30_000)
        .unwrap();
    assert_eq!(
        raw.data,
        samples
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect::<Vec<_>>()
    );
    assert_eq!(raw.format.priming_samples, 0);
}

#[cfg(all(target_os = "linux", feature = "browser-audio"))]
#[test]
fn measured_opus_priming_places_an_impulse_on_its_source_sample() {
    let mut encoder = encoder::Encoder::new(AudioCodec::Opus).unwrap();
    let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).unwrap();
    let mut decoded = Vec::new();
    let mut priming = 0;
    for frame in 0..3 {
        let mut pcm = [0; FRAME_SAMPLES * 2];
        if frame == 0 {
            pcm[0] = 20_000;
            pcm[1] = 20_000;
        }
        let packet = encoder.encode(&pcm, frame * 10_000).unwrap();
        priming = packet.format.priming_samples;
        decoder.decode(&packet.data, &mut pcm, false).unwrap();
        decoded.extend(pcm.chunks_exact(2).map(|sample| sample[0]));
    }
    let peak = decoded
        .iter()
        .enumerate()
        .max_by_key(|(_, value)| value.unsigned_abs())
        .unwrap()
        .0;
    assert_eq!(
        peak, priming as usize,
        "decoded impulse aligns after removing the reported priming"
    );
}

#[cfg(all(target_os = "linux", feature = "browser-audio"))]
#[test]
fn opus_fixture_preserves_two_impulses_with_one_priming_trim() {
    use base64::Engine;
    let mut encoder = encoder::Encoder::new(AudioCodec::Opus).unwrap();
    let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).unwrap();
    let mut original = vec![0i16; FRAME_SAMPLES * 2 * 6];
    for frame in [0, 1000] {
        original[frame * 2] = 20_000;
        original[frame * 2 + 1] = 20_000;
    }
    let mut reference = Vec::new();
    let mut packets = Vec::new();
    let mut priming = 0;
    let mut format = None;
    for (index, samples) in original.chunks_exact(FRAME_SAMPLES * 2).enumerate() {
        let packet = encoder
            .encode(samples, 1_000_000 + index as u64 * 10_000)
            .unwrap();
        priming = packet.format.priming_samples as usize;
        format = Some(packet.format.clone());
        let mut output = [0i16; FRAME_SAMPLES * 2];
        assert_eq!(
            decoder.decode(&packet.data, &mut output, false).unwrap(),
            FRAME_SAMPLES
        );
        reference.extend(output);
        packets.push(serde_json::json!({"seq":packet.seq,"ts":packet.ts,"data":base64::engine::general_purpose::STANDARD.encode(&packet.data)}));
    }
    let aligned = &reference[priming * 2..];
    for impulse in [0usize, 1000] {
        let start = impulse.saturating_sub(10);
        let peak = (start..impulse + 20)
            .max_by_key(|frame| aligned[frame * 2].unsigned_abs())
            .unwrap();
        assert_eq!(peak, impulse, "one trim preserves both source positions");
    }
    if let Some(directory) = std::env::var_os("AUDIO_IMPULSE_DIR") {
        let directory = std::path::Path::new(&directory);
        std::fs::create_dir_all(directory).unwrap();
        let fixture = serde_json::json!({"schema":"ambit.opus-impulse/v1","opusVersion":opus::version(),
            "format":format.unwrap().as_ref(),"sourceFirstTs":1_000_000,"impulseSourceFrames":[0,1000],
            "packets":packets,"originalPcmS16":original,"nativeDecodedPcmS16":reference,
            "description":"Synthetic impulses encoded and decoded by the real pinned native codec. Packet ts names input PCM; discard priming once, then use cumulative output samples."});
        std::fs::write(
            directory.join("fixture.json"),
            serde_json::to_vec_pretty(&fixture).unwrap(),
        )
        .unwrap();
    }
}
