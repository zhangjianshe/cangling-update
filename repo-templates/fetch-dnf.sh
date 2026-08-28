#!/bin/bash
# 在麒麟（RPM/dnf）机器上运行：从麒麟源下载 git/samba 等 .rpm（含依赖）到 kylin-arm 平台目录。
# 通过 PROXY 环境变量可指定代理；不设代理时用 --setopt=proxy= 直连（绕开本机残留的坏代理配置）。
set -euo pipefail

SRC_ROOT="$(cd "$(dirname "$0")" && pwd)"
DEST="$SRC_ROOT/kylin-arm"
PROXY="${PROXY:-}"

DNF_OPTS=(--setopt=proxy="$PROXY")

dl() {
  local pkgs="$1" pkgdir="$2"
  local dir="$DEST/$pkgdir"
  mkdir -p "$dir"
  rm -f "$dir"/*.rpm
  dnf "${DNF_OPTS[@]}" download --resolve --alldeps --destdir="$dir" $pkgs
  echo "==> $dir : $(ls "$dir"/*.rpm 2>/dev/null | wc -l) 个 .rpm"
}

# git（麒麟源里有）
dl "git" "git"
# samba（麒麟源里有）
dl "samba" "samba"

echo "完成，kylin-arm 的 git/samba 离线 .rpm 已就绪。"
echo "docker / k9s 的 .rpm 请用 fetch-kylin-docker-k9s.sh（在可访问 download.docker.com / github 的机器上跑）。"
