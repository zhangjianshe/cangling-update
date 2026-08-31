#!/bin/bash
# 下载 kylin-arm（麒麟 ARM，静态二进制）的 docker-ce 与 k9s 离线包。
# 需在可访问 download.docker.com / github 的机器上运行（可配 PROXY）。
# 说明：麒麟 V10 是 glibc 2.28（RHEL8 系），装不上 el9 的 docker-ce.rpm；
# 因此 docker 用官方 static/stable 静态二进制（musl），docker compose 用官方 release 二进制。
set -euo pipefail

SRC_ROOT="$(cd "$(dirname "$0")" && pwd)"
DEST="$SRC_ROOT/kylin-arm"
PROXY="${PROXY:-}"

DOCKER_VER="27.5.1"
COMPOSE_VER="v2.32.4"
K9S_VER="v0.51.0"

DOCKER_TGZ="https://download.docker.com/linux/static/stable/aarch64/docker-${DOCKER_VER}.tgz"
COMPOSE_BIN="https://github.com/docker/compose/releases/download/${COMPOSE_VER}/docker-compose-linux-aarch64"
K9S_BASE="https://github.com/derailed/k9s/releases/download/${K9S_VER}"

curl_args=(-fL --retry 3 --connect-timeout 15)
if [ -n "$PROXY" ]; then
  curl_args+=(--proxy "$PROXY")
fi

mkdir -p "$DEST/docker"
rm -f "$DEST/docker"/docker-*.tgz
echo "==> 下载 docker 静态二进制 $DOCKER_TGZ"
curl "${curl_args[@]}" -o "$DEST/docker/docker-${DOCKER_VER}.tgz" "$DOCKER_TGZ"

mkdir -p "$DEST/docker-compose"
rm -f "$DEST/docker-compose"/docker-compose-linux-*
echo "==> 下载 docker compose $COMPOSE_BIN"
curl "${curl_args[@]}" -o "$DEST/docker-compose/docker-compose-linux-aarch64" "$COMPOSE_BIN"
chmod +x "$DEST/docker-compose/docker-compose-linux-aarch64"

mkdir -p "$DEST/k9s"
rm -f "$DEST/k9s"/*.rpm
echo "==> 下载 k9s_linux_arm64.rpm"
curl "${curl_args[@]}" -o "$DEST/k9s/k9s_linux_arm64.rpm" "$K9S_BASE/k9s_linux_arm64.rpm"

echo "完成，kylin-arm 的 docker（静态）/ docker compose / k9s 离线包已就绪。"
