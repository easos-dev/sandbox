#!/usr/bin/env bash
# 把沙箱镜像打成 LXC（Proxmox）容器模板：在镜像上加 systemd（lxc/Dockerfile），导出根文件系统并用 zstd 压缩。
# 在装有 Docker 的机器上执行；Apple Silicon 上打 amd64 模板靠 Docker Desktop 的 amd64 模拟。
# 用法：scripts/build-lxc-template.sh <基础镜像> <输出文件.tar.zst> [平台，默认 linux/amd64]
# 例：  scripts/build-lxc-template.sh easit2/easos-sandbox:3060446 dist/easos-sandbox_3060446_amd64.tar.zst
set -euo pipefail
base="$1"
out="$2"
platform="${3:-linux/amd64}"
root="$(cd "$(dirname "$0")/.." && pwd)"
# 模板镜像的标签由基础镜像名派生，重复打同一基础镜像时直接覆盖。
tag="easos-sandbox-lxc:$(printf '%s' "$base" | tr '/:' '__')"
docker build --platform "$platform" -f "$root/lxc/Dockerfile" --build-arg "BASE=$base" -t "$tag" "$root"
cid="$(docker create --platform "$platform" "$tag")"
trap 'docker rm -f "$cid" >/dev/null' EXIT
mkdir -p "$(dirname "$out")"
# docker export 输出的就是顶层根文件系统 tar，正是 pct create 要的形状；镜像近 6 GB，压缩要几分钟。
docker export "$cid" | zstd -T0 -10 -f -o "$out"
ls -l "$out"
