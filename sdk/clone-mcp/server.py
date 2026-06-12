#!/usr/bin/env python3
"""Clone MCP Server — exposes clone VMM management to AI agents.

Bridges AI tools (Claude Desktop, Cursor, etc.) to the clone daemon's
HTTP REST API. The daemon holds all privileges; this server is a thin
non-privileged forwarder speaking the Model Context Protocol.

Configuration (environment variables):
  CLONE_URL    Base URL of clone daemon (default: http://127.0.0.1:8080)
  CLONE_TOKEN  Bearer token for HTTP API auth (required if daemon has --auth-token)
  CLONE_TIMEOUT  Request timeout in seconds (default: 300, generous for snapshots)
"""

from __future__ import annotations

import os
from typing import Optional

import requests
from mcp.server.fastmcp import FastMCP

mcp = FastMCP("clone")

REST_URL = os.environ.get("CLONE_URL", "http://127.0.0.1:8080").rstrip("/")
TOKEN = os.environ.get("CLONE_TOKEN", "")
TIMEOUT = int(os.environ.get("CLONE_TIMEOUT", "300"))


def _headers() -> dict:
    return {"Authorization": f"Bearer {TOKEN}"} if TOKEN else {}


def _call(method: str, path: str, **kwargs) -> dict:
    """HTTP request to clone daemon. Returns JSON or error dict."""
    url = f"{REST_URL}{path}"
    try:
        r = requests.request(method, url, headers=_headers(), timeout=TIMEOUT, **kwargs)
        ctype = r.headers.get("content-type", "")
        if ctype.startswith("application/json"):
            return r.json()
        if r.status_code >= 400:
            return {"error": r.text or r.reason, "status": r.status_code}
        return {"text": r.text}
    except requests.RequestException as e:
        return {"error": f"{type(e).__name__}: {e}"}


# ============================================================================
# VM lifecycle
# ============================================================================

@mcp.tool()
def list_vms() -> dict:
    """List all VMs currently tracked by the daemon."""
    return _call("GET", "/v1/vms")


@mcp.tool()
def create_vm(
    kernel: str,
    mem_mb: int = 512,
    vcpus: int = 1,
    rootfs: Optional[str] = None,
    overlay: Optional[str] = None,
    shared_dir: Optional[str] = None,
    block: Optional[str] = None,
    net: bool = False,
    memory_limit_mb: Optional[int] = None,
) -> dict:
    """Create and boot a new VM.

    Args:
        kernel: Path to kernel image (e.g., 'vmlinuz')
        mem_mb: Memory size in MB
        vcpus: Number of virtual CPUs
        rootfs: Path to rootfs disk image (mutually exclusive with initrd)
        overlay: 'auto' (default, named by guest IP), 'tmpfs', or a file path
        shared_dir: Host dir to share via virtio-fs, format '/host/path:tag'
        block: Additional block device image path
        net: Auto-configure networking (bridge, TAP, NAT)
        memory_limit_mb: cgroup v2 hard memory limit in MB

    Returns: {vm_id, pid}
    """
    body = {
        "kernel": kernel, "mem_mb": mem_mb, "vcpus": vcpus, "net": net,
        "rootfs": rootfs, "overlay": overlay, "shared_dir": shared_dir,
        "block": block, "memory_limit_mb": memory_limit_mb,
    }
    body = {k: v for k, v in body.items() if v is not None}
    return _call("POST", "/v1/vms", json=body)


@mcp.tool()
def get_vm_status(vm_id: str) -> dict:
    """Get the status of a specific VM by ID.

    Returns: {state, uptime_secs, memory_usage_bytes}
    """
    return _call("GET", f"/v1/vms/{vm_id}")


@mcp.tool()
def destroy_vm(vm_id: str) -> dict:
    """Destroy a VM and release all its resources (memory, cgroup, network)."""
    return _call("DELETE", f"/v1/vms/{vm_id}")


# ============================================================================
# VM runtime operations
# ============================================================================

@mcp.tool()
def pause_vm(vm_id: str) -> dict:
    """Pause all vCPUs of a VM (freeze execution without losing state)."""
    return _call("POST", f"/v1/vms/{vm_id}/pause")


@mcp.tool()
def resume_vm(vm_id: str) -> dict:
    """Resume a previously paused VM."""
    return _call("POST", f"/v1/vms/{vm_id}/resume")


@mcp.tool()
def shutdown_vm(vm_id: str) -> dict:
    """Gracefully shut down a VM."""
    return _call("POST", f"/v1/vms/{vm_id}/shutdown")


@mcp.tool()
def exec_in_vm(vm_id: str, command: str, args: Optional[list[str]] = None) -> dict:
    """Execute a command inside a running VM via the guest agent.

    Args:
        vm_id: Target VM ID
        command: Executable name (e.g., 'ls', 'cat', 'apt')
        args: Arguments to pass to the command

    Returns: {exit_code, stdout, stderr}
    """
    body = {"command": command, "args": args or []}
    return _call("POST", f"/v1/vms/{vm_id}/exec", json=body)


@mcp.tool()
def snapshot_vm(vm_id: str, output_path: str) -> dict:
    """Take a full snapshot of a VM (memory + vCPU + device state).

    The VM is paused during snapshot, then resumed. Output is written
    to a directory containing memory.raw + template.json.

    Args:
        vm_id: VM to snapshot
        output_path: Directory path for the snapshot output
    """
    return _call("POST", f"/v1/vms/{vm_id}/snapshot", json={"output_path": output_path})


