use std::time::{Duration, Instant};

use handfast_daemon::control::{send_request, ControlError, ControlRequest, ControlResponse};
use handfast_daemon::{run, DaemonArgs};
use tokio::sync::oneshot;

// Both tests scan the fixed KDE Connect TCP range. Run them one at a time so
// the port probes cannot race each other.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn run_returns_when_the_shutdown_future_completes() {
    let _serial = SERIAL.lock().await;
    // Keep the control socket under the temp data directory instead of the
    // user's runtime directory.
    std::env::remove_var("XDG_RUNTIME_DIR");
    let directory = tempfile::tempdir().expect("create the test state directory");
    let tcp_port = free_tcp_port();
    let (shutdown, shutdown_rx) = oneshot::channel();
    let args = DaemonArgs {
        data_dir: Some(directory.path().join("data")),
        incoming_dir: Some(directory.path().join("incoming")),
        tcp_port,
        discovery_port: free_udp_port(),
        log_file: None,
        print_identity: false,
        config_file: None,
    };
    let daemon = tokio::spawn(run(args, async {
        let _ = shutdown_rx.await;
    }));

    wait_for_listener(tcp_port).await;
    shutdown.send(()).expect("send the shutdown signal");
    let result = tokio::time::timeout(Duration::from_secs(6), daemon)
        .await
        .expect("run returns within the shutdown grace")
        .expect("the daemon task joins");
    assert!(result.is_ok(), "the daemon shuts down cleanly: {result:?}");
}

#[tokio::test]
async fn run_returns_when_the_control_channel_requests_shutdown() {
    let _serial = SERIAL.lock().await;
    std::env::remove_var("XDG_RUNTIME_DIR");
    let directory = tempfile::tempdir().expect("create the test state directory");
    let data_dir = directory.path().join("data");
    let tcp_port = free_tcp_port();
    let args = DaemonArgs {
        data_dir: Some(data_dir.clone()),
        incoming_dir: Some(directory.path().join("incoming")),
        tcp_port,
        discovery_port: free_udp_port(),
        log_file: None,
        print_identity: false,
        config_file: None,
    };
    let daemon = tokio::spawn(run(args, std::future::pending()));

    wait_for_listener(tcp_port).await;
    let response = wait_for_shutdown_response(&data_dir).await;
    assert_eq!(response, ControlResponse::ShuttingDown);

    let result = tokio::time::timeout(Duration::from_secs(6), daemon)
        .await
        .expect("run returns within the shutdown grace")
        .expect("the daemon task joins");
    assert!(result.is_ok(), "the daemon shuts down cleanly: {result:?}");
}

/// The control server starts after the TCP listener, so the first request can
/// arrive before the endpoint exists.
async fn wait_for_shutdown_response(data_dir: &std::path::Path) -> ControlResponse {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match send_request(data_dir, &ControlRequest::Shutdown).await {
            Ok(response) => return response,
            Err(ControlError::NotRunning) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(error) => panic!("the daemon answers the shutdown request: {error}"),
        }
    }
}

async fn wait_for_listener(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon never bound port {port}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn free_tcp_port() -> u16 {
    for port in handfast_proto::MIN_TCP_PORT..=handfast_proto::MAX_TCP_PORT {
        if let Ok(listener) = std::net::TcpListener::bind(("0.0.0.0", port)) {
            drop(listener);
            return port;
        }
    }
    panic!("no free TCP port in the KDE Connect range");
}

fn free_udp_port() -> u16 {
    let socket = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
    socket.local_addr().unwrap().port()
}
