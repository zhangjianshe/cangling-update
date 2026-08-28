#!/bin/bash
## 离线安装 k9s（麒麟 ARM，RPM 或单二进制）
set -euo pipefail

if command -v k9s >/dev/null 2>&1; then
  echo "k9s 已安装：$(k9s version --short 2>/dev/null || k9s version 2>/dev/null | head -1)"
  exit 0
fi

if ls ./*.rpm >/dev/null 2>&1; then
  echo "==> 离线安装本地 .rpm"
  dnf install -y --nogpgcheck ./*.rpm || rpm -Uvh --replacepkgs ./*.rpm
else
  ARCH="$(uname -m)"
  case "$ARCH" in
    x86_64) SRC="k9s_linux_amd64" ;;
    aarch64) SRC="k9s_linux_arm64" ;;
    *) echo "不支持的架构：$ARCH" >&2; exit 1 ;;
  esac
  if [ -f "$SRC" ]; then
    install -m 0755 "$SRC" /usr/local/bin/k9s
  elif [ -f k9s ]; then
    install -m 0755 k9s /usr/local/bin/k9s
  else
    echo "缺少 k9s 安装包（.rpm 或二进制）" >&2
    exit 1
  fi
fi

echo "k9s 安装完成：$(k9s version 2>/dev/null | head -1 || echo ok)"
