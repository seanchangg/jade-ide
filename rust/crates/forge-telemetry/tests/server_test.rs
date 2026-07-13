//! Integration tests exercising the wire contract from docs/telemetry-protocol.md
//! with a real Unix-socket client.

use base64::Engine;
use forge_telemetry::{Event, Kind, TelemetryServer};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::{timeout, Duration};

fn test_socket(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "forge-telemetry-test-{}-{}.sock",
        name,
        std::process::id()
    ))
}

async fn next_event(rx: &mut UnboundedReceiver<Event>) -> Event {
    timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timed out waiting for event")
        .expect("event channel closed")
}

fn tensor_b64(values: &[f32]) -> String {
    let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[tokio::test]
async fn decl_and_data_messages_register_and_flow() {
    let path = test_socket("decl");
    let (server, mut events) = TelemetryServer::start(path.clone()).unwrap();

    let mut client = UnixStream::connect(&path).await.unwrap();
    client
        .write_all(
            b"{\"type\":\"decl\",\"kind\":\"scalar\",\"name\":\"loss\",\"meta\":{\"label\":\"train loss\"}}\n\
              {\"type\":\"scalar\",\"name\":\"loss\",\"step\":1,\"value\":0.5}\n\
              {\"type\":\"timing\",\"name\":\"forward\",\"ms\":12.4,\"step\":1}\n",
        )
        .await
        .unwrap();

    // decl emits a Decl event with meta
    match next_event(&mut events).await {
        Event::Decl { kind, name, meta, .. } => {
            assert_eq!(kind, Kind::Scalar);
            assert_eq!(name, "loss");
            assert_eq!(meta.unwrap()["label"], "train loss");
        }
        other => panic!("expected Decl, got {other:?}"),
    }
    // scalar flows through with server-substituted timestamp
    match next_event(&mut events).await {
        Event::Scalar(s) => {
            assert_eq!(s.name, "loss");
            assert_eq!(s.step, 1);
            assert!(s.t.is_some(), "server substitutes receive time");
        }
        other => panic!("expected Scalar, got {other:?}"),
    }
    // timing auto-registers its timer (Decl) then flows
    match next_event(&mut events).await {
        Event::Decl { kind, name, .. } => {
            assert_eq!(kind, Kind::Timer);
            assert_eq!(name, "forward");
        }
        other => panic!("expected Decl for timer, got {other:?}"),
    }
    match next_event(&mut events).await {
        Event::Timing(t) => assert_eq!(t.ms, 12.4),
        other => panic!("expected Timing, got {other:?}"),
    }

    server.stop();
}

#[tokio::test]
async fn malformed_lines_and_unknown_types_are_skipped() {
    let path = test_socket("malformed");
    let (server, mut events) = TelemetryServer::start(path.clone()).unwrap();

    let mut client = UnixStream::connect(&path).await.unwrap();
    client
        .write_all(
            b"not json at all\n\
              {\"type\":\"mystery\",\"name\":\"x\"}\n\
              {\"type\":\"scalar\",\"name\":\"ok\",\"step\":7,\"value\":1.0}\n",
        )
        .await
        .unwrap();

    // Only the valid scalar should surface (as Decl + Scalar).
    match next_event(&mut events).await {
        Event::Decl { name, .. } => assert_eq!(name, "ok"),
        other => panic!("expected Decl, got {other:?}"),
    }
    match next_event(&mut events).await {
        Event::Scalar(s) => assert_eq!(s.step, 7),
        other => panic!("expected Scalar, got {other:?}"),
    }

    server.stop();
}

#[tokio::test]
async fn partial_lines_across_chunk_boundaries_reassemble() {
    let path = test_socket("partial");
    let (server, mut events) = TelemetryServer::start(path.clone()).unwrap();

    let mut client = UnixStream::connect(&path).await.unwrap();
    let line = b"{\"type\":\"scalar\",\"name\":\"chunked\",\"step\":3,\"value\":9.0}\n";
    let (a, b) = line.split_at(20);
    client.write_all(a).await.unwrap();
    client.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    client.write_all(b).await.unwrap();

    match next_event(&mut events).await {
        Event::Decl { name, .. } => assert_eq!(name, "chunked"),
        other => panic!("expected Decl, got {other:?}"),
    }
    match next_event(&mut events).await {
        Event::Scalar(s) => assert_eq!(s.value, 9.0),
        other => panic!("expected Scalar, got {other:?}"),
    }

    server.stop();
}

#[tokio::test]
async fn track_broadcasts_and_replays_to_late_joiners() {
    let path = test_socket("track");
    let (server, mut _events) = TelemetryServer::start(path.clone()).unwrap();

    // First client connects before any tracks exist.
    let client1 = UnixStream::connect(&path).await.unwrap();
    let mut reader1 = BufReader::new(client1).lines();

    // UI enables a buffer at reduced resolution with a shape hint.
    server.set_track(Kind::Buffer, "grad.layer0", true, Some(64), Some((1024, 1024)));

    // Connected client receives the broadcast.
    let line = timeout(Duration::from_secs(2), reader1.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let track: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(track["type"], "track");
    assert_eq!(track["kind"], "buffer");
    assert_eq!(track["name"], "grad.layer0");
    assert_eq!(track["enabled"], true);
    assert_eq!(track["maxDim"], 64);
    assert_eq!(track["rows"], 1024);
    assert_eq!(track["cols"], 1024);

    // A disabled track must NOT be replayed to late joiners; only enabled ones.
    server.set_track(Kind::Scalar, "loss", true, None, None);
    server.set_track(Kind::Timer, "forward", false, None, None);

    let client2 = UnixStream::connect(&path).await.unwrap();
    let mut reader2 = BufReader::new(client2).lines();
    let mut replayed = Vec::new();
    for _ in 0..2 {
        let line = timeout(Duration::from_secs(2), reader2.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["enabled"], true, "only enabled tracks are replayed");
        replayed.push(v["name"].as_str().unwrap().to_string());
    }
    replayed.sort();
    assert_eq!(replayed, vec!["grad.layer0", "loss"]);
    // Nothing further (the disabled timer would be a third line).
    let extra = timeout(Duration::from_millis(200), reader2.next_line()).await;
    assert!(extra.is_err(), "disabled tracks must not be replayed");

    server.stop();
}

#[tokio::test]
async fn tensor_frames_are_gated_and_decoded() {
    let path = test_socket("tensor");
    let (server, mut events) = TelemetryServer::start(path.clone()).unwrap();

    let mut client = UnixStream::connect(&path).await.unwrap();
    let payload = tensor_b64(&[1.0, -2.0, 3.0, -4.0]);
    let frame = format!(
        "{{\"type\":\"tensor\",\"name\":\"w\",\"step\":1,\"rows\":2,\"cols\":2,\"dtype\":\"f32\",\"data\":\"{payload}\"}}\n"
    );

    // Not enabled yet: frame is dropped, but the buffer auto-registers (Decl).
    client.write_all(frame.as_bytes()).await.unwrap();
    match next_event(&mut events).await {
        Event::Decl { kind, name, .. } => {
            assert_eq!(kind, Kind::Buffer);
            assert_eq!(name, "w");
        }
        other => panic!("expected Decl, got {other:?}"),
    }

    // Enable, resend: frame flows, decoded.
    server.set_track(Kind::Buffer, "w", true, None, None);
    // Drain the meta-merge decl re-emission if present before the tensor.
    client.write_all(frame.as_bytes()).await.unwrap();
    loop {
        match next_event(&mut events).await {
            Event::Tensor {
                name, rows, cols, data, ..
            } => {
                assert_eq!(name, "w");
                assert_eq!((rows, cols), (2, 2));
                assert_eq!(data, vec![1.0, -2.0, 3.0, -4.0]);
                break;
            }
            Event::Decl { .. } => continue,
            other => panic!("expected Tensor, got {other:?}"),
        }
    }

    server.stop();
}

#[tokio::test]
async fn rename_preserves_enabled_state_and_translates_track_names() {
    let path = test_socket("rename");
    let (server, mut events) = TelemetryServer::start(path.clone()).unwrap();

    let mut client = UnixStream::connect(&path).await.unwrap();
    let mut reader = BufReader::new(
        UnixStream::connect(&path).await.unwrap(), // second connection observes tracks
    )
    .lines();

    // Probe declares a placeholder buffer, UI enables it.
    client
        .write_all(b"{\"type\":\"decl\",\"kind\":\"buffer\",\"name\":\"buffer#3\"}\n")
        .await
        .unwrap();
    match next_event(&mut events).await {
        Event::Decl { name, .. } => assert_eq!(name, "buffer#3"),
        other => panic!("expected Decl, got {other:?}"),
    }
    server.set_track(Kind::Buffer, "buffer#3", true, None, None);
    reader.next_line().await.unwrap().unwrap(); // consume that track

    // Probe re-declares under its real name with renamedFrom.
    client
        .write_all(
            b"{\"type\":\"decl\",\"kind\":\"buffer\",\"name\":\"model.weights\",\"meta\":{\"renamedFrom\":\"buffer#3\",\"bytes\":262144}}\n",
        )
        .await
        .unwrap();
    match next_event(&mut events).await {
        Event::Decl {
            name, renamed_from, meta, ..
        } => {
            assert_eq!(name, "model.weights");
            assert_eq!(renamed_from.as_deref(), Some("buffer#3"));
            assert_eq!(meta.unwrap()["bytes"], 262144);
        }
        other => panic!("expected rename Decl, got {other:?}"),
    }

    // Enabled state carried over to the new name.
    assert!(server.is_enabled(Kind::Buffer, "model.weights"));

    // Probe-initiated rename: the probe now speaks the NEW name, so tracks go
    // out under it verbatim (this is what the real forge_probe.dylib expects
    // after a setLabel re-declaration — verified by examples/probe_e2e.rs).
    server.set_track(Kind::Buffer, "model.weights", true, Some(64), None);
    let line = timeout(Duration::from_secs(2), reader.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let track: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(track["name"], "model.weights");

    // Data arriving under the new name flows under it too.
    let payload = tensor_b64(&[5.0]);
    let frame = format!(
        "{{\"type\":\"tensor\",\"name\":\"model.weights\",\"step\":9,\"rows\":1,\"cols\":1,\"dtype\":\"f32\",\"data\":\"{payload}\"}}\n"
    );
    client.write_all(frame.as_bytes()).await.unwrap();
    loop {
        match next_event(&mut events).await {
            Event::Tensor { name, step, .. } => {
                assert_eq!(name, "model.weights");
                assert_eq!(step, 9);
                break;
            }
            Event::Decl { .. } => continue,
            other => panic!("expected Tensor, got {other:?}"),
        }
    }

    server.stop();
}

#[tokio::test]
async fn server_initiated_rename_translates_track_to_probe_name() {
    let path = test_socket("alias");
    let (server, mut events) = TelemetryServer::start(path.clone()).unwrap();

    let mut client = UnixStream::connect(&path).await.unwrap();
    let mut reader = BufReader::new(UnixStream::connect(&path).await.unwrap()).lines();

    // Probe declares an anonymous allocation-site buffer.
    client
        .write_all(b"{\"type\":\"decl\",\"kind\":\"buffer\",\"name\":\"Matrix::Matrix #3\"}\n")
        .await
        .unwrap();
    match next_event(&mut events).await {
        Event::Decl { name, .. } => assert_eq!(name, "Matrix::Matrix #3"),
        other => panic!("expected Decl, got {other:?}"),
    }

    // Server symbolication resolves the real variable name (atos path).
    server.rename(Kind::Buffer, "Matrix::Matrix #3", "attn.buf");
    match next_event(&mut events).await {
        Event::Decl { name, renamed_from, .. } => {
            assert_eq!(name, "attn.buf");
            assert_eq!(renamed_from.as_deref(), Some("Matrix::Matrix #3"));
        }
        other => panic!("expected rename Decl, got {other:?}"),
    }

    // The probe never learned the alias: tracks must be translated back.
    server.set_track(Kind::Buffer, "attn.buf", true, None, None);
    let line = timeout(Duration::from_secs(2), reader.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let track: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(
        track["name"], "Matrix::Matrix #3",
        "probe matches on its own buffer name; alias must be translated back"
    );

    // Frames arriving under the probe name surface under the display alias.
    let payload = tensor_b64(&[7.0]);
    let frame = format!(
        "{{\"type\":\"tensor\",\"name\":\"Matrix::Matrix #3\",\"step\":2,\"rows\":1,\"cols\":1,\"dtype\":\"f32\",\"data\":\"{payload}\"}}\n"
    );
    client.write_all(frame.as_bytes()).await.unwrap();
    loop {
        match next_event(&mut events).await {
            Event::Tensor { name, .. } => {
                assert_eq!(name, "attn.buf");
                break;
            }
            Event::Decl { .. } => continue,
            other => panic!("expected Tensor, got {other:?}"),
        }
    }

    server.stop();
}

#[tokio::test]
async fn rename_of_enabled_item_rebroadcasts_track_under_new_name() {
    let path = test_socket("retrack");
    let (server, mut events) = TelemetryServer::start(path.clone()).unwrap();

    let mut client = UnixStream::connect(&path).await.unwrap();
    let mut reader = BufReader::new(UnixStream::connect(&path).await.unwrap()).lines();

    // Placeholder declared and enabled BEFORE the probe labels it.
    client
        .write_all(b"{\"type\":\"decl\",\"kind\":\"buffer\",\"name\":\"buffer#0\"}\n")
        .await
        .unwrap();
    match next_event(&mut events).await {
        Event::Decl { name, .. } => assert_eq!(name, "buffer#0"),
        other => panic!("expected Decl, got {other:?}"),
    }
    server.set_track(Kind::Buffer, "buffer#0", true, Some(64), None);
    let first = reader.next_line().await.unwrap().unwrap();
    assert_eq!(serde_json::from_str::<Value>(&first).unwrap()["name"], "buffer#0");

    // Probe renames itself via setLabel. Streaming must not go dark: the
    // migrated enabled state is re-broadcast under the NEW name.
    client
        .write_all(
            b"{\"type\":\"decl\",\"kind\":\"buffer\",\"name\":\"model.weights\",\"meta\":{\"renamedFrom\":\"buffer#0\"}}\n",
        )
        .await
        .unwrap();
    let retrack = timeout(Duration::from_secs(2), reader.next_line())
        .await
        .expect("expected a re-broadcast track after rename")
        .unwrap()
        .unwrap();
    let v: Value = serde_json::from_str(&retrack).unwrap();
    assert_eq!(v["name"], "model.weights");
    assert_eq!(v["enabled"], true);
    assert_eq!(v["maxDim"], 64);

    server.stop();
}

#[tokio::test]
async fn stop_removes_socket_file() {
    let path = test_socket("stop");
    let (server, _events) = TelemetryServer::start(path.clone()).unwrap();
    assert!(path.exists());
    server.stop();
    assert!(!path.exists(), "socket file must be removed on stop");
}
