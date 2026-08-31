#!/bin/bash
## 离线安装 docker + containerd + runc（官方 static/stable 静态二进制 tarball）。
## 本包目录需包含：docker-<版本>.tgz（解压后为 docker/ 子目录，含 dockerd、containerd、runc 等）。
## 静态二进制为 musl 链接，兼容 glibc 2.28 的麒麟 V10 / RHEL 8 等老系统；
## 不要用 el9 的 docker-ce.rpm（需要 GLIBC_2.32/2.34，麒麟 V10 装不上）。
## docker compose 由独立的 docker-compose 包安装（docker-compose-linux-aarch64）。
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
  echo "请用 root 执行：bash $0" >&2
  exit 1
fi

if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
  echo "docker 已安装且运行正常：$(docker --version)"
  exit 0
fi

TGZ="$(ls docker-*.tgz 2>/dev/null | head -1 || true)"
if [ -z "$TGZ" ]; then
  echo "!! 本包内没有 docker-*.tgz，回退到联网安装（get.docker.com）"
  curl -fsSL https://get.docker.com | sh
  exit 0
fi

echo "==> 内核模块 overlay / br_netfilter"
modprobe overlay 2>/dev/null || true
modprobe br_netfilter 2>/dev/null || true
mkdir -p /etc/modules-load.d
cat >/etc/modules-load.d/docker.conf <<'EOF'
overlay
br_netfilter
EOF

echo "==> 转发与网桥 sysctl"
mkdir -p /etc/sysctl.d
cat >/etc/sysctl.d/99-docker.conf <<'EOF'
net.bridge.bridge-nf-call-iptables = 1
net.bridge.bridge-nf-call-ip6tables = 1
net.ipv4.ip_forward = 1
EOF
sysctl -p /etc/sysctl.d/99-docker.conf >/dev/null 2>&1 || true

echo "==> 安装 Docker 静态二进制（$TGZ）"
tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT
tar -xzf "$TGZ" -C "$tmpdir"
# 备份系统自带 runc，便于回滚。
if [ -e /usr/bin/runc ] && [ ! -e /usr/bin/runc.ky10.bak ]; then
  cp -a /usr/bin/runc /usr/bin/runc.ky10.bak
fi
install -m 0755 "$tmpdir"/docker/* /usr/bin/

getent group docker >/dev/null || groupadd --system docker

echo "==> 写入 /etc/docker/daemon.json"
mkdir -p /etc/docker /var/lib/docker
if [ -e /etc/docker/daemon.json ] && [ ! -e /etc/docker/daemon.json.bak.cangling ]; then
  cp -a /etc/docker/daemon.json /etc/docker/daemon.json.bak.cangling
fi
cat >/etc/docker/daemon.json <<'EOF'
{
  "exec-opts": ["native.cgroupdriver=systemd"],
  "storage-driver": "overlay2",
  "log-driver": "json-file",
  "log-opts": {
    "max-size": "100m",
    "max-file": "3"
  },
  "live-restore": true,
  "iptables": true,
  "ip-forward": true
}
EOF

echo "==> 写入 docker.service"
cat >/etc/systemd/system/docker.service <<'EOF'
[Unit]
Description=Docker Application Container Engine
Documentation=https://docs.docker.com
After=network-online.target firewalld.service
Wants=network-online.target

[Service]
Type=notify
Environment=PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
ExecStart=/usr/bin/dockerd
ExecReload=/bin/kill -s HUP $MAINPID
TimeoutStartSec=120
RestartSec=2
Restart=always
StartLimitBurst=3
StartLimitInterval=60s
LimitNOFILE=infinity
LimitNPROC=infinity
LimitCORE=infinity
TasksMax=infinity
Delegate=yes
KillMode=process
OOMScoreAdjust=-500

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable docker.service 2>/dev/null || true
systemctl restart docker.service

echo "==> 等待 Docker 守护进程就绪"
ok=0
for _ in $(seq 1 40); do
  if docker info >/dev/null 2>&1; then
    ok=1
    break
  fi
  sleep 1
done
if [ "$ok" -ne 1 ]; then
  echo "docker 守护进程启动失败" >&2
  journalctl -u docker --no-pager -n 80 >&2 || true
  exit 1
fi

echo "docker 安装完成：$(docker --version)"
