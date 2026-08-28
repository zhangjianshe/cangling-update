#!/bin/bash
## 离线安装 git。
## 本包目录可放置 git 相关 .deb（git、git-man、libcurl3-gnutls、perl 等），无 .deb 时回退到联网 apt。
set -euo pipefail

if command -v git >/dev/null 2>&1; then
  echo "git 已安装：$(git --version)"
  exit 0
fi

if ls ./*.deb >/dev/null 2>&1; then
  echo "==> 离线安装本地 .deb"
  dpkg -i ./*.deb || apt-get -f install -y
else
  echo "!! 本包内没有 .deb，回退到联网 apt 安装 git"
  apt-get update
  apt-get install -y git
fi

echo "git 安装完成：$(git --version)"
