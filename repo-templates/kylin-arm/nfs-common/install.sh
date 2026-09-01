#!/bin/bash
## 离线安装 nfs 客户端（麒麟 ARM，RPM，包名 nfs-utils）
set -euo pipefail

if [ -x /sbin/mount.nfs ] || command -v mount.nfs >/dev/null 2>&1; then
  echo "nfs 客户端已安装"
  exit 0
fi

if ls ./*.rpm >/dev/null 2>&1; then
  echo "==> 离线安装本地 .rpm"
  dnf install -y --nogpgcheck ./*.rpm || rpm -Uvh --replacepkgs ./*.rpm
else
  echo "!! 本包内没有 .rpm，回退到联网 dnf 安装"
  dnf install -y nfs-utils
fi

systemctl enable --now rpcbind 2>/dev/null || true
echo "nfs 客户端安装完成"
