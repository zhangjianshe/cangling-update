//! 用 UDP 广播发现 master（扁平子网内）。
//!
//! worker 广播 `probe`（只带令牌哈希），master 收到后单播回复自己的 HTTP 地址。
//! 令牌本身不会出现在广播里。

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::net::UdpSocket;

const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_DATAGRAM: usize = 2048;

/// worker：广播探测，等待 master 回复，返回 `http://addr:port`。
pub async fn discover(port: u16, cluster: &str) -> Option<String> {
    let sock = UdpSocket::bind("0.0.0.0:0").await.ok()?;
    sock.set_broadcast(true).ok()?;

    let probe = json!({ "t": "probe", "c": cluster }).to_string();
    let dest = format!("255.255.255.255:{port}");
    if sock.send_to(probe.as_bytes(), &dest).await.is_err() {
        return None;
    }

    let deadline = tokio::time::Instant::now() + PROBE_TIMEOUT;
    let mut buf = [0u8; MAX_DATAGRAM];
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return None;
        }
        match tokio::time::timeout(deadline - now, sock.recv_from(&mut buf)).await {
            Ok(Ok((n, _src))) => {
                let Ok(v) = serde_json::from_slice::<Value>(&buf[..n]) else {
                    continue;
                };
                if !is_announce_for(&v, cluster) {
                    continue;
                }
                let addr = v.get("addr")?.as_str()?;
                let http_port = v.get("port")?.as_u64()?;
                if addr.is_empty() {
                    continue;
                }
                return Some(format!("http://{addr}:{http_port}"));
            }
            _ => return None,
        }
    }
}

/// master：监听探测广播并回复。announce 应为 worker 能访问到的本机 IP。
pub async fn serve(port: u16, cluster: &str, announce: &str, http_port: u16) -> Result<()> {
    let sock = UdpSocket::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("绑定 UDP 发现端口 {port} 失败"))?;
    tracing::info!("集群发现监听 0.0.0.0:{port}（cluster={cluster}）");

    let mut buf = [0u8; MAX_DATAGRAM];
    loop {
        let (n, peer) = match sock.recv_from(&mut buf).await {
            Ok(x) => x,
            Err(err) => {
                tracing::warn!("UDP 发现读取失败：{err:#}");
                continue;
            }
        };
        let Ok(v) = serde_json::from_slice::<Value>(&buf[..n]) else {
            continue;
        };
        if v.get("t").and_then(|s| s.as_str()) != Some("probe") {
            continue;
        }
        if v.get("c").and_then(|s| s.as_str()) != Some(cluster) {
            continue;
        }
        let reply = json!({ "t": "announce", "c": cluster, "addr": announce, "port": http_port })
            .to_string();
        let _ = sock.send_to(reply.as_bytes(), peer).await;
    }
}

fn is_announce_for(v: &Value, cluster: &str) -> bool {
    v.get("t").and_then(|s| s.as_str()) == Some("announce")
        && v.get("c").and_then(|s| s.as_str()) == Some(cluster)
}
