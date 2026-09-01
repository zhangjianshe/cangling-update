#!/bin/bash
## 离线安装 cifs-utils（mount.cifs，用于挂载 CIFS/SMB 共享）。
## 本包目录可放置 cifs-utils 相关 .deb，无 .deb 时回退到联网 apt。
set -euo pipefail

if command -v mount.cifs >/dev/null 2>&1 || [ -x /sbin/mount.cifs ]; then
  echo "cifs-utils 已安装：$(mount.cifs -V 2>/dev/null | head -n1 || echo ok)"
  exit 0
fi

if ls ./*.deb >/dev/null 2>&1; then
  echo "==> 离线安装本地 .deb"
  dpkg -i ./*.deb || apt-get -f install -y
else
  echo "!! 本包内没有 .deb，回退到联网 apt 安装 cifs-utils"
  apt-get update
  DEBIAN_FRONTEND=noninteractive apt-get install -y cifs-utils
fi

echo "cifs-utils 安装完成：$(mount.cifs -V 2>/dev/null | head -n1 || ls -l /sbin/mount.cifs)"
