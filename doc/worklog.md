# 工作日志（worklog）

> 约定：每次操作后，把「做了什么」的摘要追加到本文件。
> 本文件是 `doc/` 目录下的唯一工作日志（单一 markdown），按时间倒序/正序追加均可，新条目统一用「日期 + 标题 + 摘要 + 涉及文件 + 验证」格式。

---

## 2026-08-28 — 软件仓库 3 平台 Tab + worker 下载安装

### 做了什么
1. **仓库按 3 平台分 Tab**（重写 `src/repo.rs`）
   - `repo/` 下分 `kylin-arm`（麒麟 OS ARM）、`linux-x86`（通用 Linux x86）、`windows` 三个平台目录，每个子目录是一个软件包。
   - 每包识别安装脚本（优先级 `install.sh/bat/ps1`、`setup.sh/bat/ps1`），`##` 注释行读作描述，统计文件数与大小。
   - 本机平台按架构归入 `kylin-arm`（aarch64/arm）或 `linux-x86`。
2. **master 暴露 m2m 接口**（`src/cluster/server.rs`，集群令牌认证）
   - `GET /api/cluster/repo`：仓库清单。
   - `GET /api/cluster/repo/{tab}/{package}/download`：返回 tar.gz。
3. **worker 拉取 master 仓库并下载/安装**
   - `AppState` 增加共享 `master_url`（worker 发现 master 后写入，供仓库代理复用）。
   - `src/cluster/http.rs` 增加 `get_json` / `get_bytes`。
   - `/api/repo` 在 worker 角色下代理 master；`/api/repo/install` 统一「取包 → 解压临时目录 → 运行安装脚本 → 清理」，不污染仓库目录。
   - 修复：`src/auth.rs` 的 `is_public` 白名单补齐仓库 m2m 路径，避免登录中间件先拦截机器间请求。
4. **前端仓库页**（`src/assets/index.html`）
   - 3 个平台 Tab（复用 `.tabs/.tab` 样式），每包显示「下载」「安装」按钮。
   - 安装按钮仅在「包平台 == 本机平台」时可用；worker 页显示「来自主节点 …」。

### 涉及文件
- `src/repo.rs`（重写）
- `src/cluster/server.rs`、`src/cluster/http.rs`、`src/cluster/client.rs`
- `src/state.rs`、`src/auth.rs`、`src/api.rs`
- `src/assets/index.html`
- `README.md`（仓库章节更新）

### 验证
- `cargo build` 通过；`cargo test` 68 个全部通过；`cargo clippy` 无新增告警。
- 实机冒烟测试（master:5599 + worker:5600）：
  - 单机 `/api/repo` 正确列出 3 Tab 与包信息。
  - 下载返回可解压 tar.gz。
  - worker `/api/repo` 显示 `remote=true, master=…`；「安装」成功返回 `installed demo from master`；「下载」正常代理。

### 备注
- Windows 包在 Linux 节点上「安装」按钮禁用（`.bat/.ps1` 无法在 Linux 运行），「下载」始终可用。
- 本次改动未提交，与之前的集群/repo 工作一起处于工作区。

---

## 2026-08-28 — 测试主机（3 台 VM）

用于测试本项目的 3 台虚拟机：

| 主机 | 用户 | 备注 |
| --- | --- | --- |
| 192.168.3.121 | root | 已通，hostname `n01`，x86_64 |
| 192.168.3.122 | root | 已通，hostname `n02`，x86_64 |
| 192.168.3.123 | root | 已通，hostname `n03`，x86_64 |

约定用途：作为集群 master/worker、软件仓库下载安装等功能的实测环境。

---

## 2026-08-28 — 三机端到端部署测试（master + 2 worker）

