#!/bin/bash
set -e

MOUNT_POINT="/tmp/ubuntu-mount"
IMAGE="$1"

if [ -z "$IMAGE" ]; then
    echo "Usage: $0 <image-file>"
    exit 1
fi

if [ ! -f "$IMAGE" ]; then
    echo "Error: Image file not found: $IMAGE"
    exit 1
fi

echo "=========================================="
echo "Installing systemd into Ubuntu image"
echo "Image: $IMAGE"
echo "=========================================="

echo "[1/5] Mounting $IMAGE..."
sudo mkdir -p "$MOUNT_POINT"
sudo mount -o loop "$IMAGE" "$MOUNT_POINT"

echo "[2/5] Mounting pseudo-filesystems..."
sudo mount -t proc /proc "$MOUNT_POINT/proc" 2>/dev/null || true
sudo mount -t sysfs /sys "$MOUNT_POINT/sys" 2>/dev/null || true
sudo mount -o bind /dev "$MOUNT_POINT/dev"

echo "[3/5] Installing systemd and essential packages..."
sudo chroot "$MOUNT_POINT" /bin/bash -c "
    export DEBIAN_FRONTEND=noninteractive
    apt-get update
    apt-get install -y \
        systemd \
        systemd-sysv \
        ubuntu-minimal \
        netplan.io \
        curl \
        wget \
        vim \
        sudo \
        passwd \
        net-tools \
        iputils-ping
"

echo "[4/5] Cleaning up..."
sudo umount "$MOUNT_POINT/dev" 2>/dev/null || true
sudo umount "$MOUNT_POINT/proc" 2>/dev/null || true
sudo umount "$MOUNT_POINT/sys" 2>/dev/null || true
sudo umount "$MOUNT_POINT"
sudo rmdir "$MOUNT_POINT"

echo "[5/5] Done!"
echo "=========================================="
echo "systemd installed successfully in $IMAGE"
echo "You can now boot with:"
echo "  sudo clone run --kernel /boot/vmlinuz-\$(uname -r) --rootfs $IMAGE --net --mem-mb 2048"
echo "=========================================="
