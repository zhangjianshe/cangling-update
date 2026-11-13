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
