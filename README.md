# 苍灵更新（cangling-update）

单文件主机升级台：在浏览器里管理本机的 Docker Compose 应用，支持导入镜像、替换 JAR、备份与回滚。

- 一个可执行文件，网页端口默认 **5400**
- 打开即是可配置的首页门户；底部「系统管理」进入 `/console`
- 数据存在程序旁边的 `config/` 目录（SQLite）
- 支持 x86_64 与 ARM64

## 能做什么

1. 首页添加入口卡片（图标、名称、简介），点击跳转到对应地址；可上传图片或 MP4 作为背景
2. 把本机某个 Compose 目录登记为「项目」
3. 上传 `docker save` 出来的 `.tar` / `.tar.gz` / `.tgz`，执行 `docker load`，并把镜像打成 `:latest`
4. 上传 `.jar`，按 compose 里的挂载文件名覆盖（例如 `./jars/cis-server-1.0.0.jar`），再强制重建对应服务
5. 每次升级前用 **Git 全量备份** 项目目录（含 PostGIS 等数据目录；相同文件只存一份，并单独记下权限/属主），可回滚。备份前请先停止应用，页面会提示并默认先 `compose down`
6. 登录保护；2 小时无操作自动退出
7. 对正在运行的容器打开 **xterm 终端**（`docker compose exec`），并按容器查看日志
8. 在 Compose 面板里在线编辑 `docker-compose.yml`，并单独管理该文件的历史版本；若项目目录有 `.env`，可同样编辑环境变量

## 安装
```bash
  mkdir update
  cd update
  curl -fL -o cangling-update https://github.com/zhangjianshe/cangling-update/releases/download/v0.1.35/cangling-update-linux-amd64
  chmod +x cangling-update
  ./cangling-update install-service
  # 访问地址 http://localhost:5400
```

发布文件是 **musl 静态链接**，不依赖主机 glibc，可在 Ubuntu 20.04 / Debian 11 等老系统上运行。若出现 `GLIBC_2.39 not found`，说明这份二进制是在较新系统上动态链接的 gnu 构建，请改用上面的 Releases 文件，或在本仓库执行 `make x86` 后把产物拷过去。

## 编译

需要本机已安装 Rust，以及 `docker`（导入镜像、操作 compose 时使用）。

```bash
# 本机架构（动态链接当前系统 glibc，只能在同代或更新的 glibc 上跑）
cargo build --release
# 产物：target/release/cangling-update

# x86_64 静态 musl（推荐拷到其它机器；需 musl-tools）
sudo apt install musl-tools
make x86
# 产物：target/x86_64-unknown-linux-musl/release/cangling-update

# ARM64 静态 musl（需 musl 交叉链接器；GitHub Actions 会编）
make arm64
```

建议把 musl 产物拷到固定目录再运行或安装服务，不要长期用 `target/debug/`，也不要把本机 `target/release/`（gnu）拷到 glibc 更旧的主机。

### GitHub Actions 发布

仓库已配置 `.github/workflows/release.yml`，用 musl 静态编译：

| 文件 | 架构 |
|---|---|
| `cangling-update-linux-amd64` | x86_64 |
| `cangling-update-linux-arm64` | ARM64 |
| 对应的 `.sha256` | 校验和 |

- 开 PR 或手动 Run workflow：在 Actions 里下载构建产物
- `./release.sh` 推送 `v*` 标签后，自动发布到 GitHub Releases（同一 commit 不会因再推 main 编第二次）：

```bash
./release.sh          # 0.1.0 -> 0.1.1，提交并推送 tag
./release.sh minor    # 0.1.3 -> 0.2.0
./release.sh --dry-run
```

## 运行

```bash
./cangling-update
# 浏览器打开 http://<主机>:5400
# 若已安装为系统服务，则只打印访问地址后退出

./cangling-update hostinfo
# 在程序目录写入 info.md（软件版本与路径、本机 IP、项目列表、磁盘、CPU/GPU）
# 指定路径：./cangling-update hostinfo -o /tmp/info.md

sudo ./cangling-update fix-k3s
# 若已安装 k3s：在 /var/lib/rancher/k3s/server/manifests/ 写入 traefik-config.yaml
# 把 Traefik HTTP/HTTPS 默认入口改为 8020 / 8443，然后重启 Traefik；未安装 k3s 时只打印提示

# 终端彩色查看（无需登录）
curl -s http://<主机>:5400/hostinfo
curl -s http://<主机>:5400/hostinfo.md | glow -
curl -s 'http://<主机>:5400/hostinfo?color=0'   # 无颜色
```

