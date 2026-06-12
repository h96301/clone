//! REST API — HTTP layer that exposes VM lifecycle operations.
//!
//! Translates HTTP requests into the same daemon operations used by the
//! Unix socket protocol. Shares `Arc<Mutex<ServerState>>` with the Unix
//! control server so both transports view the same VM registry.

use std::sync::Arc;

use axum::{
    extract::{Path, Request as AxumRequest, State},
    http::StatusCode,
    middleware::{from_fn, Next},
    response::{IntoResponse, Response as AxumResponse},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::control::prometheus_metrics;
use crate::control::protocol::{self, Request, Response, ResponseBody};
use crate::control::ServerState;

/// Shared state injected into every HTTP handler.
#[derive(Clone)]
pub struct HttpState {
    pub vm_state: Arc<Mutex<ServerState>>,
    /// Optional bearer token. When `Some`, all non-/health endpoints
    /// require `Authorization: Bearer <token>` to match exactly.
    pub auth_token: Option<String>,
}

/// Start the HTTP server. Blocks until cancelled.
pub async fn run_http_server(state: HttpState, bind_addr: &str) -> anyhow::Result<()> {
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bind HTTP listener {bind_addr}: {e}"))?;

    tracing::info!(addr = %bind_addr, "HTTP REST API listening");
    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("HTTP server error: {e}"))?;
    Ok(())
}

fn build_router(state: HttpState) -> Router {
    Router::new()
        // health & metrics
        .route("/health", get(health))
        .route("/metrics", get(prometheus_metrics))
        .route("/v1/host/metrics", get(host_metrics))
        // VM lifecycle
        .route("/v1/vms", post(create_vm).get(list_vms))
        .route(
            "/v1/vms/{id}",
            get(vm_status).delete(destroy_vm),
        )
        // per-VM actions (forwarded to per-VM control socket)
        .route("/v1/vms/{id}/pause", post(pause_vm))
        .route("/v1/vms/{id}/resume", post(resume_vm))
        .route("/v1/vms/{id}/shutdown", post(shutdown_vm))
        .route("/v1/vms/{id}/snapshot", post(snapshot_vm))
        .route("/v1/vms/{id}/incremental-snapshot", post(incremental_snapshot_vm))
        .route("/v1/vms/{id}/exec", post(exec_vm))
        .route("/v1/vms/{id}/balloon", post(set_balloon_vm))
        .route("/v1/vms/{id}/migrate", post(live_migrate_vm))
        .route("/v1/vms/{id}/branch", post(branch_vm))
        .route("/v1/vms/{id}/save", post(save_vm))
        // fork (CoW shared with template) and restore (independent copy)
        .route("/v1/fork", post(fork_vm))
        .route("/v1/restore", post(restore_vm))
        // diff chain management
        .route("/v1/snapshots/compact", post(compact_snapshot_chain))
        .route("/v1/snapshots/gc", post(gc_snapshots))
        .route("/v1/snapshots/{tag}/chain", get(get_snapshot_chain))
        .with_state(state.clone())
        .layer(from_fn(move |req, next| metrics_middleware(req, next)))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
}

/// Axum middleware that counts each HTTP request, labeled by method/path/status.
async fn metrics_middleware(
    req: AxumRequest,
    next: Next,
) -> AxumResponse {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let resp = next.run(req).await;
    let status = resp.status().as_u16();
    prometheus_metrics::inc_http_request(&method, &path, status);
    resp
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    #[allow(dead_code)]
    fn bad_request(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::BAD_REQUEST, message: msg.into() }
    }
    fn not_found(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::NOT_FOUND, message: msg.into() }
    }
    fn internal(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::INTERNAL_SERVER_ERROR, message: msg.into() }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

// ---------------------------------------------------------------------------
// Bearer token auth check
// ---------------------------------------------------------------------------

