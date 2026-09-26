use super::*;
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn an_old_capture_cannot_publish_or_fail_a_new_subscription() {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            subscribers: HashMap::new(),
            retired: false,
            failure: None,
            capture_generation: 0,
        }),
        wake: Condvar::new(),
        startup_us: 0,
        child: Weak::new(),
    });
    let first = shared.subscribe(AudioCodec::PcmS16le).unwrap();
    let old = shared.state.lock().unwrap().capture_generation;
    drop(first);
    let mut current = shared.subscribe(AudioCodec::PcmS16le).unwrap();
    let generation = shared.state.lock().unwrap().capture_generation;
    assert_ne!(old, generation);
    shared.publish(old, &[0; FRAME_BYTES], 100);
    shared.end_capture(old, AudioError::Unavailable);
    assert!(current.queue.state.lock().unwrap().packets.is_empty());
    assert!(shared.state.lock().unwrap().failure.is_none());
    shared.publish(generation, &[0; FRAME_BYTES], 200);
    assert_eq!(current.recv().await.unwrap().ts, 200);
}

#[test]
fn cancellation_and_missing_program_never_admit_a_source() {
    assert!(matches!(
        RetainedAudio::start(&AtomicBool::new(true)),
        Err(AudioError::Retired)
    ));
    assert!(matches!(
        RetainedAudio::start_with(
            Path::new("/no-such-ambit-audio-program"),
            &AtomicBool::new(false)
        ),
        Err(AudioError::Unavailable)
    ));
}

fn alive(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .is_ok_and(|stat| !stat.rsplit(") ").next().unwrap_or("").starts_with('Z'))
}

async fn ended(subscription: &mut AudioSubscription) -> AudioError {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Err(reason) = subscription.recv().await {
                return reason;
            }
        }
    })
    .await
    .expect("capture ends without waiting for an audible sample")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the task-owned Pulse runtime candidate"]
async fn e2e_private_output_lifetime_and_startup() {
    let mut startup = Vec::new();
    let mut shutdown = Vec::new();
    for _ in 0..20 {
        let owner = RetainedAudio::start(&AtomicBool::new(false)).unwrap();
        let source = owner.source();
        assert!(source.observe().ready);
        startup.push(source.observe().startup_us);
        let directory = owner.server.directory.clone();
        let pid = owner.server.child.lock().unwrap().id();
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(directory.join("cookie"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let mut command = Command::new("/usr/bin/pactl");
        owner.apply_environment(&mut command);
        let output = command.args(["list", "short", "modules"]).output().unwrap();
        assert!(output.status.success());
        let modules = String::from_utf8(output.stdout).unwrap();
        assert_eq!(modules.lines().count(), 3, "{modules}");
        for name in [
            "module-null-sink",
            "module-native-protocol-unix",
            "module-cli",
        ] {
            assert!(modules.contains(name), "{modules}");
        }
        let mut subscriber = source.subscribe(AudioCodec::PcmS16le).unwrap();
        let packet = tokio::time::timeout(Duration::from_secs(1), subscriber.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            packet.data.iter().all(|sample| *sample == 0),
            "private sink starts silent"
        );
        let retained = owner.clone();
        drop(owner);
        assert!(source.observe().ready && alive(pid));
        retained.discontinue();
        assert_eq!(ended(&mut subscriber).await, AudioError::Discontinuity);
        let mut idle_reader = source.subscribe(AudioCodec::Opus).unwrap();
        let before = Instant::now();
        drop(retained);
        shutdown.push(before.elapsed().as_micros() as u64);
        assert_eq!(ended(&mut idle_reader).await, AudioError::Retired);
        assert_eq!(source.observe().failure, Some(AudioError::Retired));
        assert!(!alive(pid) && !directory.exists());
    }
    // A broken server is visible to a handle obtained before it failed.
    let broken = RetainedAudio::start(&AtomicBool::new(false)).unwrap();
    let source = broken.source();
    let mut active = source.subscribe(AudioCodec::Opus).unwrap();
    broken.server.child.lock().unwrap().kill().unwrap();
    assert_eq!(ended(&mut active).await, AudioError::Unavailable);
    assert!(!source.observe().ready);
    assert!(matches!(
        source.subscribe(AudioCodec::Opus),
        Err(AudioError::Unavailable)
    ));
    drop(broken);

    // The sole pipe writer disappearing is exactly what the kernel does on daemon death.
    let owner = RetainedAudio::start(&AtomicBool::new(false)).unwrap();
    let source = owner.source();
    let pid = owner.server.child.lock().unwrap().id();
    drop(owner.server.child.lock().unwrap().stdin.take());
    tokio::time::timeout(Duration::from_secs(1), async {
        while alive(pid) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("Pulse exits on owner EOF without a watchdog or kill signal");
    assert!(!source.observe().ready);
    drop(owner);
    startup.sort_unstable();
    shutdown.sort_unstable();
    println!(
        "AUDIO_LIFETIME_PROOF {}",
        serde_json::json!({
            "status":"passed", "launches":startup.len(), "startupUs":startup,
            "shutdownUs":shutdown, "serverModules":"null_sink+private_unix+owner_eof",
            "ownerDrop":"reaped_directory_removed", "captureDrop":"retired",
            "daemonEof":"server_exited", "serverFailure":"unavailable",
        })
    );
}
