# 通过 SSH 部署 cangling-update 工作节点

在 **主机节点 → 主机互信 → 部署工作节点** 打开对话框，选择要部署的主机，确认工作节点可访问的 master HTTP 地址后执行。
按钮位于“主机互信”右侧；仅 master 角色可执行。

- 从主机列表复用 SSH 地址、端口、账号与已保存密码或现有密钥；目标 SSH 账号须为 root。
- 自动识别 AMD64/ARM64，优先使用与 master 运行版本一致的 `cangling-repo/binaries/linux/<架构>/cangling-update`；也可使用本机程序或 `updates/` 中的对应架构程序。远程安装前再次校验 SHA-256 与程序版本。
- 安装位置为 `/opt/cangling-update`，服务监听 `5400/tcp`，显式传入当前集群令牌及 master 地址。对已启用的 firewalld 放行该 TCP 端口。
- 本功能部署的是 cangling-update worker。K3s 的安装仍使用原有集群初始化入口。
- 部署会逐台显示进度，等待对应节点 ID 注册到当前 master 后才报告成功。单台失败不影响继续处理其他所选主机；最终汇总成功、失败与逐台原因。
- 现有 master、其他集群的 worker、自定义安装/数据目录会停止自动覆盖并提示。主机名保持原值。
- 安装前备份原程序、服务单元和顶层配置文件至 `backups/worker-deploy-<任务编号>/`；安装或服务启动失败时尝试恢复。安装成功但无法注册时保留新服务，提示检查网络、master 地址和日志。
- 页面和任务日志隐藏 SSH 密码与集群令牌；密码辅助脚本随 SSH 操作结束清理。SSH、传输及安装均有超时。

接口 `GET /api/hosts/deploy-workers` 返回默认地址和版本，`POST` 接收 `host_ids` 与 `master_url`，需要控制台登录。任务进度通过 `/api/jobs/<id>` 查询。

验证：`cargo test --locked`、`node scripts/test-accelerator-ui.cjs` 和 `python3 scripts/test-worker-deploy-http.py`。HTTP 测试使用模拟 SSH，不会操作真实远程主机。
