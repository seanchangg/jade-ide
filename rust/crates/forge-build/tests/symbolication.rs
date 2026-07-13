//! Verifies the symbolication seam end-to-end through a real `TelemetryServer`:
//! a probe buffer decl carrying allocation-site `addrs` triggers the installed
//! [`Symbolicator`], and the server renames the buffer to the recovered name.
//! Uses a stub symbolicator so the test needs no `atos`/debug binary.

use std::sync::Arc;
use std::time::Duration;

use forge_build::{Symbolicator, TelemetryServer};
use forge_telemetry::{Event, Kind};
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
    std::env::temp_dir().join(format!("forge-build-symtest-{}-{}.sock", std::process::id(), name))
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
