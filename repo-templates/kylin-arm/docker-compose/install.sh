#!/bin/bash
## 离线安装 docker compose（静态二进制 docker-compose-linux-aarch64）。
## 同时安装为 CLI 插件（`docker compose`）与独立命令（`docker-compose`）。
set -euo pipefail

if docker compose version >/dev/null 2>&1; then
  echo "docker compose 已安装：$(docker compose version | head -1)"
  exit 0
fi

BIN="$(ls docker-compose-linux-* 2>/dev/null | head -1 || true)"
if [ -z "$BIN" ]; then
  echo "!! 本包内没有 docker-compose-linux-* 二进制，回退到联网安装"
  dnf install -y docker-compose-plugin || true
  exit 0
fi

if [ "$(id -u)" -ne 0 ]; then
  echo "请用 root 执行：bash $0" >&2
  exit 1
fi

echo "==> 安装 docker compose 插件（$BIN）"
mkdir -p /usr/libexec/docker/cli-plugins /usr/lib/docker/cli-plugins
install -m 0755 "$BIN" /usr/libexec/docker/cli-plugins/docker-compose
install -m 0755 "$BIN" /usr/lib/docker/cli-plugins/docker-compose
install -m 0755 "$BIN" /usr/bin/docker-compose

echo "docker compose 安装完成：$(docker compose version 2>/dev/null | head -1 || echo ok)"
