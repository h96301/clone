#!/bin/bash
set -e

BASE="/home2/root1/clone/ubuntu-22.img"
OVERLAY="/home2/root1/clone/overlay-172.30.0.2.img"
OUTPUT="/home2/root1/clone/ubuntu-22-sshd.img"
MERGE_DIR="/tmp/clone-merge"

echo "=== 合并 base + overlay 为独立镜像 ==="

# 清理
umount "$MERGE_DIR/overlay" 2>/dev/null || true
umount "$MERGE_DIR/base" 2>/dev/null || true
umount "$MERGE_DIR/output" 2>/dev/null || true
rm -rf "$MERGE_DIR"
mkdir -p "$MERGE_DIR"/{base,overlay,output}

# 挂载 base 和 overlay
mount -o loop,ro "$BASE" "$MERGE_DIR/base"
mount -o loop "$OVERLAY" "$MERGE_DIR/overlay"

# 查看 overlay 内容结构
echo "--- overlay 根目录 ---"
ls "$MERGE_DIR/overlay/"

# 创建新镜像
BASE_SIZE=$(stat --format=%s "$BASE")
NEW_SIZE=$((BASE_SIZE + 2 * 1024 * 1024 * 1024))
echo "创建新镜像: $(numfmt --to=iec $NEW_SIZE)"
fallocate -l "$NEW_SIZE" "$OUTPUT"
mkfs.ext4 -F "$OUTPUT"

# 挂载新镜像
mount -o loop "$OUTPUT" "$MERGE_DIR/output"

# 先复制 base
echo "复制 base rootfs..."
cp -a "$MERGE_DIR/base"/. "$MERGE_DIR/output/"

# 判断 overlay 结构并合并
if [ -d "$MERGE_DIR/overlay/upper" ]; then
    echo "overlay 含 upper/ 目录，从 upper 合并..."
    cp -a "$MERGE_DIR/overlay/upper"/. "$MERGE_DIR/output/"
else
    echo "overlay 为普通文件系统，直接合并..."
    cp -a "$MERGE_DIR/overlay"/. "$MERGE_DIR/output/"
fi

# 清理
umount "$MERGE_DIR/output"
umount "$MERGE_DIR/overlay"
umount "$MERGE_DIR/base"
rmdir "$MERGE_DIR"/{base,overlay,output}
rmdir "$MERGE_DIR" 2>/dev/null || true

echo ""
echo "=== 完成 ==="
echo "新镜像: $OUTPUT ($(numfmt --to=iec $(stat --format=%s "$OUTPUT")))"
echo ""
echo "使用方式:"
echo "  CLONE_KERNEL_RELEASE=6.5.0-44-generic ./target/release/clone run \\"
echo "    --kernel ./vmlinuz --rootfs $OUTPUT \\"
echo "    --overlay --net --mem-mb 1024"
