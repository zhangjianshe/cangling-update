# 离线软件仓库模板（repo-templates）

本目录是 `cangling-update` 集群初始化的**离线安装脚本模板**。把它们连同对应二进制/安装包一起放到主节点 `repo/<平台>/<软件名>/` 下，即可在完全无互联网的集群里一键安装基线软件。

## 平台与包格式

| 平台目录 | 目标系统 | 包格式 | 安装器 |
| --- | --- | --- | --- |
| `linux-x86` | 通用 Linux x86（Ubuntu/Debian） | `.deb` | apt / dpkg |
| `kylin-arm` | 麒麟 ARM（银河麒麟高级服务器 V10，aarch64） | `.rpm`（docker 为静态二进制） | dnf / rpm（docker 为 tar 解包） |
| `windows` | Windows | 脚本 | 手动 |

> k3s 是「单二进制 + airgap 镜像」，与发行版无关，两个 Linux 平台通用（仅架构不同）。

## 目录结构

```
repo-templates/
  README.md
  fetch-all.sh                # 一键拉取（k3s/k9s/docker 等跨架构资产 + linux-x86 .deb）
  fetch-k3s.sh                # k3s v1.30.13-rc1+k3s1 二进制 + airgap 镜像（amd64/arm64）
  fetch-k9s.sh                # k9s v0.51.0 的 linux-x86 .deb
  fetch-apt.sh                # git/samba/docker 的 linux-x86 .deb（含依赖）
  fetch-dnf.sh                # git/samba 的 kylin-arm .rpm（含依赖，需在麒麟机器上跑）
  fetch-kylin-docker-k9s.sh   # kylin-arm 的 docker 静态二进制 + compose + k9s .rpm
  linux-x86/                  # 各软件 install.sh（.deb 版）
  kylin-arm/                  # 各软件 install.sh（.rpm / 静态二进制版，k3s 与 linux-x86 相同）
```

## 软件清单

| 软件 | 角色 | linux-x86 | kylin-arm |
| --- | --- | --- | --- |
| git | master + worker | `.deb` | `.rpm` |
| samba | master + worker | `.deb` | `.rpm` |
| docker | master + worker | docker-ce `.deb` | docker 静态二进制 `docker-*.tgz`（musl，兼容 glibc 2.28） |
| k3s-server | master | 二进制 + airgap | 二进制 + airgap |
| k3s-agent | worker | 二进制 + airgap | 二进制 + airgap |
| k9s | master | `.deb` | `.rpm` |

## 使用方法

### 1. 拉取离线包

```bash
cd repo-templates
# 跨架构资产（k3s / k9s / docker 静态二进制 / compose），在有网+可访问 github/docker.com 的机器上：
PROXY=http://proxy.cangling.cn:7890 ./fetch-all.sh

# linux-x86 的 git/samba/docker .deb：在 Ubuntu/Debian 干净机器/容器上：
./fetch-apt.sh

# kylin-arm 的 git/samba .rpm：在麒麟（RPM）机器上：
./fetch-dnf.sh
```

> `fetch-apt.sh` / `fetch-dnf.sh` 需要运行在「与目标集群同 OS + 同架构」的机器/容器上，才能把依赖下全。

### 2. 部署到主节点

```bash
# 在 master 上（假设可执行文件在 /opt/cangling-update/）
cp -r repo-templates/linux-x86/* /opt/cangling-update/repo/linux-x86/
cp -r repo-templates/kylin-arm/* /opt/cangling-update/repo/kylin-arm/
```

### 3. 初始化集群

在 master 控制台「集群」页填集群名 → 「初始化集群」。

## 版本

- k3s：`v1.30.13-rc1+k3s1`（改 `fetch-k3s.sh` 顶部 `VERSION`/`TAG`，及各 install.sh 的 `K3S_VERSION`）
- k9s：`v0.51.0`
- docker-ce：`27.5.1` 静态二进制（kylin-arm 固定版本在 `fetch-kylin-docker-k9s.sh` 顶部，不能用 el9 的 rpm——麒麟 V10 的 glibc 2.28 装不上）
- docker compose：`v2.32.4` 官方 release 静态二进制
