//! 分布式集群模块：master 收集各 worker 节点信息，网页控制台可查看节点在线状态与主机信息。
//!
//! 角色由启动参数决定（standalone / master / worker）。worker 通过 UDP 广播发现
//! master（也可用 --master 直接指定），随后用 HTTP 注册并周期发送心跳。

pub mod client;
pub mod discovery;
pub mod http;
pub mod init;
pub mod server;

use crate::paths::AppPaths;
use serde::{Deserialize, Serialize};

/// HTTP 端口之外，用于发现主节点的 UDP 广播端口。
pub const DEFAULT_DISCOVERY_PORT: u16 = 5401;
/// worker 心跳间隔（秒）。
pub const HEARTBEAT_INTERVAL_SECS: u64 = 15;
/// 超过该秒数没有心跳则判定为离线。
pub const OFFLINE_AFTER_SECS: u64 = 45;
/// 携带集群令牌的请求头，用于 worker↔master 的机器间认证。
pub const TOKEN_HEADER: &str = "x-cluster-token";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    Standalone,
    Master,
    Worker,
}

impl Role {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "standalone" | "" => Ok(Role::Standalone),
            "master" => Ok(Role::Master),
            "worker" => Ok(Role::Worker),
            other => Err(format!(
                "未知集群角色 {other}，可选 standalone / master / worker"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Standalone => "standalone",
            Role::Master => "master",
            Role::Worker => "worker",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClusterConfig {
    pub role: Role,
    /// 集群共享令牌；master / worker 角色必须设置。
    pub token: Option<String>,
    /// 显式指定的 master 地址（http://host:port）；worker 未设置时走 UDP 广播发现。
    pub master_url: Option<String>,
    pub discovery_port: u16,
}

impl ClusterConfig {
    /// 令牌的短哈希，只用于广播里识别同一个集群，避免把令牌明文发到子网。
    pub fn cluster_id(&self) -> Option<String> {
        self.token.as_deref().map(cluster_id)
    }
}

/// FNV-1a 64 位哈希，取低 48 位输出 12 位十六进制。仅作集群识别，不做安全用途。
pub fn cluster_id(token: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in token.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:012x}", h & 0xffff_ffff_ffff)
}

/// 节点 id 持久化在数据目录，重启后保持一致（master / worker 共用）。
pub fn load_or_create_node_id(paths: &AppPaths) -> String {
    let path = paths.config_dir.join("node-id");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    if std::fs::write(&path, &id).is_err() {
        tracing::warn!("无法持久化节点 id 到 {}", path.display());
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_parses() {
        assert_eq!(Role::parse("master").unwrap(), Role::Master);
        assert_eq!(Role::parse("WORKER").unwrap(), Role::Worker);
        assert_eq!(Role::parse("").unwrap(), Role::Standalone);
        assert!(Role::parse("wat").is_err());
    }

    #[test]
    fn cluster_id_is_stable() {
        assert_eq!(cluster_id("abc"), cluster_id("abc"));
        assert_ne!(cluster_id("abc"), cluster_id("abd"));
        assert_eq!(cluster_id("abc").len(), 12);
    }
}
