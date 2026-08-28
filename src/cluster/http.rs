//! worker→master 的极简 HTTP 客户端（复用 axum 已在用的 hyper 栈，零新增依赖）。

use anyhow::{Context, Result};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::StatusCode;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::time::Duration;

use super::TOKEN_HEADER;

fn client() -> Client<HttpConnector, Full<Bytes>> {
    let mut connector = HttpConnector::new();
    connector.set_connect_timeout(Some(Duration::from_secs(5)));
    Client::builder(TokioExecutor::new()).build(connector)
}

/// POST JSON 到 master，返回 (状态码, 响应 JSON)。
pub async fn post_json(
    url: &str,
    token: &str,
    body: &serde_json::Value,
) -> Result<(StatusCode, serde_json::Value)> {
    let payload = body.to_string();
    let req = hyper::Request::builder()
        .method(hyper::Method::POST)
        .uri(url)
        .header("content-type", "application/json")
        .header(TOKEN_HEADER, token)
        .body(Full::new(Bytes::from(payload)))
        .context("构造 HTTP 请求失败")?;

    let res = client().request(req).await.context("请求 master 失败")?;
    let status = res.status();
    let bytes = res
        .into_body()
        .collect()
        .await
        .context("读取 master 响应失败")?
        .to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(&bytes).into()))
    };
    Ok((status, json))
}

/// GET 到 master，返回 (状态码, 响应 JSON)。
pub async fn get_json(
    url: &str,
    token: &str,
) -> Result<(StatusCode, serde_json::Value)> {
    let (status, bytes) = get_bytes(url, token).await?;
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(&bytes).into()))
    };
    Ok((status, json))
}

/// GET 到 master，返回 (状态码, 原始字节)。
pub async fn get_bytes(url: &str, token: &str) -> Result<(StatusCode, Bytes)> {
    let req = hyper::Request::builder()
        .method(hyper::Method::GET)
        .uri(url)
        .header(TOKEN_HEADER, token)
        .body(Full::new(Bytes::new()))
        .context("构造 HTTP 请求失败")?;

    let res = client().request(req).await.context("请求 master 失败")?;
    let status = res.status();
    let bytes = res
        .into_body()
        .collect()
        .await
        .context("读取 master 响应失败")?
        .to_bytes();
    Ok((status, bytes))
}
