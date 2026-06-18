//! HTTP API: tenant CRUD + acquire/release + health/metrics.
//!
//! The `acquire` handler is the heart of the controller — it implements the
//! state machine that turns tenant status into the right clone daemon call:
//! NotFound → cold run, Saved → restore, SoftReclaimed → balloon up.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::clone_client::{CreateVmReq, RestoreReq};
use crate::metrics;
use crate::state::{ControllerState, TenantConfig, TenantState, VmStatus};

#[derive(Clone)]
pub struct AppState {
    pub state: Arc<ControllerState>,
}

pub fn router(state: Arc<ControllerState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(prometheus_metrics))
        .route(
            "/v1/tenants",
            post(create_tenant).get(list_tenants),
        )
        .route(
            "/v1/tenants/{id}",
            get(get_tenant).delete(delete_tenant),
        )
        .route("/v1/acquire", post(acquire))
        .route("/v1/release", post(release))
        .with_state(AppState { state })
}

// ---- Standard responses ---------------------------------------------------

#[derive(Serialize)]
struct ApiError {
    error: String,
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (
        status,
        Json(ApiError {
            error: msg.into(),
        }),
    )
        .into_response()
}

// ---- Health & metrics -----------------------------------------------------

async fn health() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

async fn prometheus_metrics() -> impl IntoResponse {
    use prometheus::TextEncoder;
    let encoder = TextEncoder::new();
    let mf = prometheus::gather();
    let body = encoder
        .encode_to_string(&mf)
        .unwrap_or_else(|e| format!("# encode error: {e}"));
    (
        [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        body,
    )
}

// ---- Tenant CRUD ----------------------------------------------------------

async fn create_tenant(
    State(app): State<AppState>,
    Json(cfg): Json<TenantConfig>,
) -> Response {
    if cfg.tenant_id.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "tenant_id is required");
    }
    if cfg.kernel.trim().is_empty() || cfg.rootfs.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "kernel and rootfs are required");
    }
    if cfg.overlay_path.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "overlay_path is required");
    }

    let mut tenants = app.state.tenants.write().await;
    if tenants.contains_key(&cfg.tenant_id) {
        return err(
            StatusCode::CONFLICT,
            format!("tenant {} already exists", cfg.tenant_id),
        );
    }
    let tenant_state = TenantState::new(cfg.clone());
    tenants.insert(cfg.tenant_id.clone(), tenant_state);
    drop(tenants);

    if let Err(e) = app.state.persist().await {
        tracing::error!(error = ?e, "persist failed");
    }
    metrics::TENANTS_TOTAL.inc();

    tracing::info!(tenant = %cfg.tenant_id, "tenant registered");
    (
        StatusCode::CREATED,
        Json(json!({"ok": true, "tenant_id": cfg.tenant_id})),
    )
        .into_response()
}

async fn list_tenants(State(app): State<AppState>) -> impl IntoResponse {
    let tenants = app.state.tenants.read().await;
    let list: Vec<TenantState> = tenants.values().cloned().collect();
    Json(list)
}

async fn get_tenant(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let tenants = app.state.tenants.read().await;
    match tenants.get(&id) {
        Some(t) => Json(t).into_response(),
        None => err(StatusCode::NOT_FOUND, format!("tenant {id} not found")),
    }
}

async fn delete_tenant(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let removed = {
        let mut tenants = app.state.tenants.write().await;
        tenants.remove(&id)
    };
    let Some(t) = removed else {
        return err(StatusCode::NOT_FOUND, format!("tenant {id} not found"));
    };

    // Best-effort: kill any running VM and delete its save files.
    if let Some(vm_id) = &t.vm_id {
        if !matches!(t.status, VmStatus::Saved | VmStatus::NotFound) {
            if let Err(e) = app.state.clone_client.destroy_vm(vm_id).await {
                tracing::warn!(tenant = %id, error = ?e, "destroy_vm during delete");
            }
        }
    }
    if let Some(save_path) = &t.save_path {
        if let Err(e) = tokio::fs::remove_dir_all(save_path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(tenant = %id, error = ?e, "remove save_path during delete");
            }
        }
    }
    let _ = app.state.persist().await;
    metrics::TENANTS_TOTAL.dec();

    tracing::info!(tenant = %id, "tenant deleted");
    Json(json!({"ok": true})).into_response()
}

// ---- acquire / release ----------------------------------------------------

#[derive(Deserialize)]
struct TenantQuery {
    id: String,
}

#[derive(Serialize)]
struct AcquireResp {
    ip: String,
    vm_id: String,
    status: &'static str,
    outcome: &'static str, // cold_create | restore | fast_path | recover
}

