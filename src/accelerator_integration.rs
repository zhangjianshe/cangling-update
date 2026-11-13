//! Explicit, node-scoped offline integration. Kernel drivers and Operator deployments are never replaced.
use crate::{error::AppError, state::AppState};
use axum::{
    extract::{Path, State},
    response::Html,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, time::Duration};

const LOCK: &str = "accelerator-integration";
const PACKAGES: &[&str] = &["nvidia-amd64", "hygon-dcu-amd64"];
fn array(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn string(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}
fn hash(v: &[u8]) -> String {
    format!("{:x}", Sha256::digest(v))
}
fn catalog() -> Value {
    serde_json::from_str(include_str!("assets/accelerator-catalog.json")).expect("embedded catalog")
}
fn profile(id: &str) -> Result<Value, AppError> {
    array(&catalog()["profiles"])
        .iter()
        .find(|p| p["id"] == id)
        .cloned()
        .ok_or_else(|| AppError::bad("未知设备方案，请查看支持表"))
}
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/k3s/accelerator-integration", get(overview))
        .route(
            "/api/k3s/accelerator-integration/catalog",
            get(|| async { Json(catalog()) }),
        )
        .route("/api/k3s/accelerator-integration/plan", post(plan))
        .route("/api/k3s/accelerator-integration/repair", post(repair))
        .route(
            "/accelerators/supported",
            get(|| async { Html(include_str!("assets/accelerator-support.html")) }),
        )
}
pub fn cluster_routes() -> Router<AppState> {
    Router::new()
        .route("/api/cluster/accelerators/info", get(agent_info))
        .route("/api/cluster/accelerators/prepare", post(prepare))
        .route("/api/cluster/accelerators/jobs/{id}", get(agent_job))
}
async fn agent_info() -> Json<Value> {
    Json(
        json!({"apiVersion":1,"ips":crate::hostinfo::list_ips(),"platform":crate::repo::host_platform()}),
    )
}
async fn agent_job(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    // Do not expose unrelated console jobs over machine authentication.
    if !s
        .accelerator_jobs
        .lock()
        .map_err(|_| AppError::internal("job lock"))?
        .contains(&id)
    {
        return Err(AppError::not_found("集成任务不存在"));
    }
    s.jobs
        .get(&id)
        .map(|v| Json(json!(v)))
        .ok_or_else(|| AppError::not_found("任务已过期"))
}
async fn overview(State(s): State<AppState>) -> Result<Json<Value>, AppError> {
    let Json(report) = crate::accelerators::cluster_report(State(s.clone())).await?;
    let packages = PACKAGES
        .iter()
        .map(|id| json!({"id":id,"available":package_stamp(&s,id).is_some()}))
        .collect::<Vec<_>>();
    Ok(Json(
        json!({"catalog":catalog(),"report":report,"packages":packages}),
    ))
}
fn package_dir(s: &AppState, id: &str) -> PathBuf {
    crate::repo::repo_root(&s.paths)
        .join("accelerators/packages")
        .join(id)
}
fn package_stamp(s: &AppState, id: &str) -> Option<String> {
    if !PACKAGES.contains(&id) {
        return None;
    }
    let dir = package_dir(s, id);
    let sums = std::fs::read_to_string(dir.join("SHA256SUMS")).ok()?;
    // LFS pointers are not offline payloads. Full hashes are checked on the target before any mutation.
    for line in sums.lines() {
        let (digest, name) = line.split_once(' ')?;
        let name = name.trim_start_matches([' ', '*']);
        if digest.len() != 64
            || !digest.bytes().all(|b| b.is_ascii_hexdigit())
            || name.contains(['/', '\\'])
            || name == ".."
        {
            return None;
        }
        let bytes = std::fs::metadata(dir.join(name)).ok()?.len();
        if bytes < 200 && name.ends_with(".tar") {
            return None;
        }
    }
    if sums.lines().count() < 4 {
        return None;
    }
    Some(hash(sums.as_bytes()))
}
#[derive(Debug, Clone, Deserialize, Serialize)]
struct Selection {
    node_name: String,
    profile_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Plan {
    node_name: String,
    profile_id: String,
    architecture: String,
    package: Option<String>,
    package_stamp: Option<String>,
    actions: Vec<String>,
    blockers: Vec<String>,
    warnings: Vec<String>,
    pods: Vec<Value>,
    runtime_repair: bool,
    create_runtime_class: bool,
    was_cordoned: bool,
    fingerprint: String,
}
fn plugin(c: &Value, vendor: &str) -> bool {
    let text = format!("{} {}", string(&c["name"]), c["images"]).to_lowercase();
    let vendor_match = if vendor == "NVIDIA" {
        text.contains("nvidia")
    } else {
        text.contains("hygon") || text.contains("dcu") || text.contains("hcu")
    };
    vendor_match && (text.contains("device-plugin") || text.contains("deviceplugin"))
}
fn inspect(
    report: &Value,
    selection: &Selection,
    p: &Value,
    stamp: Option<String>,
) -> Result<Plan, AppError> {
    let node = array(&report["nodes"])
        .iter()
        .find(|n| n["name"] == selection.node_name)
        .ok_or_else(|| AppError::bad("节点不存在，请刷新集群"))?;
    let host = &node["hostDiagnostics"];
    let vendor = string(&p["vendor"]);
    let mut plan = Plan {
        node_name: selection.node_name.clone(),
        profile_id: selection.profile_id.clone(),
        architecture: string(&node["nodeInfo"]["architecture"]).into(),
        package: p["package"].as_str().map(str::to_owned),
        package_stamp: stamp,
        actions: vec![],
        blockers: vec![],
        warnings: vec!["驱动和固件需预先安装；不启动算法容器；逻辑资源数不等于物理卡数。".into()],
        pods: vec![],
        runtime_repair: false,
        create_runtime_class: false,
        was_cordoned: node["unschedulable"] == true,
        fingerprint: String::new(),
    };
    if p["automatic"] != true {
        plan.blockers
            .push("此方案目前仅提供诊断，尚不支持自动集成。".into());
    }
    if !array(&p["architectures"])
        .iter()
        .any(|a| a == &plan.architecture)
    {
        plan.blockers.push("CPU 架构不在此方案支持范围内。".into());
    }
    if node["ready"] != true {
        plan.blockers.push("K3s 节点未就绪。".into());
    }
    if !host.is_object() {
        plan.blockers.push(
            "缺少最近 120 秒内的节点诊断，请安装或升级该节点的 cangling-update 并加入集群。".into(),
        );
    }
    let devices = array(&host["devices"])
        .iter()
        .filter(|d| d["vendor"] == vendor)
        .collect::<Vec<_>>();
    if devices.is_empty() {
        plan.blockers
            .push("主机未发现与所选方案匹配的物理设备。".into());
    }
    if !array(&host["tools"])
        .iter()
        .any(|t| t["vendor"] == vendor && t["result"]["status"] == "ok")
        || devices.iter().any(|d| string(&d["driver"]).is_empty())
    {
        plan.blockers
            .push("驱动未绑定或厂商查询工具失败；请先修复主机驱动。".into());
    }
    let resources = array(&node["resources"]);
    if vendor == "Hygon"
        && resources.iter().any(|r| {
            string(&r["name"]).starts_with("hygon.com/")
                && !string(&r["name"]).starts_with(if selection.profile_id == "hygon-dcu" {
                    "hygon.com/dcu"
                } else {
                    "hygon.com/hcu"
                })
        })
    {
        plan.blockers
            .push("检测到其他海光资源类型，不自动切换 HCU/DCU 插件。".into());
    }
    let selected = resources
        .iter()
        .filter(|r| array(&p["resources"]).contains(&r["name"]))
        .collect::<Vec<_>>();
    let registered = selected
        .iter()
        .any(|r| r["allocatable"].as_u64().unwrap_or(0) > 0);
    let components = array(&node["components"])
        .iter()
        .filter(|c| plugin(c, vendor))
        .collect::<Vec<_>>();
    if vendor == "Hygon"
        && components.iter().any(|c| {
            let text = format!("{} {}", c["name"], c["images"]).to_lowercase();
            (selection.profile_id == "hygon-dcu" && text.contains("hcu") && !text.contains("dcu"))
                || (selection.profile_id == "hygon-hcu"
                    && text.contains("dcu")
                    && !text.contains("hcu"))
        })
    {
        plan.blockers
            .push("现有海光插件属于另一个 HCU/DCU 方案，请选择匹配方案。".into());
    }
    let healthy = registered
        && components.iter().any(|c| c["status"] == "ok")
        && components.iter().all(|c| c["status"] == "ok");
    if healthy {
        plan.warnings
            .push("现有设备插件就绪且资源已注册，无需安装或修复；不变更现有版本。".into());
    } else {
        if array(&report["warnings"])
            .iter()
            .any(|w| string(w).contains("读取 Pod 失败"))
            || resources.iter().any(|r| r["requested"].is_null())
        {
            plan.blockers
                .push("无法确认设备任务占用，停止修复。请检查读取 Pod 的权限。".into());
        }
        if resources
            .iter()
            .any(|r| r["requested"].as_u64().unwrap_or(0) > 0)
        {
            plan.blockers
                .push("节点仍有申请加速设备的任务，请先停止或迁移任务。".into());
        }
        let operator = vendor == "NVIDIA"
            && array(&report["components"])
                .iter()
                .any(|c| format!("{} {}", c["name"], c["images"]).contains("gpu-operator"));
        if operator {
            plan.blockers
                .push("该集群存在 GPU Operator，请在 Operator 中修复，避免混装手工插件。".into());
        }
        if !components.is_empty() {
            for c in components {
                if c["status"] == "ok" {
                    continue;
                }
                if string(&c["message"]).contains("ImagePull")
                    || string(&c["message"]).contains("ErrImage")
                {
                    plan.blockers.push(
                        "现有插件镜像拉取失败；请补齐原版本镜像，本功能不替换未知版本插件。".into(),
                    );
                    continue;
                }
                if !array(&c["owners"])
                    .iter()
                    .any(|o| o["kind"] == "DaemonSet" && o["controller"] == true)
                    || string(&c["uid"]).is_empty()
                {
                    plan.blockers
                        .push("异常插件未由 DaemonSet 管理，不能自动重建。".into());
                    continue;
                }
                plan.pods
                    .push(json!({"name":c["name"],"namespace":c["namespace"],"uid":c["uid"]}));
            }
            if !plan.pods.is_empty() {
                plan.actions.push(
                    "只重建所选节点的异常设备插件 Pod，保留其 DaemonSet 和原镜像版本。".into(),
                );
            }
            if plan.actions.is_empty() && !registered {
                plan.blockers.push(
                    "插件已运行但未注册设备，重启不能保证修复，请检查插件日志和驱动兼容性。".into(),
                );
            }
        } else {
            if registered {
                plan.blockers
                    .push("仍有注册资源但未识别到其插件，停止新增插件以免重复注册。".into());
            }
            if plan.package_stamp.is_none() {
                plan.blockers.push(
                    "离线包缺失或未完整拉取 Git LFS；请同步 cangling-repo/accelerators。".into(),
                );
            }
            if selection.profile_id == "hygon-dcu" {
                plan.warnings.push("海光插件需要 privileged、hostNetwork，以及 /dev、/sys、DTK 和 kubelet 设备插件目录挂载。".into());
                if devices
                    .iter()
                    .any(|d| !string(&d["name"]).contains("BW100"))
                {
                    plan.blockers
                        .push("此海光离线方案仅适配 BW100，其他型号需补充匹配的软件包。".into());
                }
                if host["deviceFiles"]["kfd"] != true || host["deviceFiles"]["dtk2604"] != true {
                    plan.blockers
                        .push("BW100 集成要求 /dev/kfd 和 /opt/dtk-26.04 已就绪。".into());
                }
            }
            if vendor == "NVIDIA" {
                let runtime = array(&host["runtimes"]).iter().any(|r| {
                    r["handler"] == "nvidia"
                        && r["executable"] == true
                        && r["versionProbe"]["status"] == "ok"
                });
                if !runtime {
                    plan.runtime_repair = true;
                    if !matches!(
                        string(&host["platform"]),
                        "ubuntu-x86-22.04-deb" | "ubuntu-x86-24.04-deb"
                    ) {
                        plan.blockers.push("缺少 NVIDIA 运行时；仅支持 Ubuntu 22.04/24.04 amd64 自动安装，其余系统请先手动配置。".into());
                    }
                    if host["customRuntimeTemplate"] == true {
                        plan.blockers
                            .push("存在自定义 containerd 模板，需人工合并运行时配置。".into());
                    }
                    plan.actions.push("临时停止节点调度，安装离线 NVIDIA Toolkit 并重启本节点 K3s 服务，随后恢复原调度状态。".into());
                }
                let class = array(&report["runtimeClasses"])
                    .iter()
                    .find(|r| r["name"] == "nvidia");
                plan.create_runtime_class = class.is_none();
                if class.is_some_and(|r| r["handler"] != "nvidia") {
                    plan.blockers
                        .push("已有 nvidia RuntimeClass 的 handler 不匹配，未覆盖。".into());
                }
                if array(&report["warnings"])
                    .iter()
                    .any(|w| string(w).contains("读取 RuntimeClass 失败"))
                {
                    plan.blockers
                        .push("无法核对 RuntimeClass，请检查权限。".into());
                }
            }
            plan.actions
                .push("校验离线包并将设备插件镜像导入目标节点 K3s containerd。".into());
            plan.actions
                .push("创建仅匹配目标节点的设备插件 DaemonSet，等待资源注册。".into());
        }
    }
    if p["automatic"] != true {
        plan.actions.clear();
        plan.pods.clear();
        plan.runtime_repair = false;
        plan.create_runtime_class = false;
    }
    // Timestamps, memory consumption and resourceVersion must not invalidate a reviewed plan.
    plan.fingerprint = hash(serde_json::to_string(&plan).unwrap().as_bytes());
    Ok(plan)
}
async fn make_plan(s: &AppState, selection: &Selection) -> Result<Plan, AppError> {
    let p = profile(&selection.profile_id)?;
    let Json(report) = crate::accelerators::cluster_report(State(s.clone())).await?;
    let stamp = p["package"].as_str().and_then(|id| package_stamp(s, id));
    let mut plan = inspect(&report, selection, &p, stamp)?;
    if !plan.actions.is_empty() && plan.blockers.is_empty() {
        if let Err(e) = target(s, &selection.node_name).await {
            plan.blockers.push(e.to_string());
        }
    }
    Ok(plan)
}
async fn plan(
    State(s): State<AppState>,
    Json(selection): Json<Selection>,
) -> Result<Json<Plan>, AppError> {
    Ok(Json(make_plan(&s, &selection).await?))
}
#[derive(Deserialize)]
struct Repair {
    #[serde(flatten)]
    selection: Selection,
    fingerprint: String,
    confirmed: bool,
}
async fn repair(
    State(s): State<AppState>,
    Json(body): Json<Repair>,
) -> Result<Json<Value>, AppError> {
    if !body.confirmed {
        return Err(AppError::bad("请先检查并确认修复计划"));
    }
    let guard = s
        .lock_project(LOCK)
        .try_lock_owned()
        .map_err(|_| AppError::conflict("已有集成任务执行中"))?;
    let plan = make_plan(&s, &body.selection).await?;
    if !plan.blockers.is_empty() {
        return Err(AppError::bad(plan.blockers.join("；")));
    }
    if plan.fingerprint != body.fingerprint {
        return Err(AppError::conflict(
            "节点或离线包状态已变化，请重新生成修复计划",
        ));
    }
    let job = s.jobs.create();
    let id = job.id.clone();
    tokio::spawn(async move {
        let _guard = guard;
        match execute(&s, &plan, &id).await {
            Ok(()) => s.jobs.finish_ok(
                &id,
                if plan.actions.is_empty() {
                    "现有集成正常，无需修改"
                } else {
                    "设备插件就绪且资源已注册；尚未执行算法验证"
                },
            ),
            Err(e) => s
                .jobs
                .finish_err(&id, &format!("{e}；请查看支持型号与架构表")),
        }
    });
    Ok(Json(json!(job)))
}
#[derive(Clone)]
struct Target {
    ip: String,
    url: Option<String>,
}
async fn target(s: &AppState, name: &str) -> Result<Target, AppError> {
    let nodes: Value = serde_json::from_str(
        &kubectl(vec![
            "get".into(),
            "node".into(),
            name.into(),
            "-o".into(),
            "json".into(),
        ])
        .await?,
    )
    .map_err(|e| AppError::bad(e.to_string()))?;
    let ips = array(&nodes["status"]["addresses"])
        .iter()
        .filter(|a| a["type"] == "InternalIP")
        .filter_map(|a| a["address"].as_str())
        .collect::<Vec<_>>();
    let local = crate::hostinfo::list_ips();
    if let Some(ip) = ips.iter().find(|ip| local.iter().any(|v| v == **ip)) {
        return Ok(Target {
            ip: (**ip).into(),
            url: None,
        });
    }
    let candidates = {
        let conn = s.db.lock().map_err(|_| AppError::internal("db lock"))?;
        let mut stmt =
            conn.prepare("SELECT addr, info_json FROM cluster_nodes WHERE role = 'worker' ORDER BY last_seen DESC")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for (addr, info) in candidates {
        let info: Value = serde_json::from_str(&info).unwrap_or(Value::Null);
        if let Some(ip) = ips.iter().find(|ip| {
            array(&info["accelerator_diagnostics"]["ips"])
                .iter()
                .any(|v| string(v) == **ip)
        }) {
            let base = if addr.starts_with("http://") {
                addr
            } else {
                format!("http://{addr}")
            };
            let reply = remote_get(s, &format!("{base}/api/cluster/accelerators/info")).await?;
            if reply["apiVersion"] != 1 || !array(&reply["ips"]).iter().any(|v| string(v) == *ip) {
                return Err(AppError::bad(
                    "节点代理版本或 IP 不匹配，请升级对应节点的 cangling-update",
                ));
            }
            return Ok(Target {
                ip: (**ip).into(),
                url: Some(base),
            });
        }
    }
    Err(AppError::bad(
        "目标节点缺少支持此功能的 cangling-update 代理；请先安装并加入集群",
    ))
}
async fn remote_get(s: &AppState, url: &str) -> Result<Value, AppError> {
    let token = s
        .cluster
        .token
        .as_deref()
        .ok_or_else(|| AppError::bad("未配置集群令牌"))?;
    let (status, data) = tokio::time::timeout(
        Duration::from_secs(12),
        crate::cluster::http::get_json(url, token),
    )
    .await
    .map_err(|_| AppError::bad("节点代理请求超时"))??;
    if !status.is_success() {
        return Err(AppError::bad(format!(
            "节点代理返回 {status}，请升级节点代理并检查连接"
        )));
    }
    Ok(data)
}
#[derive(Clone, Deserialize, Serialize)]
struct Prepare {
    package_id: String,
    node_ip: String,
    runtime: bool,
    import_image: bool,
    package_stamp: String,
}
async fn prepare(
    State(s): State<AppState>,
    Json(body): Json<Prepare>,
) -> Result<Json<Value>, AppError> {
    check_prepare(&body)?;
    let guard = s
        .lock_project("accelerator-node-prepare")
        .try_lock_owned()
        .map_err(|_| AppError::conflict("节点组件准备中"))?;
    let job = s.jobs.create();
    let id = job.id.clone();
    {
        let mut ids = s
            .accelerator_jobs
            .lock()
            .map_err(|_| AppError::internal("job lock"))?;
        ids.retain(|old| s.jobs.get(old).is_some());
        ids.insert(id.clone());
    }
    tokio::spawn(async move {
        let _guard = guard;
        s.jobs.set(&id, "prepare", "校验并准备节点离线组件", 0, 1);
        match prepare_local(&s, &body, &id).await {
            Ok(()) => s.jobs.finish_ok(&id, "节点组件已准备"),
            Err(e) => s.jobs.finish_err(&id, &e.to_string()),
        }
    });
    Ok(Json(json!(job)))
}
fn check_prepare(p: &Prepare) -> Result<(), AppError> {
    if p.package_stamp.len() != 64
        || !p.package_stamp.bytes().all(|b| b.is_ascii_hexdigit())
        || !PACKAGES.contains(&p.package_id.as_str())
        || (p.runtime && p.package_id != "nvidia-amd64")
        || !crate::hostinfo::list_ips().contains(&p.node_ip)
    {
        return Err(AppError::bad("软件包或目标节点不匹配"));
    }
    Ok(())
}
async fn prepare_local(s: &AppState, p: &Prepare, id: &str) -> Result<(), AppError> {
    check_prepare(p)?;
    let result = crate::repo::install_package(
        s,
        "accelerators",
        &p.package_id,
        &[
            ("CANGLING_ACCELERATOR_APPLY".into(), "1".into()),
            (
                "CANGLING_ACCELERATOR_PACKAGE_ID".into(),
                p.package_id.clone(),
            ),
            (
                "CANGLING_ACCELERATOR_PACKAGE_STAMP".into(),
                p.package_stamp.clone(),
            ),
            (
                "CANGLING_ACCELERATOR_RUNTIME_REPAIR".into(),
                if p.runtime { "1" } else { "0" }.into(),
            ),
            (
                "CANGLING_ACCELERATOR_IMPORT_IMAGE".into(),
                if p.import_image { "1" } else { "0" }.into(),
            ),
            (
                "CANGLING_ACCELERATOR_BACKUP_DIR".into(),
                s.paths
                    .config_dir
                    .join("accelerator-backups")
                    .join(id)
                    .display()
                    .to_string(),
            ),
        ],
    )
    .await?;
    if result.timed_out || result.exit_code != Some(0) {
        return Err(AppError::bad(format!(
            "节点准备失败（退出码 {:?}）：{} {}",
            result.exit_code,
            result
                .stdout
                .chars()
                .rev()
                .take(3000)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>(),
            result.stderr.chars().take(3000).collect::<String>()
        )));
    }
    Ok(())
}
async fn prepare_target(
    s: &AppState,
    target: &Target,
    p: &Prepare,
    id: &str,
) -> Result<(), AppError> {
    let Some(base) = &target.url else {
        return prepare_local(s, p, id).await;
    };
    let token = s
        .cluster
        .token
        .as_deref()
        .ok_or_else(|| AppError::bad("缺少集群令牌"))?;
    let (status, job) = tokio::time::timeout(
        Duration::from_secs(15),
        crate::cluster::http::post_json(
            &format!("{base}/api/cluster/accelerators/prepare"),
            token,
            &json!(p),
        ),
    )
    .await
    .map_err(|_| AppError::bad("提交节点准备任务超时；请检查节点任务状态后再试"))??;
    if !status.is_success() {
        return Err(AppError::bad(format!("节点准备请求失败：{status} {job}")));
    }
    let job_id = string(&job["id"]);
    if uuid::Uuid::parse_str(job_id).is_err() {
        return Err(AppError::bad("节点返回无效任务编号"));
    }
    for _ in 0..600 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let progress =
            remote_get(s, &format!("{base}/api/cluster/accelerators/jobs/{job_id}")).await?;
        s.jobs
            .set(id, "prepare", string(&progress["message"]), 1, 4);
        if progress["done"] == true {
            return if progress["error"].is_null() {
                Ok(())
            } else {
                Err(AppError::bad(string(&progress["error"])))
            };
        }
    }
    Err(AppError::bad("节点准备超时，请核对节点任务状态"))
}
async fn kubectl(args: Vec<String>) -> Result<String, AppError> {
    crate::k3s_resources::kubectl(args, Duration::from_secs(45)).await
}
async fn create_manifest(
    s: &AppState,
    id: &str,
    name: &str,
    value: &Value,
) -> Result<(), AppError> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let dir = s.paths.config_dir.join("accelerator-manifests").join(id);
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let file = dir.join(name);
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&file)?;
    out.write_all(value.to_string().as_bytes())?;
    for dry in [true, false] {
        let mut args = vec!["create".into(), "-f".into(), file.display().to_string()];
        if dry {
            args.push("--dry-run=server".into());
        }
        kubectl(args).await?;
    }
    Ok(())
}
fn scoped_manifest(mut value: Value, node: &str, profile: &str) -> Value {
    let name = format!(
        "cangling-{profile}-device-plugin-{}",
        &hash(node.as_bytes())[..8]
    );
    let labels = json!({"app":name,"cangling.cn/managed-by":"cangling-update"});
    value["metadata"]["name"] = json!(name);
    value["metadata"]["labels"] = labels.clone();
    value["spec"]["selector"]["matchLabels"] = labels.clone();
    value["spec"]["template"]["metadata"]["labels"] = labels;
    value["spec"]["template"]["spec"]["affinity"] = json!({"nodeAffinity":{"requiredDuringSchedulingIgnoredDuringExecution":{"nodeSelectorTerms":[{"matchFields":[{"key":"metadata.name","operator":"In","values":[node]}]}]}}});
    value
}
async fn execute(s: &AppState, p: &Plan, id: &str) -> Result<(), AppError> {
    if p.actions.is_empty() {
        return Ok(());
    }
    let target = target(s, &p.node_name).await?;
    // Node runtime restarts remain a separate, explicitly reviewed action; cordon is guarded and restored.
    let should_cordon = p.runtime_repair && !p.was_cordoned;
    let marker = "cangling.cn/accelerator-integration";
    if should_cordon {
        let node: Value = serde_json::from_str(
            &kubectl(vec![
                "get".into(),
                "node".into(),
                p.node_name.clone(),
                "-o".into(),
                "json".into(),
            ])
            .await?,
        )
        .map_err(|e| AppError::bad(e.to_string()))?;
        if node["spec"]["unschedulable"] == true {
            return Err(AppError::conflict("节点调度状态已变化，请重新检查"));
        }
        let mut annotations = node["metadata"]["annotations"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        if annotations.contains_key(marker) {
            return Err(AppError::conflict("节点有未完成的集成标记，请先核对旧任务"));
        }
        annotations.insert(marker.into(), json!(id));
        kubectl(vec!["patch".into(),"node".into(),p.node_name.clone(),"--type=json".into(),"-p".into(),json!([
          {"op":"test","path":"/metadata/resourceVersion","value":node["metadata"]["resourceVersion"]},
          {"op":"add","path":"/metadata/annotations","value":annotations},
          {"op":"add","path":"/spec/unschedulable","value":true}
        ]).to_string()]).await?;
    }
    let result = execute_inner(s, p, id, &target).await;
    if should_cordon {
        let cleanup=kubectl(vec!["patch".into(),"node".into(),p.node_name.clone(),"--type=json".into(),"-p".into(),json!([
          {"op":"test","path":"/metadata/annotations/cangling.cn~1accelerator-integration","value":id},
          {"op":"test","path":"/spec/unschedulable","value":true},
          {"op":"remove","path":"/metadata/annotations/cangling.cn~1accelerator-integration"},
          {"op":"add","path":"/spec/unschedulable","value":false}
        ]).to_string()]).await;
        if let Err(e) = cleanup {
            return Err(AppError::bad(format!(
                "集成结果：{result:?}；恢复节点调度失败，请人工核对：{e}"
            )));
        }
    }
    result
}
async fn execute_inner(s: &AppState, p: &Plan, id: &str, target: &Target) -> Result<(), AppError> {
    if !p.pods.is_empty() {
        for pod in &p.pods {
            s.jobs.set(id, "restart", "重建目标节点异常插件", 1, 4);
            // UID preconditions prevent deleting a replacement Pod created after plan review.
            let uri = format!(
                "/api/v1/namespaces/{}/pods/{}",
                string(&pod["namespace"]),
                string(&pod["name"])
            );
            let dir = s.paths.config_dir.join("accelerator-manifests").join(id);
            std::fs::create_dir_all(&dir)?;
            let file = dir.join(format!("delete-{}.json", string(&pod["uid"])));
            std::fs::write(&file,json!({"apiVersion":"v1","kind":"DeleteOptions","preconditions":{"uid":pod["uid"]}}).to_string())?;
            kubectl(vec![
                "delete".into(),
                "--raw".into(),
                uri,
                "-f".into(),
                file.display().to_string(),
            ])
            .await?;
        }
    } else {
        // Recheck workload claims immediately before preparing the node.
        let Json(report) = crate::accelerators::cluster_report(State(s.clone())).await?;
        let node = array(&report["nodes"])
            .iter()
            .find(|n| n["name"] == p.node_name)
            .ok_or_else(|| AppError::bad("节点已移除"))?;
        if array(&node["resources"])
            .iter()
            .any(|r| r["requested"].is_null() || r["requested"].as_u64().unwrap_or(0) > 0)
        {
            return Err(AppError::bad("节点设备占用发生变化，停止安装"));
        }
        let package = p
            .package
            .as_deref()
            .ok_or_else(|| AppError::bad("此型号无匹配离线包"))?;
        if package_stamp(s, package) != p.package_stamp {
            return Err(AppError::bad("离线包已变化，请重新生成计划"));
        }
        let dir = package_dir(s, package);
        let bytes = std::fs::read(dir.join("plugin.json"))?;
        let sums = std::fs::read_to_string(dir.join("SHA256SUMS"))?;
        let expected = sums.lines().find_map(|l| {
            l.split_once(' ')
                .filter(|(_, n)| n.trim_start_matches([' ', '*']) == "plugin.json")
                .map(|(h, _)| h)
        });
        if expected != Some(hash(&bytes).as_str()) {
            return Err(AppError::bad("插件模板 SHA-256 校验失败"));
        }
        let manifest = scoped_manifest(
            serde_json::from_slice(&bytes).map_err(|e| AppError::bad(e.to_string()))?,
            &p.node_name,
            &p.profile_id,
        );
        let name = string(&manifest["metadata"]["name"]);
        let existing = kubectl(vec![
            "get".into(),
            "daemonset".into(),
            name.into(),
            "-n".into(),
            "kube-system".into(),
            "--ignore-not-found".into(),
            "-o".into(),
            "json".into(),
        ])
        .await?;
        // Existing controllers are never adopted or overwritten by a new plan.
        if !existing.trim().is_empty() {
            return Err(AppError::bad(
                "同名 DaemonSet 已存在，请刷新诊断后维护现有插件",
            ));
        }
        s.jobs.set(id, "prepare", "准备目标节点离线组件", 1, 4);
        prepare_target(
            s,
            target,
            &Prepare {
                package_id: package.into(),
                node_ip: target.ip.clone(),
                runtime: p.runtime_repair,
                import_image: true,
                package_stamp: p.package_stamp.clone().unwrap_or_default(),
            },
            id,
        )
        .await?;
        if p.create_runtime_class {
            create_manifest(s,id,"runtime-class.json",&json!({"apiVersion":"node.k8s.io/v1","kind":"RuntimeClass","metadata":{"name":"nvidia"},"handler":"nvidia"})).await?;
        }
        s.jobs
            .set(id, "apply", "部署限定到目标节点的设备插件", 2, 4);
        create_manifest(s, id, "plugin.json", &manifest).await?;
    }
    s.jobs.set(
        id,
        "verify",
        "等待设备插件就绪与资源注册（最多 3 分钟）",
        3,
        4,
    );
    let profile = profile(&p.profile_id)?;
    for _ in 0..36 {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let Json(report) = crate::accelerators::cluster_report(State(s.clone())).await?;
        if array(&report["nodes"]).iter().any(|n| {
            let components = array(&n["components"])
                .iter()
                .filter(|c| plugin(c, string(&profile["vendor"])))
                .collect::<Vec<_>>();
            n["name"] == p.node_name
                && !components.is_empty()
                && components.iter().all(|c| {
                    c["status"] == "ok" && !p.pods.iter().any(|old| old["uid"] == c["uid"])
                })
                && array(&n["resources"]).iter().any(|r| {
                    array(&profile["resources"]).contains(&r["name"])
                        && r["allocatable"].as_u64().unwrap_or(0) > 0
                })
        }) {
            return Ok(());
        }
    }
    Err(AppError::bad(
        "插件未在 3 分钟内就绪并注册资源；保留现场，请查看设备诊断、插件日志及支持表",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn selection(id: &str) -> Selection {
        Selection {
            node_name: "worker-a".into(),
            profile_id: id.into(),
        }
    }
    fn report(vendor: &str) -> Value {
        json!({"nodes":[{"name":"worker-a","ready":true,"nodeInfo":{"architecture":"amd64"},"resources":[],"components":[],"hostDiagnostics":{"platform":"ubuntu-x86-24.04-deb","devices":[{"vendor":vendor,"name":if vendor=="Hygon"{"BW100"}else{"GeForce RTX 5090"},"driver":"driver"}],"tools":[{"vendor":vendor,"result":{"status":"ok"}}],"runtimes":[{"handler":"nvidia","executable":true,"versionProbe":{"status":"ok"}}],"deviceFiles":{"kfd":true,"dtk2604":true}}}],"runtimeClasses":[{"name":"nvidia","handler":"nvidia"}],"warnings":[],"components":[]})
    }
    fn component(vendor: &str, status: &str) -> Value {
        json!({"name":format!("{vendor}-device-plugin"),"uid":"uid-a","namespace":"kube-system","status":status,"owners":[{"kind":"DaemonSet","name":"original","controller":true}],"images":[format!("{vendor}/device-plugin:original")],"message":""})
    }
    fn inspect_profile(r: &Value, id: &str) -> Plan {
        inspect(
            r,
            &selection(id),
            &profile(id).unwrap(),
            Some("a".repeat(64)),
        )
        .unwrap()
    }
    #[test]
    fn console_requires_session_and_machine_routes_delegate_to_token_middleware() {
        use axum::http::Method;
        for path in [
            "/api/k3s/accelerator-integration",
            "/api/k3s/accelerator-integration/repair",
            "/accelerators/supported",
        ] {
            assert!(!crate::auth::is_public(&Method::GET, path));
        }
        for path in [
            "/api/cluster/accelerators/info",
            "/api/cluster/accelerators/prepare",
            "/api/cluster/accelerators/jobs/test",
        ] {
            assert!(crate::auth::is_public(&Method::POST, path));
        }
    }
    #[test]
    fn captured_mongolia_bw100_integration_is_a_noop_without_offline_package() {
        let r: Value =
            serde_json::from_str(include_str!("../tests/fixtures/integration-bw100.json")).unwrap();
        let p = inspect(
            &r,
            &Selection {
                node_name: "worker-bw100".into(),
                profile_id: "hygon-dcu".into(),
            },
            &profile("hygon-dcu").unwrap(),
            None,
        )
        .unwrap();
        assert!(p.blockers.is_empty(), "{:?}", p.blockers);
        assert!(p.actions.is_empty());
        assert_eq!(array(&r["nodes"][0]["hostDiagnostics"]["devices"]).len(), 6);
    }
    #[test]
    fn healthy_hygon_keeps_twenty_four_logical_resources_and_six_physical_cards() {
        let mut r = report("Hygon");
        let node = &mut r["nodes"][0];
        let device = node["hostDiagnostics"]["devices"][0].clone();
        node["hostDiagnostics"]["devices"] = json!(vec![device; 6]);
        node["resources"] = json!([{"name":"hygon.com/dcunum","allocatable":24,"requested":0}]);
        node["components"] = json!([component("dcu", "ok")]);
        let p = inspect_profile(&r, "hygon-dcu");
        assert!(p.blockers.is_empty(), "{:?}", p.blockers);
        assert!(p.actions.is_empty());
    }
    #[test]
    fn new_nvidia_install_scopes_plugin_and_preserves_existing_runtime() {
        let p = inspect_profile(&report("NVIDIA"), "nvidia");
        assert!(p.blockers.is_empty(), "{:?}", p.blockers);
        assert!(!p.runtime_repair);
        assert!(!p.create_runtime_class);
        assert_eq!(p.actions.len(), 2);
        let template = json!({"metadata":{},"spec":{"selector":{},"template":{"metadata":{},"spec":{"containers":[{"image":"fixed"}]}}}});
        let ds = scoped_manifest(template, "worker-a", "nvidia");
        assert_eq!(
            ds["spec"]["template"]["spec"]["affinity"]["nodeAffinity"]
                ["requiredDuringSchedulingIgnoredDuringExecution"]["nodeSelectorTerms"][0]
                ["matchFields"][0]["values"],
            json!(["worker-a"])
        );
        assert_eq!(
            ds["spec"]["template"]["spec"]["containers"][0]["image"],
            "fixed"
        );
    }
    #[test]
    fn blocks_mixed_hcu_dcu_wrong_model_arch_missing_agent_and_packages() {
        let mut r = report("Hygon");
        r["nodes"][0]["resources"] =
            json!([{"name":"hygon.com/hcu","allocatable":4,"requested":0}]);
        assert!(inspect_profile(&r, "hygon-dcu")
            .blockers
            .iter()
            .any(|s| s.contains("其他海光")));
        r["nodes"][0]["hostDiagnostics"]["devices"][0]["name"] = json!("K100_AI");
        assert!(inspect_profile(&r, "hygon-dcu")
            .blockers
            .iter()
            .any(|s| s.contains("BW100")));
        r["nodes"][0]["nodeInfo"]["architecture"] = json!("arm64");
        assert!(inspect_profile(&r, "hygon-dcu")
            .blockers
            .iter()
            .any(|s| s.contains("架构")));
        r["nodes"][0]["hostDiagnostics"] = Value::Null;
        assert!(inspect_profile(&r, "hygon-dcu")
            .blockers
            .iter()
            .any(|s| s.contains("120")));
        assert!(inspect(
            &report("NVIDIA"),
            &selection("nvidia"),
            &profile("nvidia").unwrap(),
            None
        )
        .unwrap()
        .blockers
        .iter()
        .any(|s| s.contains("离线包")));
    }
    #[test]
    fn operator_ownership_and_workload_claims_block_mutation() {
        let mut r = report("NVIDIA");
        r["components"] = json!([{"name":"gpu-operator","images":[]}]);
        assert!(inspect_profile(&r, "nvidia")
            .blockers
            .iter()
            .any(|s| s.contains("Operator")));
        r["nodes"][0]["resources"] =
            json!([{"name":"nvidia.com/gpu","requested":1,"allocatable":1}]);
        assert!(inspect_profile(&r, "nvidia")
            .blockers
            .iter()
            .any(|s| s.contains("任务")));
        r["nodes"][0]["resources"][0]["requested"] = Value::Null;
        assert!(inspect_profile(&r, "nvidia")
            .blockers
            .iter()
            .any(|s| s.contains("占用")));
    }
    #[test]
    fn repairs_only_failed_daemonset_pods_and_never_changes_their_image() {
        let mut r = report("Hygon");
        r["nodes"][0]["components"] = json!([component("dcu", "error"), component("dcu", "ok")]);
        let p = inspect_profile(&r, "hygon-dcu");
        assert!(p.blockers.is_empty(), "{:?}", p.blockers);
        assert_eq!(p.pods.len(), 1);
        assert_eq!(p.actions.len(), 1);
        r["nodes"][0]["components"][0]["owners"] = json!([]);
        assert!(!inspect_profile(&r, "hygon-dcu").blockers.is_empty());
        r["nodes"][0]["components"][0]["message"] = json!("ImagePullBackOff");
        assert!(inspect_profile(&r, "hygon-dcu")
            .blockers
            .iter()
            .any(|s| s.contains("原版本")));
    }
    #[test]
    fn runtime_install_is_gated_and_fingerprints_ignore_observation_time() {
        let mut r = report("NVIDIA");
        r["nodes"][0]["hostDiagnostics"]["runtimes"] = json!([]);
        let p = inspect_profile(&r, "nvidia");
        assert!(p.runtime_repair);
        assert!(p.blockers.is_empty());
        r["observedAt"] = json!("later");
        r["nodes"][0]["hostDiagnostics"]["observedAt"] = json!("later");
        assert_eq!(p.fingerprint, inspect_profile(&r, "nvidia").fingerprint);
        r["nodes"][0]["hostDiagnostics"]["customRuntimeTemplate"] = json!(true);
        assert!(inspect_profile(&r, "nvidia")
            .blockers
            .iter()
            .any(|s| s.contains("模板")));
        r["nodes"][0]["hostDiagnostics"]["platform"] = json!("kylin-x86-v10-rpm");
        assert!(inspect_profile(&r, "nvidia")
            .blockers
            .iter()
            .any(|s| s.contains("Ubuntu")));
    }
}
