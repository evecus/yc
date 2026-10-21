#!/bin/sh
# build-ebpf.sh — 构建 uperf 的内核态 eBPF 目标文件（uperf-ebpf）
#
# 产物：uperf-ebpf/target/bpfel-unknown-none/release/uperf-ebpf
#       （拷贝到 module/bin/uperf-ebpf，uperf 启动时从同目录读取加载）
#
# 前置条件（一次性安装）：
#   1. nightly 工具链 + rust-src 组件：
#        rustup toolchain install nightly --profile minimal --component rust-src
#      （国内网络可设 RUSTUP_DIST_SERVER=https://rsproxy.cn 加速）
#   2. bpf-linker（BPF ELF 专用链接器，不能装的话见下）：
#        cargo install bpf-linker
#      注意：cargo install 需要匹配版本的 LLVM 开发库（llvm-sys 依赖），
#      装不上时直接下载官方预编译二进制：
#        curl -sL <ghproxy>/https://github.com/aya-rs/bpf-linker/releases/download/<ver>/bpf-linker-x86_64-unknown-linux-musl.tar.zst -o /tmp/bl.tar.zst
#        tar --zstd -xf /tmp/bl.tar.zst
#        install -m755 bpf-linker ~/.cargo/bin/bpf-linker
#      注意：bpf-linker >= 0.12 拒绝 -Os/-Oz，本 crate 的 opt-level 已固定为 2。
#
# 用法：
#   ./build-ebpf.sh          # 构建并拷入 module/bin/
#   ./build-ebpf.sh --no-cp  # 只构建不拷贝

set -e
cd "$(dirname "$0")"

echo "[build-ebpf] building uperf-ebpf for bpfel-unknown-none ..."
cd uperf-ebpf
cargo +nightly build --target bpfel-unknown-none -Z build-std=core --release
cd ..

SRC="uperf-ebpf/target/bpfel-unknown-none/release/uperf-ebpf"
[ -f "$SRC" ] || { echo "[build-ebpf] ERROR: $SRC not produced"; exit 1; }

if [ "$1" != "--no-cp" ]; then
    mkdir -p module/bin
    cp -f "$SRC" module/bin/uperf-ebpf
    echo "[build-ebpf] copied → module/bin/uperf-ebpf ($(stat -c%s "$SRC") bytes)"
fi

echo "[build-ebpf] done."
echo "[build-ebpf] 提示：用户态二进制仍需 NDK 交叉编译（见报告第五节），"
echo "[build-ebpf]       两者齐了才能打出可刷入的 zip。"
