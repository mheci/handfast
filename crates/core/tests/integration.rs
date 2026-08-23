//! Cross-crate integration tests for `handfast-core`.
//!
//! These exercise the public surface of the whole crate working together:
//! [`Bus`] + [`Supervisor`] event plumbing, [`Store`] persistence across
//! reopen, and the [`atomic_write`] crash-safe replacement helper.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use handfast_core::bus::{Bus, Event};
use handfast_core::error::Error;
use handfast_core::store::{atomic_write, DeviceRow, Store};
use handfast_core::supervise::Supervisor;

/// Deadline shared by every wait in this suite; CI runners throttle after big
/// compiles, so this is deliberately far above the ~100 ms backoff base.
const EVENT_DEADLINE: Duration = Duration::from_secs(5);

fn sample_device(id: &str, name: &str) -> DeviceRow {
    DeviceRow {
        device_id: id.to_string(),
        name: name.to_string(),
        device_type: "phone".to_string(),
        cert_fingerprint: format!("fp-{id}"),
        paired: true,
        last_seen: Some(1_700_000_000),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bus_and_supervisor_integration() {
    let bus = Bus::new();
    let mut rx = bus.subscribe();

    let supervisor = Supervisor::new(bus.clone());
    let task_bus = bus.clone();
    supervisor.spawn("announcer", move || {
        let bus = task_bus.clone();
        async move {
            bus.publish(Event::DeviceFound {
                id: "dev-42".to_string(),
                name: "Pixel".to_string(),
            });
            Ok::<(), Error>(())
        }
    });

    let received = tokio::time::timeout(EVENT_DEADLINE, rx.recv())
        .await
        .expect("event must arrive within the 5s deadline")
        .expect("bus sender must stay alive");

    match received {
        Event::DeviceFound { id, name } => {
            assert_eq!(id, "dev-42");
            assert_eq!(name, "Pixel");
        }
        other => panic!("expected DeviceFound, got {other:?}"),
    }

    // The task completed cleanly, so supervision must end without a restart.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(supervisor.restart_count("announcer"), 0);

    supervisor.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_survives_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("state.db");

    let expected = vec![
        sample_device("dev-a", "Phone"),
        sample_device("dev-b", "Laptop"),
    ];

    {
        let store = Store::open(&db_path).expect("store open");
        for device in &expected {
            store.upsert_device(device).expect("upsert");
        }
        store.kv_set("ui_pane", "transfers").expect("kv_set");
    } // store dropped here

    let reopened = Store::open(&db_path).expect("store reopen");
    // list_devices orders by device_id, matching the order of `expected`.
    assert_eq!(reopened.list_devices().expect("list_devices"), expected);
    assert_eq!(
        reopened.kv_get("ui_pane").expect("kv_get"),
        Some("transfers".to_string())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn atomic_write_is_atomic() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("device-cert.bin");

    atomic_write(&target, b"generation-one").expect("first write");
    assert_eq!(
        std::fs::read(&target).expect("read back first payload"),
        b"generation-one"
    );

    atomic_write(&target, b"generation-two").expect("overwriting write");
    assert_eq!(
        std::fs::read(&target).expect("read back second payload"),
        b"generation-two"
    );

    // After normal operation the directory holds exactly the final artifact:
    // any `.tmp-*` sibling would mean a crashed write leaked its temp file.
    let leftover_tmp: Vec<_> = std::fs::read_dir(dir.path())
        .expect("readdir")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
        .collect();
    assert!(
        leftover_tmp.is_empty(),
        "temp files leaked after normal operation: {leftover_tmp:?}"
    );

    let entries: Vec<_> = std::fs::read_dir(dir.path())
        .expect("readdir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, vec!["device-cert.bin".to_string()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_restart_preserves_bus_subscription() {
    let bus = Bus::new();
    // Subscribing before the first spawn proves the receiver still sees
    // events produced by the restarted generation.
    let mut rx = bus.subscribe();

    let supervisor = Supervisor::new(bus.clone());
    let hits = Arc::new(AtomicUsize::new(0));

    let task_bus = bus.clone();
    let task_hits = Arc::clone(&hits);
    supervisor.spawn("phoenix", move || {
        let bus = task_bus.clone();
        let hits = Arc::clone(&task_hits);
        async move {
            if hits.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("simulated crash on first attempt");
            }
            bus.publish(Event::DeviceStateChanged {
                id: "phoenix".to_string(),
                state: "recovered".to_string(),
            });
            Ok::<(), Error>(())
        }
    });

    let mut saw_crash_record = false;
    let mut saw_restart_record = false;
    let mut saw_success_event = false;

    let deadline = Instant::now() + EVENT_DEADLINE;
    while !(saw_crash_record && saw_restart_record && saw_success_event) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = tokio::time::timeout(remaining, rx.recv())
            .await
            .expect("timed out waiting for crash/restart/success on the bus")
            .expect("bus sender must stay alive");

        match event {
            Event::LogRecord { level, msg } => {
                if level == "error" && msg.contains("'phoenix'") && msg.contains("panicked") {
                    saw_crash_record = true;
                }
                if level == "warn" && msg.contains("restarting 'phoenix'") {
                    saw_restart_record = true;
                }
            }
            Event::DeviceStateChanged { ref id, state } if id == "phoenix" => {
                assert_eq!(state, "recovered");
                saw_success_event = true;
            }
            _ => {}
        }
    }

    assert_eq!(supervisor.restart_count("phoenix"), 1);
    assert_eq!(hits.load(Ordering::SeqCst), 2);

    supervisor.shutdown_all().await;
}
