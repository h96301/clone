//! Background reconciler: every N seconds, inspect each Active tenant and
//! drive it toward soft reclaim (balloon + drop_caches) or hard reclaim
//! (save + kill) based on idle time and TCP probe results.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::net::TcpStream;

use crate::metrics;
use crate::state::{ControllerState, TenantState, VmStatus};

/// Spawn the background reconciler loop. Runs until the process exits.
pub fn spawn(state: Arc<ControllerState>) {
    let interval_secs = state.config.reconcile_interval_secs;
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        // First tick fires immediately — skip it so we don't run on startup.
        ticker.tick().await;
        tracing::info!(interval_secs, "reconciler loop started");
        loop {
            ticker.tick().await;
            if let Err(e) = reconcile_cycle(&state).await {
                tracing::error!(error = ?e, "reconcile cycle failed");
            }
        }
    });
}

async fn reconcile_cycle(state: &Arc<ControllerState>) -> anyhow::Result<()> {
    // Snapshot the tenants so we can work without holding the lock.
    let snapshots: Vec<(String, TenantState)> = {
        let tenants = state.tenants.read().await;
        tenants
            .iter()
            .map(|(id, t)| (id.clone(), t.clone()))
            .collect()
    };

    let mut counts = StatusCounts::default();
    for (_, t) in &snapshots {
        counts.add(&t.status);
    }

    for (id, t) in snapshots {
        // Active and SoftReclaimed both need reconciliation: Active may go soft/hard,
        // SoftReclaimed may go hard if still idle.
        if !matches!(t.status, VmStatus::Active | VmStatus::SoftReclaimed) {
            continue;
        }
        // Each tenant reconciled independently so one failure doesn't block others.
        let st = state.clone();
        let idc = id.clone();
        tokio::spawn(async move {
            if let Err(e) = reconcile_tenant(&st, &idc, t).await {
                tracing::warn!(tenant = %idc, error = ?e, "reconcile_tenant failed");
            }
        });
    }

    // Refresh Prometheus gauges.
    metrics::TENANTS_TOTAL.set(counts.total() as i64);
    metrics::refresh_status_counts(
        counts.active,
        counts.soft,
        counts.saved,
        counts.starting,
        counts.failed,
        counts.not_found,
    );
    Ok(())
}

#[derive(Default)]
struct StatusCounts {
    active: u64,
    soft: u64,
    saved: u64,
    starting: u64,
    failed: u64,
    not_found: u64,
}

impl StatusCounts {
    fn add(&mut self, s: &VmStatus) {
        match s {
            VmStatus::Active => self.active += 1,
            VmStatus::SoftReclaimed => self.soft += 1,
            VmStatus::Saved => self.saved += 1,
            VmStatus::Starting | VmStatus::Saving => self.starting += 1,
            VmStatus::Failed(_) => self.failed += 1,
            VmStatus::NotFound => self.not_found += 1,
        }
    }
    fn total(&self) -> u64 {
        self.active + self.soft + self.saved + self.starting + self.failed + self.not_found
    }
}

