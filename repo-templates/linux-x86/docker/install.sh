#!/bin/bash
## 离线安装 docker + containerd + docker compose。
## 方式一（推荐）：本包目录放置 docker-ce、docker-ce-cli、containerd.io、docker-buildx-plugin、
##   docker-compose-plugin 的 .deb，脚本离线 dpkg 安装。
## 方式二：本包目录放置 `docker/docker`（静态二进制），脚本拷贝到 /usr/bin/docker（无守护进程）。
set -euo pipefail

if command -v docker >/dev/null 2>&1; then
  echo "docker 已安装：$(docker --version)"
else
  if ls ./docker-*.deb >/dev/null 2>&1; then
    echo "==> 离线安装 docker .deb"
    dpkg -i ./*.deb || apt-get -f install -y
  elif [ -f docker/docker ]; then
    echo "==> 安装静态 docker 二进制"
    install -m 0755 docker/docker /usr/bin/docker
  else
    echo "!! 本包内没有 .deb 也没有 docker/docker，回退到联网安装（get.docker.com）"
    curl -fsSL https://get.docker.com | sh
  fi
fi

systemctl enable --now docker 2>/dev/null || service docker start 2>/dev/null || true

echo "docker 安装完成：$(docker --version 2>/dev/null || echo ok)"
echo "compose：$(docker compose version 2>/dev/null || echo 未安装)"
