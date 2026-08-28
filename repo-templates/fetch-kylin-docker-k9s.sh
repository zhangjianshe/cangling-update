#!/bin/bash
# 下载 kylin-arm（麒麟 ARM，RPM）的 docker-ce 与 k9s 离线 .rpm。
# 需在可访问 download.docker.com / github 的机器上运行（可配 PROXY）。
set -euo pipefail

SRC_ROOT="$(cd "$(dirname "$0")" && pwd)"
DEST="$SRC_ROOT/kylin-arm"
PROXY="${PROXY:-}"

DOCKER_VER="29.7.2"
CONTAINERD_VER="2.3.3"
BUILDX_VER="0.36.1"
COMPOSE_VER="5.5.0"
K9S_VER="v0.51.0"

DOCKER_BASE="https://download.docker.com/linux/centos/9/aarch64/stable/Packages"
K9S_BASE="https://github.com/derailed/k9s/releases/download/${K9S_VER}"

curl_args=(-fL --retry 3 --connect-timeout 15)
if [ -n "$PROXY" ]; then
  curl_args+=(--proxy "$PROXY")
fi

mkdir -p "$DEST/docker"
rm -f "$DEST/docker"/*.rpm
for f in \
  "docker-ce-${DOCKER_VER}-1.el9.aarch64.rpm" \
  "docker-ce-cli-${DOCKER_VER}-1.el9.aarch64.rpm" \
  "containerd.io-${CONTAINERD_VER}-1.el9.aarch64.rpm" \
  "docker-buildx-plugin-${BUILDX_VER}-1.el9.aarch64.rpm" \
  "docker-compose-plugin-${COMPOSE_VER}-1.el9.aarch64.rpm"; do
  echo "==> 下载 $f"
  curl "${curl_args[@]}" -o "$DEST/docker/$f" "$DOCKER_BASE/$f"
done

mkdir -p "$DEST/k9s"
rm -f "$DEST/k9s"/*.rpm
echo "==> 下载 k9s_linux_arm64.rpm"
curl "${curl_args[@]}" -o "$DEST/k9s/k9s_linux_arm64.rpm" "$K9S_BASE/k9s_linux_arm64.rpm"

echo "完成，kylin-arm 的 docker-ce / k9s 离线 .rpm 已就绪。"