### 做了什么
1. **构建静态二进制**：本机 glibc 2.39 与目标机 glibc 2.35 不兼容（需 GLIBC_2.39 的 `pidfd_spawnp`/`pidfd_getpid`），且本机无 `musl-gcc`、无 sudo。下载 musl.cc 交叉工具链（`/tmp/x86_64-linux-musl-cross`）后，用 `x86_64-unknown-linux-musl` 目标编译出 `static-pie` 二进制。
2. **部署**：把二进制部署到三台机的 `/opt/cangling-update/cangling-update`。
3. **master 仓库**：在 n01 建 `repo/{kylin-arm,linux-x86,windows}` 三个平台目录，各放一个示例包（`install.sh`/`install.bat`）。
4. **启动**：n01 为 `--role master`，n02/n03 为 `--role worker --master http://192.168.3.121:5400`，共用 `--cluster-token test-token-2026`，`--data-dir /opt/cangling-update/config`，监听 `0.0.0.0:5400`。

### 验证结果
- master `/api/cluster/nodes`：3 节点全部 online（master n01 + worker n02 + worker n03）；`/api/cluster/status` `node_count=3, online=3`。
- worker（n02）`/api/repo`：`remote=true, master=http://192.168.3.121:5400`，正确列出 3 个平台 Tab 及包。
- worker「安装 demo」：从 master 下载并在 n02 本机运行安装脚本，输出 `demo installing on n02... demo OK: n02`，且 `/opt/cangling-update/installed-demo.txt` 标记文件已在 n02 生成。
- worker「下载 demo」：返回可解压 tar.gz。

### 部署信息（供后续操作）
- 二进制：`/opt/cangling-update/cangling-update`（静态 musl）
- 数据目录：`/opt/cangling-update/config`；日志：`/opt/cangling-update/server.log`
- 角色/参数：n01 master；n02/n03 worker（`--master http://192.168.3.121:5400`）；token `test-token-2026`
- Web 控制台：`http://<各机IP>:5400`，账号 `admin` / `password123`
- 停止：各机 `pkill -f "cangling-update --role"`；重启用上述启动参数。

### 备注
- 本机 musl 交叉工具链留在 `/tmp/x86_64-linux-musl-cross`，构建命令见 `doc/` 下备注（可固化到 Makefile）。
- 目标机原有 `/opt/cangling-update/config` 里已有 admin（早期部署残留），已在 n01 用 `reset-password` 重置为 `password123`。

---

## 2026-08-28 — 把 n01 设为仓库备份主机并准备跨集群部署

### 做了什么（在 n01 = 192.168.3.121 上）
1. 新增仓库备份脚本 `/opt/cangling-update/backup-repo.sh`：把 `repo/` 打包成 `backups/repo-<时间戳>.tar.gz`，自动保留最近 10 份。
2. 新增仓库部署脚本 `/opt/cangling-update/deploy-repo.sh`：`deploy-repo.sh [backup.tar.gz]`（不传参则用最新备份）解压到目标 master 的 `repo/`。
3. 在 `repo/README.md` 写入仓库布局与备份/部署说明。
4. 立即执行一次备份：`/opt/cangling-update/backups/repo-20260828-052017.tar.gz`。

### 验证
- 备份 tar.gz 内容完整（含 3 平台目录 + README + 各包 install 脚本）。
- 在临时目录演练解压部署成功，结构与线上一致。

### 备注
- 仓库每次请求实时扫描磁盘，部署/新增软件包后无需重启服务，控制台点「刷新」即可看到。
- 备份目前只存在 n01 本机；如需异地/本地留存可再把 tar.gz 拉到别处。

---

## 2026-08-28 — 集群初始化（离线基线软件一键安装）

### 做了什么
1. 新增 `src/cluster/init.rs`：定义 master/worker 软件清单（master：git/samba/docker/k3s-server/k9s；worker：git/samba/docker/k3s-agent），初始化状态跟踪（步骤/日志/耗时）。
2. DB 增加 `cluster_settings` 表存集群名；`ClusterStatus` 返回 `name`。
3. master 一键编排：本机依次安装 master 软件 → 写 Traefik 8020/8443 manifest → 读 k3s node-token → 对每个在线 worker 下发初始化。
4. 机器间接口 `POST /api/cluster/init/run`（令牌认证）：worker 从 master 仓库拉取并安装自己的角色软件，安装脚本可收到 `CANGLING_CLUSTER_NAME` / `K3S_URL` / `K3S_TOKEN` 环境变量。
5. 控制台接口 `POST /api/cluster/init` + `GET /api/cluster/init/status`；`k3s::ensure_traefik_config` 供初始化复用。
6. 前端集群页：集群名输入 + 「初始化集群」按钮 + 步骤/日志进度表（轮询）。

