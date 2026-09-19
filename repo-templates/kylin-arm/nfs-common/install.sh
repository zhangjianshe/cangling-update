#!/bin/bash
## 离线安装 NFS 客户端与服务端（麒麟 ARM，RPM，包名 nfs-utils）
set -euo pipefail

has_mount_nfs() { [ -x /sbin/mount.nfs ] || command -v mount.nfs >/dev/null 2>&1; }
has_exportfs() { [ -x /usr/sbin/exportfs ] || [ -x /sbin/exportfs ] || command -v exportfs >/dev/null 2>&1; }

if has_mount_nfs && has_exportfs; then
  echo "NFS 客户端与服务端已安装"
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
systemctl enable --now nfs-server 2>/dev/null || systemctl enable --now nfs-kernel-server 2>/dev/null || true

if ! has_mount_nfs || ! has_exportfs; then
  echo "NFS 安装不完整：需要 mount.nfs 和 exportfs" >&2
  exit 1
fi
echo "NFS 客户端与服务端安装完成"