@mcp.tool()
def branch_vm(
    vm_id: str,
    net: bool = False,
    shared_dir: Optional[str] = None,
    output_dir: Optional[str] = None,
) -> dict:
    """Live-branch a running VM: pause → snapshot → fork → resume.

    Creates a new VM that's an exact copy at the branch moment. The
    source VM stays alive. Branch VM gets a new vm_id and PID.

    Args:
        vm_id: Source VM to branch from
        net: Auto-configure networking for the branch
        shared_dir: virtio-fs shared directory for the branch
        output_dir: Where to store the intermediate snapshot (default: tmpfs)

    Returns: {new_vm_id, new_pid, pause_duration_ms, total_duration_ms}
    """
    body: dict = {"net": net}
    if shared_dir:
        body["shared_dir"] = shared_dir
    if output_dir:
        body["output_dir"] = output_dir
    return _call("POST", f"/v1/vms/{vm_id}/branch", json=body)


@mcp.tool()
def save_vm(vm_id: str, output_path: str) -> dict:
    """Save a VM to disk then shut it down. Use restore_vm to restart.

    Equivalent to snapshot_vm + shutdown_vm in sequence.
    """
    return _call("POST", f"/v1/vms/{vm_id}/save", json={"output_path": output_path})


@mcp.tool()
def fork_vm(
    template_path: str,
    net: bool = False,
    shared_dir: Optional[str] = None,
    mem_mb: Optional[int] = None,
    vcpus: Optional[int] = None,
    memory_limit_mb: Optional[int] = None,
) -> dict:
    """Fork a new VM from a template snapshot (CoW shared memory).

    Multiple forks of the same template share memory pages until written
    (copy-on-write). Fast cold start, ideal for ephemeral dev shells.

    Args:
        template_path: Directory produced by save_vm or snapshot_vm
        net: Auto-configure networking
        shared_dir: virtio-fs shared directory
        mem_mb: Memory cap (balloon reclaims excess)
        vcpus: Active vCPU count (others stay offline)
        memory_limit_mb: cgroup hard limit

    Returns: {vm_id, pid}
    """
    body: dict = {"template_path": template_path, "net": net}
    if shared_dir:
        body["shared_dir"] = shared_dir
    if mem_mb is not None:
        body["mem_mb"] = mem_mb
    if vcpus is not None:
        body["vcpus"] = vcpus
    if memory_limit_mb is not None:
        body["memory_limit_mb"] = memory_limit_mb
    return _call("POST", "/v1/fork", json=body)


@mcp.tool()
def restore_vm(
    snapshot_path: str,
    net: bool = False,
    shared_dir: Optional[str] = None,
    block: Optional[str] = None,
    memory_limit_mb: Optional[int] = None,
) -> dict:
    """Restore a previously saved VM. Replays full vCPU/device state.

    Unlike fork_vm (CoW shared), restore loads an independent memory
    copy — the VM resumes exactly where it left off.

    Args:
        snapshot_path: Directory produced by save_vm
        net: Auto-configure networking
        shared_dir: virtio-fs shared directory
        block: Override block device image
        memory_limit_mb: cgroup hard limit

    Returns: {vm_id, pid}
    """
    body: dict = {"snapshot_path": snapshot_path, "net": net}
    if shared_dir:
        body["shared_dir"] = shared_dir
    if block:
        body["block"] = block
    if memory_limit_mb is not None:
        body["memory_limit_mb"] = memory_limit_mb
    return _call("POST", "/v1/restore", json=body)


# ============================================================================
# Diff chain management
# ============================================================================

@mcp.tool()
def get_snapshot_chain(tag_or_dir: str) -> dict:
    """Get diff chain info for a snapshot.

    Walks parent_tag from the given leaf back to root, returning all
    nodes in root→leaf order.

    Args:
        tag_or_dir: Snapshot directory path or tag name

    Returns: {nodes: [{tag, snapshot_type, chain_depth, dir}], total_depth}
    """
    return _call("GET", f"/v1/snapshots/{tag_or_dir}/chain")


@mcp.tool()
def compact_chain(leaf_dir: str, output_dir: str) -> dict:
    """Flatten a diff chain into a single self-contained full snapshot.

    Resolves the chain by applying each diff in order, then writes a
    new independent snapshot with no parent. Useful before long-term
    storage or when chain depth hurts load performance.

    Args:
        leaf_dir: Chain leaf snapshot directory
        output_dir: Where to write the compacted snapshot

    Returns: {output_dir, memory_size_bytes, memory_hash, runtime_type}
    """
    return _call("POST", "/v1/snapshots/compact", json={
        "leaf_dir": leaf_dir, "output_dir": output_dir,
    })


@mcp.tool()
def gc_snapshots(base_dir: str, active_tags: list[str]) -> dict:
    """Garbage-collect unreachable snapshots under base_dir.

    A snapshot is reachable if it's in active_tags or is an ancestor of
    any active snapshot. Others are deleted.

    Args:
        base_dir: Directory containing snapshot subdirectories
        active_tags: Tags (subdirectory names) to preserve

    Returns: {removed: [...], count: N}
    """
    return _call("POST", "/v1/snapshots/gc", json={
        "base_dir": base_dir, "active_tags": active_tags,
    })


# ============================================================================
# Observability
# ============================================================================

@mcp.tool()
def get_host_metrics() -> dict:
    """Get host-level metrics: memory, VM counts, KSM, overcommit."""
    return _call("GET", "/v1/host/metrics")


@mcp.tool()
def health_check() -> dict:
    """Check daemon connectivity and auth. Returns {status: 'ok'} if healthy."""
    return _call("GET", "/health")


if __name__ == "__main__":
    mcp.run()
