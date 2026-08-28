#!/bin/bash
## 离线安装 docker-ce（麒麟 ARM，RPM：docker-ce/docker-ce-cli/containerd.io 等）
set -euo pipefail

if command -v docker >/dev/null 2>&1; then
  echo "docker 已安装：$(docker --version)"
else
  if ls ./*.rpm >/dev/null 2>&1; then
    echo "==> 离线安装本地 .rpm"
    dnf install -y --nogpgcheck ./*.rpm || rpm -Uvh --replacepkgs ./*.rpm
  else
    echo "!! 本包内没有 .rpm，回退到联网安装"
    dnf install -y docker-ce || dnf install -y docker || true
  fi
fi

systemctl enable --now docker 2>/dev/null || service docker start 2>/dev/null || true
echo "docker 安装完成：$(docker --version 2>/dev/null || echo ok)"
