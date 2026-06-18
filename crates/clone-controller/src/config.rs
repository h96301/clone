//! Controller configuration via CLI flags.

use std::path::PathBuf;

use clap::Parser;

/// Multi-tenant controller for the Clone VMM.
///
/// Sits between openresty and the clone daemon, providing tenant lifecycle
/// management and idle-driven save/restore aging.
#[derive(Parser, Debug, Clone)]
#[command(name = "clone-controller", version, about)]
pub struct Cli {
    /// HTTP listen address for the controller's REST API.
    #[arg(long, default_value = "127.0.0.1:8092", env = "CLONE_CTRL_LISTEN")]
    pub listen: String,

    /// Clone daemon base URL.
    #[arg(long, default_value = "http://127.0.0.1:8091", env = "CLONE_CTRL_DAEMON")]
    pub clone_daemon: String,

    /// Optional bearer token to authenticate against the daemon.
    #[arg(long, env = "CLONE_AUTH_TOKEN")]
    pub clone_auth_token: Option<String>,

    /// Path to the JSON state file (atomic writes).
    #[arg(
        long,
        default_value = "/tmp/clone-controller-state.json",
        env = "CLONE_CTRL_STATE_FILE"
    )]
    pub state_file: PathBuf,

    /// Root directory for per-tenant save snapshots.
    #[arg(
        long,
        default_value = "/tmp/clone-controller-saves",
        env = "CLONE_CTRL_SAVES_ROOT"
    )]
    pub saves_root: String,

    /// Reconciler tick interval in seconds.
    #[arg(long, default_value_t = 60, env = "CLONE_CTRL_RECONCILE_SECS")]
    pub reconcile_interval_secs: u64,

    /// Idle threshold to trigger soft reclaim (drop_caches + balloon), in seconds.
    #[arg(long, default_value_t = 120, env = "CLONE_CTRL_SOFT_RECLAIM_SECS")]
    pub soft_reclaim_after_secs: u64,

    /// Idle threshold to trigger hard reclaim (save + kill), in seconds.
    #[arg(long, default_value_t = 300, env = "CLONE_CTRL_HARD_RECLAIM_SECS")]
    pub hard_reclaim_after_secs: u32,

    /// Balloon target (MB) during soft reclaim.
    #[arg(long, default_value_t = 256, env = "CLONE_CTRL_BALLOON_TARGET_MB")]
    pub balloon_target_mb: u32,

    /// Enable TCP health probing inside the guest as idle fallback.
    #[arg(
        long,
        default_value_t = true,
        env = "CLONE_CTRL_HEALTH_PROBE",
        action = clap::ArgAction::Set
    )]
    pub health_probe_enabled: bool,

    /// Consecutive failed probes before treating VM as idle.
    #[arg(long, default_value_t = 3, env = "CLONE_CTRL_HEALTH_FAIL_THRESHOLD")]
    pub health_probe_fail_threshold: u32,

    /// Skip the daemon connectivity check at startup (useful for offline testing).
    #[arg(long, default_value_t = false)]
    pub skip_daemon_health_check: bool,
}

/// Resolved configuration used throughout the controller.
#[derive(Debug, Clone)]
pub struct ControllerConfig {
    pub listen: String,
    pub clone_daemon_url: String,
    pub clone_auth_token: Option<String>,
    pub state_file: PathBuf,
    pub saves_root: String,
    pub reconcile_interval_secs: u64,
    pub soft_reclaim_after_secs: u64,
    pub hard_reclaim_after_secs: u32,
    pub balloon_target_mb: u32,
    pub health_probe_enabled: bool,
    pub health_probe_fail_threshold: u32,
    pub skip_daemon_health_check: bool,
}

impl From<Cli> for ControllerConfig {
    fn from(c: Cli) -> Self {
        Self {
            listen: c.listen,
            clone_daemon_url: c.clone_daemon,
            clone_auth_token: c.clone_auth_token,
            state_file: c.state_file,
            saves_root: c.saves_root,
            reconcile_interval_secs: c.reconcile_interval_secs,
            soft_reclaim_after_secs: c.soft_reclaim_after_secs,
            hard_reclaim_after_secs: c.hard_reclaim_after_secs,
            balloon_target_mb: c.balloon_target_mb,
            health_probe_enabled: c.health_probe_enabled,
            health_probe_fail_threshold: c.health_probe_fail_threshold,
            skip_daemon_health_check: c.skip_daemon_health_check,
        }
    }
}
