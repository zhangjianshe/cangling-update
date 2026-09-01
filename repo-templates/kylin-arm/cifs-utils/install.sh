#!/bin/bash
## 离线安装 cifs-utils（麒麟 ARM，RPM）
set -euo pipefail

if command -v mount.cifs >/dev/null 2>&1 || [ -x /sbin/mount.cifs ]; then
  echo "cifs-utils 已安装：$(mount.cifs -V 2>/dev/null | head -n1 || echo ok)"
  exit 0
fi

if ls ./*.rpm >/dev/null 2>&1; then
  echo "==> 离线安装本地 .rpm"
  dnf install -y --nogpgcheck ./*.rpm || rpm -Uvh --replacepkgs ./*.rpm
else
  echo "!! 本包内没有 .rpm，回退到联网 dnf 安装"
  dnf install -y cifs-utils
fi

echo "cifs-utils 安装完成：$(mount.cifs -V 2>/dev/null | head -n1 || ls -l /sbin/mount.cifs)"
