#!/bin/bash
## 离线安装 k3s server（版本 v1.30.13-rc1+k3s1）。
## 本包目录需包含：k3s（二进制）、k3s-airgap-images-<arch>.tar.gz（离线镜像）。
## Traefik 入口端口（8020/8443）由 cangling-update 初始化流程写入 manifests，本脚本只负责安装并启动 server。
set -euo pipefail

K3S_VERSION="v1.30.13-rc1+k3s1"
BIN_DIR="/usr/local/bin"
DATA_DIR="/var/lib/rancher/k3s"
IMAGES_DIR="$DATA_DIR/agent/images"

if command -v k3s >/dev/null 2>&1; then
  echo "k3s 已安装：$(k3s --version 2>/dev/null | head -1)"
  exit 0
fi

ARCH="$(uname -m)"
AIRGAP="$(ls k3s-airgap-images-*.tar.gz 2>/dev/null | head -1 || true)"

if [ ! -f k3s ]; then
  echo "缺少 k3s 二进制：请先运行 fetch-k3s.sh 下载到本包目录。" >&2
  exit 1
fi

echo "==> 安装 k3s 二进制 $BIN_DIR/k3s"
install -m 0755 k3s "$BIN_DIR/k3s"

if [ -n "$AIRGAP" ]; then
  echo "==> 安装离线镜像 $AIRGAP"
  mkdir -p "$IMAGES_DIR"
  cp "$AIRGAP" "$IMAGES_DIR/$AIRGAP"
else
  echo "!! 未找到 k3s-airgap-images-*.tar.gz，k3s 启动时会尝试联网拉取镜像。"
fi

echo "==> 写入 systemd 单元 /etc/systemd/system/k3s.service"
cat > /etc/systemd/system/k3s.service <<'UNIT'
[Unit]
Description=Lightweight Kubernetes
Documentation=https://k3s.io
After=network-online.target
Wants=network-online.target

[Install]
WantedBy=multi-user.target

[Service]
Type=notify
ExecStart=/usr/local/bin/k3s server
KillMode=process
Delegate=yes
LimitNOFILE=1048576
TasksMax=infinity
TimeoutStartSec=0
Restart=always
RestartSec=5s
UNIT

systemctl daemon-reload
systemctl enable --now k3s.service

echo "==> 等待 node-token 生成（供工作节点加入）"
for i in $(seq 1 90); do
  if [ -f "$DATA_DIR/server/node-token" ]; then
    break
  fi
  sleep 2
done
if [ -f "$DATA_DIR/server/node-token" ]; then
  echo "k3s server 启动成功，node-token 已生成。"
else
  echo "!! k3s server 已启动但 3 分钟内未生成 node-token，请用 systemctl status k3s 检查。" >&2
fi

echo "k3s ${K3S_VERSION} server 安装完成。"
