#!/bin/bash
set -e

echo "=== 构建 Ubuntu 26.04 镜像 (debootstrap) ==="

CODENAME="resolute"
SIZE="8G"
OUTPUT="/home2/root1/clone/ubuntu-26-sshd.img"
# 优先用官方源，阿里云可能还没同步
MIRROR="http://archive.ubuntu.com/ubuntu"
ROOT="/tmp/clone-build-root"

which debootstrap >/dev/null 2>&1 || { echo "需要 debootstrap: sudo apt install debootstrap"; exit 1; }

# 清理
umount "$ROOT/proc" "$ROOT/sys" "$ROOT/dev" "$ROOT" 2>/dev/null || true
losetup -D 2>/dev/null || true
rm -rf "$ROOT"
mkdir -p "$ROOT"

# 创建磁盘镜像
echo "[1/6] 创建 ${SIZE} 磁盘镜像..."
rm -f "$OUTPUT"
fallocate -l "$SIZE" "$OUTPUT"
mkfs.ext4 -F "$OUTPUT"
mount -o loop "$OUTPUT" "$ROOT"

# debootstrap
echo "[2/6] debootstrap $CODENAME ..."
debootstrap --no-check-gpg --arch=amd64 "$CODENAME" "$ROOT" "$MIRROR"

# 验证
if [ ! -x "$ROOT/bin/sh" ]; then
    echo "错误: debootstrap 失败，rootfs 不完整"
    umount "$ROOT"
    exit 1
fi

# 配置
echo "[3/6] 配置系统..."
cat > "$ROOT/etc/apt/sources.list" << EOF
deb $MIRROR ${CODENAME} main restricted universe multiverse
deb $MIRROR ${CODENAME}-updates main restricted universe multiverse
deb $MIRROR ${CODENAME}-security main restricted universe multiverse
EOF

cp /etc/resolv.conf "$ROOT/etc/resolv.conf"
echo "clone" > "$ROOT/etc/hostname"
cat > "$ROOT/etc/fstab" << 'EOF'
/dev/vda / ext4 errors=remount-ro 0 1
EOF

# 安装软件
echo "[4/6] 安装软件..."
mount --bind /proc "$ROOT/proc"
mount --bind /sys "$ROOT/sys"
mount --bind /dev "$ROOT/dev"

chroot "$ROOT" /bin/sh -c '
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y openssh-server openssh-client python3 curl systemd-sysv dbus
sed -i "s/#PermitRootLogin.*/PermitRootLogin yes/" /etc/ssh/sshd_config
sed -i "s/PermitRootLogin prohibit-password/PermitRootLogin yes/" /etc/ssh/sshd_config
sed -i "s/#PasswordAuthentication.*/PasswordAuthentication yes/" /etc/ssh/sshd_config
# Disable SSH DNS reverse lookup
echo "UseDNS no" >> /etc/ssh/sshd_config
# Force apt IPv4 (VM has no IPv6 outbound)
echo "Acquire::ForceIPv4 \"true\";" > /etc/apt/apt.conf.d/99force-ipv4
# Disable systemd-resolved to prevent it overwriting /etc/resolv.conf
systemctl disable systemd-resolved 2>/dev/null || true
rm -f /etc/resolv.conf
echo "nameserver 223.5.5.5" > /etc/resolv.conf
echo "nameserver 8.8.8.8" >> /etc/resolv.conf
chattr +i /etc/resolv.conf 2>/dev/null || true
echo "root:root" | chpasswd
apt-get clean
'

# 提取内核（如果有的话）
echo "[5/6] 提取内核..."
KVER=$(ls "$ROOT/boot/vmlinuz-"* 2>/dev/null | grep -oP '\d+\.\d+\.\d+-\d+-generic' | head -1 || true)

if [ -z "$KVER" ]; then
    echo "cloud 镜像不带内核，安装内核..."
    chroot "$ROOT" /bin/sh -c 'DEBIAN_FRONTEND=noninteractive apt-get install -y linux-image-generic'
    KVER=$(ls "$ROOT/boot/vmlinuz-"* 2>/dev/null | grep -oP '\d+\.\d+\.\d+-\d+-generic' | head -1)
fi

if [ -n "$KVER" ]; then
    cp "$ROOT/boot/vmlinuz-${KVER}" "/home2/root1/clone/vmlinuz-26"
    echo "内核: $KVER"
fi

umount "$ROOT/dev" 2>/dev/null || true
umount "$ROOT/sys" 2>/dev/null || true
umount "$ROOT/proc" 2>/dev/null || true
umount "$ROOT"
rmdir "$ROOT" 2>/dev/null || true

echo "[6/6] 完成"
echo ""
echo "=== 镜像: $OUTPUT ($(numfmt --to=iec $(stat --format=%s "$OUTPUT"))) ==="
if [ -n "$KVER" ]; then
    echo "=== 内核: vmlinuz-26 ($KVER) ==="
    echo ""
    echo "安装内核模块:"
    echo "  sudo apt install linux-modules-${KVER}"
    echo ""
    echo "启动:"
    echo "  CLONE_KERNEL_RELEASE=${KVER} ./target/release/clone run \\"
    echo "    --kernel ./vmlinuz-26 --rootfs $OUTPUT --net --mem-mb 1024"
fi
