//! Verifies the symbolication seam end-to-end through a real `TelemetryServer`:
//! a probe buffer decl carrying allocation-site `addrs` triggers the installed
//! [`Symbolicator`], and the server renames the buffer to the recovered name.
//! Uses a stub symbolicator so the test needs no `atos`/debug binary.

use std::sync::Arc;
use std::time::Duration;

use jade_build::{Symbolicator, TelemetryServer};
use jade_telemetry::{Event, Kind};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

struct StubSymbolicator {
    names: Vec<String>,
}
impl Symbolicator for StubSymbolicator {
    fn variable_names(&self, _addrs: &[String], _exe: Option<&str>, _load: &str) -> Vec<String> {
        self.names.clone()
    }
}

fn socket_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("jade-build-symtest-{}-{}.sock", std::process::id(), name))
}

#[tokio::test]
async fn buffer_decl_with_addrs_is_symbolicated_and_renamed() {
    let sock = socket_path("rename");
    let (server, mut events) = TelemetryServer::start(sock.clone()).unwrap();
    server.set_symbolicator(Arc::new(StubSymbolicator {
        names: vec!["weights".to_string()],
    }));

    let mut client = UnixStream::connect(&sock).await.unwrap();
    let decl = r#"{"type":"decl","kind":"buffer","name":"Matrix::Matrix #3","meta":{"addrs":["0x100","0x200"],"load":"0x1000","exe":"/usr/bin/true"}}"#;
    client.write_all(decl.as_bytes()).await.unwrap();
    client.write_all(b"\n").await.unwrap();
    client.flush().await.unwrap();

    // We should observe the initial decl under the probe name, then a rename to
    // the symbolicated name "weights".
    let mut saw_rename = false;
    for _ in 0..5 {
        match tokio::time::timeout(Duration::from_secs(5), events.recv()).await {
            Ok(Some(Event::Decl {
                kind,
                name,
                renamed_from,
                ..
            })) => {
                assert_eq!(kind, Kind::Buffer);
                if let Some(from) = renamed_from {
                    assert_eq!(from, "Matrix::Matrix #3");
                    assert_eq!(name, "weights");
                    saw_rename = true;
                    break;
                }
            }
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    assert!(saw_rename, "expected the buffer to be renamed to 'weights'");

    server.stop();
    let _ = std::fs::remove_file(&sock);
}

#[tokio::test]
async fn aliases_use_full_scope_chain_and_disambiguate_siblings() {
    let sock = socket_path("chain");
    let (server, mut events) = TelemetryServer::start(sock.clone()).unwrap();
    // Same alloc site resolved twice (one instance per model layer): both
    // yield the identical innermost-first chain.
    server.set_symbolicator(Arc::new(StubSymbolicator {
        names: vec!["paramsBuffer".into(), "ln2".into(), "blocks".into()],
    }));

    let mut client = UnixStream::connect(&sock).await.unwrap();
    for decl in [
        r#"{"type":"decl","kind":"buffer","name":"Block::Block #3","meta":{"addrs":["0x100"],"load":"0x1000","exe":"/usr/bin/true"}}"#,
        r#"{"type":"decl","kind":"buffer","name":"Block::Block #9","meta":{"addrs":["0x200"],"load":"0x1000","exe":"/usr/bin/true"}}"#,
    ] {
        client.write_all(decl.as_bytes()).await.unwrap();
        client.write_all(b"\n").await.unwrap();
    }
    client.flush().await.unwrap();

    // Expect renames to the FULL chain — never the bare innermost name, which
    // used to hand the first-resolved buffer a generic "paramsBuffer" row —
    // with #2 disambiguating the sibling instance.
    let mut renames = Vec::new();
    for _ in 0..8 {
        match tokio::time::timeout(Duration::from_secs(5), events.recv()).await {
            Ok(Some(Event::Decl {
                name,
                renamed_from: Some(_),
                ..
            })) => {
                renames.push(name);
                if renames.len() == 2 {
                    break;
                }
            }
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    renames.sort();
    assert_eq!(
        renames,
        vec!["blocks.ln2.paramsBuffer".to_string(), "blocks.ln2.paramsBuffer#2".to_string()]
    );

    server.stop();
    let _ = std::fs::remove_file(&sock);
}

/// A startup burst of decls must resolve through FEW batched symbolicator
/// calls, not one per decl — the per-decl shape ran hundreds of concurrent
/// `atos` processes, and the ones that blew their timeout kept their fallback
/// names (the "MetalMLP::MetalMLP #118" rows in the sidebar).
#[tokio::test]
async fn decl_burst_is_batched_and_fully_renamed() {
    use std::sync::Mutex;

    struct BatchCountingStub {
        batch_sizes: Mutex<Vec<usize>>,
    }
    impl Symbolicator for BatchCountingStub {
        fn variable_names(&self, _a: &[String], _e: Option<&str>, _l: &str) -> Vec<String> {
            unreachable!("server must go through the batch path");
        }
        fn variable_names_batch(
            &self,
            addr_sets: &[Vec<String>],
            _exe: Option<&str>,
            _load: &str,
        ) -> Vec<Vec<String>> {
            self.batch_sizes.lock().unwrap().push(addr_sets.len());
            addr_sets.iter().map(|_| vec!["buf".to_string()]).collect()
        }
    }

    let sock = socket_path("burst");
    let (server, mut events) = TelemetryServer::start(sock.clone()).unwrap();
    let stub = Arc::new(BatchCountingStub {
        batch_sizes: Mutex::new(Vec::new()),
    });
    server.set_symbolicator(stub.clone());

    const N: usize = 20;
    let mut client = UnixStream::connect(&sock).await.unwrap();
    for i in 0..N {
        let decl = format!(
            r#"{{"type":"decl","kind":"buffer","name":"Layer::Layer #{i}","meta":{{"addrs":["0x{i}00"],"load":"0x1000","exe":"/usr/bin/true"}}}}"#
        );
        client.write_all(decl.as_bytes()).await.unwrap();
        client.write_all(b"\n").await.unwrap();
    }
    client.flush().await.unwrap();

    let mut renames = 0;
    for _ in 0..(N * 2) {
        match tokio::time::timeout(Duration::from_secs(5), events.recv()).await {
            Ok(Some(Event::Decl {
                renamed_from: Some(_),
                ..
            })) => {
                renames += 1;
                if renames == N {
                    break;
                }
            }
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    assert_eq!(renames, N, "every decl in the burst must be renamed");
    let sizes = stub.batch_sizes.lock().unwrap().clone();
    assert!(
        sizes.len() < N,
        "burst must coalesce into few batches, got {} calls: {:?}",
        sizes.len(),
        sizes
    );
    assert_eq!(sizes.iter().sum::<usize>(), N);

    server.stop();
    let _ = std::fs::remove_file(&sock);
}

#[tokio::test]
async fn buffer_decl_without_symbolicator_is_not_renamed() {
    let sock = socket_path("noop");
    let (server, mut events) = TelemetryServer::start(sock.clone()).unwrap();
    // No symbolicator installed.

    let mut client = UnixStream::connect(&sock).await.unwrap();
    let decl = r#"{"type":"decl","kind":"buffer","name":"buf","meta":{"addrs":["0x1"],"load":"0x10"}}"#;
    client.write_all(decl.as_bytes()).await.unwrap();
    client.write_all(b"\n").await.unwrap();
    client.flush().await.unwrap();

    match tokio::time::timeout(Duration::from_secs(2), events.recv()).await {
        Ok(Some(Event::Decl {
            name, renamed_from, ..
        })) => {
            assert_eq!(name, "buf");
            assert!(renamed_from.is_none(), "no rename without a symbolicator");
        }
        other => panic!("expected a plain decl, got {other:?}"),
    }

    server.stop();
    let _ = std::fs::remove_file(&sock);
}
