# Clone

A lightweight Linux VMM built for multi-tenant shell hosting and high-density VM workloads. 25K lines of Rust, single binary, KVM-based.

Fork of [unixshells/clone](https://github.com/nickelpack/clone) with significant enhancements.

**[中文文档](README_CN.md)**

---

## What's New

Enhancements on top of the original Clone:

### Bridge Networking + Auto IP Allocation

All VMs share a `clone-br0` bridge (172.30.0.0/16 subnet). IPs are allocated sequentially (172.30.0.2, 0.3, ...) — supports up to 65534 VMs. Uses `ioctl` instead of `ip` subprocess for bridge creation and IP configuration; NAT still uses iptables.

```bash
clone run --kernel vmlinuz --rootfs ubuntu.img --net
# VM automatically gets 172.30.0.2, gateway, DNS — all configured
```

### Sudo-less Operation

Linux capabilities (`CAP_NET_ADMIN`, `CAP_NET_RAW`) replace `sudo`. One-time setup after build:

```bash
sudo setcap cap_net_admin,cap_net_raw+ep target/release/clone
# All subsequent operations without root
```

### Auto Persistent Overlay

`--overlay` automatically creates an overlay file named by IP (e.g. `overlay-172.30.0.2.img`). Uses `fallocate` (not sparse files) to avoid block device stalls. Supports `--overlay-size` parameter.

```bash
clone run --kernel vmlinuz --rootfs ubuntu.img --overlay --overlay-size 10G --net
```

### Directory Mounting (virtio-fs)

`--shared-dir` supports three-segment format `path:tag:mountpoint` — auto-mounts to the specified guest path. `virtiofs.ko` is embedded into initrd, loaded and mounted by `clone-init` automatically.

```bash
clone run --kernel vmlinuz --rootfs ubuntu.img --net \
  --shared-dir /host/path:tag:/guest/mount
```

### VM Save/Restore

Save a running VM's full state (memory + vCPU + devices) to disk. Restore later — in-memory processes continue running.

```bash
clone save --vm-id $VM_ID --output ./snapshots/my-vm
clone restore --snapshot ./snapshots/my-vm --net
```

Overlay path and guest IP are saved alongside the snapshot, so `restore` reattaches disk and network automatically.

### HTTP REST API + Prometheus Metrics

Daemon exposes an HTTP API with optional Bearer token auth and CORS. Built-in Prometheus metrics at `/metrics`.

```bash
clone daemon --listen 127.0.0.1:8080 --auth-token mysecret

# Endpoints:
GET    /api/vms              List running VMs
POST   /api/vms              Create a new VM
GET    /api/vms/:id          Get VM status
POST   /api/vms/:id/fork     Fork a VM
POST   /api/vms/:id/snapshot Snapshot a VM
DELETE /api/vms/:id          Destroy a VM
GET    /metrics              Prometheus metrics
```

### cgroup v2 + Network Namespace

Per-VM cgroup memory limits (`--memory-limit-mb` on create). Per-VM network namespace isolation for stronger tenant separation.

### Guest Auto Networking

`clone-init` reads kernel parameters (`clone.net_ip/gw/mask`), configures `eth0` via `ioctl`, writes DNS (8.8.8.8), and adds default route — all automatic, no manual guest setup needed.

### SSH Login Optimization

`clone-init` auto-sets `UseDNS no` in `sshd_config`, eliminating DNS reverse lookup delay. Login drops from seconds to ~0.1s.

### Stability Fixes

- `fallocate` pre-allocation replaces sparse files — eliminates `blk_mq_requeue_work` CPU stalls
- Kernel parameter `blk_mq=0` disables problematic block multi-queue
- `clone-init` must be statically compiled (`RUSTFLAGS="-C target-feature=+crt-static"`)

### MCP Integration

`sdk/clone-mcp/` provides a Model Context Protocol server that bridges AI agents (Claude Desktop, Cursor, etc.) to the clone daemon's HTTP API.

### Build Scripts

- `build-alpine-image.sh` / `build-ubuntu-image.sh` / `build-ubuntu26-image.sh` — build rootfs images
- `merge-image.sh` — merge overlay back into base image
- `diagnose.sh` — VM diagnostics

---

## Quick Start

```bash
# Build
cargo build --release

# One-time: set capabilities for sudo-less operation
sudo setcap cap_net_admin,cap_net_raw+ep target/release/clone

# Create a rootfs
sudo clone rootfs create --distro ubuntu --size 2G -o ubuntu.img

# Boot a VM with networking + persistent overlay
clone run --kernel vmlinuz --rootfs ubuntu.img --overlay --overlay-size 10G --net --mem-mb 1024

# With host directory sharing
clone run --kernel vmlinuz --rootfs ubuntu.img --net \
  --shared-dir /host/path:tag:/guest/mount

# Save and restore VM
clone save --vm-id $VM_ID --output ./snapshots/my-vm
clone restore --snapshot ./snapshots/my-vm --net

# Start daemon with HTTP API
clone daemon --listen 127.0.0.1:8080 --auth-token secret123
```

### Prerequisites

- Linux host with KVM (`/dev/kvm`)
- Kernel 6.5+ recommended
- For networking: `/dev/net/tun` (vhost-net optional)

---

## VM Lifecycle

| Command | What it does |
|---------|-------------|
| `clone run` | Boot a new VM from kernel + rootfs/initrd |
| `clone fork` | Fork from a template snapshot (<20ms) |
| `clone snapshot` | Snapshot a running VM for later fork |
| `clone save` | Save a running VM to disk, then shut down |
| `clone restore` | Restore a saved VM, in-memory processes resume |
| `clone attach` | Attach to a running VM's serial console |
| `clone exec` | Execute a command inside a running VM |
| `clone list` | List running VMs |
| `clone migrate --live` | Pre-copy live migration to another host |
| `clone migrate-recv` | Receive a live migration |
| `clone rootfs create` | Create a bootable rootfs (Alpine, Ubuntu, Debian, Docker import) |
| `clone daemon` | Multi-VM orchestration daemon with HTTP REST API |

---

*Everything below is from the original project documentation.*

---

## Why Clone

**The problem:** Traditional shared shell hosting gives every user a login on the same kernel. Resource usage is minimal — idle users cost almost nothing. But in 2026, multi-tenant on a shared kernel is indefensible. Container escapes are routine. Kernel exploits ship monthly. There's no safe way to run untrusted users on a shared kernel.

VMs solve the security problem — KVM gives you a hardware-enforced boundary. But VMs are slow to start and each one consumes its own memory. Running 100 VMs like you'd run 100 shell users is prohibitively expensive. We looked at every existing VMM — QEMU, Firecracker, Cloud Hypervisor — and none of them could give us shared-shell-level resource efficiency with VM-level isolation.

So we built Clone.

**Clone's answer:** Shadow Clone fork from warm templates. Boot a VM once with everything loaded, snapshot it, then fork copies in <20ms. All forks share the same physical memory pages until they write — only dirty pages cost memory. 100 forked VMs use memory like 10. You get the resource profile of shared shell hosting with the full security of KVM hardware isolation.

```
Template VM (4GB Ubuntu, all tools warm)
  ├── Fork → User shell 1  ─── ~160ms, full networking, unique IP
  ├── Fork → User shell 2  ─── ~160ms, Shadow Clone diverges on write
  ├── Fork → User shell 3  ─── ~160ms, balloon reclaims when idle
  └── Fork → User shell N  ─── ~160ms, KVM hardware isolation

Lightweight template (Alpine/busybox, 128-512MB)
  ├── Fork → Lambda 1  ─── <20ms, minimal overhead
  ├── Fork → Lambda 2  ─── <20ms, ~4MB per fork
  └── Fork → Lambda N  ─── <20ms, destroy after use
```

| | Clone (measured) | Firecracker (official) | Cloud Hypervisor (official) | QEMU |
|---|--------|-------------|------------------|------|
| **Code size** | 25K Rust | ~50K Rust | ~70K Rust | ~2M+ C |
| **Fork (Alpine)** | **<20ms** (Shadow Clone) | ~5-10ms (snapshot) | stop+resume | stop+resume |
| **Fork (4GB Ubuntu + net)** | **~160ms** (Shadow Clone) | N/A | N/A | N/A |
| **Cold boot (distro kernel)** | **2,217ms** | ~2-3s | ~2s | 5-20s |
| **Cold boot (minimal kernel)** | — | <=125ms ^1 | <100ms ^1 | 500ms-2s |
| **Live migration downtime** | **1ms** | **none** | yes (unpublished) | 50-300ms |
| **2x 4GB forked VMs host RAM** | **~1GB** | N/A | N/A | N/A |
| **3 forked VMs RSS (Alpine)** | **13MB** | N/A | N/A | N/A |
| **10x 512MB idle VMs** | **~200MB** | ~5GB | variable | variable |
| **Incremental snapshot** | **192KB** (682x smaller) | full only | full only | full + incremental |
| **GPU passthrough** | **yes (VFIO)** | **no** | yes | yes |
| **Host dir sharing** | **yes (no daemon)** | **no** | virtiofsd | virtiofsd |
| **Fork networking** | **yes (userspace)** | no | no | no |
| **Fork vsock** | **yes (userspace)** | yes (userspace) | no | no |

^1 With custom minimal kernels. Distro kernels: all VMMs converge to ~2-3s.

---

## Features

### Memory Management

Three layers stacked to minimize host RAM across VMs:

1. **Overcommit** — `MAP_NORESERVE`, pages allocated on first write only
2. **KSM** — `MADV_MERGEABLE` deduplicates identical pages across all VMs
3. **Balloon** — graduated reclaim with hysteresis (idle 30s → 25%, idle 2min → 50%, idle 5min → floor)

Result: 10 idle 512MB VMs use ~200MB of host RAM, not 5GB.

### Shadow Clone Fork

```
Boot template → warm binaries → snapshot memory + registers
                                        ↓
              New VM = mmap(snapshot, MAP_PRIVATE)  ← ~160ms
                                        ↓
              Inject identity (hostname, CID, IP, MAC)
                                        ↓
              Transport reset → agent reconnects → exec ready
```

All forks share the same physical pages via Shadow Clone mapping until they write. No kernel boot on fork. The userspace vsock and net backends handle fork state without kernel involvement. Each fork gets unique IP, hostname, and vsock CID.

Measured: 2 forked 4GB Ubuntu VMs use **~1GB host RAM** (Shadow Clone sharing). 3 forked Alpine VMs use **13MB total RSS** vs 127MB template.

### Live Migration

Pre-copy over TCP. VM keeps running while memory transfers in the background.

```
Source                              Destination
  │ send full memory (skip zeros) ──→ │
  │ send dirty pages (round 1)   ──→ │
  │ send dirty pages (round 2)   ──→ │
  │ ...converge...                    │
  │ PAUSE → send final dirty + CPU ─→│
  │         ~19ms downtime            │ RESUME
  │ shutdown                          │ running
```

### Security

- **KVM hardware isolation** — each VM is a separate address space
- **Seccomp jailer** — BPF syscall filter on VMM process (`--seccomp`)
- **Measured boot** — SHA-256 kernel hash verification before loading
- **Namespace jail** — optional full jail with chroot + capabilities (`--jail`)

### Devices

- **virtio-block** — raw and qcow2 disk images, thin provisioning
- **virtio-net** — TAP + vhost-net (boot) or userspace (fork), auto bridge/NAT setup
- **virtio-balloon** — cooperative memory reclaim with hysteresis policy
- **virtio-vsock** — userspace backend for host-guest communication (fork-compatible)
- **virtio-fs** — host directory sharing via inline FUSE (no external daemon)
- **PCI bus** — ECAM config space for VFIO device passthrough
- **Serial console** — 16550A UART, bidirectional terminal I/O

### Rootfs Modes

```bash
# Mode 1: Custom initrd (everything in RAM)
clone run --kernel vmlinuz --initrd my-initrd.img

# Mode 2: Disk rootfs (persistent, read-write)
clone run --kernel vmlinuz --rootfs disk.img

# Mode 3: Shared base + overlay (multi-VM, ephemeral or persistent)
clone run --kernel vmlinuz --rootfs base.img --overlay
clone run --kernel vmlinuz --rootfs base.img --overlay /data/vm1.qcow2
```

---

## Architecture

```
src/
├── main.rs              CLI entry point
├── vmm/                 VM lifecycle, vCPU threads, MMIO bus
├── boot/                Kernel loading (bzImage/ELF), ACPI tables, page tables
├── memory/              Guest memory, overcommit, KSM, page tables, GDT
├── virtio/              Virtio devices (block, net, balloon, vsock, fs)
├── pci/                 PCI bus (ECAM), VFIO passthrough
├── migration/           Pre-copy live migration (sender, receiver, wire protocol)
├── control/             Control plane (per-VM socket + daemon for multi-VM orchestration)
│   ├── http.rs          HTTP REST API (axum)
│   ├── cgroup.rs        cgroup v2 per-VM memory limits
│   ├── prometheus_metrics.rs  Prometheus metrics
│   └── ...
├── net/                 TAP/bridge/NAT auto-setup
│   ├── netns.rs         Network namespace isolation
│   └── ...
├── storage/             Raw + QCOW2 block backends
├── rootfs.rs            Auto-generated initrd for --rootfs mode (embeds kernel modules, agent)
└── rootfs_create.rs     `clone rootfs create` (Alpine, Ubuntu, Debian, Docker)

crates/
├── guest-agent/         In-guest vsock agent (exec, networking, D-Bus recovery, heartbeat)
└── clone-init/          Minimal init for auto-generated initrd (module loading, rootfs mount, agent launch)

sdk/
└── clone-mcp/           MCP server for AI agent integration (Python)

scripts/
└── make_initrd.sh       Build custom initrd
```

**Dependencies:** kvm-ioctls, kvm-bindings, vm-memory, libc, clap, anyhow, tracing, sha2, axum, prometheus. No libvirt, no QEMU, no forked codebases.

---

## Benchmarks

All numbers measured on bare-metal (OVH dedicated server, Intel Xeon E-2386G, Ubuntu 24.04, kernel 6.8.0-106-generic).

| Metric | Value |
|--------|-------|
| Shadow Clone fork (Alpine, minimal) | **<20ms** |
| Shadow Clone fork (4GB Ubuntu, to exec) | **~160ms** |
| VMM overhead (cold boot) | **35ms** (memory, irqchip, devices, kernel load) |
| Cold boot to shell (distro kernel) | **2,217ms** (best), **2,338ms** (avg of 5 runs) |
| Live migration downtime (256MB) | **1ms** |
| Incremental snapshot size | **192KB** for 512MB VM (682x smaller than full) |
| Shadow Clone sharing (2x 4GB Ubuntu) | **~1GB host RAM** for 8GB committed |
| Shadow Clone sharing (3x Alpine) | **13MB** vs 127MB template |
| Binary size | ~3MB |
| VMM memory overhead | ~5-10MB |

---

## Test Results

**63 tests, 62 passed, 1 skipped. Full suite in ~315 seconds.**

| Category | Tests | Highlights |
|----------|-------|------------|
| Boot & ACPI | 5 PASS | Cold boot **2,218ms**, 4-vCPU SMP |
| Control Plane | 2 PASS | Socket status/pause/resume/shutdown |
| Storage | 3 PASS | virtio-block, QCOW2, backing file overlay |
| Snapshots & Fork | 4 PASS | Incremental **192KB**, Shadow Clone sharing **19MB RSS** |
| Security | 1 PASS | Seccomp BPF |
| Devices | 2 PASS, 1 SKIP | virtio-fs, PCI, VFIO (skip: no hardware) |
| Migration | 1 PASS | **1ms downtime** |
| Rootfs Boot | 2 PASS | Alpine 3.21, Ubuntu 24.04 |
| Networking | 3 PASS | Unique CID, guest networking, exec **796ms** |
| Multi-VM & Memory | 3 PASS | Concurrent VMs, overcommit, balloon |
| Shadow Clone | 1 PASS | 3 forks **18MB total RSS** |

---

## License

MIT. Copyright (c) 2026 Unix Shells Limited Company.
