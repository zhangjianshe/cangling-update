# GPU / NPU 发现与诊断（第一阶段）

入口：主机列表每行的“设备诊断”；K3s 资源页的“设备诊断”。API 沿用控制台登录认证。

- `GET /api/hostinfo/accelerators`：本机 PCI 设备、绑定驱动、管理工具结果和 K3s containerd 自定义运行时。
- `GET /api/k3s/accelerators`：节点扩展资源、活跃 Pod 的设备请求、加速器组件状态、RuntimeClass；关联最近的节点主机诊断。
- `cangling-update devices`：输出本机诊断 JSON，不创建配置或数据库。
- `cangling-update devices --cluster`：读取当前 kubeconfig 对应的集群，并合并本机诊断。

该功能不安装驱动、不修改 containerd、不创建或执行 Pod、不自动 SSH 登录其他主机。运行时配置只返回 handler 和二进制路径，不返回完整配置或仓库凭据。

## 证据与边界

1. PCI 设备按地址保留；过滤 BMC、虚拟 VGA 和主桥，厂商未知时仍保留。PCI 功能数量不自动等于物理卡数，虚拟设备也可能有独立 PCI 地址。
2. NVIDIA 使用有超时的 `nvidia-smi` 查询型号、驱动版本、显存；昇腾、海光、AMD 管理工具的原始检查结果保留。工具未安装、执行失败和超时分别展示，未经验证的厂商不会标为通过。
3. 从 K3s 实际生成的 containerd 配置读取自定义运行时路径，兼容不在 PATH 中的 `/usr/local/nvidia/toolkit`。文件存在和 `--version` 成功不等于容器或算法可用。
4. 所有节点注册的扩展资源名原样保留，支持未知厂商及切分资源。容量、可调度总量、已分配、剩余分别展示，不将已分配比例当成设备利用率。
5. 请求量统计包含普通容器、init container 的峰值、restartable init sidecar 和 Pod overhead；排除已成功/失败结束的 Pod。Pod 无权读取或读取失败时分配量显示未知。
6. 组件同时检查 Pod Ready、容器 waiting/terminated 状态，避免将 Running 的 CrashLoopBackOff 当正常；历史 Completed 验证 Pod 不作为当前算法验证通过的证据。
7. 主机诊断缓存 30 秒，通过既有主机信息/心跳上报（worker 每 60 秒刷新完整信息）；集群关联只采用 IP 匹配且采集时间不超过 120 秒的证据。旧版本 agent 或离线节点显示未采集/过期，不把节点标签中的版本当作实时检查结果。
8. 驱动工具检查限时 3 秒并限制返回输出；kubectl 查询限时 20 秒。RuntimeClass 只是集群声明，不能证明每个节点都已配置。
9. 容器设备访问与算法兼容性始终独立标记，第一阶段没有执行计算任务，不能声称算法已验证。

## 10.0.0.140 集群盘点（2026-11-13）

通过跳板机对 140/141/142 逐台执行只读检查，并对照 Kubernetes API。没有改动节点驱动、运行时、设备插件或现有服务。

| 节点 | 加速器 | 主机驱动 | K3s 注册 |
|---|---|---|---|
| 10.0.0.140 / k3s-master | 无发现的计算加速卡，QEMU VGA 已排除 | 不适用 | 无 GPU 扩展资源 |
| 10.0.0.141 / k3s-w1 | NVIDIA GeForce RTX 5090 × 4，单卡 32607 MiB | 580.126.09，nvidia-smi 成功 | nvidia.com/gpu = 4 |
| 10.0.0.142 / k3s-w2 | NVIDIA GeForce RTX 5090 × 3，单卡 32607 MiB | 580.126.09，nvidia-smi 成功 | nvidia.com/gpu = 3 |

所有节点为 amd64 / Ubuntu 22.04.5；K3s v1.34.3+k3s1、containerd 2.1.5-k3s1。未发现昇腾、海光、AMD 或 Intel 计算加速设备，因此这些厂商没有完成硬件实测。

两个 GPU 节点的设备插件为 `nvcr.io/nvidia/k8s-device-plugin:v0.18.1`，工具包为 `nvcr.io/nvidia/k8s/container-toolkit:v1.18.1`，Pod 就绪；containerd 中存在 nvidia/nvidia-cdi handler，二进制位于 `/usr/local/nvidia/toolkit`。默认 PATH 中找不到 nvidia-container-runtime 不代表它未安装。

