//! worker 侧：发现/连接 master，注册并周期发送心跳。

use super::{discovery, http, load_or_create_node_id, HEARTBEAT_INTERVAL_SECS};
use crate::hostinfo::{self, HostSnapshot};
use crate::paths::AppPaths;
use crate::state::AppState;
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::time::{Duration, Instant};

/// 每隔多久重新采集一次完整主机信息（毫秒级心跳里只带轻量数据）。
const SNAPSHOT_REFRESH: Duration = Duration::from_secs(300);

enum Tick {
    Ok,
    Reregister,
}

pub async fn run(state: AppState) {
    let cfg = state.cluster.clone();
    let Some(_token) = cfg.token.as_deref() else {
        tracing::error!("worker 角色需要 --cluster-token（或环境变量 CANGLING_CLUSTER_TOKEN）");
        return;
    };

    let master_slot = state.master_url.clone();
    let node_id = load_or_create_node_id(&state.paths);
    let mut registered = false;
    let mut failures: u32 = 0;
    let mut last_snapshot: Option<Instant> = None;
    let mut upgrade_backoff_until: Option<Instant> = None;

    tracing::info!(
        node_id = %node_id,
        role = "worker",
        master = cfg.master_url.as_deref().unwrap_or("（自动发现）"),
        "集群 worker 启动"
    );

    loop {
        let mut master = master_slot.lock().unwrap().clone();
        if master.is_none() {
            if let Some(cid) = cfg.cluster_id() {
                if let Some(m) = discovery::discover(cfg.discovery_port, &cid).await {
                    tracing::info!("发现 master：{m}");
                    if let Ok(mut slot) = master_slot.lock() {
                        *slot = Some(m.clone());
                    }
                    master = Some(m);
                    registered = false;
                }
            }
        }
        if let Some(m) = master.as_deref() {
            let want_snapshot = last_snapshot
                .map(|t| t.elapsed() >= SNAPSHOT_REFRESH)
                .unwrap_or(true);
            match tick(&state, &node_id, m, registered, want_snapshot).await {
                Ok((Tick::Ok, upgrade)) => {
                    registered = true;
                    failures = 0;
                    if want_snapshot {
                        last_snapshot = Some(Instant::now());
                    }
                    if let Some(offer) = upgrade {
                        let blocked = upgrade_backoff_until
                            .map(|t| Instant::now() < t)
                            .unwrap_or(false);
                        if blocked {
                            tracing::debug!("升级失败后冷却中，暂不重试");
                        } else {
                            let token = state.cluster.token.as_deref().unwrap_or_default();
                            tracing::info!(
                                "master 要求升级到 v{}（{}）",
                                offer.version,
                                offer.arch
                            );
                            match crate::cluster::self_update::apply(m, token, &offer).await {
                                Ok(crate::cluster::self_update::ApplyOutcome::Done) => {
                                    tracing::info!("程序已写入 v{}，等待重启生效", offer.version);
                                }
                                Ok(crate::cluster::self_update::ApplyOutcome::InProgress) => {}
                                Err(err) => {
                                    tracing::warn!("自动升级失败：{err:#}");
                                    upgrade_backoff_until = Some(
                                        Instant::now() + crate::cluster::self_update::retry_delay(),
                                    );
                                }
                            }
                        }
                    }
                }
                Ok((Tick::Reregister, _)) => {
                    registered = false;
                }
                Err(err) => {
                    failures += 1;
                    tracing::warn!("与 master {m} 通信失败（{failures}/3）：{err:#}");
                    if failures >= 3 && cfg.master_url.is_none() {
                        if let Ok(mut slot) = master_slot.lock() {
                            *slot = None;
                        }
                        registered = false;
                        failures = 0;
                    }
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(HEARTBEAT_INTERVAL_SECS)).await;
    }
}

async fn tick(
    state: &AppState,
    node_id: &str,
    master: &str,
    registered: bool,
    want_snapshot: bool,
) -> Result<(Tick, Option<crate::cluster::self_update::UpgradeOffer>)> {
    let token = state.cluster.token.as_deref().unwrap_or_default();

    if !registered {
        let snap = collect_snapshot(&state.paths).await;
        let body = json!({
            "id": node_id,
            "name": snap.hostname,
            "addr": format!("{}:{}", snap.primary_ip, state.port),
            "version": env!("CARGO_PKG_VERSION"),
            "arch": std::env::consts::ARCH,
            "host": snap,
        });
        let url = format!("{master}/api/cluster/register");
        let (status, resp) = http::post_json(&url, token, &body)
            .await
            .with_context(|| format!("注册到 {url}"))?;
        if status == axum::http::StatusCode::UNAUTHORIZED
            || status == axum::http::StatusCode::FORBIDDEN
        {
            tracing::error!("注册被 master 拒绝（集群令牌可能不一致）：{resp}");
        }
        if !status.is_success() {
            bail!("注册失败 {status}: {resp}");
        }
        tracing::info!("已注册到 master {master}（{node_id}）");
        let upgrade = crate::cluster::self_update::parse_offer(&resp);
        return Ok((Tick::Ok, upgrade));
    }

    let host = if want_snapshot {
        Some(collect_snapshot(&state.paths).await)
    } else {
        None
    };
    let body = json!({
        "id": node_id,
        "version": env!("CARGO_PKG_VERSION"),
        "arch": std::env::consts::ARCH,
        "host": host,
    });
    let url = format!("{master}/api/cluster/heartbeat");
    let (status, resp) = http::post_json(&url, token, &body)
        .await
        .with_context(|| format!("心跳到 {url}"))?;
    if status == axum::http::StatusCode::UNAUTHORIZED || status == axum::http::StatusCode::FORBIDDEN
    {
        tracing::error!("心跳被 master 拒绝（集群令牌可能不一致）：{resp}");
    }
    if !status.is_success() {
        bail!("心跳失败 {status}: {resp}");
    }
    let known = resp.get("known").and_then(|v| v.as_bool()).unwrap_or(true);
    let upgrade = crate::cluster::self_update::parse_offer(&resp);
    Ok((if known { Tick::Ok } else { Tick::Reregister }, upgrade))
}

async fn collect_snapshot(paths: &AppPaths) -> HostSnapshot {
    let paths = paths.clone();
    tokio::task::spawn_blocking(move || hostinfo::collect(&paths).unwrap_or_default())
        .await
        .unwrap_or_default()
}
