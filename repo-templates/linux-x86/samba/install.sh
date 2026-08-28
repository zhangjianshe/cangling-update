#!/bin/bash
## 离线安装 samba（smbd / smbpasswd）。
## 本包目录可放置 samba 相关 .deb，无 .deb 时回退到联网 apt。
set -euo pipefail

if command -v smbd >/dev/null 2>&1; then
  echo "samba 已安装：$(smbd --version 2>/dev/null || echo ok)"
  exit 0
fi

if ls ./*.deb >/dev/null 2>&1; then
  echo "==> 离线安装本地 .deb"
  dpkg -i ./*.deb || apt-get -f install -y
else
  echo "!! 本包内没有 .deb，回退到联网 apt 安装 samba"
  apt-get update
  DEBIAN_FRONTEND=noninteractive apt-get install -y samba
fi

systemctl enable --now smbd 2>/dev/null || true
echo "samba 安装完成：$(smbd --version 2>/dev/null || dpkg -s samba 2>/dev/null | grep -i '^Version' || echo ok)"
