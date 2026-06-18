//! Prometheus metrics exported at /metrics.

use lazy_static::lazy_static;
use prometheus::{
    register_histogram_vec, register_int_counter_vec, register_int_gauge,
    register_int_gauge_vec, HistogramVec, IntCounterVec, IntGauge, IntGaugeVec,
};

lazy_static! {
    /// Total number of registered tenants (any status).
    pub static ref TENANTS_TOTAL: IntGauge =
        register_int_gauge!("clone_controller_tenants_total", "Total tenants registered").unwrap();

    /// VM count broken down by status (active/soft_reclaimed/saved/...).
    pub static ref VMS_BY_STATUS: IntGaugeVec = register_int_gauge_vec!(
        "clone_controller_vms_by_status",
        "VMs in each lifecycle status",
        &["status"]
    )
    .unwrap();

    /// Reclaim operations by type (soft/hard).
    pub static ref RECLAIM_TOTAL: IntCounterVec = register_int_counter_vec!(
        "clone_controller_reclaim_total",
        "Reclaim operations triggered",
        &["type"]
    )
    .unwrap();

    /// Acquire call latency by outcome (cold_create / restore / fast_path / error).
    pub static ref ACQUIRE_DURATION: HistogramVec = register_histogram_vec!(
        "clone_controller_acquire_duration_seconds",
        "Time spent in /v1/acquire",
        &["outcome"],
        vec![0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0]
    )
    .unwrap();

    /// Number of failed acquire calls since startup.
    pub static ref ACQUIRE_ERRORS: IntCounterVec = register_int_counter_vec!(
        "clone_controller_acquire_errors_total",
        "Failed acquire calls",
        &["reason"]
    )
    .unwrap();
}

/// Snapshot current gauge values based on the live tenants map.
/// Called from the reconciler after every cycle.
pub fn refresh_status_counts(
    active: u64,
    soft: u64,
    saved: u64,
    starting: u64,
    failed: u64,
    not_found: u64,
) {
    VMS_BY_STATUS.with_label_values(&["active"]).set(active as i64);
    VMS_BY_STATUS.with_label_values(&["soft_reclaimed"]).set(soft as i64);
    VMS_BY_STATUS.with_label_values(&["saved"]).set(saved as i64);
    VMS_BY_STATUS.with_label_values(&["starting"]).set(starting as i64);
    VMS_BY_STATUS.with_label_values(&["failed"]).set(failed as i64);
    VMS_BY_STATUS.with_label_values(&["not_found"]).set(not_found as i64);
}
