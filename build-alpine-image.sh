#!/bin/bash
set -e

# === 构建参数 ===
ALPINE_VER="3.21"
SIZE="2G"
OUTPUT="/home2/root1/clone/alpine-${ALPINE_VER}-sshd.img"
MIRROR="http://mirrors.aliyun.com/alpine/v${ALPINE_VER}"

echo "=== 构建 Alpine ${ALPINE_VER} 镜像 ==="

# 下载 minirootfs
TARBALL="alpine-minirootfs-${ALPINE_VER}.0-x86_64.tar.gz"
URL="${MIRROR}/releases/x86_64/minirootfs/${TARBALL}"

if [ ! -f "/tmp/${TARBALL}" ]; then
    echo "[1/5] 下载 Alpine minirootfs..."
    curl -fSL "$URL" -o "/tmp/${TARBALL}" || {
        # 尝试列出可用版本
        echo "下载失败，尝试查找最新版本..."
        LATEST=$(curl -sL "${MIRROR}/releases/x86_64/minirootfs/" | grep -oP 'alpine-minirootfs-\d+\.\d+\.0-x86_64\.tar\.gz' | tail -1)
        if [ -n "$LATEST" ]; then
            echo "找到: $LATEST"
            curl -fSL "${MIRROR}/releases/x86_64/minirootfs/${LATEST}" -o "/tmp/${TARBALL}"
            TARBALL="$LATEST"
        else
            echo "无法自动获取版本，请手动指定"
            exit 1
        fi
    }
fi

# 创建磁盘镜像
echo "[2/5] 创建 ${SIZE} 磁盘镜像..."
fallocate -l "$SIZE" "$OUTPUT"
mkfs.ext4 -F "$OUTPUT"

# 挂载并解压
ROOT="/tmp/clone-build-root"
umount "$ROOT" 2>/dev/null || true
rm -rf "$ROOT"
mkdir -p "$ROOT"
mount -o loop "$OUTPUT" "$ROOT"

echo "[3/5] 解压 rootfs..."
tar xzf "/tmp/${TARBALL}" -C "$ROOT"

# 配置
echo "[4/5] 配置系统..."
cp /etc/resolv.conf "$ROOT/etc/resolv.conf"

# 配置 APK 源
cat > "$ROOT/etc/apk/repositories" << EOF
${MIRROR}/main
${MIRROR}/community
EOF

# 设置 hostname
echo "clone" > "$ROOT/etc/hostname"

# fstab
cat > "$ROOT/etc/fstab" << 'EOF'
/dev/vda / ext4 errors=remount-ro 0 1
EOF

# 初始化 SSL 并安装软件（chroot）
mount --bind /proc "$ROOT/proc"
mount --bind /sys "$ROOT/sys"
mount --bind /dev "$ROOT/dev"

chroot "$ROOT" /bin/sh -c '
apk update
apk add openssh-server openssh-client python3 curl nodejs npm openrc bash
# 配置 SSH
sed -i "s/#PermitRootLogin.*/PermitRootLogin yes/" /etc/ssh/sshd_config
sed -i "s/#PasswordAuthentication.*/PasswordAuthentication yes/" /etc/ssh/sshd_config
echo "root:root" | chpasswd
# 生成 SSH host keys
ssh-keygen -A
# 启用 sshd
rc-update add sshd default
# 清理
rm -rf /var/cache/apk/*
'

umount "$ROOT/dev" 2>/dev/null || true
umount "$ROOT/sys" 2>/dev/null || true
umount "$ROOT/proc" 2>/dev/null || true

# 卸载
echo "[5/5] 完成"
umount "$ROOT"
rmdir "$ROOT"

echo ""
echo "=== 镜像已创建: $OUTPUT ($(numfmt --to=iec $(stat --format=%s "$OUTPUT"))) ==="
echo ""
echo "注意: Alpine 使用 musl libc，需要对应架构的内核模块"