### 验证（三机实机）
- master 初始化：git/samba/docker/k3s-server/k9s + traefik 全部 ok，traefik manifest 写入 8020/8443。
- 两个 worker：git/samba/docker/k3s-agent 全部 ok，安装脚本在各自节点执行（标记文件已生成），且收到 `cluster=prod-cluster`、`K3S_URL=https://192.168.3.121:6443`。
- `cargo build/test` 通过（68 测试）；集群名持久化生效。

### 备注
- 真实离线安装仍需在仓库里准备各软件包的真实 install 脚本（含 .deb/二进制/镜像等），本次用演示脚本验证机制。
- k3s node-token 在 k3s-server 真实安装后才存在，未安装时 worker 的 `K3S_TOKEN` 为空。

---

## 2026-08-28 — 离线安装脚本模板 + k3s v1.30.13-rc1+k3s1 离线包

### 做了什么
1. 新增 `repo-templates/`（版本化模板）：README + fetch-k3s.sh + 6 个软件的离线 install.sh（linux-x86 与 kylin-arm）。
   - git/samba/docker：优先离线 `.deb`，无则回退联网 apt。
   - k9s：单二进制安装。
   - k3s-server/k3s-agent：安装 k3s 二进制 + airgap 镜像 + 写 systemd 单元；agent 用 `K3S_URL`/`K3S_TOKEN` 加入；版本固定 **v1.30.13-rc1+k3s1**。
2. `fetch-k3s.sh` 支持 `PROXY` 环境变量，每个唯一资产只下载一次（缓存到 /tmp）再复制到 server/agent 包目录。
3. 在 n01（有网）用代理 `http://proxy.cangling.cn:7890` 拉取 k3s v1.30.13-rc1+k3s1：
   - linux-x86：k3s(67MB) + airgap-amd64(178MB)
   - kylin-arm：k3s-arm64(63MB) + airgap-arm64(165MB)
4. 部署到 live 仓库 `/opt/cangling-update/repo/`，两个平台各 6 个初始化包（git/samba/docker/k3s-server/k9s/k3s-agent）齐备，repo 总量 943MB。

### 验证
- k3s 二进制版本确认为 `v1.30.13-rc1+k3s1`（go1.23.8）。
- 所有 install.sh 通过 `bash -n` 语法检查。

### 备注
- git/samba/docker/k9s 目前只有 install.sh，真实离线部署还需在对应包目录放入 `.deb`/静态二进制。
- 代理地址已写入 fetch-k3s.sh 用法注释，后续在有网机器上备包可用：`PROXY=http://proxy.cangling.cn:7890 ./fetch-k3s.sh`。

---

## 2026-08-28 — 补全 git/samba/docker/k9s 离线包 + fetch 脚本

### 做了什么
1. 新增 `repo-templates/fetch-k9s.sh`：拉取 k9s v0.51.0 的 `.deb`（amd64 + arm64，跨架构）。
2. 新增 `repo-templates/fetch-apt.sh`：在目标 OS+架构机器上拉取 git/samba/docker 的 `.deb`（含依赖；docker 先加官方源）。
3. 新增 `repo-templates/fetch-all.sh`：一键拉全部（k3s + k9s + git/samba/docker）。
4. 更新 k9s/install.sh 支持 `.deb`（优先）或单二进制。
5. 在 n01（Ubuntu 22.04 x86_64，代理 `http://proxy.cangling.cn:7890`）实测拉取并部署到 live 仓库。

### 结果（live 仓库）
- linux-x86：git 3.0MB、samba 19.2MB(39 文件)、docker 97.4MB(6 deb)、k3s-server/k3s-agent 244.8MB 各、k9s 39.6MB。
- kylin-arm：k3s-server/k3s-agent 226.3MB 各、k9s 35.8MB；git/samba/docker 需在 ARM 机器上跑 fetch-apt.sh。
- repo 总量 1.2GB，linux-x86 平台已可完全离线初始化。

