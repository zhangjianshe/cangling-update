#!/bin/bash
# 在有网络的机器上运行：下载 k3s v1.30.13-rc1+k3s1 二进制与离线镜像到各平台包目录。
# 可通过 PROXY 环境变量指定代理，例如：PROXY=http://proxy.cangling.cn:7890 ./fetch-k3s.sh
set -euo pipefail

VERSION="v1.30.13-rc1+k3s1"
# GitHub 的 release tag 中 `+` 需编码为 `%2B`
TAG="v1.30.13-rc1%2Bk3s1"
BASE="https://github.com/k3s-io/k3s/releases/download/${TAG}"
PROXY="${PROXY:-}"
CACHE="${TMPDIR:-/tmp}/k3s-fetch-cache"

curl_args=(-fL --retry 3 --connect-timeout 15)
if [ -n "$PROXY" ]; then
  curl_args+=(--proxy "$PROXY")
fi

mkdir -p "$CACHE"

dl() {
  local url="$1" out="$2"
  if [ -s "$out" ]; then
    echo "    已缓存 $out，跳过"
    return 0
  fi
  echo "    下载 $url"
  curl "${curl_args[@]}" -o "$out.tmp" "$url"
  mv "$out.tmp" "$out"
}

# 每个唯一资产只下载一次，再复制到各包目录
fetch_arch() {
  local bin="$1" airgap="$2" server_dir="$3" agent_dir="$4"
  local bin_cache="$CACHE/$bin"
  local airgap_cache="$CACHE/$airgap"

  dl "$BASE/$bin" "$bin_cache"
  dl "$BASE/$airgap" "$airgap_cache"

  for dest in "$server_dir" "$agent_dir"; do
    mkdir -p "$dest"
    cp "$bin_cache" "$dest/k3s"
    chmod +x "$dest/k3s"
    cp "$airgap_cache" "$dest/$airgap"
    echo "==> 已就绪 $dest"
  done
}

# amd64 → linux-x86
fetch_arch "k3s" "k3s-airgap-images-amd64.tar.gz" \
  "linux-x86/k3s-server" "linux-x86/k3s-agent"
# arm64 → kylin-arm（二进制资产名是 k3s-arm64，落地后统一叫 k3s）
fetch_arch "k3s-arm64" "k3s-airgap-images-arm64.tar.gz" \
  "kylin-arm/k3s-server" "kylin-arm/k3s-agent"

echo "完成，k3s ${VERSION} 离线包已就绪（缓存目录 $CACHE 可删除）。"