async fn acquire(
    State(app): State<AppState>,
    Query(q): Query<TenantQuery>,
) -> Response {
    let start = Instant::now();
    let tenant_id = q.id.clone();

    // Short-lived write lock to transition state.
    let outcome = {
        let mut tenants = app.state.tenants.write().await;
        let Some(t) = tenants.get_mut(&tenant_id) else {
            return err(StatusCode::NOT_FOUND, format!("tenant {tenant_id} not found"));
        };
        match t.status.clone() {
            VmStatus::NotFound => "cold_create",
            VmStatus::Saved => "restore",
            VmStatus::SoftReclaimed => "recover",
            VmStatus::Active => "fast_path",
            VmStatus::Starting | VmStatus::Saving => {
                return err(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("tenant {} is {}, retry later", tenant_id, t.status.as_str()),
                );
            }
            VmStatus::Failed(msg) => {
                tracing::warn!(tenant = %tenant_id, prior_error = %msg, "retrying from Failed");
                // Decide between restore (have save) and cold create (no save).
                if t.save_path.is_some() {
                    "restore"
                } else {
                    "cold_create"
                }
            }
        }
    };

    // Perform the actual clone daemon call outside of the lock.
    let result = match outcome {
        "cold_create" => cold_create(&app.state, &tenant_id).await,
        "restore" => do_restore(&app.state, &tenant_id).await,
        "recover" => recover_from_soft(&app.state, &tenant_id).await,
        "fast_path" => Ok(()),
        _ => unreachable!(),
    };

    let duration = start.elapsed();
    metrics::ACQUIRE_DURATION
        .with_label_values(&[outcome])
        .observe(duration.as_secs_f64());

    if let Err(e) = result {
        metrics::ACQUIRE_ERRORS.with_label_values(&[outcome]).inc();
        // Mark tenant as Failed.
        {
            let mut tenants = app.state.tenants.write().await;
            if let Some(t) = tenants.get_mut(&tenant_id) {
                t.status = VmStatus::Failed(e.to_string());
                t.last_error = Some(e.to_string());
            }
        }
        let _ = app.state.persist().await;
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    }

    // Touch last_request_at and read back the IP/vm_id for the response.
    let (ip, vm_id) = {
        let mut tenants = app.state.tenants.write().await;
        let Some(t) = tenants.get_mut(&tenant_id) else {
            return err(StatusCode::INTERNAL_SERVER_ERROR, "tenant vanished");
        };
        t.last_request_at = Some(chrono::Utc::now());
        t.health_fail_count = 0;
        t.last_error = None;
        let ip = t.guest_ip.clone().unwrap_or_default();
        let vm_id = t.vm_id.clone().unwrap_or_default();
        (ip, vm_id)
    };
    let _ = app.state.persist().await;

    tracing::info!(
        tenant = %tenant_id,
        outcome,
        duration_ms = duration.as_millis() as u64,
        ip = %ip,
        "acquire completed"
    );

    Json(AcquireResp {
        ip,
        vm_id,
        status: "active",
        outcome,
    })
    .into_response()
}

async fn release(State(app): State<AppState>, Query(q): Query<TenantQuery>) -> Response {
    let now = chrono::Utc::now();
    let mut tenants = app.state.tenants.write().await;
    let Some(t) = tenants.get_mut(&q.id) else {
        return err(StatusCode::NOT_FOUND, format!("tenant {} not found", q.id));
    };
    t.last_request_at = Some(now);
    // If VM was soft-reclaimed, restore to Active since traffic just arrived.
    if matches!(t.status, VmStatus::SoftReclaimed) {
        t.status = VmStatus::Active;
    }
    drop(tenants);
    let _ = app.state.persist().await;
    Json(json!({"ok": true})).into_response()
}

// ---- State machine helpers (called outside RwLock) ------------------------

async fn cold_create(state: &ControllerState, tenant_id: &str) -> anyhow::Result<()> {
    // Mark as Starting.
    {
        let mut tenants = state.tenants.write().await;
        let t = tenants
            .get_mut(tenant_id)
            .ok_or_else(|| anyhow::anyhow!("tenant vanished"))?;
        t.status = VmStatus::Starting;
    }
    let _ = state.persist().await;

    let cfg = {
        let tenants = state.tenants.read().await;
        tenants.get(tenant_id).map(|t| t.config.clone()).unwrap()
    };

    // Build kernel cmdline. Always append clone.init if specified so we skip systemd.
    let cmdline = build_cmdline(&cfg);

    // Ensure overlay file exists (4GB ext4 default; reuse if present).
    ensure_overlay(&cfg.overlay_path).await?;

    let req = CreateVmReq {
        kernel: cfg.kernel.clone(),
        rootfs: Some(cfg.rootfs.clone()),
        overlay: Some(cfg.overlay_path.clone()),
        shared_dir: cfg.shared_dir.clone(),
        cmdline: Some(cmdline),
        mem_mb: Some(cfg.mem_mb),
        vcpus: Some(1),
        net: Some(true),
        memory_limit_mb: cfg.memory_limit_mb,
    };

    let resp = state.clone_client.create_vm(req).await?;
    let vm_id = resp.vm_id;

    // Fetch the guest IP. clone daemon doesn't return it from /v1/vms in
    // current versions, so we probe via exec.
    let ip = match state.clone_client.fetch_guest_ip(&vm_id).await {
        Ok(ip) => ip,
        Err(e) => {
            tracing::warn!(tenant = tenant_id, vm_id = %vm_id, error = ?e, "fetch_guest_ip failed");
            // Cleanup the half-booted VM so a retry is clean.
            let _ = state.clone_client.destroy_vm(&vm_id).await;
            return Err(e.context("fetch_guest_ip"));
        }
    };

    // Optional: wait for app port to come up.
    if let Some(port) = cfg.app_health_port {
        wait_for_port(&ip, port, Duration::from_secs(15)).await?;
    }

    // Persist active state.
    {
        let mut tenants = state.tenants.write().await;
        let t = tenants.get_mut(tenant_id).unwrap();
        t.status = VmStatus::Active;
        t.vm_id = Some(vm_id);
        t.guest_ip = Some(ip);
        t.save_path = Some(TenantState::default_save_path(
            &state.config.saves_root,
            tenant_id,
        ));
    }
    let _ = state.persist().await;
    Ok(())
}

