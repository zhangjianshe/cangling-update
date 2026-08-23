use crate::docker::Docker;
use crate::error::AppError;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_SQL_BYTES: usize = 64 * 1024;
const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 200;

#[derive(Debug, Clone)]
struct PgConn {
    user: String,
    password: Option<String>,
    host: bool,
    container_user: Option<String>,
    default_database: String,
}

#[derive(Debug, Serialize)]
pub struct DbMeta {
    pub engines: Vec<String>,
    pub default_database: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct NameList {
    pub items: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct DbObject {
    pub name: String,
    pub kind: String,
    pub approx_rows: i64,
}

#[derive(Debug, Serialize)]
pub struct ObjectList {
    pub items: Vec<DbObject>,
}

#[derive(Debug, Serialize)]
pub struct RowPage {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub pk: Vec<String>,
    pub total: u64,
    pub offset: u32,
    pub limit: u32,
}

#[derive(Debug, Serialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub command: Option<String>,
    pub truncated: bool,
}

#[derive(Debug, Deserialize)]
pub struct QueryBody {
    pub service: String,
    pub engine: Option<String>,
    pub database: String,
    pub sql: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateRowBody {
    pub service: String,
    pub engine: Option<String>,
    pub database: String,
    pub schema: String,
    pub table: String,
    pub keys: serde_json::Map<String, serde_json::Value>,
    pub values: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct UpdateRowResult {
    pub updated: u64,
}

pub fn require_engine(engine: Option<&str>) -> Result<(), AppError> {
    match engine.unwrap_or("postgres") {
        "postgres" | "postgresql" | "postgis" => Ok(()),
        other => Err(AppError::bad(format!("暂不支持数据库类型：{other}"))),
    }
}

pub fn is_safe_sql_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && !name.contains("..")
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
}

pub fn quote_ident(name: &str) -> Result<String, AppError> {
    if !is_safe_sql_name(name) {
        return Err(AppError::bad(format!("无效的名称：{name}")));
    }
    Ok(format!("\"{}\"", name.replace('"', "\"\"")))
}

pub fn clamp_limit(limit: Option<u32>) -> u32 {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

pub fn is_json_query(sql: &str) -> bool {
    let t = strip_sql_lead(sql);
    matches!(t.as_str(), "SELECT" | "WITH" | "TABLE" | "VALUES")
}

fn strip_sql_lead(sql: &str) -> String {
    let mut s = sql.trim();
    loop {
        if s.starts_with("--") {
            s = s.split_once('\n').map(|(_, rest)| rest.trim()).unwrap_or("");
            continue;
        }
        if s.starts_with("/*") {
            s = s.split_once("*/").map(|(_, rest)| rest.trim()).unwrap_or("");
            continue;
        }
        break;
    }
    s.split_whitespace()
        .next()
        .unwrap_or("")
        .trim_end_matches(';')
        .to_ascii_uppercase()
}

fn strip_trailing_semicolons(sql: &str) -> &str {
    sql.trim_end().trim_end_matches(';').trim_end()
}

fn has_internal_semicolon(sql: &str) -> bool {
    strip_trailing_semicolons(sql).contains(';')
}

async fn printenv(docker: &Docker, dir: &Path, service: &str) -> Result<String, AppError> {
    match docker
        .compose_exec_output(
            dir,
            service,
            None,
            &[],
            &["printenv".into()],
            Duration::from_secs(10),
        )
        .await
    {
        Ok(s) => Ok(s),
        Err(_) => docker
            .compose_exec_output(
                dir,
                service,
                None,
                &[],
                &["env".into()],
                Duration::from_secs(10),
            )
            .await
            .map_err(|e| AppError::bad(format!("无法读取容器环境变量：{e}"))),
    }
}

fn env_value(raw: &str, key: &str) -> Option<String> {
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix(key) {
            if let Some(v) = rest.strip_prefix('=') {
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

async fn try_psql(
    docker: &Docker,
    dir: &Path,
    service: &str,
    conn: &PgConn,
    database: &str,
    sql: &str,
) -> Result<String> {
    if !is_safe_sql_name(database) {
        bail!("无效的数据库名：{database}");
    }
    let mut env: Vec<(&str, &str)> = Vec::new();
    if let Some(p) = conn.password.as_deref() {
        env.push(("PGPASSWORD", p));
    }
    let mut cmd = vec!["psql".into()];
    if conn.host {
        cmd.push("-h".into());
        cmd.push("127.0.0.1".into());
    }
    cmd.push("-U".into());
    cmd.push(conn.user.clone());
    cmd.push("-d".into());
    cmd.push(database.to_string());
    cmd.push("-v".into());
    cmd.push("ON_ERROR_STOP=1".into());
    cmd.push("-tAc".into());
    cmd.push(sql.to_string());
    docker
        .compose_exec_output(
            dir,
            service,
            conn.container_user.as_deref(),
            &env,
            &cmd,
            QUERY_TIMEOUT,
        )
        .await
}

async fn probe_conn(docker: &Docker, dir: &Path, service: &str) -> Result<PgConn, AppError> {
    let env = printenv(docker, dir, service).await.unwrap_or_default();
    let default_database = env_value(&env, "POSTGRES_DB").unwrap_or_else(|| "postgres".into());
    let user = env_value(&env, "POSTGRES_USER").unwrap_or_else(|| "postgres".into());
    let password = env_value(&env, "POSTGRES_PASS").or_else(|| env_value(&env, "POSTGRES_PASSWORD"));

    let candidates = [
        PgConn {
            user: user.clone(),
            password: password.clone(),
            host: true,
            container_user: None,
            default_database: default_database.clone(),
        },
        PgConn {
            user: "postgres".into(),
            password: None,
            host: false,
            container_user: Some("postgres".into()),
            default_database: default_database.clone(),
        },
        PgConn {
            user: "postgres".into(),
            password: None,
            host: false,
            container_user: None,
            default_database: default_database.clone(),
        },
    ];

    let mut last = String::new();
    for conn in candidates {
        match try_psql(docker, dir, service, &conn, "postgres", "SELECT 1").await {
            Ok(s) if s.trim().starts_with('1') => return Ok(conn),
            Ok(_) => {
                if try_psql(docker, dir, service, &conn, &conn.default_database, "SELECT 1")
                    .await
                    .map(|s| s.trim().starts_with('1'))
                    .unwrap_or(false)
                {
                    return Ok(conn);
                }
            }
            Err(err) => last = err.to_string(),
        }
    }
    Err(AppError::bad(format!(
        "容器内未找到可用的 psql / PostgreSQL 连接。{last}"
    )))
}

async fn psql(
    docker: &Docker,
    dir: &Path,
    service: &str,
    conn: &PgConn,
    database: &str,
    sql: &str,
) -> Result<String, AppError> {
    try_psql(docker, dir, service, conn, database, sql)
        .await
        .map_err(|e| AppError::bad(e.to_string()))
}

pub async fn meta(docker: &Docker, dir: &Path, service: &str) -> Result<DbMeta, AppError> {
    crate::docker::validate_service_name(service).map_err(|e| AppError::bad(e.to_string()))?;
    let conn = probe_conn(docker, dir, service).await?;
    Ok(DbMeta {
        engines: vec!["postgres".into()],
        default_database: Some(conn.default_database),
    })
}

pub async fn databases(docker: &Docker, dir: &Path, service: &str) -> Result<NameList, AppError> {
    crate::docker::validate_service_name(service).map_err(|e| AppError::bad(e.to_string()))?;
    let conn = probe_conn(docker, dir, service).await?;
    let sql = "SELECT coalesce(json_agg(datname ORDER BY datname), '[]'::json) \
               FROM pg_database WHERE datistemplate = false AND datallowconn";
    let db = if conn.default_database.is_empty() {
        "postgres"
    } else {
        &conn.default_database
    };
    let raw = psql(docker, dir, service, &conn, db, sql).await?;
    Ok(NameList {
        items: parse_string_array(&raw)?,
    })
}

pub async fn schemas(
    docker: &Docker,
    dir: &Path,
    service: &str,
    database: &str,
) -> Result<NameList, AppError> {
    crate::docker::validate_service_name(service).map_err(|e| AppError::bad(e.to_string()))?;
    let _ = quote_ident(database)?;
    let conn = probe_conn(docker, dir, service).await?;
    let sql = "SELECT coalesce(json_agg(nspname ORDER BY nspname), '[]'::json) \
               FROM pg_namespace \
               WHERE nspname NOT LIKE 'pg\\_%' AND nspname <> 'information_schema'";
    let raw = psql(docker, dir, service, &conn, database, sql).await?;
    Ok(NameList {
        items: parse_string_array(&raw)?,
    })
}

pub async fn objects(
    docker: &Docker,
    dir: &Path,
    service: &str,
    database: &str,
    schema: &str,
) -> Result<ObjectList, AppError> {
    crate::docker::validate_service_name(service).map_err(|e| AppError::bad(e.to_string()))?;
    let _ = quote_ident(schema)?;
    let _ = quote_ident(database)?;
    let conn = probe_conn(docker, dir, service).await?;
    let sql = format!(
        "SELECT coalesce(json_agg(json_build_object(\
            'name', c.relname, \
            'kind', CASE c.relkind WHEN 'r' THEN 'table' WHEN 'p' THEN 'table' \
                    WHEN 'v' THEN 'view' WHEN 'm' THEN 'view' ELSE 'other' END, \
            'approx_rows', GREATEST(c.reltuples, 0)::bigint) ORDER BY c.relkind, c.relname), '[]'::json) \
         FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = {} AND c.relkind IN ('r','p','v','m')",
        sql_literal(schema)
    );
    let raw = psql(docker, dir, service, &conn, database, &sql).await?;
    let value: serde_json::Value = parse_json(&raw)?;
    let mut items = Vec::new();
    if let Some(arr) = value.as_array() {
        for v in arr {
            items.push(DbObject {
                name: v
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                kind: v
                    .get("kind")
                    .and_then(|x| x.as_str())
                    .unwrap_or("table")
                    .to_string(),
                approx_rows: v.get("approx_rows").and_then(|x| x.as_i64()).unwrap_or(0),
            });
        }
    }
    Ok(ObjectList { items })
}

pub async fn rows(
    docker: &Docker,
    dir: &Path,
    service: &str,
    database: &str,
    schema: &str,
    name: &str,
    offset: u32,
    limit: u32,
) -> Result<RowPage, AppError> {
    crate::docker::validate_service_name(service).map_err(|e| AppError::bad(e.to_string()))?;
    let fq = format!("{}.{}", quote_ident(schema)?, quote_ident(name)?);
    let _ = quote_ident(database)?;
    let limit = clamp_limit(Some(limit));
    let conn = probe_conn(docker, dir, service).await?;
    let count_sql = format!("SELECT count(*)::bigint FROM {fq}");
    let count_raw = psql(docker, dir, service, &conn, database, &count_sql).await?;
    let total: u64 = count_raw.trim().parse().unwrap_or(0);
    let data_sql = format!(
        "SELECT coalesce(json_agg(row_to_json(q)), '[]'::json) FROM (SELECT * FROM {fq} LIMIT {limit} OFFSET {offset}) q"
    );
    let raw = psql(docker, dir, service, &conn, database, &data_sql).await?;
    let (columns, rows) = rows_from_json_agg(&raw)?;
    let pk_sql = format!(
        "SELECT coalesce(json_agg(kcu.column_name ORDER BY kcu.ordinal_position), '[]'::json) \
         FROM information_schema.table_constraints tc \
         JOIN information_schema.key_column_usage kcu \
           ON tc.constraint_name = kcu.constraint_name \
          AND tc.table_schema = kcu.table_schema \
          AND tc.table_name = kcu.table_name \
         WHERE tc.constraint_type = 'PRIMARY KEY' \
           AND tc.table_schema = {} AND tc.table_name = {}",
        sql_literal(schema),
        sql_literal(name)
    );
    let pk_raw = psql(docker, dir, service, &conn, database, &pk_sql).await?;
    let pk = parse_string_array(&pk_raw).unwrap_or_default();
    Ok(RowPage {
        columns,
        rows,
        pk,
        total,
        offset,
        limit,
    })
}

pub fn sql_value(v: &serde_json::Value) -> Result<String, AppError> {
    match v {
        serde_json::Value::Null => Ok("NULL".into()),
        serde_json::Value::Bool(b) => Ok(if *b { "TRUE".into() } else { "FALSE".into() }),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        serde_json::Value::String(s) => Ok(sql_literal(s)),
        other => Ok(format!("{}::jsonb", sql_literal(&other.to_string()))),
    }
}

fn sql_eq(ident: &str, v: &serde_json::Value) -> Result<String, AppError> {
    if v.is_null() {
        Ok(format!("{ident} IS NULL"))
    } else {
        Ok(format!("{ident} IS NOT DISTINCT FROM {}", sql_value(v)?))
    }
}

pub async fn update_row(
    docker: &Docker,
    dir: &Path,
    body: &UpdateRowBody,
) -> Result<UpdateRowResult, AppError> {
    crate::docker::validate_service_name(&body.service).map_err(|e| AppError::bad(e.to_string()))?;
    let fq = format!("{}.{}", quote_ident(&body.schema)?, quote_ident(&body.table)?);
    let _ = quote_ident(&body.database)?;
    if body.values.is_empty() || body.keys.is_empty() {
        return Err(AppError::bad("缺少要更新的行"));
    }
    if body.values.len() > 64 || body.keys.len() > 64 {
        return Err(AppError::bad("列数过多"));
    }
    let mut sets = Vec::new();
    for (col, val) in &body.values {
        let ident = quote_ident(col)?;
        if body.keys.get(col) == Some(val) {
            continue;
        }
        sets.push(format!("{ident} = {}", sql_value(val)?));
    }
    if sets.is_empty() {
        return Err(AppError::bad("没有修改"));
    }
    let mut wheres = Vec::new();
    for (col, val) in &body.keys {
        let ident = quote_ident(col)?;
        wheres.push(sql_eq(&ident, val)?);
    }
    let sql = format!(
        "WITH u AS (UPDATE {fq} SET {} WHERE {} RETURNING 1) SELECT count(*)::bigint FROM u",
        sets.join(", "),
        wheres.join(" AND ")
    );
    let conn = probe_conn(docker, dir, &body.service).await?;
    let raw = psql(docker, dir, &body.service, &conn, &body.database, &sql).await?;
    let updated: u64 = raw.trim().parse().unwrap_or(0);
    if updated == 0 {
        return Err(AppError::conflict("未更新任何行，记录可能已被修改或不存在"));
    }
    Ok(UpdateRowResult { updated })
}

pub async fn query(
    docker: &Docker,
    dir: &Path,
    service: &str,
    database: &str,
    sql: &str,
) -> Result<QueryResult, AppError> {
    crate::docker::validate_service_name(service).map_err(|e| AppError::bad(e.to_string()))?;
    let _ = quote_ident(database)?;
    if sql.len() > MAX_SQL_BYTES {
        return Err(AppError::bad("SQL 不能超过 64 KB"));
    }
    if sql.trim().is_empty() {
        return Err(AppError::bad("SQL 不能为空"));
    }
    let conn = probe_conn(docker, dir, service).await?;
    let trimmed = strip_trailing_semicolons(sql);
    if is_json_query(sql) && !has_internal_semicolon(sql) {
        let wrapped = format!(
            "SELECT coalesce(json_agg(row_to_json(q)), '[]'::json) FROM (SELECT * FROM ({trimmed}) s LIMIT {MAX_LIMIT}) q"
        );
        let raw = psql(docker, dir, service, &conn, database, &wrapped).await?;
        let (columns, rows) = rows_from_json_agg(&raw)?;
        let truncated = rows.len() as u32 >= MAX_LIMIT;
        return Ok(QueryResult {
            columns,
            rows,
            command: None,
            truncated,
        });
    }
    let raw = psql(docker, dir, service, &conn, database, sql).await?;
    Ok(QueryResult {
        columns: vec!["output".into()],
        rows: raw
            .lines()
            .filter(|l| !l.is_empty())
            .take(MAX_LIMIT as usize)
            .map(|l| vec![serde_json::Value::String(l.to_string())])
            .collect(),
        command: Some(raw.trim().to_string()),
        truncated: false,
    })
}

fn sql_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn parse_json(raw: &str) -> Result<serde_json::Value, AppError> {
    let s = raw.trim();
    if s.is_empty() || s == "null" {
        return Ok(serde_json::json!([]));
    }
    serde_json::from_str(s).map_err(|e| AppError::bad(format!("解析查询结果失败：{e}")))
}

fn parse_string_array(raw: &str) -> Result<Vec<String>, AppError> {
    let value = parse_json(raw)?;
    Ok(value
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect())
}

fn rows_from_json_agg(raw: &str) -> Result<(Vec<String>, Vec<Vec<serde_json::Value>>), AppError> {
    let value = parse_json(raw)?;
    let arr = value.as_array().cloned().unwrap_or_default();
    let mut columns = Vec::new();
    if let Some(serde_json::Value::Object(map)) = arr.first() {
        columns = map.keys().cloned().collect();
    }
    let mut rows = Vec::new();
    for item in arr {
        match item {
            serde_json::Value::Object(map) => {
                if columns.is_empty() {
                    columns = map.keys().cloned().collect();
                }
                rows.push(
                    columns
                        .iter()
                        .map(|c| map.get(c).cloned().unwrap_or(serde_json::Value::Null))
                        .collect(),
                );
            }
            other => rows.push(vec![other]),
        }
    }
    Ok((columns, rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_quotes() {
        assert!(is_safe_sql_name("public"));
        assert!(is_safe_sql_name("cl-base"));
        assert!(is_safe_sql_name("gis_geoserver"));
        assert!(!is_safe_sql_name("public;drop"));
        assert!(!is_safe_sql_name("a b"));
        assert_eq!(quote_ident("public").unwrap(), "\"public\"");
        assert_eq!(quote_ident("cl-base").unwrap(), "\"cl-base\"");
    }

    #[test]
    fn query_kind() {
        assert!(is_json_query("SELECT 1"));
        assert!(is_json_query("  /* x */\n select * from t"));
        assert!(is_json_query("WITH a AS (SELECT 1) SELECT * FROM a"));
        assert!(!is_json_query("INSERT INTO t VALUES (1)"));
        assert!(!is_json_query("EXPLAIN SELECT 1"));
        assert!(!has_internal_semicolon("SELECT 1;"));
        assert!(has_internal_semicolon("SELECT 1; SELECT 2"));
    }

    #[test]
    fn json_rows() {
        let (cols, rows) = rows_from_json_agg(r#"[{"id":1,"name":"a"},{"id":2,"name":"b"}]"#).unwrap();
        assert!(cols.contains(&"id".into()));
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn sql_values() {
        assert_eq!(sql_value(&serde_json::Value::Null).unwrap(), "NULL");
        assert_eq!(sql_value(&serde_json::json!(true)).unwrap(), "TRUE");
        assert_eq!(sql_value(&serde_json::json!(3)).unwrap(), "3");
        assert_eq!(sql_value(&serde_json::json!("a'b")).unwrap(), "'a''b'");
        assert!(sql_eq("\"id\"", &serde_json::Value::Null)
            .unwrap()
            .contains("IS NULL"));
    }
}
