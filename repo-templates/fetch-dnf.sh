#!/bin/bash
# 在麒麟（RPM/dnf）机器上运行：从麒麟源下载当前架构的 git/samba 等 .rpm（含依赖）。
# 通过 PROXY 环境变量可指定代理；不设代理时用 --setopt=proxy= 直连（绕开本机残留的坏代理配置）。
set -euo pipefail

SRC_ROOT="$(cd "$(dirname "$0")" && pwd)"
PROXY="${PROXY:-}"

case "$(uname -m)" in
  x86_64) PLATFORM="kylin-x86" ;;
  aarch64) PLATFORM="kylin-arm" ;;
  *) echo "不支持的架构：$(uname -m)（本脚本需在目标架构上运行）" >&2; exit 1 ;;
esac
DEST="$SRC_ROOT/$PLATFORM"

DNF_OPTS=(--setopt=proxy="$PROXY")

dl() {
  local pkgs="$1" pkgdir="$2"
  local dir="$DEST/$pkgdir"
  mkdir -p "$dir"
  # RPM 平台安装逻辑一致；首次生成 kylin-x86 时复用已有模板脚本。
  if [ ! -f "$dir/install.sh" ] && [ -f "$SRC_ROOT/kylin-arm/$pkgdir/install.sh" ]; then
    cp "$SRC_ROOT/kylin-arm/$pkgdir/install.sh" "$dir/install.sh"
    chmod +x "$dir/install.sh"
  fi
  rm -f "$dir"/*.rpm
  dnf "${DNF_OPTS[@]}" download --resolve --alldeps --destdir="$dir" $pkgs
  echo "==> $dir : $(ls "$dir"/*.rpm 2>/dev/null | wc -l) 个 .rpm"
}

# git（麒麟源里有）
dl "git" "git"
# samba（麒麟源里有）
dl "samba" "samba"
# cifs-utils（CIFS 挂载客户端）
dl "cifs-utils" "cifs-utils"
# nfs 客户端（麒麟包名 nfs-utils，仓库目录名 nfs-common）
dl "nfs-utils" "nfs-common"

echo "完成，$PLATFORM 的 git/samba/cifs-utils/nfs-utils 离线 .rpm 已就绪。"
echo "docker / k9s 的 .rpm 请用 fetch-kylin-docker-k9s.sh（在可访问 download.docker.com / github 的机器上跑）。"
