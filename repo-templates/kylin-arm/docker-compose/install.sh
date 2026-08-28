#!/bin/bash
## 离线安装 docker compose（docker-compose-plugin，提供 `docker compose` 命令）
set -euo pipefail

if docker compose version >/dev/null 2>&1; then
  echo "docker compose 已安装：$(docker compose version | head -1)"
  exit 0
fi

if ls ./*.rpm >/dev/null 2>&1; then
  echo "==> 离线安装本地 .rpm"
  dnf install -y --nogpgcheck ./*.rpm || rpm -Uvh --replacepkgs ./*.rpm
else
  echo "!! 本包内没有 .rpm，回退到联网 dnf 安装"
  dnf install -y docker-compose-plugin
fi

echo "docker compose 安装完成：$(docker compose version 2>/dev/null | head -1 || echo ok)"
