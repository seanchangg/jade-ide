//! End-to-end check against the REAL Metal probe: the Rust analogue of
//! `probe/mock_server.py`. Launches `probe/test_train` with the injected
//! `jade_probe.dylib` pointed at this server, enables `model.weights` when it
//! declares, and reports what arrives.
//!
//! Run from the repo root:
//!   cargo run -p jade-telemetry --example probe_e2e -- /path/to/jade-ide/probe

use jade_telemetry::{Event, Kind, TelemetryServer};
use std::path::PathBuf;
use tokio::time::{timeout, Duration, Instant};

#[tokio::main]
async fn main() {
    let probe_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("../probe"));
    let dylib = probe_dir.join("jade_probe.dylib");
    let train = probe_dir.join("test_train");
    for p in [&dylib, &train] {
        if !p.exists() {
            eprintln!("missing {} — run `make` in probe/ first", p.display());
            std::process::exit(2);
        }
    }

    let sock = std::env::temp_dir().join("jade-telemetry-rust-e2e.sock");
    let (server, mut events) = TelemetryServer::start(sock.clone()).expect("bind socket");
    println!("[e2e] rust telemetry server on {}", sock.display());

    let mut child = std::process::Command::new(&train)
        .current_dir(&probe_dir)
        .env("DYLD_INSERT_LIBRARIES", dylib.canonicalize().unwrap())
        .env("JADE_TELEMETRY_SOCK", &sock)
        .env_remove("JADE_TRACK_ALL") // selection must come from track messages only
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn test_train");

    let mut decls: Vec<String> = Vec::new();
    let mut scalars = 0u64;
    let mut timings = 0u64;
    let mut tensors = 0u64;
    let mut last_shape = (0u32, 0u32);
    let deadline = Instant::now() + Duration::from_secs(30);

    loop {
        if Instant::now() > deadline {
            println!("[e2e] deadline reached");
            break;
        }
        match timeout(Duration::from_secs(5), events.recv()).await {
            Err(_) => {
                // Quiet for 5s: if the program exited, we're done.
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
            }
            Ok(None) => break,
            Ok(Some(event)) => match event {
                Event::Decl { kind, name, .. } => {
                    decls.push(format!("{}:{}", kind.as_str(), name));
                    // IDE-side rule from mock_server.py: the user checked
                    // model.weights in the sidebar.
                    if kind == Kind::Buffer && name == "model.weights" {
                        println!("[e2e] model.weights declared -> track enabled, maxDim 32");
                        server.set_track(Kind::Buffer, &name, true, Some(32), None);
                    }
                }
                Event::Scalar(_) => scalars += 1,
                Event::Timing(_) => timings += 1,
                Event::Tensor { name, rows, cols, data, .. } => {
                    tensors += 1;
                    last_shape = (rows, cols);
                    assert_eq!(data.len(), (rows * cols) as usize, "decoded length");
                    if tensors == 1 {
                        println!("[e2e] first tensor frame: {name} {rows}x{cols}");
                    }
                }
            },
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    server.stop();

    println!("[e2e] decls:   {}", decls.join(", "));
    println!("[e2e] scalars: {scalars}  timings: {timings}  tensors: {tensors}  last shape: {last_shape:?}");

    let pass = tensors > 0 && last_shape.0 <= 32 && last_shape.1 <= 32;
    println!("[e2e] {}", if pass { "PASS" } else { "FAIL" });
    std::process::exit(if pass { 0 } else { 1 });
}