async fn reconcile_tenant(
    state: &Arc<ControllerState>,
    tenant_id: &str,
    mut t: TenantState,
) -> anyhow::Result<()> {
    let cfg = state.config.clone();
    let client = state.clone_client.clone();

    // 1. Health probe as idle fallback. A successful probe resets last_request_at
    //    (so a tenant that's serving but openresty forgot to call /release
    //    still stays alive).
    if cfg.health_probe_enabled {
        if let (Some(ip), Some(port)) = (&t.guest_ip, t.config.app_health_port) {
            match tcp_probe(ip, port).await {
                Ok(true) => {
                    t.last_request_at = Some(Utc::now());
                    t.last_health_ok_at = Some(Utc::now());
                    t.health_fail_count = 0;
                }
                Ok(false) => {
                    t.health_fail_count += 1;
                    tracing::debug!(
                        tenant = tenant_id,
                        fails = t.health_fail_count,
                        "health probe failed"
                    );
                }
                Err(e) => {
                    t.health_fail_count += 1;
                    tracing::debug!(tenant = tenant_id, error = ?e, "health probe error");
                }
            }
        }
    }

    let idle_secs = t.idle_secs().unwrap_or(i64::MAX);

    // 2. Hard reclaim: idle for too long → save + kill.
    if idle_secs >= cfg.hard_reclaim_after_secs as i64
        || t.health_fail_count >= cfg.health_probe_fail_threshold
    {
        if let Some(vm_id) = t.vm_id.clone() {
            let save_path = t
                .save_path
                .clone()
                .unwrap_or_else(|| TenantState::default_save_path(&cfg.saves_root, tenant_id));

            tracing::info!(tenant = tenant_id, vm_id = %vm_id, save_path = %save_path, "hard reclaim: save + destroy");

            // Save may take ~20s/GB; mark Saving so acquire returns 503 meanwhile.
            {
                let mut tenants = state.tenants.write().await;
                if let Some(t) = tenants.get_mut(tenant_id) {
                    t.status = VmStatus::Saving;
                }
            }

            match client.save_vm(&vm_id, &save_path).await {
                Ok(()) => {}
                Err(e) => {
                    // Save failed — try to kill anyway so we don't leak a VM,
                    // but keep status as Failed so the next acquire retries cold.
                    tracing::error!(tenant = tenant_id, error = ?e, "save failed; destroying anyway");
                    let _ = client.destroy_vm(&vm_id).await;
                    let mut tenants = state.tenants.write().await;
                    if let Some(st) = tenants.get_mut(tenant_id) {
                        st.status = VmStatus::Failed(format!("save: {e}"));
                        st.last_error = Some(e.to_string());
                        st.vm_id = None;
                        st.guest_ip = None;
                    }
                    let _ = state.persist().await;
                    metrics::RECLAIM_TOTAL.with_label_values(&["hard_error"]).inc();
                    return Ok(());
                }
            }

            if let Err(e) = client.destroy_vm(&vm_id).await {
                tracing::warn!(tenant = tenant_id, error = ?e, "destroy after save failed; VM may linger");
            }

            t.status = VmStatus::Saved;
            t.vm_id = None;
            t.guest_ip = None;
            t.save_path = Some(save_path);
            t.reclaim_count += 1;
            t.health_fail_count = 0;

            let mut tenants = state.tenants.write().await;
            if let Some(st) = tenants.get_mut(tenant_id) {
                *st = t.clone();
            }
            drop(tenants);
            let _ = state.persist().await;
            metrics::RECLAIM_TOTAL.with_label_values(&["hard"]).inc();
            return Ok(());
        }
    }

    // 3. Soft reclaim: only when still Active (SoftReclaimed already did this once).
    //    Drop_caches + balloon inflate. Subsequent ticks fall through until hard threshold.
    if t.status == VmStatus::Active && idle_secs >= cfg.soft_reclaim_after_secs as i64 {
        if let Some(vm_id) = t.vm_id.clone() {
            tracing::info!(
                tenant = tenant_id,
                vm_id = %vm_id,
                idle_secs,
                target_mb = cfg.balloon_target_mb,
                "soft reclaim: drop_caches + balloon"
            );
            if let Err(e) = client
                .exec_vm(&vm_id, "sh", &["-c", "echo 3 > /proc/sys/vm/drop_caches"])
                .await
            {
                tracing::warn!(tenant = tenant_id, error = ?e, "drop_caches exec failed");
            }
            if let Err(e) = client.balloon_vm(&vm_id, cfg.balloon_target_mb).await {
                tracing::warn!(tenant = tenant_id, error = ?e, "balloon inflate failed");
            }
            t.status = VmStatus::SoftReclaimed;
            t.reclaim_count += 1;
            let mut tenants = state.tenants.write().await;
            if let Some(st) = tenants.get_mut(tenant_id) {
                *st = t.clone();
            }
            drop(tenants);
            let _ = state.persist().await;
            metrics::RECLAIM_TOTAL.with_label_values(&["soft"]).inc();
            return Ok(());
        }
    }

    // 4. Still active — only persist probe results if they changed.
    if t.health_fail_count > 0 {
        let mut tenants = state.tenants.write().await;
        if let Some(st) = tenants.get_mut(tenant_id) {
            st.health_fail_count = t.health_fail_count;
            st.last_health_ok_at = t.last_health_ok_at;
        }
        drop(tenants);
        let _ = state.persist().await;
    }
    Ok(())
}

/// Quick TCP connect probe from the host side.
async fn tcp_probe(ip: &str, port: u16) -> anyhow::Result<bool> {
    let addr: std::net::SocketAddr = format!("{ip}:{port}")
        .parse()
        .map_err(|e| anyhow::anyhow!("bad addr: {e}"))?;
    match tokio::time::timeout(
        Duration::from_millis(800),
        TcpStream::connect(&addr),
    )
    .await
    {
        Ok(Ok(_)) => Ok(true),
        Ok(Err(_) ) => Ok(false),
        Err(_) => Ok(false), // timeout = not ready
    }
}