### 备注
- `fetch-apt.sh` 的 .deb 依赖完整性要求运行在「干净机器/容器」；n01 因已装 git/docker，git 只取到主包、docker 取到 6 个 docker-ce 系包（基础依赖在目标 OS 已具备）。
- kylin-arm 的 git/samba/docker .deb 需要一台 Kylin ARM 机器（或 arm64 容器）跑 `fetch-apt.sh`。

---

## 2026-08-28 — kylin-arm 改用 RPM（麒麟高级服务器 V10）

### 发现
- `ssh hn`（HostName localhost:2222）= 银河麒麟高级服务器 V10（aarch64），用 **dnf/rpm**，不是 .deb。
- hn 的 dnf.conf 里有 `proxy=http://127.0.0.1:7890`（本机代理已失效），需 `--setopt=proxy=` 直连麒麟源（update.cs2c.com.cn，可直连）。
- 麒麟源有 git/samba，无 docker-ce（只有 podman）；docker-ce 需从 download.docker.com 拉 el9 aarch64 rpm；k9s 从 github 拉 arm64 rpm。

### 做了什么
1. kylin-arm 的 install.sh 改为 RPM 版（git/samba/docker/k9s 用 `dnf install --nogpgcheck ./*.rpm` / `rpm -Uvh`；k3s 不变）。
2. 新增 `fetch-dnf.sh`（在麒麟机器上跑，dnf download 拉 git/samba .rpm + 依赖，支持 PROXY / 直连）。
3. 新增 `fetch-kylin-docker-k9s.sh`（在可访问 docker.com/github 的机器上拉 docker-ce 29.7.2 el9 aarch64 5 个 rpm + k9s v0.51.0 arm64 rpm）。
4. 更新 fetch-k9s.sh（只拉 linux-x86 .deb）、fetch-all.sh、README。

### 结果（live 仓库 kylin-arm，RPM）
- git 268 文件 120MB、samba 157 文件 100MB、docker 6 文件 87MB、k9s 37MB、k3s-server/k3s-agent 各 226MB。
- 全部 install.sh 通过 bash -n。repo 总量 1.5GB，linux-x86 + kylin-arm 均已可完全离线初始化。

### 备注
- hn 的 git/samba .rpm 通过 hn→本机→n01 中转搬运完成。
- 麒麟源里没有 docker-ce，所以 kylin-arm 的 docker 用 Docker 官方 el9 aarch64 rpm（Kylin V10 兼容 RHEL9）。

---

## 2026-08-28 — 集群「检查并修复」按钮 + 新节点自动安装

### 做了什么
1. 新增 `POST /api/cluster/check`（`cluster::init::start_check`）：复用初始化编排，但使用已保存的集群名，`InitStatus` 增加 `mode`（init/check）。
2. 集群页增加「检查并修复」按钮：对每个节点检查缺失软件并补装（含新加入 worker），已安装的由各 install.sh 的 `command -v` 检测自动跳过。

### 验证（三机实机，离线）
- 触发「检查」后，n01 依次：git（跳过）/samba（安装）/docker（跳过）/k3s-server（安装）/k9s（安装）/traefik（写配置），全部 ok。
- n02/n03 依次：git（跳过）/samba（安装）/docker（跳过）/k3s-agent（安装加入），全部 ok。
- **离线装出了真实 k3s 集群**：`kubectl get nodes` 显示 n01(control-plane,master) + n02 + n03 全部 Ready，版本 v1.30.13-rc1+k3s1。
- k9s v0.51.0、samba 4.15.13 已安装。

### 备注
- 「检查并修复」与「初始化」共用同一套幂等安装脚本；新加入的 worker 会在 `online_workers` 里被自动纳入。

---

## 2026-08-28 — 补充安装 docker-compose（master + worker）

### 做了什么
1. 软件清单加入 `docker-compose`：MASTER = git/samba/docker/docker-compose/k3s-server/k9s，WORKER = git/samba/docker/docker-compose/k3s-agent。
2. 新增 `docker-compose` 离线包（linux-x86 .deb + kylin-arm .rpm），安装 `docker-compose-plugin`（提供 `docker compose`）。

### 验证
- 「检查并修复」后，三节点 docker-compose 全部 ok，`docker compose version` 显示 Docker Compose v5.5.0（n01/n02/n03）。
