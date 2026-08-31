#!/bin/bash
## 离线安装 k3s agent（版本 v1.30.13-rc1+k3s1），通过 K3S_URL / K3S_TOKEN 加入集群。
## 本包目录需包含：k3s（二进制）、k3s-airgap-images-<arch>.tar.gz（离线镜像）。
## K3S_URL / K3S_TOKEN 由 cangling-update 初始化流程注入。
## 幂等：重复执行会重写 agent.env / systemd 单元并重启，用于修复「已装 k3s 但未加入」的节点。
## --with-node-id 避免多台机器默认主机名相同（如 localhost.localdomain）被 k3s 判定为重复节点。
set -euo pipefail

K3S_VERSION="v1.30.13-rc1+k3s1"
BIN_DIR="/usr/local/bin"
DATA_DIR="/var/lib/rancher/k3s"
IMAGES_DIR="$DATA_DIR/agent/images"

: "${K3S_URL:?缺少 K3S_URL（主节点地址）}"
: "${K3S_TOKEN:?缺少 K3S_TOKEN（加入令牌）}"

if [ ! -f k3s ] && ! command -v k3s >/dev/null 2>&1; then
  echo "缺少 k3s 二进制：请先运行 fetch-k3s.sh 下载到本包目录。" >&2
  exit 1
fi

if [ -f k3s ]; then
  echo "==> 安装 k3s 二进制 $BIN_DIR/k3s"
  install -m 0755 k3s "$BIN_DIR/k3s"
else
  echo "k3s 已安装：$(k3s --version 2>/dev/null | head -1)"
fi

AIRGAP="$(ls k3s-airgap-images-*.tar.gz 2>/dev/null | head -1 || true)"
if [ -n "$AIRGAP" ]; then
  echo "==> 安装离线镜像 $AIRGAP"
  mkdir -p "$IMAGES_DIR"
  cp "$AIRGAP" "$IMAGES_DIR/$AIRGAP"
fi

echo "==> 写入加入信息 /etc/rancher/k3s/agent.env"
mkdir -p /etc/rancher/k3s
cat > /etc/rancher/k3s/agent.env <<EOF
K3S_URL=${K3S_URL}
K3S_TOKEN=${K3S_TOKEN}
EOF
chmod 600 /etc/rancher/k3s/agent.env

echo "==> 写入 systemd 单元 /etc/systemd/system/k3s-agent.service"
cat > /etc/systemd/system/k3s-agent.service <<'UNIT'
[Unit]
Description=Lightweight Kubernetes Agent
Documentation=https://k3s.io
After=network-online.target
Wants=network-online.target

[Install]
WantedBy=multi-user.target

[Service]
Type=exec
ExecStart=/usr/local/bin/k3s agent --with-node-id
EnvironmentFile=-/etc/rancher/k3s/agent.env
KillMode=process
Delegate=yes
LimitNOFILE=1048576
TasksMax=infinity
TimeoutStartSec=0
Restart=always
RestartSec=5s
UNIT

systemctl daemon-reload
systemctl enable k3s-agent.service 2>/dev/null || true
systemctl restart k3s-agent.service

echo "k3s ${K3S_VERSION} agent 已启动（--with-node-id），正在加入 ${K3S_URL}"
