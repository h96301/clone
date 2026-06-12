#!/bin/bash
set -e

# === 构建参数 ===
CODENAME="resolute"  # Ubuntu 26.04 LTS "Resolute Raccoon"
SIZE="8G"
OUTPUT="/home2/root1/clone/ubuntu-26-sshd.img"
MIRROR="http://mirrors.aliyun.com/ubuntu"

echo "=== 构建 Ubuntu ($CODENAME) 镜像 ==="

# 检查工具
which debootstrap >/dev/null 2>&1 || { echo "需要 debootstrap: sudo apt install debootstrap"; exit 1; }

# 创建磁盘镜像
echo "[1/5] 创建 ${SIZE} 磁盘镜像..."
fallocate -l "$SIZE" "$OUTPUT"
mkfs.ext4 -F "$OUTPUT"

# 挂载
ROOT="/tmp/clone-build-root"
umount "$ROOT" 2>/dev/null || true
rm -rf "$ROOT"
mkdir -p "$ROOT"
mount -o loop "$OUTPUT" "$ROOT"

# debootstrap
echo "[2/5] debootstrap $CODENAME ..."
debootstrap --no-check-gpg --arch=amd64 "$CODENAME" "$ROOT" "$MIRROR"

# 配置基础系统
echo "[3/5] 配置系统..."
cat > "$ROOT/etc/apt/sources.list" << EOF
deb $MIRROR ${CODENAME} main restricted universe multiverse
deb $MIRROR ${CODENAME}-updates main restricted universe multiverse
deb $MIRROR ${CODENAME}-security main restricted universe multiverse
EOF

# 设置 DNS
cp /etc/resolv.conf "$ROOT/etc/resolv.conf"

# 配置 fstab
cat > "$ROOT/etc/fstab" << 'EOF'
/dev/vda / ext4 errors=remount-ro 0 1
EOF

# 设置 hostname
echo "clone" > "$ROOT/etc/hostname"

# 安装软件（chroot）
echo "[4/5] 安装软件..."
mount --bind /proc "$ROOT/proc"
mount --bind /sys "$ROOT/sys"
mount --bind /dev "$ROOT/dev"

chroot "$ROOT" bash -c '
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y openssh-server openssh-client python3 curl systemd-sysv dbus
sed -i "s/#PermitRootLogin.*/PermitRootLogin yes/" /etc/ssh/sshd_config
sed -i "s/PermitRootLogin prohibit-password/PermitRootLogin yes/" /etc/ssh/sshd_config
sed -i "s/#PasswordAuthentication.*/PasswordAuthentication yes/" /etc/ssh/sshd_config
echo "root:root" | chpasswd
apt-get clean
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
echo "使用方式:"
echo "  CLONE_KERNEL_RELEASE=\$(strings vmlinuz | grep -oP '\\d+\\.\\d+\\.\\d+-\\d+-generic' | head -1) \\"
echo "  ./target/release/clone run --kernel ./vmlinuz --rootfs $OUTPUT --net --mem-mb 1024"
