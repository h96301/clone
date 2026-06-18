//! Tenant state machine and JSON persistence.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::sync::RwLock;

use crate::clone_client::CloneClient;
use crate::config::ControllerConfig;

/// Per-tenant configuration provided at registration time.
/// Stored verbatim in JSON and used to drive clone daemon API calls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantConfig {
    pub tenant_id: String,
    /// Path to guest kernel image, e.g. "vmlinuz-26".
    pub kernel: String,
    /// Path to rootfs image, e.g. "ubuntu-26-sshd.img".
    pub rootfs: String,
    /// Path to the persistent overlay file (created automatically if missing).
    pub overlay_path: String,
    /// virtio-fs shared dir spec: "host_path:tag:guest_mountpoint".
    pub shared_dir: Option<String>,
    /// Memory size in MB shown to the guest.
    pub mem_mb: u32,
    /// cgroup v2 hard memory limit (None = unlimited).
    pub memory_limit_mb: Option<u32>,
    /// If set, passed as clone.init=<cmd> kernel arg to bypass systemd and
    /// exec the user's app directly.
    pub init_cmd: Option<String>,
    /// TCP port the user app listens on inside the guest, used for health
    /// probing. None disables probing.
    pub app_health_port: Option<u16>,
}

/// Lifecycle states a tenant's VM can be in.
/// Transitions are driven by `api::acquire` and `reconciler::reconcile_tenant`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "detail")]
pub enum VmStatus {
    /// No VM ever created for this tenant.
    NotFound,
    /// clone run/restore in progress.
    Starting,
    /// VM is running and serving traffic.
    Active,
    /// Idle: balloon inflated + drop_caches, awaiting hard reclaim.
    SoftReclaimed,
    /// clone save in progress.
    Saving,
    /// Saved to disk, VM killed. Next acquire triggers restore.
    Saved,
    /// Last operation failed; detail holds error message.
    Failed(String),
}

impl VmStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            VmStatus::NotFound => "not_found",
            VmStatus::Starting => "starting",
            VmStatus::Active => "active",
            VmStatus::SoftReclaimed => "soft_reclaimed",
            VmStatus::Saving => "saving",
            VmStatus::Saved => "saved",
            VmStatus::Failed(_) => "failed",
        }
    }
}

/// Full per-tenant state, persisted across controller restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantState {
    pub config: TenantConfig,
    pub status: VmStatus,
    /// clone daemon's vm-{XXXX} identifier, set when VM is alive.
    pub vm_id: Option<String>,
    /// Guest IP assigned by clone daemon (e.g. 172.30.0.5).
    pub guest_ip: Option<String>,
    /// Where the VM memory snapshot lives on disk (set after first hard reclaim).
    pub save_path: Option<String>,
    /// Timestamp of last acquire/release call.
    pub last_request_at: Option<DateTime<Utc>>,
    /// Timestamp of last successful health probe.
    pub last_health_ok_at: Option<DateTime<Utc>>,
    /// Consecutive failed health probes (reset on success).
    pub health_fail_count: u32,
    pub created_at: DateTime<Utc>,
    pub reclaim_count: u32,
    pub last_error: Option<String>,
}

impl TenantState {
    pub fn new(config: TenantConfig) -> Self {
        Self {
            config,
            status: VmStatus::NotFound,
            vm_id: None,
            guest_ip: None,
            save_path: None,
            last_request_at: None,
            last_health_ok_at: None,
            health_fail_count: 0,
            created_at: Utc::now(),
            reclaim_count: 0,
            last_error: None,
        }
    }

    /// Idle time in seconds; returns None if no activity yet.
    pub fn idle_secs(&self) -> Option<i64> {
        self.last_request_at
            .map(|t| (Utc::now() - t).num_seconds().max(0))
    }

    /// Returns the canonical save path under the controller's data dir.
    pub fn default_save_path(saves_root: &str, tenant_id: &str) -> String {
        format!("{saves_root}/{tenant_id}")
    }
}

/// Shared controller state: tenants map + config + clone daemon client.
#[derive(Clone)]
pub struct ControllerState {
    pub tenants: Arc<RwLock<HashMap<String, TenantState>>>,
    pub config: ControllerConfig,
    pub state_file: PathBuf,
    pub clone_client: Arc<CloneClient>,
}

impl ControllerState {
    pub fn new(config: ControllerConfig, clone_client: CloneClient) -> Self {
        let state_file = config.state_file.clone();
        Self {
            tenants: Arc::new(RwLock::new(HashMap::new())),
            config,
            state_file,
            clone_client: Arc::new(clone_client),
        }
    }

    /// Atomically persist the tenants map to JSON.
    /// Writes to `<state_file>.tmp` then renames — crash-safe on POSIX.
    pub async fn persist(&self) -> anyhow::Result<()> {
        let tenants = self.tenants.read().await;
        let json = serde_json::to_string_pretty(&*tenants)?;
        drop(tenants);

        if let Some(parent) = self.state_file.parent() {
            fs::create_dir_all(parent).await.ok();
        }
        let tmp = self.state_file.with_extension("json.tmp");
        fs::write(&tmp, json).await?;
        fs::rename(&tmp, &self.state_file).await?;
        Ok(())
    }

    /// Load tenants from JSON file. Returns empty map if file missing.
    /// Tenants that were mid-lifecycle (Active/Starting/Saving/SoftReclaimed) when
    /// the previous controller died cannot be safely restored — the VM process is
    /// gone and any in-flight save may be partial. They get downgraded to NotFound
    /// so the next acquire does a clean cold create. Only `Saved` survives restart
    /// (its snapshot file is complete and the VM is already gone).
    pub async fn load(state_file: &PathBuf) -> anyhow::Result<HashMap<String, TenantState>> {
        if !state_file.exists() {
            return Ok(HashMap::new());
        }
        let data = fs::read(state_file).await?;
        let mut map: HashMap<String, TenantState> = serde_json::from_slice(&data)?;
        let now = Utc::now();
        for (_, t) in map.iter_mut() {
            match t.status {
                VmStatus::Active
                | VmStatus::Starting
                | VmStatus::Saving
                | VmStatus::SoftReclaimed => {
                    tracing::warn!(
                        tenant = %t.config.tenant_id,
                        prev_status = %t.status.as_str(),
                        "downgrading to NotFound after controller restart"
                    );
                    t.status = VmStatus::NotFound;
                    t.vm_id = None;
                    t.guest_ip = None;
                    t.save_path = None;
                }
                VmStatus::NotFound | VmStatus::Saved | VmStatus::Failed(_) => {}
            }
            // Reset transient counters.
            t.health_fail_count = 0;
            t.last_error = None;
            let _ = now;
        }
        Ok(map)
    }
}
