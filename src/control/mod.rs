// Control plane — Unix socket API for VM management.
//
// Protocol: length-prefixed JSON over Unix domain socket.
// Each message is 4 bytes LE length + JSON body.

#[cfg(target_os = "linux")]
pub mod cgroup;
pub mod daemon;
#[cfg(target_os = "linux")]
pub mod http;
pub mod jailer;
pub mod metrics;
pub mod prometheus_metrics;
pub mod protocol;
#[cfg(target_os = "linux")]
pub mod sync_server;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use protocol::{read_frame, write_frame, Request, Response, ResponseBody, VmSummary};
use metrics::{EventLogger, MetricsCollector, VmEvent, VmMetrics};

/// Default socket path for the control plane.
pub const DEFAULT_SOCKET_PATH: &str = "/run/clone/control.sock";

// ---------------------------------------------------------------------------
// VM tracking (in-process state)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct VmRecord {
    pub vm_id: String,
    pub pid: u32,
    pub state: VmState,
    pub boot_time: Instant,
    pub config: VmRecordConfig,
    /// Path to the cgroup directory under /sys/fs/cgroup/clone/, if any.
    pub cgroup_path: Option<std::path::PathBuf>,
    /// Configured memory limit in MB (None = unlimited).
    pub memory_limit_mb: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct VmRecordConfig {
    pub kernel: String,
    pub initrd: Option<String>,
    pub cmdline: String,
    pub mem_mb: u32,
    pub vcpus: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmState {
    Running,
    Stopped,
    Paused,
}

impl std::fmt::Display for VmState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VmState::Running => write!(f, "running"),
            VmState::Stopped => write!(f, "stopped"),
            VmState::Paused => write!(f, "paused"),
        }
    }
}

// ---------------------------------------------------------------------------
// Shared state for the control server
// ---------------------------------------------------------------------------

pub struct ServerState {
    vms: HashMap<String, VmRecord>,
    next_id: u64,
    next_cid: u64,
    metrics: MetricsCollector,
    events: EventLogger,
    /// Optional JSON state file for persisting next_id/next_cid across restarts.
    /// When set, load on construction and atomic-write on every allocation.
    state_file: Option<std::path::PathBuf>,
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct PersistedState {
    next_id: u64,
    next_cid: u64,
}

impl ServerState {
    fn new(state_file: Option<std::path::PathBuf>) -> Self {
        let mut state = Self {
            vms: HashMap::new(),
            next_id: 1,
            next_cid: 3,
            metrics: MetricsCollector::new(),
            events: EventLogger::new(4096),
            state_file: state_file.clone(),
        };
        if let Some(path) = state_file {
            state.load_from_file(&path);
        }
        state
    }

    fn load_from_file(&mut self, path: &std::path::Path) {
        match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice::<PersistedState>(&bytes) {
                Ok(p) => {
                    tracing::info!(
                        path = %path.display(),
                        next_id = p.next_id,
                        next_cid = p.next_cid,
                        "Loaded persisted daemon state"
                    );
                    self.next_id = p.next_id.max(1);
                    // 保留 >= 3 的约束：VM 寄主机协议保留 cid 1/2。
                    self.next_cid = p.next_cid.max(3);
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "Failed to parse daemon state file, starting with defaults"
                    );
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(
                    path = %path.display(),
                    "No existing daemon state file, starting fresh"
                );
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to read daemon state file, starting with defaults"
                );
            }
        }
    }

    fn persist(&self) {
        let Some(path) = self.state_file.as_ref() else { return; };
        let payload = PersistedState {
            next_id: self.next_id,
            next_cid: self.next_cid,
        };
        let json = match serde_json::to_vec(&payload) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to serialize daemon state");
                return;
            }
        };
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, &json) {
            tracing::warn!(path = %tmp.display(), error = %e, "Failed to write daemon state tmp file");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            tracing::warn!(path = %path.display(), error = %e, "Failed to rename daemon state file");
            let _ = std::fs::remove_file(&tmp);
        }
    }

    fn alloc_id(&mut self) -> String {
        let id = format!("vm-{:04}", self.next_id);
        self.next_id += 1;
        id
    }
}

