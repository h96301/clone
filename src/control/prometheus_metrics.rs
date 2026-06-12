//! Prometheus metrics — gauges/counters/histograms for VM lifecycle and HTTP API.
//!
//! Exposed at `GET /metrics` in the Prometheus text exposition format.

use std::sync::OnceLock;

use prometheus::{Counter, CounterVec, Encoder, Gauge, GaugeVec, Histogram, Registry, TextEncoder};

/// Global registry — initialized once on first access.
static REGISTRY: OnceLock<Registry> = OnceLock::new();

// Individual metric handles — also initialized once.
static VM_COUNT: OnceLock<Gauge> = OnceLock::new();
static VM_MEMORY: OnceLock<GaugeVec> = OnceLock::new();
static FORK_DURATION: OnceLock<Histogram> = OnceLock::new();
static BRANCH_DURATION: OnceLock<Histogram> = OnceLock::new();
static SNAPSHOT_DURATION: OnceLock<Histogram> = OnceLock::new();
static HTTP_REQUESTS: OnceLock<CounterVec> = OnceLock::new();
static FORK_TOTAL: OnceLock<Counter> = OnceLock::new();
static SNAPSHOT_TOTAL: OnceLock<Counter> = OnceLock::new();

/// Initialize (or return) the global registry and register all metrics.
/// Safe to call repeatedly.
pub fn registry() -> &'static Registry {
    REGISTRY.get_or_init(|| {
        let r = Registry::new();

        let vm_count = Gauge::new("clone_vms_total", "Number of VMs currently tracked by the daemon").unwrap();
        r.register(Box::new(vm_count.clone())).ok();
        VM_COUNT.set(vm_count).ok();

        let vm_memory = GaugeVec::new(
            prometheus::Opts::new(
                "clone_vm_memory_bytes",
                "Memory usage of a single VM in bytes",
            ),
            &["vm_id"],
        )
        .unwrap();
        r.register(Box::new(vm_memory.clone())).ok();
        VM_MEMORY.set(vm_memory).ok();

        let fork_dur = Histogram::with_opts(
            prometheus::HistogramOpts::new(
                "clone_fork_duration_seconds",
                "Time spent forking a VM from a template",
            ),
        )
        .unwrap();
        r.register(Box::new(fork_dur.clone())).ok();
        FORK_DURATION.set(fork_dur).ok();

        let branch_dur = Histogram::with_opts(
            prometheus::HistogramOpts::new(
                "clone_branch_duration_seconds",
                "Time spent live-branching a VM (UFFD_WP based)",
            ),
        )
        .unwrap();
        r.register(Box::new(branch_dur.clone())).ok();
        BRANCH_DURATION.set(branch_dur).ok();

        let snap_dur = Histogram::with_opts(
            prometheus::HistogramOpts::new(
                "clone_snapshot_duration_seconds",
                "Time spent snapshotting a VM",
            ),
        )
        .unwrap();
        r.register(Box::new(snap_dur.clone())).ok();
        SNAPSHOT_DURATION.set(snap_dur).ok();

        let http_reqs = CounterVec::new(
            prometheus::Opts::new(
                "clone_http_requests_total",
                "Number of HTTP requests served",
            ),
            &["method", "path", "status"],
        )
        .unwrap();
        r.register(Box::new(http_reqs.clone())).ok();
        HTTP_REQUESTS.set(http_reqs).ok();

        let fork_total = Counter::new("clone_fork_total", "Total number of fork operations").unwrap();
        r.register(Box::new(fork_total.clone())).ok();
        FORK_TOTAL.set(fork_total).ok();

        let snap_total =
            Counter::new("clone_snapshot_total", "Total number of snapshot operations").unwrap();
        r.register(Box::new(snap_total.clone())).ok();
        SNAPSHOT_TOTAL.set(snap_total).ok();

        r
    })
}

// ---------------------------------------------------------------------------
// Recording helpers
// ---------------------------------------------------------------------------

/// Set the count of currently-tracked VMs.
pub fn set_vm_count(n: i64) {
    let _ = registry();
    if let Some(g) = VM_COUNT.get() {
        g.set(n as f64);
    }
}

/// Update memory usage for a specific VM (label vm_id).
pub fn set_vm_memory(vm_id: &str, bytes: u64) {
    let _ = registry();
    if let Some(g) = VM_MEMORY.get() {
        g.with_label_values(&[vm_id]).set(bytes as f64);
    }
}

/// Clear memory metric labels for a VM that's been destroyed.
pub fn remove_vm_memory(vm_id: &str) {
    let _ = registry();
    if let Some(g) = VM_MEMORY.get() {
        let _ = g.remove_label_values(&[vm_id]);
    }
}

/// Observe a fork operation's duration (seconds).
pub fn observe_fork_duration(secs: f64) {
    let _ = registry();
    if let Some(h) = FORK_DURATION.get() {
        h.observe(secs);
    }
    if let Some(c) = FORK_TOTAL.get() {
        c.inc();
    }
}

/// Observe a branch operation's duration (seconds).
pub fn observe_branch_duration(secs: f64) {
    let _ = registry();
    if let Some(h) = BRANCH_DURATION.get() {
        h.observe(secs);
    }
}

/// Observe a snapshot operation's duration (seconds).
pub fn observe_snapshot_duration(secs: f64) {
    let _ = registry();
    if let Some(h) = SNAPSHOT_DURATION.get() {
        h.observe(secs);
    }
    if let Some(c) = SNAPSHOT_TOTAL.get() {
        c.inc();
    }
}

/// Increment the HTTP request counter for (method, path, status).
pub fn inc_http_request(method: &str, path: &str, status: u16) {
    let _ = registry();
    if let Some(c) = HTTP_REQUESTS.get() {
        c.with_label_values(&[method, path, &status.to_string()]).inc();
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render all metrics in Prometheus text exposition format.
pub fn render() -> String {
    let r = registry();
    let encoder = TextEncoder::new();
    let mut buf = Vec::new();
    let metrics = r.gather();
    if encoder.encode(&metrics, &mut buf).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&buf).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_includes_all_metrics() {
        set_vm_count(3);
        set_vm_memory("vm-0001", 1024 * 1024 * 256);
        observe_fork_duration(0.123);
        observe_snapshot_duration(0.456);
        inc_http_request("GET", "/v1/vms", 200);

        let out = render();
        assert!(out.contains("clone_vms_total"));
        assert!(out.contains("clone_vm_memory_bytes"));
        assert!(out.contains("clone_fork_duration_seconds"));
        assert!(out.contains("clone_snapshot_duration_seconds"));
        assert!(out.contains("clone_http_requests_total"));
    }
}