GPU Operator 控制器镜像 `nvcr.io/nvidia/gpu-operator:v25.10.1` 拉取失败（访问 nvcr.io manifest 请求 EOF），Pod 长期 ImagePullBackOff；现有插件仍在运行并注册资源，两者应分开判断。建议后续修复镜像访问或提供可验证的离线镜像，先不升级驱动或重装整个 GPU 栈。

GFD 的 CUDA 标签/驱动能力信息不是已安装 CUDA Toolkit 或算法兼容性的证明。现有 CUDA validator 是数月前完成的历史任务，不能作为本次新版本/算法的验证结果。

复现样本：`tests/fixtures/accelerator-cluster-140.json`，只包含节点设备信息和组件状态，不包含 kubeconfig、Secret 或容器环境变量。

新静态诊断二进制已临时上传到三台节点执行 `devices`，140 额外执行 `devices --cluster`，退出后清理临时目录。实际程序输出正确识别 0/4/3 个 PCI 加速设备、两个 GPU 节点的驱动 580.126.09、单卡 32607 MiB，并通过配置中的绝对路径执行 NVIDIA Container Runtime，确认版本 1.18.1。所有主机探测命令均通过；集群检查识别到 18 个相关组件及 1 个 Operator 异常。此次查询时已分配 GPU 请求为 0，这不等于主机 GPU 实际利用率为 0。

## 海光 DCU 实测（2026-11-13）

内蒙古 `np4-master`（麒麟 V10）有 6 个海光 DCU PCI 设备，厂商/设备 ID 为
`1d94:6370`，PCI 类别为 `0x0b4000`，绑定 `hycu` 驱动。这个类别不属于显示控制器
或处理加速器类别，必须单独识别；同厂商的 CPU、桥接、加密控制器和 SR-IOV 虚拟函数不计入物理 DCU 数量。

`hy-smi` 安装于 `/opt/hyhal/bin`，实际目标在 `/usr/local/hyhal/bin`，不一定出现在服务的 PATH 中。
使用只读 JSON 查询按 PCI 地址合并型号和显存，不按照 `cardN` 顺序或逻辑资源配额推算物理卡数。
实测为 BW100，单卡显存 65520 MiB，`/sys/module/hycu/version` 为 `6.3.30-V1.4.1a`。

同一节点的 HAMI DCU 设备插件注册了 `hygon.com/dcunum=24`。页面分别显示 6 个物理设备与
24 个插件逻辑资源；这不表示 24 张物理卡，也不表示已经运行过算法兼容性测试。
脱敏管理工具样本：`tests/fixtures/hygon-bw100-smi.json`。

## GPU/NPU/DCU 集成

K3s 资源面板新增同名按钮，先选择节点和方案，再生成修复计划。方案表也可从
`/accelerators/supported` 打开；链接保留 `/update/` 等反向代理前缀，页面需要登录。
所有错误界面提供“支持型号与架构表”链接。

第一批覆盖 NVIDIA amd64 与海光 BW100 DCU；HCU 仅维护现有 DaemonSet 插件。
昇腾、ARM64 NVIDIA、DPU 明确标记待适配。支持表区分实测设备发现、既有集成、离线包
准备和算法验证，避免把注册的逻辑资源数当作物理卡数。

离线组件集中在配套仓库 `cangling-repo/accelerators/packages/`，由主节点向代理分发，
须完整拉取 Git LFS。安装前校验 `SHA256SUMS` 和审核过的包版本；模板、安装器也在清单内。
NVIDIA Toolkit 仅自动安装到 Ubuntu 22.04/24.04 amd64；海光新增集成要求已有 BW100
驱动和 `/opt/dtk-26.04`。不安装内核驱动、固件，不覆盖自定义 containerd 模板，不混装
GPU Operator 或切换 HCU/DCU 插件。

已有健康插件与资源时无需修改。异常插件只重建所选节点上由 DaemonSet 管理的 Pod，
删除操作携带 UID 前提；新插件通过节点名称 affinity 限制部署范围。安装 NVIDIA
运行时前会暂时 cordon 节点，完成或失败后按本任务的标记恢复调度；原先 cordon 的节点
保持原状。升级服务或中断任务前应先核对执行状态，进程退出后不自动重试安装。

测试包括 Rust 检查策略回归、Node 界面转义和代理路径检查、Python 离线安装器检查，
以及 `scripts/test-accelerator-integration-http.py` 的独立模拟集群 HTTP 流程。
设置 `ACCELERATOR_HTTP_RUNTIME=1` 可测试运行时安装计划与调度状态恢复分支；模拟程序
不会操作真实 K3s。两种厂商模板另已通过内蒙古现有 K3s 的 server dry-run。
新增离线安装尚未在空白物理 GPU 节点上验收。
