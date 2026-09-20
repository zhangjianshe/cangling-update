# K3s 离线安装 NVIDIA GPU 支持

本文记录在无互联网访问的 Ubuntu 22.04 amd64 主机上，为 K3s 安装 NVIDIA Container Toolkit 和 NVIDIA Kubernetes Device Plugin 的完整流程。

## 已验证环境

- 操作系统：Ubuntu 22.04 amd64
- K3s：`v1.30.13-rc1+k3s1`
- GPU：3 × NVIDIA A100 80GB PCIe
- NVIDIA 驱动：`615.71.09`
- NVIDIA Container Toolkit：`1.20.0`
- NVIDIA Device Plugin：`v0.19.3`
- CUDA 验证镜像：`nvcr.io/nvidia/k8s/cuda-sample:vectoradd-cuda12.5.0`

目标主机必须已经安装 NVIDIA 驱动，且 `nvidia-smi` 能正常显示 GPU。此离线包不包含 GPU 驱动。

## 离线包位置

离线文件保存在 `cangling-repo/linux-x86/nvidia-k3s/`：

```text
install.sh
libnvidia-container1_1.20.0-1_amd64.deb
libnvidia-container-tools_1.20.0-1_amd64.deb
nvidia-container-toolkit-base_1.20.0-1_amd64.deb
nvidia-container-toolkit_1.20.0-1_amd64.deb
nvidia-k3s-images.tar
nvidia-device-plugin.yml
nvidia-vectoradd-test.yml
SHA256SUMS
```

`nvidia-k3s-images.tar` 包含以下镜像：

- `nvcr.io/nvidia/k8s-device-plugin:v0.19.3`
- `nvcr.io/nvidia/k8s/cuda-sample:vectoradd-cuda12.5.0`

## 一键安装

在每个带 NVIDIA GPU 的 K3s 节点上执行：

```bash
cd /path/to/cangling-repo/linux-x86/nvidia-k3s
sudo ./install.sh
```

脚本会依次完成：

1. 校验 CPU 架构、NVIDIA 驱动、K3s 和离线文件；
2. 安装 NVIDIA Container Toolkit；
3. 重启本节点的 `k3s` 或 `k3s-agent` 服务，让 K3s 自动生成 NVIDIA runtime 配置；
4. 将 Device Plugin 和 CUDA 测试镜像导入本节点的 K3s containerd；
5. 如果当前节点可访问 Kubernetes API，则部署 Device Plugin；
6. 等待节点上报 `nvidia.com/gpu`，并运行 CUDA VectorAdd 测试。

在 agent 节点执行时通常只能完成前四步，这是正常现象；在 server 节点执行后会统一创建 DaemonSet。

如只安装、不运行 CUDA 测试：

```bash
RUN_GPU_TEST=0 sudo ./install.sh
```

## 手工安装过程

### 1. 检查宿主机

```bash
uname -m
nvidia-smi
k3s --version
```

应为 `x86_64`，且 `nvidia-smi` 能看到 GPU。

### 2. 安装 Container Toolkit

```bash
sudo dpkg -i \
  libnvidia-container1_1.20.0-1_amd64.deb \
  libnvidia-container-tools_1.20.0-1_amd64.deb \
  nvidia-container-toolkit-base_1.20.0-1_amd64.deb \
  nvidia-container-toolkit_1.20.0-1_amd64.deb

nvidia-container-runtime --version
nvidia-ctk --version
```

### 3. 重启 K3s

K3s 会在启动时检测 PATH 中的 `nvidia-container-runtime`，并生成 containerd runtime 配置：

```bash
sudo systemctl restart k3s       # server 节点
# 或
sudo systemctl restart k3s-agent # agent 节点

grep -A 12 -B 2 nvidia \
  /var/lib/rancher/k3s/agent/etc/containerd/config.toml
k3s kubectl get runtimeclass nvidia
```

配置中应包含：

```toml
[plugins."io.containerd.grpc.v1.cri".containerd.runtimes."nvidia"]
  runtime_type = "io.containerd.runc.v2"
[plugins."io.containerd.grpc.v1.cri".containerd.runtimes."nvidia".options]
  BinaryName = "/usr/bin/nvidia-container-runtime"
```

不要直接修改 K3s 自动生成的 `config.toml`，服务重启后该文件会重新生成。

### 4. 导入镜像并部署插件

每个 GPU 节点都要导入镜像：

```bash
sudo k3s ctr images import nvidia-k3s-images.tar
```

在 K3s server 节点部署插件：

```bash
sudo k3s kubectl apply -f nvidia-device-plugin.yml
sudo k3s kubectl rollout status \
  daemonset/nvidia-device-plugin-daemonset \
  -n kube-system --timeout=180s
```

清单显式设置了 `runtimeClassName: nvidia`。GPU 工作负载也应设置该字段，并声明 GPU 数量：

```yaml
spec:
  runtimeClassName: nvidia
  containers:
  - name: application
    resources:
      limits:
        nvidia.com/gpu: 1
```

### 5. 验证资源与 CUDA

```bash
k3s kubectl get node \
  -o custom-columns=NAME:.metadata.name,GPU:.status.allocatable.nvidia\\.com/gpu

k3s kubectl apply -f nvidia-vectoradd-test.yml
k3s kubectl wait --for=jsonpath='{.status.phase}'=Succeeded \
  pod/nvidia-vectoradd-test --timeout=180s
k3s kubectl logs nvidia-vectoradd-test
```

成功日志应包含：

```text
Test PASSED
Done
```

测试完成后可删除 Pod：

```bash
k3s kubectl delete pod nvidia-vectoradd-test
```

## 故障排查

查看插件状态和日志：

```bash
k3s kubectl get pods -n kube-system -l name=nvidia-device-plugin-ds -o wide
k3s kubectl logs -n kube-system daemonset/nvidia-device-plugin-daemonset
```

插件正常但节点没有 `nvidia.com/gpu` 时，依次检查：

```bash
nvidia-smi
nvidia-container-runtime --version
grep -n nvidia /var/lib/rancher/k3s/agent/etc/containerd/config.toml
k3s kubectl get runtimeclass nvidia
```

如果 Pod 报找不到 NVIDIA runtime，确认清单包含 `runtimeClassName: nvidia`，然后重启对应节点的 `k3s` 或 `k3s-agent`。