常用参数：

| 参数 / 环境变量 | 默认 | 说明 |
|---|---|---|
| `--bind` / `CANGLING_BIND` | `0.0.0.0` | 监听地址 |
| `--port` / `CANGLING_PORT` | `5400` | 监听端口 |
| `--data-dir` / `CANGLING_HOME` | `<程序目录>/config` | 数据目录 |

```bash
./cangling-update --bind 0.0.0.0 --port 5400
./cangling-update --data-dir /var/lib/cangling-update
```

进程需要对 Docker 有权限（加入 `docker` 组，或用 root）。备份/恢复要保留文件属主时，建议用 root 跑。

本程序启动时若 Docker 守护进程还没起来，页面会显示「守护进程未就绪」，此时不能做基线备份或 Compose 操作。Docker 启动后页面会自动恢复，不必重启本程序。已安装过服务的机器请再执行一次 `install-service`，以便开机时先拉起 docker。

## 安装为系统服务

把**当前这份二进制**装成 systemd 服务，工作目录就是程序所在目录：

```bash
sudo ./cangling-update --port 5400 install-service
sudo ./cangling-update restart
sudo ./cangling-update uninstall-service
```

- 单元文件：`/etc/systemd/system/cangling-update.service`
- 服务名：`cangling-update`
- 命令链接：`/usr/local/bin/cangling-update` → 当前二进制（卸载服务时删除该符号链接）
- 查看状态：`systemctl status cangling-update`

安装后 `config/` 仍在程序旁边（除非指定了 `--data-dir`）。换新版本时：覆盖二进制，再执行 `restart`。

安装完成后，再次直接运行本程序会打印当前服务的访问地址后退出，不会再启动第二个进程：

```bash
./cangling-update
# 已安装为系统服务，不会在前台再次启动。
# 访问地址：
#   http://127.0.0.1:5400
#   http://<本机IP>:5400
```

### 自我更新