// ---------------------------------------------------------------------------
// ControlServer
// ---------------------------------------------------------------------------

/// Unix domain socket server for VM management commands.
pub struct ControlServer {
    socket_path: String,
    state: Arc<Mutex<ServerState>>,
}

impl ControlServer {
    /// Create a new control server bound to the given socket path.
    /// `state_file` 若提供，则用于持久化 next_id/next_cid（跨重启）。
    pub fn new(
        socket_path: impl Into<String>,
        state_file: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            socket_path: socket_path.into(),
            state: Arc::new(Mutex::new(ServerState::new(state_file))),
        }
    }

    /// Get a clone of the shared state for monitoring.
    pub fn state(&self) -> Arc<Mutex<ServerState>> {
        Arc::clone(&self.state)
    }

    /// Run the server, accepting connections until cancelled.
    pub async fn run(&self) -> Result<()> {
        // Remove stale socket if present.
        let _ = std::fs::remove_file(&self.socket_path);

        // Ensure parent directory exists.
        if let Some(parent) = std::path::Path::new(&self.socket_path).parent() {
            std::fs::create_dir_all(parent)?;
        }

        let listener = UnixListener::bind(&self.socket_path)?;
        tracing::info!(path = %self.socket_path, "Control server listening");

        loop {
            let (stream, _addr) = listener.accept().await?;
            let state = Arc::clone(&self.state);
            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, state).await {
                    tracing::error!(error = %e, "Connection handler error");
                }
            });
        }
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

async fn handle_connection(
    stream: UnixStream,
    state: Arc<Mutex<ServerState>>,
) -> Result<()> {
    let (mut reader, mut writer) = stream.into_split();

    loop {
        let request: Request = match read_frame(&mut reader).await {
            Ok(r) => r,
            Err(protocol::ProtocolError::ConnectionClosed) => return Ok(()),
            Err(e) => {
                let resp = Response::Error {
                    message: format!("Protocol error: {e}"),
                };
                let _ = write_frame(&mut writer, &resp).await;
                return Err(e.into());
            }
        };

        let response = dispatch(request, &state).await;

        write_frame(&mut writer, &response).await?;
    }
}

pub async fn dispatch(req: Request, state: &Arc<Mutex<ServerState>>) -> Response {
    let started = std::time::Instant::now();
    let req_kind = request_kind(&req);
    let resp = dispatch_inner(req, state).await;

    // Prometheus metrics — record request-scoped timing and counters.
    // Failures here must never break the actual response.
    let elapsed = started.elapsed().as_secs_f64();
    match req_kind.as_str() {
        "fork" | "restore" => crate::control::prometheus_metrics::observe_fork_duration(elapsed),
        "snapshot" => crate::control::prometheus_metrics::observe_snapshot_duration(elapsed),
        _ => {}
    }
    resp
}

/// Classify a request for timing buckets.
fn request_kind(req: &Request) -> String {
    match req {
        Request::ForkVm { .. } => "fork".into(),
        Request::RestoreVm { .. } => "restore".into(),
        Request::Snapshot { .. } => "snapshot".into(),
        Request::CreateVm { .. } => "create".into(),
        Request::DestroyVm { .. } => "destroy".into(),
        Request::VmStatus { .. } => "status".into(),
        Request::ListVms => "list".into(),
        Request::Metrics { .. } => "metrics".into(),
        _ => "other".into(),
    }
}

