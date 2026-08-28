#!/bin/bash
# 下载 k9s v0.51.0 的离线 .deb（amd64 + arm64）。
# 可通过 PROXY 环境变量指定代理，例如：PROXY=http://proxy.cangling.cn:7890 ./fetch-k9s.sh
set -euo pipefail

VERSION="v0.51.0"
BASE="https://github.com/derailed/k9s/releases/download/${VERSION}"
PROXY="${PROXY:-}"

curl_args=(-fL --retry 3 --connect-timeout 15)
if [ -n "$PROXY" ]; then
  curl_args+=(--proxy "$PROXY")
fi

fetch() {
  local asset="$1" dest="$2"
  mkdir -p "$dest"
  echo "==> $dest"
  curl "${curl_args[@]}" -o "$dest/$asset" "$BASE/$asset"
}

# linux-x86（Debian/Ubuntu）用 .deb；kylin-arm 的 .rpm 见 fetch-kylin-docker-k9s.sh
fetch "k9s_linux_amd64.deb" "linux-x86/k9s"

echo "完成，k9s ${VERSION} 的 linux-x86 离线 .deb 已就绪。"
