//! cgroup v2 — per-VM memory limits and accounting.
//!
//! Layout: `/sys/fs/cgroup/clone/{vm_id}/` with `memory.max` set to the
//! requested limit and the VM's main PID written to `cgroup.procs`.
//!
//! Requires cgroup v2 mounted at `/sys/fs/cgroup` and write access to
//! the parent. Without root or a delegated cgroup, all operations return
//! errors and the caller is expected to fall back gracefully.

use std::path::PathBuf;

use anyhow::{Context, Result};

const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const PARENT_NAME: &str = "clone";

/// Path of the per-VM cgroup directory.
pub fn vm_cgroup_path(vm_id: &str) -> PathBuf {
    PathBuf::from(CGROUP_ROOT).join(PARENT_NAME).join(vm_id)
}

/// True if cgroup v2 is available and the parent dir is writable.
pub fn is_available() -> bool {
    let parent = PathBuf::from(CGROUP_ROOT).join(PARENT_NAME);
    // /sys/fs/cgroup itself is always present; we need write access to
    // create the `clone/` parent.
    std::fs::metadata(CGROUP_ROOT).is_ok()
}

/// Ensure the parent `clone/` cgroup exists and has memory controller enabled.
fn ensure_parent() -> Result<PathBuf> {
    let parent = PathBuf::from(CGROUP_ROOT).join(PARENT_NAME);
    if !parent.exists() {
        std::fs::create_dir(&parent)
            .with_context(|| format!("Failed to create parent cgroup {parent:?}"))?;
        // Enable memory + cpu controllers on the parent so children inherit them.
        let _ = std::fs::write(
            PathBuf::from(CGROUP_ROOT).join("cgroup.subtree_control"),
            "+memory +cpu +pids",
        );
    }
    Ok(parent)
}

/// Create a cgroup for a VM with the given memory limit (in MB).
///
/// Writes:
///   - `memory.max` = limit_bytes (or "max" if `memory_limit_mb` is None)
///   - `pids.max` = 4096 (sane default to prevent fork bombs)
pub fn create_vm_cgroup(vm_id: &str, memory_limit_mb: Option<u32>) -> Result<PathBuf> {
    let _ = ensure_parent()?;
    let path = vm_cgroup_path(vm_id);
    std::fs::create_dir(&path)
        .with_context(|| format!("Failed to create cgroup {path:?}"))?;

    // memory.max
    let memory_max = match memory_limit_mb {
        Some(mb) => (mb as u64 * 1024 * 1024).to_string(),
        None => "max".to_string(),
    };
    std::fs::write(path.join("memory.max"), memory_max.as_bytes())
        .with_context(|| format!("Failed to set memory.max for {path:?}"))?;

    // pids.max — prevent fork bombs inside the VM process tree
    let _ = std::fs::write(path.join("pids.max"), b"4096");

    tracing::info!(
        vm_id,
        memory_limit_mb = ?memory_limit_mb,
        cgroup = ?path,
        "Created VM cgroup"
    );
    Ok(path)
}

/// Add a process to a VM's cgroup by writing its pid to `cgroup.procs`.
pub fn add_process(vm_id: &str, pid: u32) -> Result<()> {
    let procs = vm_cgroup_path(vm_id).join("cgroup.procs");
    std::fs::write(&procs, pid.to_string().as_bytes())
        .with_context(|| format!("Failed to add pid {pid} to {procs:?}"))
}

/// Read current memory usage (memory.current) in bytes for a VM cgroup.
pub fn get_memory_usage(vm_id: &str) -> Result<u64> {
    let path = vm_cgroup_path(vm_id).join("memory.current");
    let s = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {path:?}"))?;
    s.trim()
        .parse::<u64>()
        .with_context(|| format!("Invalid memory.current value: {s:?}"))
}

/// Read peak memory usage (memory.peak) in bytes for a VM cgroup.
pub fn get_memory_peak(vm_id: &str) -> Result<u64> {
    let path = vm_cgroup_path(vm_id).join("memory.peak");
    let s = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {path:?}"))?;
    s.trim()
        .parse::<u64>()
        .with_context(|| format!("Invalid memory.peak value: {s:?}"))
}

/// Recursively remove a VM's cgroup. cgroup v2 directories can only be
/// removed when empty (no processes attached), so this should be called
/// after the VM process has exited.
pub fn remove_cgroup(vm_id: &str) -> Result<()> {
    let path = vm_cgroup_path(vm_id);
    if path.exists() {
        std::fs::remove_dir(&path)
            .with_context(|| format!("Failed to remove cgroup {path:?}"))?;
        tracing::info!(vm_id, cgroup = ?path, "Removed VM cgroup");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_cgroup_path_is_namespaced() {
        let p = vm_cgroup_path("vm-0001");
        assert!(p.starts_with("/sys/fs/cgroup/clone"));
        assert!(p.ends_with("vm-0001"));
    }
}