从 [GitHub Releases](https://github.com/zhangjianshe/cangling-update/releases) 检查并下载对应架构的二进制，保存为程序目录下的 `cangling-update`。**不会**重启 systemd 服务，当前进程继续跑旧文件，下次启动或手动 `restart` 后才用新版本。

```bash
./cangling-update update --check    # 只看有没有新版本
sudo ./cangling-update update       # 下载并替换文件
sudo ./cangling-update restart      # 需要时再重启服务
```

x86_64 下载 `cangling-update-linux-amd64`，ARM64 下载 `cangling-update-linux-arm64`。需要本机能访问 GitHub，并安装 `curl` 或 `wget`。走代理时请带 `http://`：

```bash
https_proxy=http://10.1.1.2:7890 sudo -E ./cangling-update update
# 或
sudo ./cangling-update update --proxy http://10.1.1.2:7890
```

## 首次使用

1. 打开 `http://<主机>:5400`，进入可配置的首页门户（底部固定「系统管理」，指向 `/console`）
2. 点「编辑首页」或「初始化」完成**管理员初始化**（用户名 2–32 位字母数字，密码至少 8 位），之后可添加入口、上传背景图或 MP4
3. 点底部「系统管理」进入 Docker Compose 升级台；右上角可退出
4. **新建项目**，填写名称、说明（会出现在左侧项目列表里）和本机**绝对路径**，目录里必须有：
   - `docker-compose.yml` / `docker-compose.yaml` / `compose.yml` / `compose.yaml`

创建项目时会打一份 **v1 全量基线快照**（含数据库数据目录，可能很大，页面会显示备份进度）。请先停止应用；默认会先执行 Compose Down，备份完成后再启动。

如果首次备份失败或进程中断，磁盘上可能留下没有项目记录的 `config/backups/<编号>/`。首页和「新建项目」页会列出这些残留，可在页面上清理。

## 升级

### 镜像包

1. 在构建机：`docker save 镜像:标签 | gzip > app.tar.gz`
2. 在页面上传一个或多个 `.tar` / `.tar.gz` / `.tgz`
3. 服务端 `docker load`，再把载入的镜像打成同名 `:latest`
4. 勾选「导入成功后重启 Compose」则执行 `docker compose up -d --remove-orphans`

compose 里请写 `:latest`（或升级后会打上的那个名字），这样新镜像才会被用到。

### JAR 包

适用于把 JAR 挂进容器的应用，例如：

```yaml
cis-server:
  image: hub.example/gdal-base:v4
  volumes:
    - ./jars/cis-server-1.0.0.jar:/app/app.jar:ro
  command: ["java", "-jar", "/app/app.jar"]
```

上传的文件名必须与挂载文件名一致（如 `cis-server-1.0.0.jar`）。系统会覆盖该路径，然后：

```bash
docker compose up -d --force-recreate --remove-orphans <用到该 JAR 的服务>
docker compose up -d --remove-orphans
```

先强制重建 JAR 服务，再把整个 Compose 栈拉起来。备份前若执行了 Compose Down，PostGIS、broker 等其它容器也会一并启动。

镜像包和 JAR 可以同一次上传。对不上挂载名时，文件会落到项目下的 `jars/`（若存在），但不会自动重建服务。

## 版本与回滚

苍灵更新的 **v1 / v2 / v3** 是本系统的操作序号，和镜像 tag、JAR 文件名里的版本号没有对应关系。

| 类型 | 含义 |
|---|---|
| 基线 | 新建项目时的目录快照 |
| 升级 | 一次导入镜像或 JAR |
| 回滚前快照 | 点「恢复」时，先给当前线上目录再打一份保险 |

版本列表里每一行会显示：

- **当前应用**：现在线上 Compose 目录的体积（跳过 `.git`、`lost+found`）
- **备份目录**：该版本自己的备份文件夹（`tree.gitref` / tar、当时上传的镜像包和 JAR）

Git 对象在项目级的 `repo.git` 里跨版本去重，体积单独标在标题旁，不算进某一行的「备份目录」。

恢复某版本时：先自动备份当前目录，再从 Git 快照检出该版本（并写回权限/属主），必要时重新 `docker load` 当时保存的镜像包，并重建 JAR 相关服务。旧版 tar.gz 备份仍可恢复。

进行中的恢复/升级不能重复点击。

## 容器终端与日志

项目页的 Compose 服务表里：

- **日志**：打开该服务最近约 500 行日志（`docker compose logs`），可刷新。支持 ANSI 颜色、粗体、斜体、暗色（如 tracing / mqtt 日志）。日志字体与终端相同，为 **Iosevka Term**。
- **终端**：仅运行中的服务可点。弹出 xterm.js 窗口，通过 WebSocket 进入容器（默认 `/bin/sh`）。可在里面执行命令；关闭窗口即断开。没有 shell 的镜像会失败。终端字体为 **Iosevka Term**（已内置 Regular/Bold；本机若已安装同名字体则优先用本机的）。
- **重启**：只重启这一行对应的服务（`docker compose restart <服务>`），其它容器不动。顶部「启动」执行 `docker compose up -d --remove-orphans`；顶部「重启」执行 `docker compose up -d --force-recreate --remove-orphans`，会清掉 compose 文件里已经不存在的服务。点启动 / 重启 / 停止后按钮会立刻变成「…中」并提示正在执行，完成后刷新服务表。

Compose 面板里的容器状态、详情每 **2 秒**自动刷新，不会打断正在编辑的升级表单、已打开的终端或 Compose 文件编辑窗口。

终端需要本机 `docker compose exec` 可用，浏览器会自动带上登录 Cookie。

### 编辑 Compose 文件

Compose 面板的 **编辑** 会打开当前目录里的 `docker-compose.yml` / `docker-compose.yaml` / `compose.yml` / `compose.yaml`（没有文件时保存会创建 `docker-compose.yml`）。

- 编辑器是内置的 **Ace**（字体 **Iosevka Term**）：YAML / `.env` 高亮、折叠、`Ctrl+F` 查找、Tab 两个空格、`Ctrl+S` / `⌘S` 保存。资源打在程序里，离线可用
- 保存前会先按 YAML 解析。本机有 Compose 时还会把草稿交给 `docker compose -f <草稿> config`；任一校验失败都不会改线上文件
- 每次真正写入都会留下完整文件内容，可在右侧历史里点开预览，或 **恢复此版本**（恢复本身也会再记一条历史，因此还能再改回去）
- 这是 **Compose 文件自己的版本**，和页面「版本切换」里的全量目录备份（镜像 / JAR / 数据）不是一回事
- 默认只改文件。勾选「保存后执行 Compose Up」才会 `docker compose up -d --remove-orphans`
- 磁盘文件若被外部改过，历史里会先补记一条「外部变更」，避免覆盖时丢掉那段内容
- 保存时若文件已在别处被改，会提示冲突，需关闭后重新打开再保存

### 编辑环境变量

项目目录里若存在 **`.env`**，应用容器面板会出现 **环境变量** 按钮，编辑方式和 Compose 文件相同（高亮、历史版本、保存冲突检测、可选 Compose Up）。没有 `.env` 时不显示该按钮，也不会在保存时新建这个文件。

- 每行须是注释、空行或 `KEY=VALUE`（可写 `export KEY=VALUE`）
- `.env` 的历史与 Compose 文件历史分开存放

## 数据目录

默认在可执行文件旁边：

```
cangling-update                 # 程序
logs/cangling-update.log        # 运行日志（与程序同目录）
config/
  cangling.db                   # SQLite（项目、版本、Compose / .env 文件历史、用户、会话）
  backups/<项目ID>/<版本ID>/
    repo.git/                   # 项目级 Git 仓库（跨版本去重）
    <版本ID>/tree.gitref        # 指向某次 commit
    repo.git/cangling-meta/     # uid/gid/mode 等属性
    images/                     # 当时上传的镜像包
    jars/                       # 当时上传的 JAR
  uploads/                      # 上传临时目录
  portal/                       # 首页背景与入口图标
```

备份是全量的：会包含项目目录里的数据库数据文件。跳过 `.git`、`lost+found`，不跟随符号链接。正在运行的数据库可能锁住文件，备份会失败；请先停止 Compose。Docker 命名卷（`/var/lib/docker`）不在项目目录里，不会被备份。

备份失败时不会写入项目/版本记录；若磁盘上已生成 `repo.git` 等目录，会在失败时删除。进程被杀掉后残留的目录会作为「未完成的备份残留」出现在页面上。

## 登录与忘记密码

- 密码用 Argon2 存放
- 会话 Cookie：`cangling_session`（HttpOnly）
- **2 小时没有任何操作**（页面交互或接口）会退出

网页上不能自助找回密码。能登录这台主机的人可以重置：

```bash
# 只有一个账号时可省略 -u
sudo ./cangling-update reset-password

sudo ./cangling-update reset-password -u admin -p '新密码'

# 数据目录不是默认位置时
sudo ./cangling-update --data-dir /var/lib/cangling-update reset-password
```

不写 `-p` 会生成一串随机密码，只打印一次。重置后该用户的旧登录全部失效。

## 自带测试镜像

`test-docker/` 是一个 nginx 小应用，页面上显示镜像版本，方便验证升级是否生效。

```bash
cd test-docker
./bump.sh current          # 打包当前 VERSION（默认 1.0.0）
docker tag cangling-test:1.0.0 cangling-test:latest
docker compose up -d       # http://<主机>:8088 应显示 1.0.0

./bump.sh                  # 1.0.0 -> 1.0.1，生成 dist/cangling-test-1.0.1.tar.gz
```

在苍灵更新里把项目目录指到 `test-docker/`，上传新的 tar.gz，刷新 8088 应看到新版本号。

## 命令一览

```
cangling-update [选项] [命令]

命令：
  version              显示当前程序版本
  reset-password       重置登录密码
  install-service      安装 systemd 服务，并在 /usr/local/bin 创建命令符号链接
  uninstall-service    卸载 systemd 服务，并删除该符号链接
  restart              重启本服务
  update               从 GitHub 下载新版本（不重启服务）
  hostinfo             采集主机信息，写入程序目录下的 info.md
                       网页：GET /hostinfo（ANSI 彩色）  GET /hostinfo.md

选项：
  --bind               监听地址
  --port               监听端口
  --data-dir           数据目录
```
