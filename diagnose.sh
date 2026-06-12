#!/bin/bash
set -e

echo "=========================================="
echo "Clone VMM 诊断脚本"
echo "=========================================="

# 检查二进制文件
echo "[1/5] 检查二进制文件..."
if [ ! -f "./target/release/clone" ]; then
    echo "❌ clone 二进制不存在"
    exit 1
fi
if [ ! -f "./target/release/clone-init" ]; then
    echo "❌ clone-init 二进制不存在"
    exit 1
fi
echo "✅ 二进制文件存在"

# 检查镜像文件
echo "[2/5] 检查镜像文件..."
if [ ! -f "ubuntu.img" ]; then
    echo "❌ ubuntu.img 不存在"
    exit 1
fi
echo "✅ ubuntu.img 存在 ($(du -h ubuntu.img | cut -f1))"

# 检查内核
echo "[3/5] 检查内核..."
KERNEL="/boot/vmlinuz-$(uname -r)"
if [ ! -f "$KERNEL" ]; then
    echo "❌ 内核不存在: $KERNEL"
    exit 1
fi
echo "✅ 内核存在: $KERNEL"

# 检查镜像内的 init
echo "[4/5] 检查镜像内的 init..."
MOUNT_POINT="/tmp/clone-diagnose-$$"
mkdir -p "$MOUNT_POINT"
if sudo mount -o loop ubuntu.img "$MOUNT_POINT" 2>/dev/null; then
    if [ -f "$MOUNT_POINT/sbin/init" ]; then
        echo "✅ /sbin/init 存在 ($(ls -lh "$MOUNT_POINT/sbin/init" | awk '{print $5}'))"
        ls -lh "$MOUNT_POINT/sbin/init" "$MOUNT_POINT/lib/systemd/systemd" 2>/dev/null || true
    else
        echo "❌ /sbin/init 不存在"
        ls -lh "$MOUNT_POINT/sbin/" 2>/dev/null || echo "  /sbin/ 目录不存在"
    fi
    sudo umount "$MOUNT_POINT" 2>/dev/null || true
else
    echo "⚠️  无法挂载镜像"
fi
rmdir "$MOUNT_POINT" 2>/dev/null || true

# 测试 initrd 生成
echo "[5/5] 测试 initrd 生成..."
TEST_OUTPUT=$(./target/release/clone run --kernel "$KERNEL" --rootfs ubuntu.img --mem-mb 512 2>&1 | head -20)
if echo "$TEST_OUTPUT" | grep -q "Using init binary"; then
    echo "✅ initrd 生成逻辑被触发"
    echo "$TEST_OUTPUT" | grep -E "(Using init|Generated initrd|Rootfs mode)"
else
    echo "❌ initrd 生成逻辑未被触发"
    echo "输出："
    echo "$TEST_OUTPUT"
fi

echo "=========================================="
echo "诊断完成"
echo "=========================================="
