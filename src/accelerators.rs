//! Read-only accelerator inventory. Hardware, driver, runtime, registration and
//! algorithm validation are separate evidence levels; unknown is never healthy.
use crate::{error::AppError, state::AppState};
use axum::{extract::State, routing::get, Json, Router};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Seek},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/hostinfo/accelerators", get(host_report))
        .route("/api/k3s/accelerators", get(cluster_report))
}
async fn host_report() -> Result<Json<Value>, AppError> {
    tokio::task::spawn_blocking(local_report)
        .await
        .map(Json)
        .map_err(|e| AppError::internal(e.to_string()))
}
fn check(stage: &str, status: &str, message: impl Into<String>) -> Value {
    json!({"stage":stage,"status":status,"message":message.into()})
}
fn read(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_string()
}
fn vendor(id: &str) -> &'static str {
    match id {
        "0x10de" => "NVIDIA",
        "0x19e5" => "Ascend",
        "0x1d94" => "Hygon",
        "0x1002" => "AMD",
        "0x8086" => "Intel",
        _ => "Other",
    }
}
fn tool(name: &str) -> Option<PathBuf> {
    let mut dirs =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect::<Vec<_>>();
    dirs.extend(
        [
            "/usr/bin",
            "/usr/sbin",
            "/usr/local/bin",
            "/usr/local/nvidia/toolkit",
            "/usr/local/Ascend/driver/tools",
            "/opt/hyhal/bin",
            "/usr/local/hyhal/bin",
            "/opt/dtk/bin",
        ]
        .map(PathBuf::from),
    );
    tool_in_dirs(name, &dirs)
}
fn tool_in_dirs(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter().map(|p| p.join(name)).find(|p| executable(p))
}
fn executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
// Temporary output is private and immediately unlinked. Commands have a deadline,
// no shell, no inherited stdin and a bounded result. Never expose full runtime config.
fn probe(program: &Path, args: &[&str]) -> Value {
    let path = std::env::temp_dir().join(format!("cangling-device-{}", uuid::Uuid::new_v4()));
    let result = (|| -> std::io::Result<Value> {
        let mut f = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        fs::remove_file(&path)?;
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(f.try_clone()?)
            .stderr(f.try_clone()?)
            .spawn()?;
        let until = Instant::now() + Duration::from_secs(3);
        let status = loop {
            if let Some(s) = child.try_wait()? {
                break Some(s);
            }
            if Instant::now() >= until {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        f.rewind()?;
        let mut bytes = Vec::new();
        f.take(65536).read_to_end(&mut bytes)?;
        Ok(
            json!({"path":program.display().to_string(),"status":if status.is_none(){"timeout"}else if status.unwrap().success(){"ok"}else{"error"},"output":String::from_utf8_lossy(&bytes).trim()}),
        )
    })();
    let _ = fs::remove_file(path);
    result.unwrap_or_else(
        |e| json!({"path":program.display().to_string(),"status":"error","output":e.to_string()}),
    )
}
fn pci_devices(root: &Path) -> Vec<Value> {
    let mut devices = Vec::new();
    for e in fs::read_dir(root).into_iter().flatten().flatten() {
        let p = e.path();
        let class = read(p.join("class"));
        let id = read(p.join("vendor"));
        // Hygon DCUs use the processor/co-processor class rather than display/3D.
        // The same vendor also supplies host bridges and CPUs: vendor alone is insufficient.
        let hygon_dcu = id == "0x1d94" && class.starts_with("0x0b40");
        if !(class.starts_with("0x03") || class.starts_with("0x12") || hygon_dcu) {
            continue;
        }
        if hygon_dcu && p.join("physfn").exists() {
            continue; // SR-IOV virtual functions must not inflate the physical DCU count.
        }
        // BMC consoles and virtual VGA adapters are not algorithm accelerators.
        if ["0x1a03", "0x1234", "0x1af4", "0x1b36", "0x15ad", "0x1013"].contains(&id.as_str()) {
            continue;
        }
        let driver = fs::read_link(p.join("driver"))
            .ok()
            .and_then(|v| v.file_name().map(|v| v.to_string_lossy().into_owned()))
            .unwrap_or_default();
        devices.push(json!({"pciAddress":e.file_name().to_string_lossy(),"pciClass":class,"vendor":vendor(&id),"vendorId":id,"deviceId":read(p.join("device")),"name":format!("{} {}",vendor(&id),read(p.join("device"))),"driver":driver,"driverVersion":null,"memoryMiB":null}));
    }
    devices.sort_by_key(|d| d["pciAddress"].as_str().unwrap_or_default().to_string());
    devices
}
fn normalized_bus(bus: &str) -> String {
    let bus = bus.trim().to_ascii_lowercase();
    if bus.len() == 16 && bus.starts_with("00000000:") {
        bus[4..].to_string()
    } else {
        bus
    }
}
fn apply_nvidia(devices: &mut [Value], csv: &str) {
    for line in csv.lines() {
        let f = line.split(',').map(str::trim).collect::<Vec<_>>();
        if f.len() != 4 {
            continue;
        }
        let bus = normalized_bus(f[0]);
        if let Some(d) = devices
            .iter_mut()
            .find(|d| d["vendor"] == "NVIDIA" && d["pciAddress"] == bus)
        {
            d["name"] = json!(f[1]);
            d["driverVersion"] = json!(f[2]);
            d["memoryMiB"] = f[3].parse::<u64>().map_or(Value::Null, |n| json!(n));
        }
    }
}
fn apply_hygon(devices: &mut [Value], output: &str) -> usize {
    let Ok(Value::Object(cards)) = serde_json::from_str::<Value>(output) else {
        return 0;
    };
    let mut matched = BTreeSet::new();
    for card in cards.values() {
        let Some(bus) = card["PCI Bus"]
            .as_str()
            .and_then(|v| v.split_whitespace().next())
        else {
            continue;
        };
        let bus = normalized_bus(bus);
        let Some(device) = devices
            .iter_mut()
            .find(|d| d["vendor"] == "Hygon" && d["pciAddress"] == bus)
        else {
            continue;
        };
        if let Some(name) = ["Card Series", "Card series", "Card Model", "Card model"]
            .iter()
            .find_map(|key| card[*key].as_str().filter(|v| !v.trim().is_empty()))
        {
            device["name"] = json!(name.trim());
        }
        let number = |key: &str| {
            card[key]
                .as_u64()
                .or_else(|| card[key].as_str()?.trim().parse::<u64>().ok())
        };
        let memory = number("vram Total Memory (MiB)")
            .or_else(|| number("VRAM Total Memory (MiB)"))
            .or_else(|| number("VRAM Total Memory (B)").map(|v| v / (1024 * 1024)));
        if let Some(memory) = memory.filter(|v| *v > 0) {
            device["memoryMiB"] = json!(memory);
        }
        matched.insert(bus);
    }
    matched.len()
}
fn runtime_entries(text: &str) -> Vec<Value> {
    let mut handler = String::new();
    let mut entries = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            handler.clear();
            if let Some((_, tail)) = line.split_once(".runtimes.") {
                handler = tail
                    .trim_end_matches(']')
                    .split('.')
                    .next()
                    .unwrap_or_default()
                    .trim_matches(['\'', '"'])
                    .to_string();
            }
        } else if let Some((k, v)) = line.split_once('=') {
            if k.trim() == "BinaryName" && !handler.is_empty() {
                let binary = v.trim().trim_matches(['\'', '"']);
                entries.push(json!({"handler":handler,"binary":binary,"executable":executable(Path::new(binary))}));
            }
        }
    }
    entries
}
pub fn local_report() -> Value {
    static CACHE: OnceLock<Mutex<Option<(Instant, Value)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    // Serialize collection as well as cache access: concurrent heartbeats must not
    // start multiple driver probes on a busy node.
    let mut guard = cache.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((at, v)) = &*guard {
        if at.elapsed() < Duration::from_secs(30) {
            return v.clone();
        }
    }
    let v = collect_local();
    *guard = Some((Instant::now(), v.clone()));
    v
}
fn collect_local() -> Value {
    let mut devices = pci_devices(Path::new("/sys/bus/pci/devices"));
    for device in &mut devices {
        if device["vendor"] == "Hygon" {
            let driver = device["driver"].as_str().unwrap_or("");
            if !driver.is_empty() {
                let version = read(Path::new("/sys/module").join(driver).join("version"));
                if !version.is_empty() {
                    device["driverVersion"] = json!(version);
                }
            }
        }
    }
    let mut tools = Vec::new();
    let mut checks = Vec::new();
    let vendors = devices
        .iter()
        .filter_map(|d| d["vendor"].as_str())
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    if !devices.is_empty() {
        if let Some(p) = tool("lspci") {
            let result = probe(&p, &["-D", "-nn"]);
            if result["status"] == "ok" {
                for line in result["output"].as_str().unwrap_or_default().lines() {
                    if let Some((addr, rest)) = line.split_once(' ') {
                        if let Some(d) = devices.iter_mut().find(|d| d["pciAddress"] == addr) {
                            if let Some((_, name)) = rest.split_once(": ") {
                                d["name"] = json!(name);
                            }
                        }
                    }
                }
            }
        }
    }
    for v in &vendors {
        let spec = match v.as_str() {
            "NVIDIA" => Some((
                "nvidia-smi",
                vec![
                    "--query-gpu=pci.bus_id,name,driver_version,memory.total",
                    "--format=csv,noheader,nounits",
                ],
            )),
            "Ascend" => Some(("npu-smi", vec!["info", "-l"])),
            "Hygon" => Some((
                "hy-smi",
                vec![
                    "--showproductname",
                    "--showbus",
                    "--showdriverversion",
                    "--showmeminfo",
                    "vram",
                    "--json",
                ],
            )),
            "AMD" => Some(("rocm-smi", vec!["--showproductname"])),
            _ => None,
        };
        if let Some((bin, args)) = spec {
            let result=tool(bin).map(|p|probe(&p,&args)).unwrap_or_else(||json!({"path":bin,"status":"unknown","output":"未找到管理工具，不能确认驱动可用性"}));
            if v == "NVIDIA" && result["status"] == "ok" {
                apply_nvidia(&mut devices, result["output"].as_str().unwrap_or_default());
            }
            let incomplete_hygon = v == "Hygon"
                && result["status"] == "ok"
                && apply_hygon(&mut devices, result["output"].as_str().unwrap_or_default())
                    != devices.iter().filter(|d| d["vendor"] == "Hygon").count();
            let incomplete_nvidia = v == "NVIDIA"
                && result["status"] == "ok"
                && devices
                    .iter()
                    .filter(|d| d["vendor"] == "NVIDIA")
                    .any(|d| d["driverVersion"].is_null());
            checks.push(check(
                "driver",
                if incomplete_nvidia || incomplete_hygon {
                    "warning"
                } else {
                    result["status"].as_str().unwrap_or("unknown")
                },
                format!("{v}: {}", result["output"].as_str().unwrap_or_default()),
            ));
            tools.push(json!({"vendor":v,"name":bin,"result":result}));
        } else {
            checks.push(check(
                "driver",
                "unknown",
                format!("{v}: 已识别 PCI 设备；尚未验证厂商管理工具"),
            ));
        }
    }
    for d in &devices {
        if d["driver"].as_str().unwrap_or_default().is_empty() {
            checks.push(check(
                "driver",
                "error",
                format!("{} 未绑定内核驱动", d["pciAddress"]),
            ));
        }
    }
    if devices.is_empty() {
        checks.push(check(
            "hardware",
            "unknown",
            "未发现可识别的 PCI 加速设备；不代表虚拟设备或权限受限环境中没有设备",
        ));
    }
    let config_path = Path::new("/var/lib/rancher/k3s/agent/etc/containerd/config.toml");
    let mut runtimes = runtime_entries(&read(config_path));
    for r in &mut runtimes {
        if r["executable"] == true && r["handler"] == "nvidia" {
            let result = probe(
                Path::new(r["binary"].as_str().unwrap_or("")),
                &["--version"],
            );
            r["versionProbe"] = result;
        }
    }
    for r in &runtimes {
        checks.push(check(
            "runtime",
            if r["executable"] == true {
                "ok"
            } else {
                "error"
            },
            format!(
                "{} → {}{}",
                r["handler"].as_str().unwrap_or(""),
                r["binary"].as_str().unwrap_or(""),
                if r["executable"] == true {
                    "（文件可执行，尚未验证容器）"
                } else {
                    "（缺失或不可执行）"
                }
            ),
        ));
    }
    if vendors.contains("NVIDIA")
        && !runtimes.iter().any(|r| {
            r["handler"]
                .as_str()
                .is_some_and(|h| h.starts_with("nvidia"))
        })
    {
        checks.push(check(
            "runtime",
            "unknown",
            "未在 K3s containerd 配置中发现 NVIDIA 运行时；需进一步核对 CDI 或外部运行时配置",
        ));
    }
    checks.push(check(
        "container",
        "unknown",
        "未启动测试容器，容器内设备访问尚未验证",
    ));
    checks.push(check("algorithm", "unknown", "尚未执行算法兼容性测试"));
    json!({"platform":crate::repo::host_platform(),"deviceFiles":{"kfd":Path::new("/dev/kfd").exists(),"dtk2604":Path::new("/opt/dtk-26.04").is_dir()},"customRuntimeTemplate":(["config.toml.tmpl","config-v3.toml.tmpl"].iter().any(|n|Path::new("/var/lib/rancher/k3s/agent/etc/containerd").join(n).exists())),"observedAt":chrono::Utc::now().to_rfc3339(),"hostname":crate::hostinfo::hostname(),"ips":crate::hostinfo::list_ips(),"devices":devices,"tools":tools,"runtimeConfig":if config_path.is_file(){Some(config_path.display().to_string())}else{None},"runtimes":runtimes,"checks":checks})
}
fn items(v: &Value) -> Vec<Value> {
    v["items"].as_array().cloned().unwrap_or_default()
}
fn quantity(v: &Value) -> u64 {
    v.as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| v.as_u64())
        .unwrap_or(0)
}
fn container_request(c: &Value, key: &str) -> u64 {
    let r = &c["resources"];
    quantity(r["requests"].get(key).unwrap_or(&r["limits"][key]))
}
fn pod_request(p: &Value, key: &str) -> u64 {
    let regular = p["spec"]["containers"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| container_request(c, key))
        .sum::<u64>();
    let mut sidecars = 0;
    let mut init_peak = 0;
    for c in p["spec"]["initContainers"].as_array().into_iter().flatten() {
        let n = container_request(c, key);
        if c["restartPolicy"] == "Always" {
            sidecars += n;
            init_peak = init_peak.max(sidecars);
        } else {
            init_peak = init_peak.max(sidecars + n);
        }
    }
    (regular + sidecars).max(init_peak) + quantity(&p["spec"]["overhead"][key])
}
fn component(p: &Value) -> bool {
    let text = format!(
        "{} {}",
        p["metadata"]["name"].as_str().unwrap_or(""),
        p["spec"]["containers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c["image"].as_str())
            .collect::<Vec<_>>()
            .join(" ")
    )
    .to_lowercase();
    [
        "device-plugin",
        "deviceplugin",
        "gpu-operator",
        "gpu-feature",
        "nvidia-container-toolkit",
        "dcgm",
        "npu-exporter",
        "ascend-plugin",
    ]
    .iter()
    .any(|s| text.contains(s))
}
fn component_report(p: &Value) -> Value {
    let phase = p["status"]["phase"].as_str().unwrap_or("Unknown");
    let mut failures = Vec::new();
    for c in p["status"]["initContainerStatuses"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(
            p["status"]["containerStatuses"]
                .as_array()
                .into_iter()
                .flatten(),
        )
    {
        if let Some(reason) = c["state"]["waiting"]["reason"].as_str() {
            if !matches!(reason, "ContainerCreating" | "PodInitializing") {
                failures.push(format!(
                    "{reason}: {}",
                    c["state"]["waiting"]["message"].as_str().unwrap_or("")
                ));
            }
        }
        if c["state"]["terminated"]["exitCode"]
            .as_i64()
            .is_some_and(|n| n != 0)
        {
            failures.push(format!(
                "{}: {}",
                c["name"].as_str().unwrap_or(""),
                c["state"]["terminated"]["reason"]
                    .as_str()
                    .unwrap_or("Error")
            ));
        }
    }
    let ready = p["status"]["conditions"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|c| c["type"] == "Ready" && c["status"] == "True");
    let status = if phase == "Succeeded" {
        "completed"
    } else if !failures.is_empty() || phase == "Failed" {
        "error"
    } else if ready && phase == "Running" {
        "ok"
    } else {
        "unknown"
    };
    json!({"uid":p["metadata"]["uid"],"owners":p["metadata"]["ownerReferences"],"name":p["metadata"]["name"],"namespace":p["metadata"]["namespace"],"node":p["spec"]["nodeName"],"phase":phase,"status":status,"images":p["spec"]["containers"].as_array().into_iter().flatten().map(|c|c["image"].clone()).collect::<Vec<_>>(),"message":failures.join("; ").chars().take(2000).collect::<String>()})
}
fn matching_host<'a>(node: &Value, hosts: &'a [Value]) -> Option<&'a Value> {
    let ips = node["status"]["addresses"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| a["type"] == "InternalIP")
        .filter_map(|a| a["address"].as_str())
        .collect::<Vec<_>>();
    hosts.iter().find(|h| {
        let fresh = h["observedAt"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| chrono::Utc::now().signed_duration_since(t).num_seconds())
            .is_some_and(|age| (0..=120).contains(&age));
        fresh
            && h["ips"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .any(|ip| ips.contains(&ip))
    })
}
pub(crate) fn build_cluster(
    nodes: &Value,
    pods: Option<&Value>,
    classes: Option<&Value>,
    hosts: &[Value],
    mut warnings: Vec<String>,
) -> Value {
    let pod_items = pods.map(items).unwrap_or_default();
    let mut components = Vec::new();
    for p in &pod_items {
        if component(p) {
            components.push(component_report(p));
        }
    }
    for c in &components {
        if c["status"] == "error" {
            warnings.push(format!(
                "{}/{}: {}",
                c["namespace"].as_str().unwrap_or(""),
                c["name"].as_str().unwrap_or(""),
                c["message"].as_str().unwrap_or("")
            ));
        }
    }
    let reports=items(nodes).iter().map(|n|{
        let name=n["metadata"]["name"].as_str().unwrap_or("");let labels=&n["metadata"]["labels"];let mut keys=BTreeSet::new();
        for field in ["capacity","allocatable"]{if let Some(m)=n["status"][field].as_object(){keys.extend(m.keys().filter(|k|k.contains('/')).cloned());}}
        let resources=keys.iter().map(|key|{
            let requested=pods.map(|_|pod_items.iter().filter(|p|p["spec"]["nodeName"]==name && !matches!(p["status"]["phase"].as_str(),Some("Succeeded"|"Failed"))).map(|p|pod_request(p,key)).sum::<u64>());
            let available=quantity(&n["status"]["allocatable"][key]);
            json!({"name":key,"capacity":quantity(&n["status"]["capacity"][key]),"allocatable":available,"requested":requested,"free":requested.map(|r|available.saturating_sub(r)),"overcommitted":requested.map(|r|r>available)})
        }).collect::<Vec<_>>();
        let host=matching_host(n,hosts);let ready=n["status"]["conditions"].as_array().into_iter().flatten().any(|c|c["type"]=="Ready"&&c["status"]=="True");
        let node_components=components.iter().filter(|c|c["node"]==name).cloned().collect::<Vec<_>>();
        let mut checks=vec![check("node",if ready{"ok"}else{"error"},if ready{"节点 Ready"}else{"节点未就绪"}),check("registration",if resources.is_empty(){"unknown"}else if resources.iter().any(|r|quantity(&r["allocatable"])==0){"warning"}else{"ok"},if resources.is_empty(){"未注册扩展设备资源；普通 CPU 节点无需安装 GPU 插件"}else{"插件注册的逻辑资源数量，不等于物理卡数或实际利用率"})];
        if pods.is_none(){checks.push(check("plugin","unknown","无权读取 Pod 或读取失败，不能判定设备插件状态"));}
        else if node_components.iter().any(|c|c["status"]=="error"){checks.push(check("plugin","error","节点存在异常的加速器组件，请查看组件列表"));}
        else if node_components.iter().any(|c|c["status"]=="ok" && c["name"].as_str().is_some_and(|s|s.contains("device-plugin") || s.contains("deviceplugin"))) {
            checks.push(check("plugin","ok","已观察到该节点的设备插件 Pod 就绪；尚未验证实际设备分配"));
        } else {checks.push(check("plugin","unknown","未识别到就绪的设备插件 Pod；可在组件列表核对自定义插件"));}
        if let Some(h)=host {
            if h["devices"].as_array().is_some_and(|d|!d.is_empty()) && resources.is_empty() {
                checks.push(check("registration","error","主机已发现加速设备，但节点没有注册任何扩展设备资源"));
            }
        }
        if host.is_none(){checks.push(check("driver","unknown","未收到该节点最近 120 秒内的主机诊断；节点标签中的驱动版本只作参考"));checks.push(check("runtime","unknown","RuntimeClass 是集群级声明，不能证明该节点已配置对应运行时"));}
        checks.push(check("algorithm","unknown","尚未运行算法验证任务"));
        let device_labels=labels.as_object().into_iter().flatten().filter(|(k,_)|["nvidia.com/","huawei.com/","hygon.com/","amd.com/","gpu.intel.com/","accelerator","gpu.type"].iter().any(|prefix|k.starts_with(prefix)) || keys.iter().any(|r|r.split_once('/').is_some_and(|(domain,_)| k.starts_with(&format!("{domain}/"))))).map(|(k,v)|(k.clone(),v.clone())).collect::<serde_json::Map<_,_>>();
        json!({"name":name,"addresses":n["status"]["addresses"],"nodeInfo":n["status"]["nodeInfo"],"ready":ready,"unschedulable":n["spec"]["unschedulable"].as_bool().unwrap_or(false),"labels":device_labels,"resources":resources,"components":node_components,"hostDiagnostics":host,"checks":checks})
    }).collect::<Vec<_>>();
    json!({"observedAt":chrono::Utc::now().to_rfc3339(),"nodes":reports,"components":components,"runtimeClasses":classes.map(items).unwrap_or_default().iter().map(|v|json!({"name":v["metadata"]["name"],"handler":v["handler"],"scheduling":v["scheduling"]})).collect::<Vec<_>>(),"warnings":warnings,"readOnly":true})
}
async fn fetch(kind: &str) -> Result<Value, AppError> {
    let mut args = vec!["get".into(), kind.into(), "-o".into(), "json".into()];
    if kind == "pods" {
        args.push("--all-namespaces".into());
    }
    let s = crate::k3s_resources::kubectl(args, Duration::from_secs(20)).await?;
    serde_json::from_str(&s).map_err(|e| AppError::internal(format!("解析设备资源失败：{e}")))
}
pub(crate) async fn cluster_inventory(hosts: Vec<Value>) -> Result<Value, AppError> {
    let (nodes, pods, classes) =
        tokio::join!(fetch("nodes"), fetch("pods"), fetch("runtimeclasses"));
    let nodes = nodes?;
    let mut warnings = Vec::new();
    if let Err(e) = &pods {
        warnings.push(format!("读取 Pod 失败：{e}"));
    }
    if let Err(e) = &classes {
        warnings.push(format!("读取 RuntimeClass 失败：{e}"));
    }
    Ok(build_cluster(
        &nodes,
        pods.as_ref().ok(),
        classes.as_ref().ok(),
        &hosts,
        warnings,
    ))
}
pub(crate) async fn cluster_report(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let mut hosts = vec![tokio::task::spawn_blocking(local_report)
        .await
        .map_err(|e| AppError::internal(e.to_string()))?];
    {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        let mut stmt = conn.prepare("SELECT info_json FROM cluster_nodes")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for row in rows {
            if let Ok(v) = serde_json::from_str::<Value>(&row?) {
                if v["accelerator_diagnostics"].is_object() {
                    hosts.push(v["accelerator_diagnostics"].clone());
                }
            }
        }
    }
    cluster_inventory(hosts).await.map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_cluster_preserves_seven_cards_and_reports_operator_pull_failure() {
        let f: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/accelerator-cluster-140.json"
        ))
        .unwrap();
        let r = build_cluster(
            &f["nodes"],
            Some(&f["pods"]),
            Some(&f["classes"]),
            &[],
            vec![],
        );
        let nodes = r["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 3);
        assert!(nodes[0]["resources"].as_array().unwrap().is_empty());
        let total = nodes
            .iter()
            .flat_map(|n| n["resources"].as_array().unwrap())
            .map(|v| quantity(&v["allocatable"]))
            .sum::<u64>();
        assert_eq!(total, 7);
        assert!(r["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s.as_str().unwrap().contains("ImagePullBackOff")));
        for n in nodes {
            assert!(n["hostDiagnostics"].is_null());
            assert!(n["checks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["stage"] == "driver" && c["status"] == "unknown"));
        }
    }
    #[test]
    fn unfamiliar_and_virtual_resources_are_not_dropped_and_allocations_are_not_utilization() {
        let n = json!({"items":[{"metadata":{"name":"n","labels":{"vendor.test/model":"Custom NPU"}},"status":{"capacity":{"vendor.test/partition":"8"},"allocatable":{"vendor.test/partition":"8"}}}]});
        let make = |phase: &str, init: Value| json!({"spec":{"nodeName":"n","containers":[{"resources":{"limits":{"vendor.test/partition":"2"}}}],"initContainers":init},"status":{"phase":phase}});
        let p = json!({"items":[make("Running",json!([{"resources":{"limits":{"vendor.test/partition":"3"}}}])),make("Succeeded",json!([])),make("Failed",json!([]))]});
        let r = build_cluster(&n, Some(&p), None, &[], vec![]);
        let q = &r["nodes"][0]["resources"][0];
        assert_eq!(r["nodes"][0]["labels"]["vendor.test/model"], "Custom NPU");
        assert_eq!(q["name"], "vendor.test/partition");
        assert_eq!(q["requested"], 3);
        assert_eq!(q["free"], 5);
        let r = build_cluster(&n, None, None, &[], vec![]);
        assert!(r["nodes"][0]["resources"][0]["requested"].is_null());
        assert!(r["nodes"][0]["resources"][0]["free"].is_null());
    }
    #[test]
    fn restartable_init_and_terminal_pods_use_kubernetes_allocation_rules() {
        let p = json!({"spec":{"containers":[{"resources":{"requests":{"x/gpu":"2"}}}],"initContainers":[{"restartPolicy":"Always","resources":{"limits":{"x/gpu":"1"}}},{"resources":{"limits":{"x/gpu":"4"}}}]}});
        assert_eq!(pod_request(&p, "x/gpu"), 5);
    }
    #[test]
    fn running_phase_does_not_hide_a_crashing_plugin_and_completed_is_not_current_validation() {
        let p = json!({"metadata":{"name":"vendor-device-plugin"},"status":{"phase":"Running","containerStatuses":[{"state":{"waiting":{"reason":"CrashLoopBackOff","message":"driver missing"}}}]}});
        let r = component_report(&p);
        assert_eq!(r["status"], "error");
        assert!(r["message"].as_str().unwrap().contains("CrashLoopBackOff"));
        let r = component_report(&json!({"status":{"phase":"Succeeded"}}));
        assert_eq!(r["status"], "completed");
        let starting = component_report(
            &json!({"status":{"phase":"Pending","containerStatuses":[{"state":{"waiting":{"reason":"ContainerCreating"}}}]}}),
        );
        assert_eq!(starting["status"], "unknown");
    }
    #[test]
    fn driver_evidence_requires_fresh_host_ip_match() {
        let n = json!({"status":{"addresses":[{"type":"InternalIP","address":"10.0.0.141"}]}});
        let h = json!({"observedAt":chrono::Utc::now().to_rfc3339(),"ips":["10.0.0.141"]});
        assert!(matching_host(&n, &[h.clone()]).is_some());
        let mut stale = h.clone();
        stale["observedAt"] =
            json!((chrono::Utc::now() - chrono::Duration::minutes(3)).to_rfc3339());
        assert!(matching_host(&n, &[stale]).is_none());
        let mut other = h;
        other["ips"] = json!(["10.0.0.142"]);
        assert!(matching_host(&n, &[other]).is_none());
    }
    #[test]
    fn toolkit_runtime_outside_path_and_driver_csv_are_discovered() {
        let entries=runtime_entries("[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.'nvidia'.options]\nBinaryName = \"/usr/local/nvidia/toolkit/nvidia-container-runtime\"\n");
        assert_eq!(entries[0]["handler"], "nvidia");
        assert_eq!(
            entries[0]["binary"],
            "/usr/local/nvidia/toolkit/nvidia-container-runtime"
        );
        let mut d = vec![json!({"vendor":"NVIDIA","pciAddress":"0000:01:00.0"})];
        apply_nvidia(
            &mut d,
            "00000000:01:00.0, NVIDIA GeForce RTX 5090, 580.126.09, 32607\n",
        );
        assert_eq!(d[0]["driverVersion"], "580.126.09");
        assert_eq!(d[0]["memoryMiB"], 32607);
    }
    #[test]
    fn pci_scan_excludes_bmc_virtual_display_and_host_bridges() {
        let root = std::env::temp_dir().join(format!("cangling-pci-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        for (addr, vendor, class) in [
            ("a", "0x10de", "0x030000"),
            ("b", "0x1234", "0x030000"),
            ("c", "0x1a03", "0x030000"),
            ("d", "0x1d94", "0x060000"),
            ("e", "0x19e5", "0x120000"),
            ("f", "0x9999", "0x120000"),
        ] {
            let p = root.join(addr);
            fs::create_dir(&p).unwrap();
            fs::write(p.join("vendor"), vendor).unwrap();
            fs::write(p.join("class"), class).unwrap();
            fs::write(p.join("device"), "0x1234").unwrap();
        }
        let d = pci_devices(&root);
        assert_eq!(d.len(), 3);
        assert_eq!(d[0]["vendor"], "NVIDIA");
        assert_eq!(d[1]["vendor"], "Ascend");
        assert_eq!(d[2]["vendor"], "Other");
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn hygon_coprocessors_are_counted_without_host_bridges_or_virtual_functions() {
        let root = std::env::temp_dir().join(format!("cangling-hygon-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        for i in 0..6 {
            let p = root.join(format!("0000:{i:02x}:00.0"));
            fs::create_dir(&p).unwrap();
            fs::write(p.join("vendor"), "0x1d94").unwrap();
            fs::write(p.join("class"), "0x0b4000").unwrap();
            fs::write(p.join("device"), "0x6370").unwrap();
        }
        for (name, vendor, class) in [
            ("bridge", "0x1d94", "0x060000"),
            ("pcie", "0x1d94", "0x060400"),
            ("crypto", "0x1d94", "0x108000"),
            ("processor", "0x1d94", "0x130000"),
            ("other-coprocessor", "0x9999", "0x0b4000"),
            ("virtual-function", "0x1d94", "0x0b4000"),
        ] {
            let p = root.join(name);
            fs::create_dir(&p).unwrap();
            fs::write(p.join("vendor"), vendor).unwrap();
            fs::write(p.join("class"), class).unwrap();
            fs::write(p.join("device"), "0x1480").unwrap();
        }
        std::os::unix::fs::symlink(
            root.join("0000:00:00.0"),
            root.join("virtual-function/physfn"),
        )
        .unwrap();
        let devices = pci_devices(&root);
        assert_eq!(devices.len(), 6);
        assert!(devices
            .iter()
            .all(|d| d["vendor"] == "Hygon" && d["pciClass"] == "0x0b4000"));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn hygon_smi_inventory_matches_pci_addresses_and_deduplicates_cards() {
        let mut inventory: Value =
            serde_json::from_str(include_str!("../tests/fixtures/hygon-bw100-smi.json")).unwrap();
        let mut devices = inventory.as_object().unwrap().values().map(|card| {
            json!({"vendor":"Hygon", "pciAddress":card["PCI Bus"].as_str().unwrap().split_whitespace().next().unwrap(), "name":"unknown", "memoryMiB":null})
        }).collect::<Vec<_>>();
        devices.reverse();
        inventory["duplicate"] = inventory["card0"].clone();
        inventory["unmatched"] =
            json!({"PCI Bus":"0000:ff:00.0", "Card Series":"do not invent a device"});
        assert_eq!(apply_hygon(&mut devices, &inventory.to_string()), 6);
        assert_eq!(devices.len(), 6);
        assert!(devices
            .iter()
            .all(|d| d["name"] == "BW100" && d["memoryMiB"] == 65520));
        let before = devices.clone();
        assert_eq!(
            apply_hygon(&mut devices, "unparseable management output"),
            0
        );
        assert_eq!(devices, before);
    }
    #[test]
    fn hygon_smi_accepts_byte_memory_units_and_does_not_change_other_vendors() {
        let mut devices = vec![
            json!({"vendor":"Hygon","pciAddress":"0000:06:00.0","memoryMiB":null}),
            json!({"vendor":"NVIDIA","pciAddress":"0000:09:00.0","memoryMiB":null}),
        ];
        let inventory = json!({"card0":{"PCI Bus":"00000000:06:00.0", "VRAM Total Memory (B)":65520_u64*1024*1024},
                               "card1":{"PCI Bus":"0000:09:00.0", "vram Total Memory (MiB)":"65520"}});
        assert_eq!(apply_hygon(&mut devices, &inventory.to_string()), 1);
        assert_eq!(devices[0]["memoryMiB"], 65520);
        assert!(devices[1]["memoryMiB"].is_null());
    }
    #[test]
    fn management_tool_resolution_supports_non_path_directories_and_executable_symlinks() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("cangling-tool-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let first = root.join("non-executable");
        let second = root.join("hyhal/bin");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(first.join("hy-smi"), "not executable").unwrap();
        fs::set_permissions(first.join("hy-smi"), fs::Permissions::from_mode(0o600)).unwrap();
        let executable = root.join("real-hy-smi");
        fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&executable, second.join("hy-smi")).unwrap();
        assert_eq!(
            tool_in_dirs("hy-smi", &[first, second.clone()]),
            Some(second.join("hy-smi"))
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn probes_fail_promptly_for_missing_tools_and_time_out_without_hanging() {
        let r = probe(Path::new("/no-such-cangling-tool"), &[]);
        assert_eq!(r["status"], "error");
        let now = Instant::now();
        let r = probe(Path::new("/bin/sleep"), &["10"]);
        assert_eq!(r["status"], "timeout");
        assert!(now.elapsed() < Duration::from_secs(5));
    }
}

#[cfg(test)]
mod compatibility_tests {
    use super::*;
    #[test]
    fn old_worker_snapshots_and_mig_children_do_not_create_false_physical_devices() {
        let host: crate::hostinfo::HostSnapshot =
            serde_json::from_value(json!({"hostname":"old-worker"})).unwrap();
        assert!(host.accelerator_diagnostics.is_none());
        let g=crate::hostinfo::parse_nvidia_smi_l("GPU 0: NVIDIA A100 (UUID: GPU-a)\n  MIG 1g.5gb Device 0: (UUID: MIG-a)\nFailed to initialize NVML\n");
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].count, 1);
        assert!(crate::hostinfo::parse_lspci_display("00:01.0 VGA compatible controller: Device 1234:1111\n00:02.0 VGA compatible controller: ASPEED Technology, Inc. ASPEED Graphics Family\n").is_empty());
    }
}
