#!/bin/bash
## 离线安装 samba（麒麟 ARM，RPM）
set -euo pipefail

if command -v smbd >/dev/null 2>&1; then
  echo "samba 已安装：$(smbd --version 2>/dev/null || echo ok)"
  exit 0
fi

if ls ./*.rpm >/dev/null 2>&1; then
  echo "==> 离线安装本地 .rpm"
  dnf install -y --nogpgcheck ./*.rpm || rpm -Uvh --replacepkgs ./*.rpm
else
  echo "!! 本包内没有 .rpm，回退到联网 dnf 安装"
  dnf install -y samba
fi

systemctl enable --now smb 2>/dev/null || systemctl enable --now smbd 2>/dev/null || true
echo "samba 安装完成：$(smbd --version 2>/dev/null || echo ok)"
