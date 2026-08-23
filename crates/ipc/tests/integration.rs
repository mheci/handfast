#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! End-to-end IPC tests over a real Unix domain socket.
//!
//! Everything here needs Unix sockets, so the whole file is unix-gated; the
//! codec-level unit tests inside `src/codec.rs` cover the platform-independent
//! parts.

#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::broadcast;

use handfast_ipc::{Client, Request, Response, Server, ServerEvent};

const SOCK_NAME: &str = "handfast-test.sock";

/// Install a tracing subscriber exactly once so breadcrumbs reach CI logs.
fn init_logging() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("trace")),
            )
            .try_init();
    });
}

/// Handler answering Ping and rejecting everything else with a 404.
fn handler() -> Arc<dyn Fn(Request) -> BoxFuture<'static, Response> + Send + Sync> {
    Arc::new(|req| {
        Box::pin(async move {
            match req {
                Request::Ping => Response::ok_json(serde_json::json!({ "pong": true })),
                other => Response::err(404, format!("unimplemented method: {other:?}")),
            }
        })
    })
}

/// Bind a server on a socket inside `dir` and run it in the background.
async fn spawn_server(dir: &tempfile::TempDir) -> broadcast::Sender<ServerEvent> {
    let sock = dir.path().join(SOCK_NAME);
    let server = Server::bind(&sock).await.expect("bind succeeds");
    let (events_tx, events_rx) = broadcast::channel(64);
    let _server_task = tokio::spawn(server.serve(handler(), events_rx));
    events_tx
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ping_roundtrip_over_real_socket() {
    init_logging();
    let dir = tempfile::tempdir().expect("tempdir");
    let _events = spawn_server(&dir).await;
    let sock = dir.path().join(SOCK_NAME);

    let client = Client::connect(&sock).await.expect("connect");
    let response = client.request(Request::Ping).await.expect("ping");
    match response {
        Response::Ok { result } => assert_eq!(result["pong"], serde_json::json!(true)),
        other => panic!("expected Ok response, got {other:?}"),
    }

    // Unknown methods come back as structured errors.
    let err = client
        .request(Request::DaemonInfo)
        .await
        .expect("daemon_info exchange");
    match err {
        Response::Err { code, .. } => assert_eq!(code, 404),
        other => panic!("expected Err response, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_and_broadcast_events_reach_the_client() {
    init_logging();
    let dir = tempfile::tempdir().expect("tempdir");
    let events_tx = spawn_server(&dir).await;
    let sock = dir.path().join(SOCK_NAME);

    let mut client = Client::connect(&sock).await.expect("connect");
    let mut events = client.take_event_receiver().expect("event receiver");

    // The handshake is always the first event.
    let hello = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .expect("hello within timeout")
        .expect("event channel alive");
    match hello {
        ServerEvent::Hello { version, app, pid } => {
            assert_eq!(version, handfast_ipc::IPC_VERSION);
            assert_eq!(app, "handfast");
            assert_ne!(pid, 0);
        }
        other => panic!("expected Hello, got {other:?}"),
    }

    // A core bus event converted via the From impl reaches the subscriber.
    let bus_event = handfast_core::bus::Event::DeviceFound {
        id: "d1".to_string(),
        name: "Phone".to_string(),
    };
    events_tx.send(bus_event.into()).expect("broadcast send");

    let found = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .expect("device_found within timeout")
        .expect("event channel alive");
    match found {
        ServerEvent::DeviceFound { id, name } => {
            assert_eq!(id, "d1");
            assert_eq!(name, "Phone");
        }
        other => panic!("expected DeviceFound, got {other:?}"),
    }

    // Plain ServerEvents flow through unchanged.
    events_tx.send(ServerEvent::DaemonShutdown).expect("send");
    let shutdown = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .expect("shutdown within timeout")
        .expect("event channel alive");
    assert!(matches!(shutdown, ServerEvent::DaemonShutdown));

    // The receiver is one-shot per connection.
    assert!(client.take_event_receiver().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_frame_gets_connection_dropped_and_server_survives() {
    init_logging();
    let dir = tempfile::tempdir().expect("tempdir");
    let _events = spawn_server(&dir).await;
    let sock = dir.path().join(SOCK_NAME);

    // Speak raw framing to simulate a malicious/broken client: claim a
    // gigantic frame without sending any payload.
    let mut raw = tokio::net::UnixStream::connect(&sock)
        .await
        .expect("raw connect");
    raw.write_all(&(u32::MAX).to_le_bytes())
        .await
        .expect("write header");
    raw.flush().await.expect("flush");

    // The server must reject the frame and close this connection promptly.
    let mut sink = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(2), raw.read_to_end(&mut sink)).await;
    assert!(
        read.is_ok(),
        "server did not close the oversized-frame connection"
    );
    read.expect("timeout already checked")
        .expect("read_to_end io");

    // And the daemon must still serve well-behaved clients afterwards.
    let client = Client::connect(&sock).await.expect("reconnect");
    match client
        .request(Request::Ping)
        .await
        .expect("ping after abuse")
    {
        Response::Ok { .. } => {}
        other => panic!("expected Ok after abusive client, got {other:?}"),
    }
}
