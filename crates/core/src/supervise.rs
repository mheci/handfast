//! Minimal supervision tree for long-running tasks.
//!
//! [`Supervisor`] spawns named tasks from restartable factories. Whenever a
//! child finishes with [`Err`] or panics, the supervisor logs the crash as a
//! [`Event::LogRecord`] on the [`Bus`] and respawns the factory after an
//! exponential backoff: 100 ms doubled per consecutive failure, capped at 30 s,
//! and reset to 100 ms once a child has stayed alive for 30 seconds.
//!
//! Children are plain `tokio::spawn`ed tasks; panics surface through their
//! [`JoinHandle`](tokio::task::JoinHandle) as [`JoinError`](tokio::task::JoinError)
//! values, so no catch-unwind machinery is needed here. Spawning a name twice
//! supersedes the previous generation: the old child and its watcher are
//! aborted.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;

use crate::bus::{Bus, Event};
use crate::error::Result;

/// First retry delay after a crash.
const BASE_BACKOFF: Duration = Duration::from_millis(100);
/// Upper bound for retry delays.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Continuous healthy runtime that resets the backoff sequence.
const HEALTHY_WINDOW: Duration = Duration::from_secs(30);

/// One supervised task: its bookkeeping generation plus the handles needed to
/// tear it down again.
#[derive(Debug)]
struct Entry {
    /// Monotonic generation counter bumped on every (re)spawn request.
    generation: u64,
    /// The watcher loop that owns the current child and schedules retries.
    watcher: JoinHandle<()>,
    /// Most recently spawned child, if any; cleared by [`Supervisor::shutdown_all`].
    latest_child: Arc<Mutex<Option<JoinHandle<()>>>>,
}

#[derive(Debug)]
struct Inner {
    bus: Bus,
    /// Global kill switch checked by watchers between attempts.
    shutdown: AtomicBool,
    /// Monotonic source for [`Entry::generation`] values.
    generations: AtomicU64,
    /// Restart counters per task name.
    restarts: Mutex<HashMap<String, u64>>,
    /// Live supervision entries keyed by task name.
    children: Mutex<HashMap<String, Entry>>,
}

impl Inner {
    fn report_crash(&self, name: &str, detail: &str) {
        tracing::warn!(target: "handfast::supervisor", task = %name, "{detail}");
        self.bus.publish(Event::LogRecord {
            level: "error".to_string(),
            msg: format!("supervisor: task '{name}' {detail}"),
        });
    }

    fn bump_restarts(&self, name: &str) -> u64 {
        let mut restarts = lock(&self.restarts);
        let counter = restarts.entry(name.to_string()).or_insert(0);
        *counter += 1;
        *counter
    }
}

/// Lock a possibly poisoned mutex, recovering the guard regardless of poison.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Compute the retry delay for the n-th consecutive failure.
///
/// Attempt 1 retries after [`BASE_BACKOFF`], each further consecutive failure
/// doubles the delay up to [`MAX_BACKOFF`].
fn backoff_delay(consecutive_failures: u32) -> Duration {
    let shift = consecutive_failures.saturating_sub(1).min(20);
    BASE_BACKOFF.saturating_mul(1u32 << shift).min(MAX_BACKOFF)
}

/// Supervision tree root; cheap to clone and share across tasks.
#[derive(Debug, Clone)]
pub struct Supervisor {
    inner: Arc<Inner>,
}

