# 苍岭更新（cangling-update）

单文件主机升级台：在浏览器里管理本机的 Docker Compose 应用，支持导入镜像、替换 JAR、备份与回滚。

- 一个可执行文件，网页端口默认 **5400**
- 数据存在程序旁边的 `config/` 目录（SQLite）
- 支持 x86_64 与 ARM64

## 能做什么

1. 把本机某个 Compose 目录登记为「项目」
2. 上传 `docker save` 出来的 `.tar` / `.tar.gz` / `.tgz`，执行 `docker load`，并把镜像打成 `:latest`
3. 上传 `.jar`，按 compose 里的挂载文件名覆盖（例如 `./jars/cis-server-1.0.0.jar`），再强制重建对应服务
4. 每次升级前用 **tar.gz** 备份整个项目目录（保留权限和属主），可回滚
5. 登录保护；2 小时无操作自动退出

## 编译

需要本机已安装 Rust，以及 `docker`（导入镜像、操作 compose 时使用）。

```bash
# 本机架构
cargo build --release
# 产物：target/release/cangling-update

# x86_64
make x86

# ARM64（需：rustup target add aarch64-unknown-linux-gnu
#         以及 gcc-aarch64-linux-gnu）
make arm64
```

建议把 `target/release/cangling-update` 拷到固定目录再运行或安装服务，不要长期用 `target/debug/`。

## 运行

```bash
./cangling-update
# 浏览器打开 http://<主机>:5400
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

## 安装为系统服务

把**当前这份二进制**装成 systemd 服务，工作目录就是程序所在目录：

```bash
sudo ./cangling-update --port 5400 install-service
sudo ./cangling-update restart
sudo ./cangling-update uninstall-service
```

- 单元文件：`/etc/systemd/system/cangling-update.service`
- 服务名：`cangling-update`
- 查看状态：`systemctl status cangling-update`

安装后 `config/` 仍在程序旁边（除非指定了 `--data-dir`）。换新版本时：覆盖二进制，再执行 `restart`。

## 首次使用

1. 打开 `http://<主机>:5400`
2. 首次进入会要求**初始化管理员**（用户名 2–32 位字母数字，密码至少 8 位）
3. 之后用该账号登录；右上角可退出
4. **新建项目**，填写本机**绝对路径**，目录里必须有：
   - `docker-compose.yml` / `docker-compose.yaml` / `compose.yml` / `compose.yaml`

创建项目时会打一份 **v1 基线快照**（目录可能很大，页面会显示备份进度）。

## 升级

### 镜像包

1. 在构建机：`docker save 镜像:标签 | gzip > app.tar.gz`
2. 在页面上传一个或多个 `.tar` / `.tar.gz` / `.tgz`
3. 服务端 `docker load`，再把载入的镜像打成同名 `:latest`
4. 勾选「导入成功后重启 Compose」则执行 `docker compose up -d`

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

上传的文件名必须与挂载文件名一致（如 `cis-server-1.0.0.jar`）。系统会覆盖该路径，并对用到这些 JAR 的服务执行：

```bash
docker compose up -d --force-recreate <服务名>
```

镜像包和 JAR 可以同一次上传。对不上挂载名时，文件会落到项目下的 `jars/`（若存在），但不会自动重建服务。

## 版本与回滚

苍岭更新的 **v1 / v2 / v3** 是本系统的操作序号，和镜像 tag、JAR 文件名里的版本号没有对应关系。

| 类型 | 含义 |
|---|---|
| 基线 | 新建项目时的目录快照 |
| 升级 | 一次导入镜像或 JAR |
| 回滚前快照 | 点「恢复」时，先给当前线上目录再打一份保险 |

恢复某版本时：先自动备份当前目录，再解压该版本的 tar.gz，必要时重新 `docker load` 当时保存的镜像包，并重建 JAR 相关服务。

进行中的恢复/升级不能重复点击。

## 数据目录

默认在可执行文件旁边：

```
cangling-update                 # 程序
config/
  cangling.db                   # SQLite（项目、版本、用户、会话）
  backups/<项目ID>/<版本ID>/
    tree.tar.gz                 # 目录备份（保留权限/属主）
    tree.tar.gz.meta.json       # 备份体积，用于进度
    images/                     # 当时上传的镜像包
    jars/                       # 当时上传的 JAR
  uploads/                      # 上传临时目录
```

备份跳过 `.git`、`lost+found`，不跟随符号链接。Docker 命名卷（`/var/lib/docker`）不在项目目录里，不会被备份。

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

在苍岭更新里把项目目录指到 `test-docker/`，上传新的 tar.gz，刷新 8088 应看到新版本号。

## 命令一览

```
cangling-update [选项] [命令]

命令：
  reset-password       重置登录密码
  install-service      安装 systemd 服务
  uninstall-service    卸载 systemd 服务
  restart              重启本服务

选项：
  --bind               监听地址
  --port               监听端口
  --data-dir           数据目录
```