#[cfg(target_os = "linux")]
fn setup_cgroup(vm_id: &str, pid: u32, memory_limit_mb: Option<u32>) -> Option<std::path::PathBuf> {
    match crate::control::cgroup::create_vm_cgroup(vm_id, memory_limit_mb) {
        Ok(path) => {
            if let Err(e) = crate::control::cgroup::add_process(vm_id, pid) {
                tracing::warn!(vm_id, pid, error = %e, "Failed to add process to cgroup");
            }
            Some(path)
        }
        Err(e) => {
            // cgroup creation is optional — log and continue without it.
            tracing::warn!(vm_id, error = %e, "cgroup setup skipped");
            None
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn setup_cgroup(_vm_id: &str, _pid: u32, _memory_limit_mb: Option<u32>) -> Option<std::path::PathBuf> {
    None
}

async fn dispatch_inner(req: Request, state: &Arc<Mutex<ServerState>>) -> Response {
    match req {
        Request::CreateVm {
            kernel,
            initrd,
            cmdline,
            mem_mb,
            vcpus,
            rootfs,
            overlay,
            shared_dir,
            block,
            net,
            tap,
            seccomp,
            jail,
            memory_limit_mb,
        } => {
            let mut s = state.lock().await;
            let vm_id = s.alloc_id();
            let cid = s.next_cid;
            s.next_cid += 1;
            s.persist();
            let vm_index = cid - 3; // 0-based index for IP derivation

            // Derive guest IP from CID; agent port is always the fixed base port.
            let mut effective_cmdline = cmdline;
            effective_cmdline.push_str(" clone.agent_port=9999");
            if net {
                let host_part = 2 + vm_index;
                let guest_ip = format!("172.30.{}.{}", host_part / 256, host_part % 256);
                effective_cmdline.push_str(&format!(
                    " clone.net_ip={} clone.net_gw=172.30.0.1 clone.net_mask=16",
                    guest_ip
                ));
            }

            // Spawn the VM child process
            match daemon::spawn_vm(
                &kernel,
                initrd.as_deref(),
                &effective_cmdline,
                mem_mb,
                vcpus,
                rootfs.as_deref(),
                overlay.as_deref(),
                shared_dir.as_deref(),
                block.as_deref(),
                net,
                tap.as_deref(),
                seccomp,
                jail.as_deref(),
                Some(cid),
            ) {
                Ok(pid) => {
                    // Optional: create cgroup for memory isolation.
                    let cgroup_path = setup_cgroup(&vm_id, pid, memory_limit_mb);

                    let record = VmRecord {
                        vm_id: vm_id.clone(),
                        pid,
                        state: VmState::Running,
                        boot_time: Instant::now(),
                        config: VmRecordConfig {
                            kernel,
                            initrd,
                            cmdline: effective_cmdline,
                            mem_mb,
                            vcpus,
                        },
                        cgroup_path,
                        memory_limit_mb,
                    };
                    s.vms.insert(vm_id.clone(), record);

                    // Initialize empty metrics for this VM.
                    s.metrics.update(&vm_id, VmMetrics::default());
                    s.events.log(VmEvent::Boot {
                        vm_id: vm_id.clone(),
                    });

                    // Update Prometheus gauges.
                    crate::control::prometheus_metrics::set_vm_count(s.vms.len() as i64);

                    tracing::info!(vm_id = %vm_id, pid = pid, "VM created");
                    Response::Ok {
                        body: ResponseBody::VmCreated { vm_id, pid },
                    }
                }
                Err(e) => Response::Error {
                    message: format!("Failed to spawn VM: {e}"),
                },
            }
        }

        Request::DestroyVm { vm_id } => {
            let mut s = state.lock().await;
            if let Some(record) = s.vms.remove(&vm_id) {
                // Send shutdown to the VM's per-VM control socket
                let socket_path = format!("/tmp/clone-{}.sock", record.pid);
                if let Err(e) = daemon::shutdown_vm(&socket_path) {
                    tracing::warn!(vm_id = %vm_id, pid = record.pid, "Graceful shutdown failed: {e}, force killing");
                    // Force kill if graceful shutdown fails
                    unsafe { libc::kill(record.pid as i32, libc::SIGKILL); }
                }
                s.metrics.remove(&vm_id);
                s.events.log(VmEvent::Shutdown {
                    vm_id: vm_id.clone(),
                });
                crate::control::prometheus_metrics::remove_vm_memory(&vm_id);
                crate::control::prometheus_metrics::set_vm_count(s.vms.len() as i64);

                // Best-effort cgroup cleanup.
                #[cfg(target_os = "linux")]
                if let Err(e) = crate::control::cgroup::remove_cgroup(&vm_id) {
                    tracing::warn!(vm_id = %vm_id, error = %e, "cgroup cleanup failed");
                }

                tracing::info!(vm_id = %vm_id, pid = record.pid, "VM destroyed");
                Response::Ok {
                    body: ResponseBody::Ack {},
                }
            } else {
                Response::Error {
                    message: format!("VM not found: {vm_id}"),
                }
            }
        }

        Request::VmStatus { vm_id } => {
            let s = state.lock().await;
            match s.vms.get(&vm_id) {
                Some(record) => {
                    // Try to query live status from the VM's control socket
                    let socket_path = format!("/tmp/clone-{}.sock", record.pid);
                    let live_state = daemon::query_vm_status(&socket_path)
                        .ok()
                        .and_then(|resp| {
                            if let Response::Ok { body: ResponseBody::VmStatus { state, .. } } = resp {
                                Some(state)
                            } else {
                                None
                            }
                        })
                        .unwrap_or_else(|| record.state.to_string());

                    let uptime = record.boot_time.elapsed().as_secs_f64();
                    let mem = s.metrics.get(&vm_id).map_or(0, |m| {
                        m.private_rss_bytes + m.shared_rss_bytes
                    });
                    Response::Ok {
                        body: ResponseBody::VmStatus {
                            state: live_state,
                            uptime_secs: uptime,
                            memory_usage_bytes: mem,
                        },
                    }
                }
                None => Response::Error {
                    message: format!("VM not found: {vm_id}"),
                },
            }
        }

        Request::ListVms => {
            let s = state.lock().await;
            let vms: Vec<VmSummary> = s
                .vms
                .values()
                .map(|r| VmSummary {
                    vm_id: r.vm_id.clone(),
                    state: r.state.to_string(),
                    uptime_secs: r.boot_time.elapsed().as_secs_f64(),
                })
                .collect();
            Response::Ok {
                body: ResponseBody::VmList { vms },
            }
        }

        Request::Snapshot { vm_id, output_path } => {
            let s = state.lock().await;
            if let Some(record) = s.vms.get(&vm_id) {
                let sock = format!("/tmp/clone-{}.sock", record.pid);
                tracing::info!(vm_id = %vm_id, output = %output_path, "Forwarding snapshot to per-VM socket");
                drop(s); // release lock before blocking I/O
                match daemon::snapshot_vm(&sock, &output_path) {
                    Ok(resp) => resp,
                    Err(e) => Response::Error {
                        message: format!("Snapshot failed: {e}"),
                    },
                }
            } else {
                Response::Error {
                    message: format!("VM not found: {vm_id}"),
                }
            }
        }

        Request::ForkVm { template_path, net, shared_dir, mem_mb, vcpus, overlay_size, memory_limit_mb } => {
            let mut s = state.lock().await;
            let vm_id = s.alloc_id();
            let cid = s.next_cid;
            s.next_cid += 1;
            s.persist();

            tracing::info!(
                vm_id = %vm_id,
                template = %template_path,
                cid,
                "Forking VM from template"
            );

            match daemon::spawn_fork(
                &template_path,
                net,
                shared_dir.as_deref(),
                Some(cid),
                mem_mb,
                vcpus,
                overlay_size.as_deref(),
            ) {
                Ok(pid) => {
                    let cgroup_path = setup_cgroup(&vm_id, pid, memory_limit_mb);
                    let record = VmRecord {
                        vm_id: vm_id.clone(),
                        pid,
                        state: VmState::Running,
                        boot_time: Instant::now(),
                        config: VmRecordConfig {
                            kernel: String::new(),
                            initrd: None,
                            cmdline: String::new(),
                            mem_mb: 0,
                            vcpus: 0,
                        },
                        cgroup_path,
                        memory_limit_mb,
                    };
                    s.vms.insert(vm_id.clone(), record);
                    s.metrics.update(&vm_id, VmMetrics::default());
                    s.events.log(VmEvent::TemplateHit {
                        vm_id: vm_id.clone(),
                        template: template_path,
                    });
                    crate::control::prometheus_metrics::set_vm_count(s.vms.len() as i64);

                    Response::Ok {
                        body: ResponseBody::VmCreated { vm_id, pid },
                    }
                }
                Err(e) => Response::Error {
                    message: format!("Fork failed: {e}"),
                },
            }
        }

        Request::Metrics { vm_id } => {
            let s = state.lock().await;
            if vm_id == "_host" {
                // Return host-level metrics
                let host = metrics::collect_host_metrics();
                Response::Ok {
                    body: ResponseBody::Metrics {
                        metrics: serde_json::to_value(host).unwrap_or_default(),
                    },
                }
            } else {
                match s.metrics.get(&vm_id) {
                    Some(m) => Response::Ok {
                        body: ResponseBody::Metrics {
                            metrics: serde_json::to_value(m).unwrap_or_default(),
                        },
                    },
                    None => Response::Error {
                        message: format!("No metrics for VM: {vm_id}"),
                    },
                }
            }
        }

        Request::RestoreVm { snapshot_path, net, shared_dir, block, memory_limit_mb } => {
            let mut s = state.lock().await;
            let vm_id = s.alloc_id();
            let cid = s.next_cid;
            s.next_cid += 1;
            s.persist();

            tracing::info!(
                vm_id = %vm_id,
                snapshot = %snapshot_path,
                cid,
                "Restoring VM from snapshot"
            );

            match daemon::spawn_restore(&snapshot_path, net, shared_dir.as_deref(), block.as_deref(), Some(cid)) {
                Ok(pid) => {
                    let cgroup_path = setup_cgroup(&vm_id, pid, memory_limit_mb);
                    let record = VmRecord {
                        vm_id: vm_id.clone(),
                        pid,
                        state: VmState::Running,
                        boot_time: Instant::now(),
                        config: VmRecordConfig {
                            kernel: String::new(),
                            initrd: None,
                            cmdline: String::new(),
                            mem_mb: 0,
                            vcpus: 0,
                        },
                        cgroup_path,
                        memory_limit_mb,
                    };
                    s.vms.insert(vm_id.clone(), record);
                    s.metrics.update(&vm_id, VmMetrics::default());
                    s.events.log(VmEvent::Boot {
                        vm_id: vm_id.clone(),
                    });
                    crate::control::prometheus_metrics::set_vm_count(s.vms.len() as i64);

                    Response::Ok {
                        body: ResponseBody::VmCreated { vm_id, pid },
                    }
                }
                Err(e) => Response::Error {
                    message: format!("Restore failed: {e}"),
                },
            }
        }

        // Pause/Resume/Shutdown are handled by the per-VM sync_server,
        // not the async control server.
        Request::Pause | Request::Resume | Request::Shutdown | Request::IncrementalSnapshot { .. } | Request::LiveMigrate { .. } | Request::Exec { .. } | Request::SetBalloon { .. } | Request::Branch { .. } => Response::Error {
            message: "Use the per-VM control socket for pause/resume/shutdown/incremental-snapshot/live-migrate/exec/set-balloon/branch".to_string(),
        },
    }
}

// ---------------------------------------------------------------------------
// ControlClient
// ---------------------------------------------------------------------------

/// Client for sending commands to a running ControlServer.
pub struct ControlClient {
    socket_path: String,
}

impl ControlClient {
    pub fn new(socket_path: impl Into<String>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    /// Send a request and receive the response.
    pub async fn send(&self, request: &Request) -> Result<Response> {
        let stream = UnixStream::connect(&self.socket_path).await?;
        let (mut reader, mut writer) = stream.into_split();

        write_frame(&mut writer, request).await?;
        let response: Response = read_frame(&mut reader).await?;

        Ok(response)
    }
}
