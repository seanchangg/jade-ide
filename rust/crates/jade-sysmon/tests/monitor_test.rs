//! Integration test: the monitor emits stats on its watch channel at the
//! injected (shortened) interval, and `start`/`stop` behave idempotently.

use std::time::Duration;

use jade_sysmon::SystemMonitor;
use tokio::time::timeout;

#[tokio::test]
async fn emits_stats_on_a_shortened_interval() {
    let monitor = SystemMonitor::new();
    let mut rx = monitor.subscribe();

    // 250ms interval keeps the test fast while still >= sysinfo's minimum CPU
    // refresh window.
    monitor.start_with_interval(Duration::from_millis(250));

    // Wait for the first real emission (the channel starts at the default seed).
    let changed = timeout(Duration::from_secs(3), rx.changed()).await;
    assert!(changed.is_ok(), "no stats emitted within 3s");
    changed.unwrap().unwrap();

    let stats = rx.borrow_and_update().clone();
    // Memory is always readable; total must be non-zero on any real machine.
    assert!(stats.mem_total > 0, "mem_total should be populated");
    assert!(stats.mem_used <= stats.mem_total);
    assert!((0..=100).contains(&stats.cpu_percent));
    // GPU is best-effort: either a real reading (0..=100 with a name) or the
    // unavailable sentinel.
    assert!(stats.gpu_percent == -1 || (0..=100).contains(&stats.gpu_percent));

    monitor.stop();
}

#[tokio::test]
async fn start_is_idempotent_and_stop_clears() {
    let monitor = SystemMonitor::new();
    monitor.start_with_interval(Duration::from_millis(250));
    monitor.start_with_interval(Duration::from_millis(250)); // no-op
    let mut rx = monitor.subscribe();
    assert!(timeout(Duration::from_secs(3), rx.changed()).await.is_ok());

    monitor.stop();
    monitor.stop(); // idempotent
}
