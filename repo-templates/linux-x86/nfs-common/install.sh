#!/bin/bash
## 离线安装 NFS 客户端与服务端（mount.nfs + exportfs）。
## 本包目录可放置 nfs-common/nfs-kernel-server 相关 .deb，无 .deb 时回退到联网 apt。
set -euo pipefail

has_mount_nfs() { [ -x /sbin/mount.nfs ] || command -v mount.nfs >/dev/null 2>&1; }
has_exportfs() { [ -x /usr/sbin/exportfs ] || [ -x /sbin/exportfs ] || command -v exportfs >/dev/null 2>&1; }

if has_mount_nfs && has_exportfs; then
  echo "NFS 客户端与服务端已安装"
  exit 0
fi

if ls ./*.deb >/dev/null 2>&1; then
  echo "==> 离线安装本地 .deb"
  dpkg -i ./*.deb || apt-get -f install -y
  if ! has_exportfs; then
    echo "!! 当前离线包未包含 nfs-kernel-server，尝试从 apt 软件源补装"
    if ! apt-get update || ! DEBIAN_FRONTEND=noninteractive apt-get install -y nfs-kernel-server; then
      echo "安装 nfs-kernel-server 失败；完全离线环境请重新运行 fetch-apt.sh，并同步更新后的 nfs-common 软件目录" >&2
      exit 1
    fi
  fi
else
  echo "!! 本包内没有 .deb，回退到联网 apt 安装 NFS 客户端与服务端"
  apt-get update
  DEBIAN_FRONTEND=noninteractive apt-get install -y nfs-common nfs-kernel-server
fi

systemctl enable --now rpcbind 2>/dev/null || true
systemctl enable --now nfs-kernel-server 2>/dev/null || systemctl enable --now nfs-server 2>/dev/null || true

if ! has_mount_nfs || ! has_exportfs; then
  echo "NFS 安装不完整：需要 mount.nfs 和 exportfs" >&2
  exit 1
fi
echo "NFS 客户端与服务端安装完成"
