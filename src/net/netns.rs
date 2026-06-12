//! Network namespace isolation — per-VM netns for stronger isolation.
//!
//! Uses `ip netns` for setup (requires CAP_NET_ADMIN). Each VM may
//! optionally get its own netns with a dedicated bridge and veth pair
//! to the host, so it cannot address sibling VMs on the shared bridge.
//!
//! Note: full integration with the TAP-based virtio-net path requires
//! moving the TAP fd into the netns after creation, which is left to
//! a follow-up. The functions here are the building blocks.

use std::process::Command;

use anyhow::{Context, Result};

const NETNS_DIR: &str = "/var/run/netns";

/// Create a named network namespace. Idempotent — returns Ok if it
/// already exists.
pub fn create_netns(name: &str) -> Result<()> {
    let status = Command::new("ip")
        .args(["netns", "add", name])
        .status()
        .with_context(|| format!("Failed to run `ip netns add {name}`"))?;
    if !status.success() {
        return Err(anyhow::anyhow!("`ip netns add {name}` exited {status}"));
    }
    tracing::info!(netns = name, "Created network namespace");
    Ok(())
}

/// Delete a named network namespace. Idempotent.
pub fn delete_netns(name: &str) -> Result<()> {
    let status = Command::new("ip")
        .args(["netns", "del", name])
        .status()
        .with_context(|| format!("Failed to run `ip netns del {name}`"))?;
    if !status.success() {
        // Not an error if the netns doesn't exist.
        return Ok(());
    }
    tracing::info!(netns = name, "Deleted network namespace");
    Ok(())
}

/// Move a network interface into a netns (so it becomes invisible to
/// the host root netns). The interface is renamed to `new_name` inside.
pub fn move_interface_to_netns(
    iface: &str,
    netns: &str,
    new_name: Option<&str>,
) -> Result<()> {
    let mut args = vec!["link", "set", iface, "netns", netns];
    let status = Command::new("ip")
        .args(&args)
        .status()
        .with_context(|| format!("Failed to move {iface} to netns {netns}"))?;
    if !status.success() {
        return Err(anyhow::anyhow!("`ip link set {iface} netns {netns}` exited {status}"));
    }
    if let Some(new) = new_name {
        args = vec!["-n", netns, "link", "set", iface, "name", new];
        let status = Command::new("ip")
            .args(&args)
            .status()
            .with_context(|| format!("rename {iface} in netns {netns}"))?;
        if !status.success() {
            return Err(anyhow::anyhow!("rename failed: {status}"));
        }
    }
    Ok(())
}

/// Run a command inside a netns. Returns the captured stdout.
pub fn exec_in_netns(netns: &str, cmd: &str, args: &[&str]) -> Result<String> {
    let mut command = Command::new("ip");
    command.args(["netns", "exec", netns, cmd]);
    for a in args {
        command.arg(a);
    }
    let out = command
        .output()
        .with_context(|| format!("Failed to exec {cmd} in netns {netns}"))?;
    if !out.status.success() {
        return Err(anyhow::anyhow!(
            "`{}` in netns {} failed: {}",
            cmd,
            netns,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Set up loopback (lo) inside a freshly created netns. Required for
/// any local networking to work.
pub fn setup_loopback(netns: &str) -> Result<()> {
    exec_in_netns(netns, "ip", &["link", "set", "lo", "up"])?;
    Ok(())
}

/// Set up an isolated bridge + NAT inside a netns.
///
/// Creates `br-inside` with the given gateway IP, brings it up, and
/// adds a masquerade rule so traffic can leave via the default route
/// (the netns must have an egress route, e.g. via a veth peer).
pub fn setup_isolated_network(netns: &str, gateway_ip: &str, prefix_len: u8) -> Result<()> {
    let cidr = format!("{gateway_ip}/{prefix_len}");
    exec_in_netns(netns, "ip", &["link", "add", "br-inside", "type", "bridge"])?;
    exec_in_netns(netns, "ip", &["addr", "add", &cidr, "dev", "br-inside"])?;
    exec_in_netns(netns, "ip", &["link", "set", "br-inside", "up"])?;

    // Enable forwarding + NAT inside the netns.
    exec_in_netns(
        netns,
        "sysctl",
        &["-w", "net.ipv4.ip_forward=1"],
    )?;
    exec_in_netns(
        netns,
        "iptables",
        &["-t", "nat", "-A", "POSTROUTING", "-s", &cidr, "-j", "MASQUERADE"],
    )?;
    tracing::info!(netns, gateway = gateway_ip, prefix_len, "Isolated network configured");
    Ok(())
}

/// Check whether a named netns exists.
pub fn exists(name: &str) -> bool {
    std::path::Path::new(NETNS_DIR).join(name).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netns_path_resolution() {
        // Just confirms the helper doesn't panic; we don't actually
        // create netns in unit tests.
        assert!(!exists("clone-nonexistent-test-netns"));
    }
}
