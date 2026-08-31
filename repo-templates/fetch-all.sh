#!/bin/bash
# 一键拉取全部离线软件包。
# 注意：
#   - k3s/k9s/docker 等跨架构资产走本脚本（可在任意可联网机器上跑，配 PROXY）。
#   - git/samba 的 .deb（linux-x86）走 fetch-apt.sh（需在 Ubuntu/Debian 机器上跑）。
#   - git/samba 的 .rpm（kylin-arm）走 fetch-dnf.sh（需在麒麟机器上跑）。
# 用法：PROXY=http://proxy.cangling.cn:7890 ./fetch-all.sh
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"

echo "### 1/3 拉取 k3s v1.30.13-rc1+k3s1"
"$DIR/fetch-k3s.sh"

echo "### 2/3 拉取 k9s（linux-x86 .deb + kylin-arm .rpm）与 kylin-arm docker 静态二进制 / compose"
"$DIR/fetch-k9s.sh"
"$DIR/fetch-kylin-docker-k9s.sh"

echo "### 3/3 拉取 git/samba/docker 的 linux-x86 .deb"
"$DIR/fetch-apt.sh"

echo "全部完成。"
echo "提醒：kylin-arm 的 git/samba .rpm 需另在麒麟机器上执行 ./fetch-dnf.sh。"