impl Supervisor {
    /// Create a supervisor reporting crashes through `bus`.
    #[must_use]
    pub fn new(bus: Bus) -> Self {
        Self {
            inner: Arc::new(Inner {
                bus,
                shutdown: AtomicBool::new(false),
                generations: AtomicU64::new(0),
                restarts: Mutex::new(HashMap::new()),
                children: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Spawn a named task built by `factory`.
    ///
    /// The factory is invoked once per attempt, producing a fresh future each
    /// time; state that must survive restarts has to live behind the closure
    /// (e.g. an `Arc`). A task whose future resolves to `Ok(())` ends its own
    /// supervision; anything else is retried with backoff. Reusing a name
    /// aborts and replaces the previous incarnation.
    pub fn spawn<F, Fut>(&self, name: &'static str, factory: F)
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        if self.inner.shutdown.load(Ordering::SeqCst) {
            return;
        }

        let generation = self.inner.generations.fetch_add(1, Ordering::SeqCst) + 1;
        let latest_child: Arc<Mutex<Option<JoinHandle<()>>>> = Arc::new(Mutex::new(None));
        let watcher = tokio::spawn(watch_loop(
            self.inner.clone(),
            name.to_string(),
            factory,
            latest_child.clone(),
        ));

        let previous =
            lock(&self.inner.children).insert(name.to_string(), Entry {
                generation,
                watcher,
                latest_child,
            });

        // Supersede the previous generation, if any.
        if let Some(old) = previous {
            if let Some(child) = lock(&old.latest_child).take() {
                child.abort();
            }
            old.watcher.abort();
        }
    }

    /// How often the named task has been restarted so far (0 if unknown).
    #[must_use]
    pub fn restart_count(&self, name: &str) -> u64 {
        lock(&self.inner.restarts).get(name).copied().unwrap_or(0)
    }

    /// Abort every supervised task and its watcher, waiting for both to finish.
    ///
    /// Idempotent; later [`Supervisor::spawn`] calls become no-ops.
    pub async fn shutdown_all(&self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);

        let entries: Vec<Entry> = lock(&self.inner.children)
            .drain()
            .map(|(_, entry)| entry)
            .collect();

        // Stop respawn loops first so they cannot race the child teardown below.
        for entry in &entries {
            entry.watcher.abort();
        }
        let mut children = Vec::with_capacity(entries.len());
        for entry in entries {
            // Awaiting the dead watcher guarantees no further child writes.
            let _ = entry.watcher.await;
            if let Some(child) = lock(&entry.latest_child).take() {
                children.push(child);
            }
        }

        for child in &children {
            child.abort();
        }
        for child in children {
            let _ = child.await;
        }
    }
}

/// Watcher body: run the child, classify the outcome, schedule retries.
async fn watch_loop<F, Fut>(
    inner: Arc<Inner>,
    name: String,
    factory: F,
    latest_child: Arc<Mutex<Option<JoinHandle<()>>>>,
) where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    let mut consecutive_failures: u32 = 0;

    while !inner.shutdown.load(Ordering::SeqCst) {
        let started = Instant::now();
        let child = tokio::spawn(factory());
        *lock(&latest_child) = Some(child.clone());

        match child.await {
            // Clean completion: this task's supervision ends.
            Ok(Ok(())) => {
                tracing::debug!(
                    target: "handfast::supervisor",
                    task = %name,
                    "supervised task finished cleanly"
                );
                return;
            }
            Ok(Err(err)) => inner.report_crash(&name, &format!("failed: {err}")),
            Err(join_err) if join_err.is_cancelled() => return,
            Err(join_err) => {
                inner.report_crash(&name, &format!("panicked: {join_err}"));
            }
        }

        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }

        let restart_count = inner.bump_restarts(&name);
        if started.elapsed() >= HEALTHY_WINDOW {
            consecutive_failures = 0;
        } else {
            consecutive_failures = consecutive_failures.saturating_add(1);
        }
        let delay = backoff_delay(consecutive_failures);

        inner.bus.publish(Event::LogRecord {
            level: "warn".to_string(),
            msg: format!(
                "supervisor: restarting '{name}' (restart #{restart_count}, \
                 retry in {delay:?})"
            ),
        });

        tokio::time::sleep(delay).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use std::sync::atomic::AtomicUsize;

    /// Poll `check` until it returns true, with a generous real-time deadline.
    ///
    /// The workspace tokio does not enable the `test-util` feature, so paused
    /// clocks are unavailable; polling keeps these tests deterministic enough
    /// while staying fast (backoff base is only 100 ms).
    async fn wait_for(mut check: impl FnMut() -> bool, label: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !check() {
            assert!(Instant::now() < deadline, "timed out waiting for {label}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Factory whose first invocation panics and whose second succeeds.
    ///
    /// Returns a boxed-future closure because nested `impl Trait` is not
    /// allowed and the concrete async block type is unnameable.
    fn panic_once_factory(
        hits: Arc<AtomicUsize>,
    ) -> impl Fn() -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send>>
           + Send
           + Sync
           + 'static {
        move || {
            let hits = Arc::clone(&hits);
            Box::pin(async move {
                if hits.fetch_add(1, Ordering::SeqCst) == 0 {
                    panic!("boom");
                }
                Ok::<(), Error>(())
            })
        }
    }

    #[tokio::test]
    async fn restarts_panicking_task_once_then_succeeds() {
        let supervisor = Supervisor::new(Bus::new());
        let hits = Arc::new(AtomicUsize::new(0));

        supervisor.spawn("panicky", panic_once_factory(Arc::clone(&hits)));

        wait_for(
            || supervisor.restart_count("panicky") == 1 && hits.load(Ordering::SeqCst) == 2,
            "panic-then-success restart",
        )
        .await;

        supervisor.shutdown_all().await;
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn clean_completion_stops_supervision() {
        let supervisor = Supervisor::new(Bus::new());
        supervisor.spawn("one_shot", || async { Ok::<(), Error>(()) });

        // Let the task finish; it must not be restarted afterwards.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(supervisor.restart_count("one_shot"), 0);

        supervisor.shutdown_all().await;
    }

    #[tokio::test]
    async fn crashes_are_reported_on_the_bus() {
        let bus = Bus::new();
        let mut rx = bus.subscribe();
        let supervisor = Supervisor::new(bus);

        let hits = Arc::new(AtomicUsize::new(0));
        supervisor.spawn("crasher", panic_once_factory(Arc::clone(&hits)));

        let mut saw_crash_record = false;
        let mut saw_restart_record = false;
        wait_for(
            || {
                while let Ok(event) = rx.try_recv() {
                    if let Event::LogRecord { level, ref msg } = event {
                        if level == "error" && msg.contains("'crasher'") {
                            saw_crash_record = true;
                        }
                        if level == "warn" && msg.contains("restarting 'crasher'") {
                            saw_restart_record = true;
                        }
                    }
                }
                saw_crash_record && saw_restart_record
            },
            "crash and restart LogRecords",
        )
        .await;

        supervisor.shutdown_all().await;
    }

    #[tokio::test]
    async fn shutdown_aborts_children_promptly() {
        let supervisor = Supervisor::new(Bus::new());
        supervisor.spawn("forever", || std::future::pending::<Result<()>>());

        // Give the child a moment to start, then verify teardown completes.
        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::time::timeout(Duration::from_secs(2), supervisor.shutdown_all())
            .await
            .expect("shutdown_all must not hang");
    }

    #[test]
    fn backoff_doubles_and_caps_at_thirty_seconds() {
        assert_eq!(backoff_delay(1), Duration::from_millis(100));
        assert_eq!(backoff_delay(2), Duration::from_millis(200));
        assert_eq!(backoff_delay(3), Duration::from_millis(400));
        assert_eq!(backoff_delay(10), Duration::from_millis(51_200));
        assert_eq!(backoff_delay(u32::MAX), MAX_BACKOFF);
    }
}