async fn do_restore(state: &ControllerState, tenant_id: &str) -> anyhow::Result<()> {
    let (save_path, shared_dir, mem_limit) = {
        let tenants = state.tenants.read().await;
        let t = tenants
            .get(tenant_id)
            .ok_or_else(|| anyhow::anyhow!("tenant vanished"))?;
        let save = t
            .save_path
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no save_path; cannot restore"))?;
        (save, t.config.shared_dir.clone(), t.config.memory_limit_mb)
    };

    {
        let mut tenants = state.tenants.write().await;
        tenants.get_mut(tenant_id).unwrap().status = VmStatus::Starting;
    }
    let _ = state.persist().await;

    let req = RestoreReq {
        snapshot_path: save_path,
        net: Some(true),
        shared_dir,
        memory_limit_mb: mem_limit,
    };
    let resp = state.clone_client.restore_vm(req).await?;
    let vm_id = resp.vm_id;

    // Fetch guest IP (restore may assign a different one than the original).
    let ip = match state.clone_client.fetch_guest_ip(&vm_id).await {
        Ok(ip) => ip,
        Err(e) => {
            tracing::warn!(tenant = tenant_id, vm_id = %vm_id, error = ?e, "fetch_guest_ip failed after restore");
            let _ = state.clone_client.destroy_vm(&vm_id).await;
            return Err(e.context("fetch_guest_ip after restore"));
        }
    };

    {
        let mut tenants = state.tenants.write().await;
        let t = tenants.get_mut(tenant_id).unwrap();
        t.status = VmStatus::Active;
        t.vm_id = Some(vm_id);
        t.guest_ip = Some(ip);
    }
    let _ = state.persist().await;
    Ok(())
}

async fn recover_from_soft(state: &ControllerState, tenant_id: &str) -> anyhow::Result<()> {
    let (vm_id, mem_mb) = {
        let tenants = state.tenants.read().await;
        let t = tenants.get(tenant_id).unwrap();
        (
            t.vm_id.clone().ok_or_else(|| anyhow::anyhow!("no vm_id; cannot recover"))?,
            t.config.mem_mb,
        )
    };
    // Inflate balloon back to full guest memory.
    state.clone_client.balloon_vm(&vm_id, mem_mb).await?;

    let mut tenants = state.tenants.write().await;
    tenants.get_mut(tenant_id).unwrap().status = VmStatus::Active;
    drop(tenants);
    let _ = state.persist().await;
    Ok(())
}

// ---- Helpers --------------------------------------------------------------

fn build_cmdline(cfg: &TenantConfig) -> String {
    let base = "console=ttyS0 reboot=k panic=1 nokaslr quiet";
    match &cfg.init_cmd {
        Some(init) => format!("{base} clone.init={init}"),
        None => base.to_string(),
    }
}

async fn ensure_overlay(path: &str) -> anyhow::Result<()> {
    if std::path::Path::new(path).exists() {
        return Ok(());
    }
    tracing::info!(path, "creating new overlay (4GB ext4)");
    let status = std::process::Command::new("fallocate")
        .args(["-l", "4G", path])
        .status();
    if let Err(e) = status {
        return Err(anyhow::anyhow!("fallocate failed: {e}"));
    }
    let status = std::process::Command::new("mkfs.ext4")
        .args(["-q", "-F", path])
        .status()?;
    if !status.success() {
        return Err(anyhow::anyhow!("mkfs.ext4 exit {:?}", status.code()));
    }
    Ok(())
}

async fn wait_for_port(ip: &str, port: u16, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    let addr: SocketAddr = format!("{ip}:{port}")
        .parse()
        .map_err(|e| anyhow::anyhow!("bad ip:port: {e}"))?;
    loop {
        if tokio::net::TcpStream::connect(&addr).await.is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(anyhow::anyhow!(
                "app port {port} on {ip} not up within {:?}",
                timeout
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
