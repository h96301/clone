# Clone

一个轻量级 Linux VMM，专为多租户 Shell 托管和高密度 VM 负载设计。25K 行 Rust，单二进制文件，基于 KVM。

基于 [unixshells/clone](https://github.com/nickelpack/clone) 的增强版本。

**[English](README.md)**

---

## 新增功能

在原版 Clone 基础上的增强：

### Bridge 网络 + 自动 IP 分配

所有 VM 共享 `clone-br0` 桥（172.30.0.0/16 网段）。IP 顺序分配（172.30.0.2、0.3...），支持 65534 个 VM。用 `ioctl` 替代 `ip` 子进程创建 bridge 和配置 IP，NAT 仍用 iptables。

```bash
clone run --kernel vmlinuz --rootfs ubuntu.img --net
# VM 自动获得 172.30.0.2、网关、DNS — 全部自动配置
```

### 免 sudo 运行

通过 Linux capabilities（`CAP_NET_ADMIN`、`CAP_NET_RAW`）替代 `sudo`。构建后一次性设置：

```bash
sudo setcap cap_net_admin,cap_net_raw+ep target/release/clone
# 之后所有 VM 操作无需 root
```

### 自动持久化 Overlay

`--overlay` 自动创建按 IP 命名的 overlay 文件（如 `overlay-172.30.0.2.img`）。使用 `fallocate` 预分配（非稀疏文件），避免块设备卡顿。支持 `--overlay-size` 参数。

```bash
clone run --kernel vmlinuz --rootfs ubuntu.img --overlay --overlay-size 10G --net
```

### 目录挂载（virtio-fs）

`--shared-dir` 支持三段格式 `path:tag:mountpoint`，自动挂载到 guest 指定路径。`virtiofs.ko` 嵌入 initrd，由 `clone-init` 自动加载和挂载。

```bash
clone run --kernel vmlinuz --rootfs ubuntu.img --net \
  --shared-dir /host/path:tag:/guest/mount
```

### VM 快照恢复（Save/Restore）

保存运行中 VM 的完整状态（内存 + vCPU + 设备）到磁盘。恢复时内存中的进程继续运行。

```bash
clone save --vm-id $VM_ID --output ./snapshots/my-vm
clone restore --snapshot ./snapshots/my-vm --net
```

Overlay 路径和 guest IP 会随快照一起保存，restore 时自动恢复网络和磁盘。

### HTTP REST API + Prometheus 指标

Daemon 暴露 HTTP API，支持 Bearer token 认证和 CORS。内置 Prometheus 指标（`/metrics`）。

```bash
clone daemon --listen 127.0.0.1:8080 --auth-token mysecret

# API 端点：
GET    /api/vms              列出运行中的 VM
POST   /api/vms              创建 VM
GET    /api/vms/:id          查看 VM 状态
POST   /api/vms/:id/fork     Fork 一个 VM
POST   /api/vms/:id/snapshot 快照 VM
DELETE /api/vms/:id          销毁 VM
GET    /metrics              Prometheus 指标
```

### cgroup v2 + 网络命名空间

每个 VM 独立的 cgroup 内存限制（create 时 `--memory-limit-mb`）。每个 VM 独立网络命名空间，加强租户间网络隔离。

### Guest 自动配网

`clone-init` 读取内核参数（`clone.net_ip/gw/mask`），通过 `ioctl` 配置 `eth0`，写 DNS（8.8.8.8），添加默认路由 — 全自动，无需手动配置。

### SSH 登录优化

`clone-init` 自动设置 `UseDNS no` 到 `sshd_config`，消除 DNS 反查延迟。登录从数秒降到 ~0.1 秒。

### 稳定性修复

- `fallocate` 预分配替代稀疏文件 — 消除 `blk_mq_requeue_work` CPU 卡顿
- 内核参数 `blk_mq=0` 禁用有问题的块设备多队列
- `clone-init` 必须静态编译（`RUSTFLAGS="-C target-feature=+crt-static"`）

### MCP 集成

`sdk/clone-mcp/` 提供 Model Context Protocol 服务器，将 AI 代理（Claude Desktop、Cursor 等）桥接到 clone daemon 的 HTTP API。

### 构建脚本

- `build-alpine-image.sh` / `build-ubuntu-image.sh` / `build-ubuntu26-image.sh` — 构建 rootfs 镜像
- `merge-image.sh` — 将 overlay 合并回基础镜像
- `diagnose.sh` — VM 诊断工具

---

## 快速开始

```bash
# 构建
cargo build --release

# 一次性设置：免 sudo 运行
sudo setcap cap_net_admin,cap_net_raw+ep target/release/clone

# 创建 rootfs
sudo clone rootfs create --distro ubuntu --size 2G -o ubuntu.img

# 启动 VM（网络 + 持久化 overlay）
clone run --kernel vmlinuz --rootfs ubuntu.img --overlay --overlay-size 10G --net --mem-mb 1024

# 挂载宿主机目录
clone run --kernel vmlinuz --rootfs ubuntu.img --net \
  --shared-dir /host/path:tag:/guest/mount

# 保存和恢复 VM
clone save --vm-id $VM_ID --output ./snapshots/my-vm
clone restore --snapshot ./snapshots/my-vm --net

# 启动带 HTTP API 的 daemon
clone daemon --listen 127.0.0.1:8080 --auth-token secret123
```

### 前置条件

- Linux 主机，支持 KVM（`/dev/kvm`）
- 内核 6.5+ 推荐
- 网络需要：`/dev/net/tun`（vhost-net 可选）

---

## VM 生命周期命令

| 命令 | 功能 |
|------|------|
| `clone run` | 从 kernel + rootfs/initrd 启动新 VM |
| `clone fork` | 从模板快照 fork（<20ms） |
| `clone snapshot` | 快照运行中的 VM |
| `clone save` | 保存 VM 到磁盘，然后关机 |
| `clone restore` | 从快照恢复 VM，内存中的进程继续运行 |
| `clone attach` | 附加到 VM 的串口控制台 |
| `clone exec` | 在 VM 内执行命令 |
| `clone list` | 列出运行中的 VM |
| `clone migrate --live` | 预拷贝热迁移到另一台主机 |
| `clone migrate-recv` | 接收热迁移 |
| `clone rootfs create` | 创建可启动的 rootfs（Alpine、Ubuntu、Debian、Docker 导入） |
| `clone daemon` | 多 VM 编排 daemon，支持 HTTP REST API |

---

*以下为原项目文档。*

---

## 为什么做 Clone

**问题：** 传统共享 Shell 托管让所有用户登录同一个内核。资源消耗极低 — 空闲用户几乎不占资源。但到 2026 年，共享内核上的多租户已无法接受。容器逃逸已是常态，内核漏洞月月发布。共享内核上运行不可信用户已无安全保障。

VM 解决了安全问题 — KVM 提供硬件强制隔离。但 VM 启动慢，每个 VM 独占内存。像运行 100 个 Shell 用户那样运行 100 个 VM 成本过高。我们研究了所有现有 VMM — QEMU、Firecracker、Cloud Hypervisor — 没有一个能提供共享 Shell 级别的资源效率和 VM 级别的隔离。

所以我们构建了 Clone。

**Clone 的方案：** 从热模板 Shadow Clone fork。启动一个加载好一切的 VM，快照它，然后在 <20ms 内 fork 副本。所有 fork 共享相同的物理内存页，直到写入 — 只有脏页占用内存。100 个 fork 的 VM 内存消耗像 10 个一样。

| | Clone（实测） | Firecracker（官方） | Cloud Hypervisor（官方） | QEMU |
|---|--------|-------------|------------------|------|
| **代码量** | 25K Rust | ~50K Rust | ~70K Rust | ~2M+ C |
| **Fork（Alpine）** | **<20ms**（Shadow Clone） | ~5-10ms（snapshot） | stop+resume | stop+resume |
| **Fork（4GB Ubuntu+网络）** | **~160ms**（Shadow Clone） | N/A | N/A | N/A |
| **冷启动（发行版内核）** | **2,217ms** | ~2-3s | ~2s | 5-20s |
| **冷启动（精简内核）** | — | <=125ms ^1 | <100ms ^1 | 500ms-2s |
| **热迁移停机时间** | **1ms** | **无** | 有（未公布） | 50-300ms |
| **2x 4GB fork VM 宿主机内存** | **~1GB** | N/A | N/A | N/A |
| **3 个 fork VM RSS（Alpine）** | **13MB** | N/A | N/A | N/A |
| **10x 512MB 空闲 VM** | **~200MB** | ~5GB | 不定 | 不定 |
| **增量快照** | **192KB**（小 682 倍） | 仅全量 | 仅全量 | 全量+增量 |
| **GPU 直通** | **支持（VFIO）** | **不支持** | 支持 | 支持 |
| **宿主机目录共享** | **支持（无需守护进程）** | **不支持** | virtiofsd | virtiofsd |
| **Fork 网络** | **支持（用户态）** | 不支持 | 不支持 | 不支持 |
| **Fork vsock** | **支持（用户态）** | 支持（用户态） | 不支持 | 不支持 |

^1 使用精简定制内核。发行版内核：所有 VMM 约为 ~2-3s。

---

## 核心特性

### 内存管理

三层叠加，最小化宿主机内存占用：

1. **Overcommit** — `MAP_NORESERVE`，首次写入时才分配页
2. **KSM** — `MADV_MERGEABLE` 跨 VM 去重相同页
3. **Balloon** — 渐进回收（空闲 30s → 25%，2min → 50%，5min → 最低值）

结果：10 个空闲 512MB VM 只用 ~200MB 宿主机内存，而非 5GB。

### Shadow Clone Fork

```
启动模板 → 预热二进制 → 快照内存 + 寄存器
                                    ↓
          新 VM = mmap(snapshot, MAP_PRIVATE)  ← ~160ms
                                    ↓
          注入身份（hostname、CID、IP、MAC）
                                    ↓
          传输重置 → agent 重连 → exec 就绪
```

所有 fork 通过 Shadow Clone 映射共享相同物理页，直到写入。Fork 时无需内核启动。用户态 vsock 和 net 后端无需内核介入即可处理 fork 状态。每个 fork 获得独立 IP、hostname 和 vsock CID。

实测：2 个 fork 的 4GB Ubuntu VM 使用 **~1GB 宿主机内存**。3 个 fork 的 Alpine VM 使用 **13MB 总 RSS**。

### 热迁移

基于 TCP 的预拷贝。VM 持续运行，内存在后台传输。

```
源端                              目标端
  │ 发送全部内存（跳过零页）──→ │
  │ 发送脏页（第 1 轮）    ──→ │
  │ 发送脏页（第 2 轮）    ──→ │
  │ ...收敛...                   │
  │ 暂停 → 发送最终脏页 + CPU ─→│
  │         ~19ms 停机时间       │ 恢复
  │ 关闭                         │ 运行中
```

### 安全

- **KVM 硬件隔离** — 每个 VM 是独立地址空间
- **Seccomp 沙箱** — VMM 进程的 BPF 系统调用过滤（`--seccomp`）
- **度量启动** — 加载前 SHA-256 内核哈希校验
- **命名空间沙箱** — 可选完整沙箱（chroot + capabilities，`--jail`）

### 设备

- **virtio-block** — 原始和 qcow2 磁盘镜像，精简配置
- **virtio-net** — TAP + vhost-net（启动）或用户态（fork），自动 bridge/NAT
- **virtio-balloon** — 协作式内存回收
- **virtio-vsock** — 用户态 host-guest 通信后端（兼容 fork）
- **virtio-fs** — 内联 FUSE 宿主机目录共享（无需外部守护进程）
- **PCI 总线** — ECAM 配置空间，支持 VFIO 直通
- **串口控制台** — 16550A UART，双向终端 I/O

### Rootfs 模式

```bash
# 模式 1：自定义 initrd（全部在 RAM 中）
clone run --kernel vmlinuz --initrd my-initrd.img

# 模式 2：磁盘 rootfs（持久化，可读写）
clone run --kernel vmlinuz --rootfs disk.img

# 模式 3：共享基础 + overlay（多 VM，临时或持久）
clone run --kernel vmlinuz --rootfs base.img --overlay
clone run --kernel vmlinuz --rootfs base.img --overlay /data/vm1.qcow2
```

---

## 架构

```
src/
├── main.rs              CLI 入口
├── vmm/                 VM 生命周期、vCPU 线程、MMIO 总线
├── boot/                内核加载（bzImage/ELF）、ACPI 表、页表
├── memory/              Guest 内存、overcommit、KSM、页表、GDT
├── virtio/              Virtio 设备（block、net、balloon、vsock、fs）
├── pci/                 PCI 总线（ECAM）、VFIO 直通
├── migration/           预拷贝热迁移（发送端、接收端、协议）
├── control/             控制平面（每 VM socket + 多 VM 编排 daemon）
│   ├── http.rs          HTTP REST API（axum）
│   ├── cgroup.rs        cgroup v2 每 VM 内存限制
│   ├── prometheus_metrics.rs  Prometheus 指标
│   └── ...
├── net/                 TAP/bridge/NAT 自动配置
│   ├── netns.rs         网络命名空间隔离
│   └── ...
├── storage/             原始 + QCOW2 块后端
├── rootfs.rs            --rootfs 模式的自动 initrd（嵌入内核模块、agent）
└── rootfs_create.rs     `clone rootfs create`（Alpine、Ubuntu、Debian、Docker）

crates/
├── guest-agent/         Guest 内 vsock agent（exec、网络、D-Bus 恢复、心跳）
└── clone-init/          自动 initrd 的最小 init（模块加载、rootfs 挂载、agent 启动）

sdk/
└── clone-mcp/           AI agent 集成的 MCP 服务器（Python）

scripts/
└── make_initrd.sh       构建自定义 initrd
```

**依赖：** kvm-ioctls、kvm-bindings、vm-memory、libc、clap、anyhow、tracing、sha2、axum、prometheus。无 libvirt，无 QEMU，无 fork 代码库。

---

## 性能基准

所有数据在裸金属服务器上实测（OVH 独立服务器，Intel Xeon E-2386G，Ubuntu 24.04，内核 6.8.0-106-generic）。

| 指标 | 数值 |
|------|------|
| Shadow Clone fork（Alpine，精简） | **<20ms** |
| Shadow Clone fork（4GB Ubuntu，到 exec） | **~160ms** |
| VMM 开销（冷启动） | **35ms**（内存、irqchip、设备、内核加载） |
| 冷启动到 Shell（发行版内核） | **2,217ms**（最佳），**2,338ms**（5 次平均） |
| 热迁移停机时间（256MB） | **1ms** |
| 增量快照大小 | **192KB**（512MB VM，比全量小 682 倍） |
| Shadow Clone 共享（2x 4GB Ubuntu） | **~1GB 宿主机内存**（承诺 8GB） |
| Shadow Clone 共享（3x Alpine） | **13MB** vs 127MB 模板 |
| 二进制大小 | ~3MB |
| VMM 内存开销 | ~5-10MB |

---

## 测试结果

**63 个测试，62 通过，1 跳过。完整测试套件 ~315 秒。**

| 类别 | 结果 | 亮点 |
|------|------|------|
| 启动 & ACPI | 5 PASS | 冷启动 **2,218ms**，4-vCPU SMP |
| 控制平面 | 2 PASS | Socket 状态/暂停/恢复/关闭 |
| 存储 | 3 PASS | virtio-block、QCOW2、backing file overlay |
| 快照 & Fork | 4 PASS | 增量 **192KB**，Shadow Clone 共享 **19MB RSS** |
| 安全 | 1 PASS | Seccomp BPF |
| 设备 | 2 PASS, 1 SKIP | virtio-fs、PCI、VFIO（跳过：无硬件） |
| 迁移 | 1 PASS | **1ms 停机时间** |
| Rootfs 启动 | 2 PASS | Alpine 3.21、Ubuntu 24.04 |
| 网络 | 3 PASS | 唯一 CID、guest 网络、exec **796ms** |
| 多 VM & 内存 | 3 PASS | 并发 VM、overcommit、balloon |
| Shadow Clone | 1 PASS | 3 个 fork **18MB 总 RSS** |

---

## 许可证

MIT。Copyright (c) 2026 Unix Shells Limited Company.
