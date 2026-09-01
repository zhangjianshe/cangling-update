#!/bin/bash
# 下载 git / samba / docker 的离线 .deb（含依赖）到当前架构对应的平台目录。
# 建议在「与目标集群同 OS + 同架构」的干净机器/容器上运行，以保证依赖齐全。
# 可通过 PROXY 环境变量指定代理，例如：PROXY=http://proxy.cangling.cn:7890 ./fetch-apt.sh
set -euo pipefail

PROXY="${PROXY:-}"
SRC_ROOT="$(cd "$(dirname "$0")" && pwd)"

case "$(uname -m)" in
  x86_64) PLATFORM="linux-x86" ;;
  aarch64) PLATFORM="kylin-arm" ;;
  *) echo "不支持的架构：$(uname -m)（本脚本需在目标架构上运行）" >&2; exit 1 ;;
esac

if [ -n "$PROXY" ]; then
  export http_proxy="$PROXY" https_proxy="$PROXY"
fi

# 下载指定包及其依赖到目标目录；已安装的包用 --reinstall 强制重新下载主包。
dl_apt() {
  local pkgs="$1" dest="$2"
  mkdir -p "$dest"
  apt-get clean
  apt-get update
  for p in $pkgs; do
    if dpkg -s "$p" >/dev/null 2>&1; then
      apt-get install --reinstall --download-only -y "$p"
    else
      apt-get install --download-only -y "$p"
    fi
  done
  cp /var/cache/apt/archives/*.deb "$dest/" 2>/dev/null || true
  echo "==> $dest : $(ls "$dest"/*.deb 2>/dev/null | wc -l) 个 .deb"
}

# git
dl_apt "git" "$SRC_ROOT/$PLATFORM/git"
# samba
dl_apt "samba" "$SRC_ROOT/$PLATFORM/samba"
# cifs-utils（CIFS/SMB 挂载客户端）
dl_apt "cifs-utils" "$SRC_ROOT/$PLATFORM/cifs-utils"
# nfs-common（NFS 挂载客户端）
dl_apt "nfs-common" "$SRC_ROOT/$PLATFORM/nfs-common"

# docker：先加官方源（Ubuntu/Debian）
if [ ! -f /etc/apt/sources.list.d/docker.list ]; then
  . /etc/os-release
  install -m 0755 -d /etc/apt/keyrings
  curl -fsSL "https://download.docker.com/linux/ubuntu/gpg" | gpg --dearmor -o /etc/apt/keyrings/docker.gpg
  echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.gpg] https://download.docker.com/linux/ubuntu ${VERSION_CODENAME} stable" \
    > /etc/apt/sources.list.d/docker.list
fi
dl_apt "docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin" "$SRC_ROOT/$PLATFORM/docker"

echo "完成，git/samba/cifs-utils/nfs-common/docker 离线 .deb 已就绪（平台 $PLATFORM）。"
