#!/bin/bash
## 离线安装 git（麒麟 ARM，RPM）
set -euo pipefail

if command -v git >/dev/null 2>&1; then
  echo "git 已安装：$(git --version)"
  exit 0
fi

if ls ./*.rpm >/dev/null 2>&1; then
  echo "==> 离线安装本地 .rpm"
  dnf install -y --nogpgcheck ./*.rpm || rpm -Uvh --replacepkgs ./*.rpm
else
  echo "!! 本包内没有 .rpm，回退到联网 dnf 安装"
  dnf install -y git
fi

echo "git 安装完成：$(git --version)"
