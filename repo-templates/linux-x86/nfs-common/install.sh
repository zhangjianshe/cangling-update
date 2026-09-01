#!/bin/bash
## 离线安装 nfs-common（mount.nfs，用于挂载 NFS 共享）。
## 本包目录可放置 nfs-common 相关 .deb，无 .deb 时回退到联网 apt。
set -euo pipefail

if [ -x /sbin/mount.nfs ] || command -v mount.nfs >/dev/null 2>&1; then
  echo "nfs-common 已安装"
  exit 0
fi

if ls ./*.deb >/dev/null 2>&1; then
  echo "==> 离线安装本地 .deb"
  dpkg -i ./*.deb || apt-get -f install -y
else
  echo "!! 本包内没有 .deb，回退到联网 apt 安装 nfs-common"
  apt-get update
  DEBIAN_FRONTEND=noninteractive apt-get install -y nfs-common
fi

systemctl enable --now rpcbind 2>/dev/null || true
echo "nfs-common 安装完成"