fn require_auth(token: Option<&str>, expected: &str) -> ApiResult<()> {
    match token {
        Some(t) if subtle_eq(t, expected) => Ok(()),
        _ => Err(ApiError {
            status: StatusCode::UNAUTHORIZED,
            message: "missing or invalid bearer token".into(),
        }),
    }
}

/// Constant-time comparison to discourage timing attacks on the token.
fn subtle_eq(a: &str, b: &str) -> bool {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    if ab.len() != bb.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in ab.iter().zip(bb.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Extract the bearer token from an `Authorization` header value.
fn extract_bearer(header: Option<&str>) -> Option<&str> {
    header?.strip_prefix("Bearer ").or_else(|| header?.strip_prefix("bearer "))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn health(
    State(state): State<HttpState>,
    headers: axum::http::HeaderMap,
) -> ApiResult<Json<Value>> {
    if let Some(ref expected) = state.auth_token {
        let auth = headers.get("authorization").and_then(|h| h.to_str().ok());
        require_auth(extract_bearer(auth), expected)?;
    }
    Ok(Json(json!({ "status": "ok" })))
}

/// Prometheus text exposition. Returned with `Content-Type: text/plain;
/// version=0.0.4` so Prometheus scrape succeeds without auth headers.
async fn prometheus_metrics() -> impl IntoResponse {
    let body = crate::control::prometheus_metrics::render();
    (
        StatusCode::OK,
        [
            ("content-type", "text/plain; version=0.0.4; charset=utf-8"),
        ],
        body,
    )
}

// --- Create VM ---

#[derive(Debug, Deserialize)]
struct CreateVmBody {
    kernel: String,
    #[serde(default)]
    initrd: Option<String>,
    #[serde(default)]
    cmdline: Option<String>,
    #[serde(default = "default_mem_mb")]
    mem_mb: u32,
    #[serde(default = "default_vcpus")]
    vcpus: u32,
    #[serde(default)]
    rootfs: Option<String>,
    #[serde(default)]
    overlay: Option<String>,
    #[serde(default)]
    shared_dir: Option<String>,
    #[serde(default)]
    block: Option<String>,
    #[serde(default)]
    net: bool,
    #[serde(default)]
    tap: Option<String>,
    #[serde(default)]
    seccomp: bool,
    #[serde(default)]
    jail: Option<String>,
    /// cgroup v2 memory hard limit in MB (None = unlimited).
    #[serde(default)]
    memory_limit_mb: Option<u32>,
}

fn default_mem_mb() -> u32 { 512 }
fn default_vcpus() -> u32 { 1 }

async fn create_vm(
    State(state): State<HttpState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<CreateVmBody>,
) -> ApiResult<Json<Value>> {
    if let Some(ref expected) = state.auth_token {
        let auth = headers.get("authorization").and_then(|h| h.to_str().ok());
        require_auth(extract_bearer(auth), expected)?;
    }

    let req = Request::CreateVm {
        kernel: body.kernel,
        initrd: body.initrd,
        cmdline: body.cmdline.unwrap_or_else(|| {
            "console=ttyS0 reboot=k panic=1 pci=off nokasnr quiet".replace("nokasnr", "nokaslr")
        }),
        mem_mb: body.mem_mb,
        vcpus: body.vcpus,
        rootfs: body.rootfs,
        overlay: body.overlay,
        shared_dir: body.shared_dir,
        block: body.block,
        net: body.net,
        tap: body.tap,
        seccomp: body.seccomp,
        jail: body.jail,
        memory_limit_mb: body.memory_limit_mb,
    };

    let resp = dispatch_with_state(req, &state.vm_state).await;
    response_to_json(resp, StatusCode::CREATED)
}

// --- List VMs ---

async fn list_vms(
    State(state): State<HttpState>,
    headers: axum::http::HeaderMap,
) -> ApiResult<Json<Value>> {
    check_auth(&state, &headers)?;
    let resp = dispatch_with_state(Request::ListVms, &state.vm_state).await;
    response_to_json(resp, StatusCode::OK)
}

// --- VM status ---

async fn vm_status(
    State(state): State<HttpState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    check_auth(&state, &headers)?;
    let resp = dispatch_with_state(Request::VmStatus { vm_id: id }, &state.vm_state).await;
    response_to_json(resp, StatusCode::OK)
}

// --- Destroy VM ---

async fn destroy_vm(
    State(state): State<HttpState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    check_auth(&state, &headers)?;
    let resp = dispatch_with_state(Request::DestroyVm { vm_id: id }, &state.vm_state).await;
    response_to_json(resp, StatusCode::OK)
}

// --- Pause ---

async fn pause_vm(
    State(state): State<HttpState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    check_auth(&state, &headers)?;
    forward_per_vm(&state, &id, Request::Pause).await
}

// --- Resume ---

async fn resume_vm(
    State(state): State<HttpState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    check_auth(&state, &headers)?;
    forward_per_vm(&state, &id, Request::Resume).await
}

// --- Shutdown ---

async fn shutdown_vm(
    State(state): State<HttpState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    check_auth(&state, &headers)?;
    forward_per_vm(&state, &id, Request::Shutdown).await
}

// --- Snapshot ---

#[derive(Debug, Deserialize)]
struct SnapshotBody {
    output_path: String,
}

async fn snapshot_vm(
    State(state): State<HttpState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<SnapshotBody>,
) -> ApiResult<Json<Value>> {
    check_auth(&state, &headers)?;
    forward_per_vm(&state, &id, Request::Snapshot {
        vm_id: id.clone(),
        output_path: body.output_path,
    })
    .await
}

// --- Incremental snapshot ---

#[derive(Debug, Deserialize)]
struct IncSnapshotBody {
    output_path: String,
    base_template: String,
}

async fn incremental_snapshot_vm(
    State(state): State<HttpState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<IncSnapshotBody>,
) -> ApiResult<Json<Value>> {
    check_auth(&state, &headers)?;
    forward_per_vm(&state, &id, Request::IncrementalSnapshot {
        output_path: body.output_path,
        base_template: body.base_template,
    })
    .await
}

// --- Exec ---

#[derive(Debug, Deserialize)]
struct ExecBody {
    command: String,
    #[serde(default)]
    args: Vec<String>,
}

async fn exec_vm(
    State(state): State<HttpState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<ExecBody>,
) -> ApiResult<Json<Value>> {
    check_auth(&state, &headers)?;
    forward_per_vm(&state, &id, Request::Exec {
        command: body.command,
        args: body.args,
    })
    .await
}

// --- Balloon ---

#[derive(Debug, Deserialize)]
struct BalloonBody {
    target_mb: u32,
}

async fn set_balloon_vm(
    State(state): State<HttpState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<BalloonBody>,
) -> ApiResult<Json<Value>> {
    check_auth(&state, &headers)?;
    forward_per_vm(&state, &id, Request::SetBalloon { target_mb: body.target_mb }).await
}

// --- Live migrate ---

#[derive(Debug, Deserialize)]
struct MigrateBody {
    dest_host: String,
    dest_port: u16,
}

async fn live_migrate_vm(
    State(state): State<HttpState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<MigrateBody>,
) -> ApiResult<Json<Value>> {
    check_auth(&state, &headers)?;
    forward_per_vm(&state, &id, Request::LiveMigrate {
        dest_host: body.dest_host,
        dest_port: body.dest_port,
    })
    .await
}

// --- Live branch ---

#[derive(Debug, Deserialize, Default)]
struct BranchBody {
    /// Optional output directory for the intermediate snapshot.
    /// If None, defaults to /tmp/clone-branch-{source_pid}.
    #[serde(default)]
    output_dir: Option<String>,
    /// Whether to set up networking for the branch VM.
    #[serde(default)]
    net: bool,
    /// Optional shared directory for the branch VM (virtio-fs).
    #[serde(default)]
    shared_dir: Option<String>,
}

async fn branch_vm(
    State(state): State<HttpState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<BranchBody>,
) -> ApiResult<Json<Value>> {
    check_auth(&state, &headers)?;
    forward_per_vm(&state, &id, Request::Branch {
        output_dir: body.output_dir,
        net: body.net,
        shared_dir: body.shared_dir,
    })
    .await
}

// --- Save (snapshot + shutdown) ---

#[derive(Debug, Deserialize)]
struct SaveBody {
    output_path: String,
}

async fn save_vm(
    State(state): State<HttpState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<SaveBody>,
) -> ApiResult<Json<Value>> {
    check_auth(&state, &headers)?;
    let pid = lookup_pid(&state, &id).await?;
    let sock = per_vm_socket(pid);

    let snap_req = Request::Snapshot {
        vm_id: id.clone(),
        output_path: body.output_path.clone(),
    };
    let snap_resp = forward_request_sync(&sock, &snap_req).await?;

    // Best-effort shutdown — ignore errors if socket already torn down.
    let _ = forward_request_sync(&sock, &Request::Shutdown).await;

    response_to_json(snap_resp, StatusCode::OK)
}

// --- Fork (CoW from template) ---

#[derive(Debug, Deserialize)]
struct ForkBody {
    /// Template directory produced by `clone save` or `clone snapshot`.
    template_path: String,
    #[serde(default)]
    net: bool,
    #[serde(default)]
    shared_dir: Option<String>,
    /// Memory cap (balloon reclaims excess).
    #[serde(default)]
    mem_mb: Option<u32>,
    /// Active vCPU count (others stay offline).
    #[serde(default)]
    vcpus: Option<u32>,
    #[serde(default)]
    overlay_size: Option<String>,
    /// cgroup v2 memory hard limit in MB (None = unlimited).
    #[serde(default)]
    memory_limit_mb: Option<u32>,
}

async fn fork_vm(
    State(state): State<HttpState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<ForkBody>,
) -> ApiResult<Json<Value>> {
    check_auth(&state, &headers)?;
    let req = Request::ForkVm {
        template_path: body.template_path,
        net: body.net,
        shared_dir: body.shared_dir,
        mem_mb: body.mem_mb,
        vcpus: body.vcpus,
        overlay_size: body.overlay_size,
        memory_limit_mb: body.memory_limit_mb,
    };
    let resp = dispatch_with_state(req, &state.vm_state).await;
    response_to_json(resp, StatusCode::CREATED)
}

// --- Restore (independent memory copy, replays full state) ---

#[derive(Debug, Deserialize)]
struct RestoreBody {
    /// Snapshot directory produced by `clone save`.
    snapshot_path: String,
    #[serde(default)]
    net: bool,
    #[serde(default)]
    shared_dir: Option<String>,
    #[serde(default)]
    block: Option<String>,
    /// cgroup v2 memory hard limit in MB (None = unlimited).
    #[serde(default)]
    memory_limit_mb: Option<u32>,
}

async fn restore_vm(
    State(state): State<HttpState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<RestoreBody>,
) -> ApiResult<Json<Value>> {
    check_auth(&state, &headers)?;
    let req = Request::RestoreVm {
        snapshot_path: body.snapshot_path,
        net: body.net,
        shared_dir: body.shared_dir,
        block: body.block,
        memory_limit_mb: body.memory_limit_mb,
    };
    let resp = dispatch_with_state(req, &state.vm_state).await;
    response_to_json(resp, StatusCode::CREATED)
}

// --- Diff chain management ---

#[derive(Debug, Deserialize)]
struct CompactBody {
    /// Directory of the chain leaf snapshot to flatten.
    leaf_dir: String,
    /// Where to write the resulting self-contained snapshot.
    output_dir: String,
}

async fn compact_snapshot_chain(
    State(state): State<HttpState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<CompactBody>,
) -> ApiResult<Json<Value>> {
    check_auth(&state, &headers)?;
    let leaf = body.leaf_dir.clone();
    let output = body.output_dir.clone();
    let snapshot = tokio::task::spawn_blocking(move || {
        crate::boot::template::compact_chain(&leaf, &output)
            .map_err(|e| ApiError::internal(format!("compact_chain: {e}")))
    })
    .await
    .map_err(|e| ApiError::internal(format!("join: {e}")))??;

    Ok(Json(json!({
        "output_dir": body.output_dir,
        "memory_size_bytes": snapshot.memory_size,
        "memory_hash": snapshot.memory_hash,
        "runtime_type": snapshot.runtime_type,
    })))
}

#[derive(Debug, Deserialize)]
struct GcBody {
    /// Base directory containing snapshots.
    base_dir: String,
    /// Tags that should be preserved (plus their ancestors).
    #[serde(default)]
    active_tags: Vec<String>,
}

async fn gc_snapshots(
    State(state): State<HttpState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<GcBody>,
) -> ApiResult<Json<Value>> {
    check_auth(&state, &headers)?;
    let base_dir = body.base_dir.clone();
    let active = body.active_tags.clone();
    let removed = tokio::task::spawn_blocking(move || {
        crate::boot::template::gc_snapshots(&base_dir, &active)
            .map_err(|e| ApiError::internal(format!("gc_snapshots: {e}")))
    })
    .await
    .map_err(|e| ApiError::internal(format!("join: {e}")))??;

    Ok(Json(json!({ "removed": removed, "count": removed.len() })))
}

async fn get_snapshot_chain(
    State(state): State<HttpState>,
    headers: axum::http::HeaderMap,
    Path(tag): Path<String>,
) -> ApiResult<Json<Value>> {
    check_auth(&state, &headers)?;
    // `tag` is interpreted as a snapshot directory name or path.
    let info = tokio::task::spawn_blocking(move || {
        crate::boot::template::chain_info(&tag)
            .map_err(|e| ApiError::internal(format!("chain_info: {e}")))
    })
    .await
    .map_err(|e| ApiError::internal(format!("join: {e}")))??;

    Ok(Json(serde_json::to_value(&info).unwrap_or_default()))
}

// --- Host metrics ---

async fn host_metrics(
    State(state): State<HttpState>,
    headers: axum::http::HeaderMap,
) -> ApiResult<Json<Value>> {
    check_auth(&state, &headers)?;
    let req = Request::Metrics { vm_id: "_host".to_string() };
    let resp = dispatch_with_state(req, &state.vm_state).await;
    response_to_json(resp, StatusCode::OK)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Auth check helper for handlers that don't already extract auth inline.
fn check_auth(state: &HttpState, headers: &axum::http::HeaderMap) -> ApiResult<()> {
    if let Some(ref expected) = state.auth_token {
        let auth = headers
            .get("authorization")
            .and_then(|h| h.to_str().ok());
        require_auth(extract_bearer(auth), expected)?;
    }
    Ok(())
}

/// Look up a VM's PID by vm_id. Returns 404 if not tracked.
async fn lookup_pid(state: &HttpState, vm_id: &str) -> ApiResult<u32> {
    let s = state.vm_state.lock().await;
    s.vms
        .get(vm_id)
        .map(|r| r.pid)
        .ok_or_else(|| ApiError::not_found(format!("VM not found: {vm_id}")))
}

fn per_vm_socket(pid: u32) -> String {
    format!("/tmp/clone-{pid}.sock")
}

/// Dispatch a request via the in-process dispatch function (same as Unix socket).
async fn dispatch_with_state(
    req: Request,
    state: &Arc<Mutex<ServerState>>,
) -> Response {
    crate::control::dispatch(req, state).await
}

/// Forward a request to a VM's per-VM control socket (synchronous I/O,
/// wrapped in spawn_blocking to avoid blocking the async runtime).
async fn forward_request_sync(socket: &str, req: &Request) -> ApiResult<Response> {
    let socket = socket.to_string();
    let req = req.clone();
    tokio::task::spawn_blocking(move || {
        use std::io::{BufReader, BufWriter};
        let stream = std::os::unix::net::UnixStream::connect(&socket)
            .map_err(|e| ApiError::internal(format!("connect {socket}: {e}")))?;
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(300)))
            .map_err(|e| ApiError::internal(format!("set_read_timeout: {e}")))?;
        stream
            .set_write_timeout(Some(std::time::Duration::from_secs(30)))
            .map_err(|e| ApiError::internal(format!("set_write_timeout: {e}")))?;
        let mut writer = BufWriter::new(&stream);
        let mut reader = BufReader::new(&stream);
        protocol::write_frame_sync(&mut writer, &req)
            .map_err(|e| ApiError::internal(format!("write_frame: {e}")))?;
        let resp: Response = protocol::read_frame_sync(&mut reader)
            .map_err(|e| ApiError::internal(format!("read_frame: {e}")))?;
        Ok(resp)
    })
    .await
    .map_err(|e| ApiError::internal(format!("join: {e}")))?
}

/// Forward a request to a per-VM socket, after looking up the socket path
/// from the VM registry.
async fn forward_per_vm(
    state: &HttpState,
    vm_id: &str,
    req: Request,
) -> ApiResult<Json<Value>> {
    let pid = lookup_pid(state, vm_id).await?;
    let sock = per_vm_socket(pid);
    let resp = forward_request_sync(&sock, &req).await?;
    response_to_json(resp, StatusCode::OK)
}

/// Translate the wire-protocol Response into a JSON HTTP response.
fn response_to_json(resp: Response, ok_status: StatusCode) -> ApiResult<Json<Value>> {
    match resp {
        Response::Ok { body } => {
            let payload: Value = match body {
                ResponseBody::VmCreated { vm_id, pid } => json!({ "vm_id": vm_id, "pid": pid }),
                ResponseBody::ExecResult { exit_code, stdout, stderr } => json!({
                    "exit_code": exit_code,
                    "stdout": stdout,
                    "stderr": stderr,
                }),
                ResponseBody::VmStatus { state, uptime_secs, memory_usage_bytes } => json!({
                    "state": state,
                    "uptime_secs": uptime_secs,
                    "memory_usage_bytes": memory_usage_bytes,
                }),
                ResponseBody::VmList { vms } => json!({ "vms": vms }),
                ResponseBody::Metrics { metrics } => json!({ "metrics": metrics }),
                ResponseBody::SnapshotComplete { path } => json!({ "path": path }),
                ResponseBody::Status { state, pid, vcpus } => json!({
                    "state": state,
                    "pid": pid,
                    "vcpus": vcpus,
                }),
                ResponseBody::MigrationComplete {
                    total_pages_sent,
                    rounds,
                    downtime_ms,
                    total_time_ms,
                } => json!({
                    "total_pages_sent": total_pages_sent,
                    "rounds": rounds,
                    "downtime_ms": downtime_ms,
                    "total_time_ms": total_time_ms,
                }),
                ResponseBody::BranchComplete {
                    new_vm_id,
                    new_pid,
                    pause_duration_ms,
                    total_duration_ms,
                } => json!({
                    "new_vm_id": new_vm_id,
                    "new_pid": new_pid,
                    "pause_duration_ms": pause_duration_ms,
                    "total_duration_ms": total_duration_ms,
                }),
                ResponseBody::Ack {} => json!({ "ok": true }),
            };
            // status override: ack stays 200, created stays 201, etc.
            let _ = ok_status;
            Ok(Json(payload))
        }
        Response::Error { message } => Err(ApiError::internal(message)),
    }
}
