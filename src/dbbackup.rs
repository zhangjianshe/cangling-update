use crate::dbadmin::{
    is_safe_sql_name, parse_json, probe_conn, psql, quote_ident, sql_literal, PgConn,
};
use crate::docker::Docker;
use crate::error::AppError;
use crate::models::Project;
use crate::state::AppState;
use chrono::{Local, Timelike};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

const DATABASE_COMMAND_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BackupObject {
    pub schema: String,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupManifest {
    pub id: String,
    pub project_id: String,
    pub service: String,
    pub database: String,
    #[serde(default = "default_backup_kind")]
    pub kind: String,
    pub engine: String,
    pub server_version: String,
    pub server_platform: String,
    pub postgis_version: String,
    pub created_at: String,
    pub dump_bytes: u64,
    pub dump_sha256: String,
    pub globals_bytes: u64,
    pub globals_sha256: String,
    pub objects: Vec<BackupObject>,
    #[serde(default = "default_true")]
    pub valid: bool,
}

fn default_backup_kind() -> String {
    "manual".into()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupSchedule {
    pub project_id: String,
    pub enabled: bool,
    pub service: String,
    pub database: String,
    pub time_of_day: String,
    pub retain_count: u32,
    pub storage_id: String,
    pub backup_before_update: bool,
    pub last_run_date: String,
    pub last_started_at: String,
    pub last_finished_at: String,
    pub last_status: String,
    pub last_message: String,
    pub updated_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupScheduleBody {
    pub enabled: bool,
    pub service: String,
    pub database: String,
    pub time_of_day: String,
    pub retain_count: u32,
    #[serde(default)]
    pub storage_id: String,
    #[serde(default = "default_true")]
    pub backup_before_update: bool,
}

fn default_true() -> bool { true }

#[derive(Debug, Deserialize)]
pub struct BackupBody {
    pub service: String,
    pub engine: Option<String>,
    pub database: String,
    pub job_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RestoreBody {
    pub service: String,
    pub engine: Option<String>,
    pub database: String,
    pub mode: String,
    #[serde(default)]
    pub schema: String,
    #[serde(default)]
    pub tables: Vec<String>,
    pub confirm_database: String,
    pub job_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreResult {
    pub restored_backup_id: String,
    pub safety_backup_id: String,
    pub tables: Vec<String>,
}

fn safe_component(value: &str) -> Result<(), AppError> {
    if !is_safe_sql_name(value) {
        return Err(AppError::bad("无效的数据库备份编号"));
    }
    Ok(())
}

fn backup_dir(root: &Path, project_id: &str, backup_id: &str) -> Result<PathBuf, AppError> {
    safe_component(project_id)?;
    safe_component(backup_id)?;
    Ok(root.join(project_id).join(backup_id))
}

fn sha256_file(path: &Path) -> Result<String, AppError> {
    let mut file = fs::File::open(path).map_err(|error| AppError::internal(error.to_string()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| AppError::internal(error.to_string()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn read_manifest(dir: &Path) -> Result<BackupManifest, AppError> {
    let data = fs::read(dir.join("manifest.json"))
        .map_err(|_| AppError::not_found("数据库备份不存在或不完整"))?;
    serde_json::from_slice(&data).map_err(|error| AppError::internal(error.to_string()))
}

pub fn list(root: &Path, project_id: &str) -> Result<Vec<BackupManifest>, AppError> {
    safe_component(project_id)?;
    let mut backups = Vec::new();
    if let Ok(entries) = fs::read_dir(root.join(project_id)) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                if let Ok(mut manifest) = read_manifest(&entry.path()) {
                    manifest.valid = manifest.dump_bytes > 0
                        && manifest.globals_bytes > 0
                        && entry.path().join("database.dump").metadata().map(|m| m.len()).ok()
                            == Some(manifest.dump_bytes)
                        && entry.path().join("globals.sql").metadata().map(|m| m.len()).ok()
                            == Some(manifest.globals_bytes);
                    backups.push(manifest);
                }
            }
        }
    }
    backups.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    Ok(backups)
}

fn pg_env(conn: &PgConn) -> Vec<(&str, &str)> {
    conn.password
        .as_deref()
        .map(|password| vec![("PGPASSWORD", password)])
        .unwrap_or_default()
}

fn connection_args(conn: &PgConn, database: &str) -> Vec<String> {
    let mut args = Vec::new();
    if conn.host {
        args.extend(["-h".into(), "127.0.0.1".into()]);
    }
    args.extend(["-U".into(), conn.user.clone(), "-d".into(), database.into()]);
    args
}

fn postgres_major(version: &str) -> Option<u32> {
    version
        .trim()
        .split('.')
        .next()
        .and_then(|part| part.parse().ok())
}

async fn inventory(
    docker: &Docker,
    project_dir: &Path,
    service: &str,
    conn: &PgConn,
    database: &str,
) -> Result<Vec<BackupObject>, AppError> {
    let sql = "SELECT coalesce(json_agg(json_build_object(\
      'schema',n.nspname,'name',c.relname,'kind',CASE c.relkind \
      WHEN 'r' THEN 'table' WHEN 'p' THEN 'table' WHEN 'v' THEN 'view' \
      WHEN 'm' THEN 'materialized-view' WHEN 'S' THEN 'sequence' END) \
      ORDER BY n.nspname,c.relkind,c.relname),'[]'::json) \
      FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace \
      WHERE n.nspname NOT LIKE 'pg\\_%' AND n.nspname<>'information_schema' \
      AND c.relkind IN ('r','p','v','m','S')";
    let raw = psql(docker, project_dir, service, conn, database, sql).await?;
    serde_json::from_value(parse_json(&raw)?).map_err(|error| AppError::internal(error.to_string()))
}

pub async fn create(
    docker: &Docker,
    project_dir: &Path,
    root: &Path,
    project_id: &str,
    service: &str,
    database: &str,
    kind: &str,
) -> Result<BackupManifest, AppError> {
    crate::docker::validate_service_name(service)
        .map_err(|error| AppError::bad(error.to_string()))?;
    let _ = quote_ident(database)?;
    safe_component(project_id)?;
    let conn = probe_conn(docker, project_dir, service).await?;
    let id = uuid::Uuid::new_v4().to_string();
    let dir = backup_dir(root, project_id, &id)?;
    fs::create_dir_all(&dir).map_err(|error| AppError::internal(error.to_string()))?;
    #[cfg(unix)]
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
        .map_err(|error| AppError::internal(error.to_string()))?;
    let dump = dir.join("database.dump");
    let globals = dir.join("globals.sql");
    let env = pg_env(&conn);
    let mut dump_command = vec![
        "pg_dump".into(),
        "--format=custom".into(),
        "--no-owner".into(),
        "--no-privileges".into(),
    ];
    dump_command.extend(connection_args(&conn, database));
    if let Err(error) = docker
        .compose_exec_to_file(
            project_dir,
            service,
            conn.container_user.as_deref(),
            &env,
            &dump_command,
            &dump,
            DATABASE_COMMAND_TIMEOUT,
        )
        .await
    {
        let _ = fs::remove_dir_all(&dir);
        return Err(AppError::bad(format!("数据库备份失败：{error}")));
    }
    #[cfg(unix)]
    fs::set_permissions(&dump, fs::Permissions::from_mode(0o600))
        .map_err(|error| AppError::internal(error.to_string()))?;
    let mut globals_command = vec!["pg_dumpall".into(), "--globals-only".into()];
    if conn.host {
        globals_command.extend(["-h".into(), "127.0.0.1".into()]);
    }
    globals_command.extend(["-U".into(), conn.user.clone()]);
    if let Err(error) = docker
        .compose_exec_to_file(
            project_dir,
            service,
            conn.container_user.as_deref(),
            &env,
            &globals_command,
            &globals,
            DATABASE_COMMAND_TIMEOUT,
        )
        .await
    {
        let _ = fs::remove_dir_all(&dir);
        return Err(AppError::bad(format!("数据库角色备份失败：{error}")));
    }
    #[cfg(unix)]
    fs::set_permissions(&globals, fs::Permissions::from_mode(0o600))
        .map_err(|error| AppError::internal(error.to_string()))?;
    let server_version = psql(
        docker,
        project_dir,
        service,
        &conn,
        database,
        "SHOW server_version",
    )
    .await?
    .trim()
    .to_string();
    let server_platform = psql(
        docker,
        project_dir,
        service,
        &conn,
        database,
        "SELECT version()",
    )
    .await?
    .trim()
    .to_string();
    let postgis_version = psql(
        docker,
        project_dir,
        service,
        &conn,
        database,
        "SELECT CASE WHEN EXISTS(SELECT 1 FROM pg_extension WHERE extname='postgis') THEN postgis_lib_version() ELSE '' END",
    )
    .await
    .unwrap_or_default()
    .trim()
    .to_string();
    let objects = inventory(docker, project_dir, service, &conn, database).await?;
    let manifest = BackupManifest {
        id,
        project_id: project_id.into(),
        service: service.into(),
        database: database.into(),
        kind: kind.into(),
        engine: "postgres".into(),
        server_version,
        server_platform,
        postgis_version,
        created_at: chrono::Utc::now().to_rfc3339(),
        dump_bytes: dump.metadata().map(|meta| meta.len()).unwrap_or(0),
        dump_sha256: sha256_file(&dump)?,
        globals_bytes: globals.metadata().map(|meta| meta.len()).unwrap_or(0),
        globals_sha256: sha256_file(&globals)?,
        objects,
        valid: true,
    };
    let data = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| AppError::internal(error.to_string()))?;
    fs::write(dir.join("manifest.json"), data)
        .map_err(|error| AppError::internal(error.to_string()))?;
    Ok(manifest)
}

async fn external_references(
    docker: &Docker,
    dir: &Path,
    service: &str,
    conn: &PgConn,
    database: &str,
    schema: &str,
    tables: &[String],
) -> Result<Vec<String>, AppError> {
    let names = tables
        .iter()
        .map(|name| sql_literal(name))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT coalesce(json_agg(DISTINCT sn.nspname||'.'||src.relname),'[]'::json) \
         FROM pg_constraint fk JOIN pg_class src ON src.oid=fk.conrelid \
         JOIN pg_namespace sn ON sn.oid=src.relnamespace JOIN pg_class dst ON dst.oid=fk.confrelid \
         JOIN pg_namespace dn ON dn.oid=dst.relnamespace WHERE fk.contype='f' \
         AND dn.nspname={} AND dst.relname IN ({}) \
         AND NOT (sn.nspname={} AND src.relname IN ({}))",
        sql_literal(schema),
        names,
        sql_literal(schema),
        names
    );
    let raw = psql(docker, dir, service, conn, database, &sql).await?;
    Ok(parse_json(&raw)?
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect())
}

async fn reset_sequences(
    docker: &Docker,
    dir: &Path,
    service: &str,
    conn: &PgConn,
    database: &str,
    schema: &str,
    tables: &[String],
) -> Result<(), AppError> {
    let names = tables
        .iter()
        .map(|name| sql_literal(name))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT coalesce(json_agg(json_build_object('table',table_name,'column',column_name, \
         'sequence',pg_get_serial_sequence(format('%I.%I',table_schema,table_name),column_name))),'[]'::json) \
         FROM information_schema.columns WHERE table_schema={} AND table_name IN ({}) \
         AND (is_identity='YES' OR column_default LIKE 'nextval(%')",
        sql_literal(schema), names
    );
    let raw = psql(docker, dir, service, conn, database, &sql).await?;
    let mut statements = Vec::new();
    for value in parse_json(&raw)?.as_array().into_iter().flatten() {
        let Some(table) = value.get("table").and_then(|item| item.as_str()) else {
            continue;
        };
        let Some(column) = value.get("column").and_then(|item| item.as_str()) else {
            continue;
        };
        let Some(sequence) = value.get("sequence").and_then(|item| item.as_str()) else {
            continue;
        };
        statements.push(format!(
            "SELECT setval({},COALESCE(MAX({}),1),MAX({}) IS NOT NULL) FROM {}.{}",
            sql_literal(sequence),
            quote_ident(column)?,
            quote_ident(column)?,
            quote_ident(schema)?,
            quote_ident(table)?
        ));
    }
    if !statements.is_empty() {
        psql(docker, dir, service, conn, database, &statements.join(";")).await?;
    }
    Ok(())
}

pub async fn restore(
    docker: &Docker,
    project_dir: &Path,
    root: &Path,
    project_id: &str,
    backup_id: &str,
    body: &RestoreBody,
) -> Result<RestoreResult, AppError> {
    crate::dbadmin::require_engine(body.engine.as_deref())?;
    crate::docker::validate_service_name(&body.service)
        .map_err(|error| AppError::bad(error.to_string()))?;
    let _ = quote_ident(&body.database)?;
    if body.confirm_database != body.database {
        return Err(AppError::bad("确认数据库名不一致，已取消恢复"));
    }
    let source_dir = backup_dir(root, project_id, backup_id)?;
    let manifest = read_manifest(&source_dir)?;
    if manifest.dump_bytes == 0 || manifest.globals_bytes == 0 {
        return Err(AppError::bad("该备份是 0 字节无效备份，拒绝恢复"));
    }
    if manifest.database != body.database {
        return Err(AppError::bad("备份数据库与目标数据库不一致"));
    }
    let dump = source_dir.join("database.dump");
    if !sha256_file(&dump)?.eq_ignore_ascii_case(&manifest.dump_sha256) {
        return Err(AppError::bad("数据库备份 SHA-256 校验失败，拒绝恢复"));
    }
    // This must complete before any destructive command is issued.
    let safety = create(
        docker,
        project_dir,
        root,
        project_id,
        &body.service,
        &body.database,
        "pre-restore",
    )
    .await?;
    let conn = probe_conn(docker, project_dir, &body.service).await?;
    let target_version = psql(
        docker,
        project_dir,
        &body.service,
        &conn,
        &body.database,
        "SHOW server_version",
    )
    .await?;
    if postgres_major(&target_version) < postgres_major(&manifest.server_version) {
        return Err(AppError::bad(format!(
            "不能把 PostgreSQL {} 的备份恢复到较旧的 PostgreSQL {}",
            manifest.server_version,
            target_version.trim()
        )));
    }
    if !manifest.postgis_version.is_empty() {
        let available_postgis = psql(
            docker,
            project_dir,
            &body.service,
            &conn,
            &body.database,
            "SELECT coalesce((SELECT default_version FROM pg_available_extensions WHERE name='postgis'),'')",
        )
        .await?
        .trim()
        .to_string();
        if available_postgis.is_empty() {
            return Err(AppError::bad(
                "目标 PostgreSQL 容器没有安装 PostGIS，拒绝恢复",
            ));
        }
    }
    let env = pg_env(&conn);
    let mut command = vec!["pg_restore".into()];
    match body.mode.as_str() {
        "full" => command.extend([
            "--clean".into(),
            "--if-exists".into(),
            "--exit-on-error".into(),
            "--single-transaction".into(),
            "--no-owner".into(),
            "--no-privileges".into(),
        ]),
        "tables" => {
            let _ = quote_ident(&body.schema)?;
            if body.tables.is_empty() {
                return Err(AppError::bad("请至少选择一张表"));
            }
            for table in &body.tables {
                let _ = quote_ident(table)?;
                if !manifest.objects.iter().any(|object| {
                    object.kind == "table" && object.schema == body.schema && object.name == *table
                }) {
                    return Err(AppError::bad(format!(
                        "备份中不存在表 {}.{table}",
                        body.schema
                    )));
                }
            }
            let references = external_references(
                docker,
                project_dir,
                &body.service,
                &conn,
                &body.database,
                &body.schema,
                &body.tables,
            )
            .await?;
            if !references.is_empty() {
                return Err(AppError::bad(format!(
                    "以下未选择表通过外键引用所选表，请一并选择后再恢复：{}",
                    references.join("、")
                )));
            }
            let targets = body
                .tables
                .iter()
                .map(|table| {
                    Ok(format!(
                        "{}.{}",
                        quote_ident(&body.schema)?,
                        quote_ident(table)?
                    ))
                })
                .collect::<Result<Vec<_>, AppError>>()?;
            psql(
                docker,
                project_dir,
                &body.service,
                &conn,
                &body.database,
                &format!("TRUNCATE TABLE {} RESTART IDENTITY", targets.join(",")),
            )
            .await?;
            command.extend([
                "--data-only".into(),
                "--exit-on-error".into(),
                "--single-transaction".into(),
                "--disable-triggers".into(),
                "--no-owner".into(),
                "--no-privileges".into(),
                "--schema".into(),
                body.schema.clone(),
            ]);
            for table in &body.tables {
                command.extend(["--table".into(), table.clone()]);
            }
        }
        _ => return Err(AppError::bad("无效的数据库恢复方式")),
    }
    command.extend(connection_args(&conn, &body.database));
    let restore_result = docker
        .compose_exec_from_file(
            project_dir,
            &body.service,
            conn.container_user.as_deref(),
            &env,
            &command,
            &dump,
            DATABASE_COMMAND_TIMEOUT,
        )
        .await;
    if let Err(error) = restore_result {
        if body.mode == "tables" {
            let safety_dump = backup_dir(root, project_id, &safety.id)?.join("database.dump");
            let rollback = docker
                .compose_exec_from_file(
                    project_dir,
                    &body.service,
                    conn.container_user.as_deref(),
                    &env,
                    &command,
                    &safety_dump,
                    DATABASE_COMMAND_TIMEOUT,
                )
                .await;
            return match rollback {
                Ok(_) => {
                    let _ = reset_sequences(
                        docker,
                        project_dir,
                        &body.service,
                        &conn,
                        &body.database,
                        &body.schema,
                        &body.tables,
                    )
                    .await;
                    Err(AppError::bad(format!(
                        "数据库恢复失败，已用恢复前安全备份还原所选表：{error}"
                    )))
                }
                Err(rollback_error) => Err(AppError::bad(format!(
                    "数据库恢复失败，自动还原也失败；请使用安全备份 {} 手动恢复。原错误：{error}；还原错误：{rollback_error}",
                    safety.id
                ))),
            };
        }
        return Err(AppError::bad(format!("数据库恢复失败：{error}")));
    }
    if body.mode == "tables" {
        reset_sequences(
            docker,
            project_dir,
            &body.service,
            &conn,
            &body.database,
            &body.schema,
            &body.tables,
        )
        .await?;
    }
    Ok(RestoreResult {
        restored_backup_id: manifest.id,
        safety_backup_id: safety.id,
        tables: body.tables.clone(),
    })
}

fn validate_time(value: &str) -> Result<(u32, u32), AppError> {
    let Some((hour, minute)) = value.split_once(':') else {
        return Err(AppError::bad("备份时间格式必须为 HH:MM"));
    };
    let hour: u32 = hour.parse().map_err(|_| AppError::bad("无效的备份小时"))?;
    let minute: u32 = minute.parse().map_err(|_| AppError::bad("无效的备份分钟"))?;
    if hour > 23 || minute > 59 || value.len() != 5 {
        return Err(AppError::bad("备份时间必须介于 00:00 和 23:59"));
    }
    Ok((hour, minute))
}

fn map_schedule(row: &rusqlite::Row<'_>) -> rusqlite::Result<BackupSchedule> {
    Ok(BackupSchedule {
        project_id: row.get(0)?, enabled: row.get::<_, i64>(1)? != 0,
        service: row.get(2)?, database: row.get(3)?, time_of_day: row.get(4)?,
        retain_count: row.get::<_, i64>(5)?.max(1) as u32, storage_id: row.get(6)?,
        backup_before_update: row.get::<_, i64>(7)? != 0, last_run_date: row.get(8)?,
        last_started_at: row.get(9)?, last_finished_at: row.get(10)?,
        last_status: row.get(11)?, last_message: row.get(12)?, updated_at: row.get(13)?,
    })
}

const SCHEDULE_COLUMNS: &str = "project_id, enabled, service, database_name, time_of_day, retain_count, storage_id, backup_before_update, last_run_date, last_started_at, last_finished_at, last_status, last_message, updated_at";

pub fn get_schedule(conn: &Connection, project_id: &str) -> Result<BackupSchedule, AppError> {
    let mut stmt = conn.prepare(&format!("SELECT {SCHEDULE_COLUMNS} FROM db_backup_schedules WHERE project_id=?1"))
        .map_err(|error| AppError::internal(error.to_string()))?;
    let found = stmt.query_row([project_id], map_schedule).optional()
        .map_err(|error| AppError::internal(error.to_string()))?;
    Ok(found.unwrap_or_else(|| BackupSchedule {
        project_id: project_id.into(), enabled: false, service: String::new(), database: String::new(),
        time_of_day: "03:00".into(), retain_count: 7, storage_id: String::new(),
        backup_before_update: true, last_run_date: String::new(), last_started_at: String::new(),
        last_finished_at: String::new(), last_status: String::new(), last_message: String::new(),
        updated_at: String::new(),
    }))
}

pub fn save_schedule(conn: &Connection, project_id: &str, body: &BackupScheduleBody) -> Result<BackupSchedule, AppError> {
    validate_time(&body.time_of_day)?;
    if body.retain_count == 0 || body.retain_count > 365 { return Err(AppError::bad("定时备份保留份数必须介于 1 和 365")); }
    if body.enabled && (body.service.trim().is_empty() || body.database.trim().is_empty()) {
        return Err(AppError::bad("启用自动备份前必须选择 PostgreSQL 容器和数据库"));
    }
    if !body.service.is_empty() { crate::docker::validate_service_name(&body.service).map_err(|e| AppError::bad(e.to_string()))?; }
    if !body.database.is_empty() { let _ = quote_ident(&body.database)?; }
    if !body.storage_id.is_empty() && crate::storage::get_storage_db(conn, &body.storage_id)
        .map_err(|e| AppError::internal(e.to_string()))?.is_none() { return Err(AppError::bad("选择的存储不存在")); }
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute("INSERT INTO db_backup_schedules (project_id,enabled,service,database_name,time_of_day,retain_count,storage_id,backup_before_update,last_run_date,last_started_at,last_finished_at,last_status,last_message,updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'','','','','',?9) ON CONFLICT(project_id) DO UPDATE SET enabled=excluded.enabled,service=excluded.service,database_name=excluded.database_name,time_of_day=excluded.time_of_day,retain_count=excluded.retain_count,storage_id=excluded.storage_id,backup_before_update=excluded.backup_before_update,updated_at=excluded.updated_at",
        params![project_id, body.enabled as i64, body.service.trim(), body.database.trim(), body.time_of_day, body.retain_count, body.storage_id, body.backup_before_update as i64, now])
        .map_err(|e| AppError::internal(e.to_string()))?;
    get_schedule(conn, project_id)
}

fn enabled_schedules(conn: &Connection) -> Result<Vec<BackupSchedule>, AppError> {
    let mut stmt = conn.prepare(&format!("SELECT {SCHEDULE_COLUMNS} FROM db_backup_schedules WHERE enabled=1"))
        .map_err(|e| AppError::internal(e.to_string()))?;
    let rows = stmt.query_map([], map_schedule).map_err(|e| AppError::internal(e.to_string()))?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e| AppError::internal(e.to_string()))
}

pub(crate) fn set_run_status(conn: &Connection, project_id: &str, run_date: Option<&str>, status: &str, message: &str, finished: bool) -> Result<(), AppError> {
    let now = chrono::Utc::now().to_rfc3339();
    if finished {
        conn.execute("UPDATE db_backup_schedules SET last_finished_at=?2,last_status=?3,last_message=?4 WHERE project_id=?1", params![project_id, now, status, message])
    } else {
        conn.execute("UPDATE db_backup_schedules SET last_run_date=?2,last_started_at=?3,last_finished_at='',last_status=?4,last_message=?5 WHERE project_id=?1", params![project_id, run_date.unwrap_or(""), now, status, message])
    }.map_err(|e| AppError::internal(e.to_string()))?;
    Ok(())
}

fn copy_to_storage(state: &AppState, schedule: &BackupSchedule, manifest: &BackupManifest) -> Result<Option<PathBuf>, AppError> {
    if schedule.storage_id.is_empty() { return Ok(None); }
    let storage = { let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        crate::storage::get_storage_db(&conn, &schedule.storage_id).map_err(|e| AppError::internal(e.to_string()))?
            .ok_or_else(|| AppError::bad("自动备份存储不存在"))? };
    let root = PathBuf::from(&storage.target_dir);
    if storage.target_dir.is_empty() || !root.is_dir() { return Err(AppError::bad(format!("备份存储 {} 尚未挂载", storage.name))); }
    let source = backup_dir(&state.paths.db_backups_dir, &manifest.project_id, &manifest.id)?;
    let parent = root.join("cangling-update-db-backups").join(&manifest.project_id);
    fs::create_dir_all(&parent).map_err(|e| AppError::internal(e.to_string()))?;
    let target = parent.join(&manifest.id); let staging = parent.join(format!(".{}.part", manifest.id));
    fs::create_dir(&staging).map_err(|e| AppError::internal(e.to_string()))?;
    let result = (|| { for name in ["database.dump", "globals.sql", "manifest.json"] {
        fs::copy(source.join(name), staging.join(name)).map_err(|e| AppError::internal(e.to_string()))?;
    }
        if sha256_file(&staging.join("database.dump"))? != manifest.dump_sha256
            || sha256_file(&staging.join("globals.sql"))? != manifest.globals_sha256
        {
            return Err(AppError::bad("复制到备份存储后的 SHA-256 校验失败"));
        }
        fs::rename(&staging, &target).map_err(|e| AppError::internal(e.to_string()))?;
        Ok(target)
    })();
    if result.is_err() { let _ = fs::remove_dir_all(&staging); } result.map(Some)
}

fn apply_retention(root: &Path, project_id: &str, service: &str, database: &str, retain: u32) -> Result<(), AppError> {
    let scheduled: Vec<_> = list(root, project_id)?.into_iter()
        .filter(|x| x.kind == "scheduled" && x.service == service && x.database == database).collect();
    for old in scheduled.into_iter().skip(retain as usize) {
        fs::remove_dir_all(backup_dir(root, project_id, &old.id)?).map_err(|e| AppError::internal(e.to_string()))?;
    } Ok(())
}

pub async fn run_schedule(state: &AppState, project: &Project, schedule: &BackupSchedule) -> Result<BackupManifest, AppError> {
    let manifest = create(&state.docker, Path::new(&project.directory), &state.paths.db_backups_dir,
        &project.id, &schedule.service, &schedule.database, "scheduled").await?;
    let state_copy = state.clone(); let schedule_copy = schedule.clone(); let manifest_copy = manifest.clone();
    tokio::task::spawn_blocking(move || {
        let mirrored = copy_to_storage(&state_copy, &schedule_copy, &manifest_copy)?;
        apply_retention(&state_copy.paths.db_backups_dir, &manifest_copy.project_id, &manifest_copy.service,
            &manifest_copy.database, schedule_copy.retain_count)?;
        if let Some(target) = mirrored {
            if let Some(remote_root) = target.parent().and_then(Path::parent) {
                apply_retention(remote_root, &manifest_copy.project_id, &manifest_copy.service,
                    &manifest_copy.database, schedule_copy.retain_count)?;
            }
        }
        Ok::<(), AppError>(())
    }).await.map_err(|e| AppError::internal(e.to_string()))??;
    Ok(manifest)
}

pub async fn create_before_update(state: &AppState, project: &Project) -> Result<Option<BackupManifest>, AppError> {
    let schedule = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        get_schedule(&conn, &project.id)?
    };
    if !schedule.backup_before_update || schedule.service.is_empty() || schedule.database.is_empty() {
        return Ok(None);
    }
    let manifest = create(
        &state.docker, Path::new(&project.directory), &state.paths.db_backups_dir,
        &project.id, &schedule.service, &schedule.database, "pre-update",
    ).await?;
    if !schedule.storage_id.is_empty() {
        let state_copy = state.clone(); let schedule_copy = schedule; let manifest_copy = manifest.clone();
        tokio::task::spawn_blocking(move || copy_to_storage(&state_copy, &schedule_copy, &manifest_copy))
            .await.map_err(|e| AppError::internal(e.to_string()))??;
    }
    Ok(Some(manifest))
}

pub async fn scheduler(state: AppState) {
    if let Ok(conn) = state.db.lock() {
        let now = chrono::Utc::now().to_rfc3339();
        let _ = conn.execute(
            "UPDATE db_backup_schedules SET last_run_date='',last_finished_at=?1,last_status='failed',last_message='上一次自动备份因程序重启而中断' WHERE last_status='running'",
            [now],
        );
    }
    let mut timer = tokio::time::interval(Duration::from_secs(30));
    loop {
        timer.tick().await;
        let now = Local::now(); let today = now.format("%Y-%m-%d").to_string();
        let schedules = match state.db.lock() { Ok(conn) => enabled_schedules(&conn), Err(_) => Err(AppError::internal("db lock")) };
        let Ok(schedules) = schedules else { tracing::error!("读取数据库自动备份计划失败"); continue; };
        for schedule in schedules {
            let Ok((hour, minute)) = validate_time(&schedule.time_of_day) else { continue };
            if schedule.last_run_date == today || (now.hour(), now.minute()) < (hour, minute) { continue; }
            let project = match state.db.lock() { Ok(conn) => crate::db::get_project(&conn, &schedule.project_id).ok().flatten(), Err(_) => None };
            let Some(project) = project else { continue };
            let lock = state.lock_project(&project.id); let _guard = lock.lock().await;
            let current = match state.db.lock() { Ok(conn) => get_schedule(&conn, &project.id).ok(), Err(_) => None };
            let Some(current) = current else { continue };
            if !current.enabled || current.last_run_date == today { continue; }
            if let Ok(conn) = state.db.lock() { let _ = set_run_status(&conn, &project.id, Some(&today), "running", "自动备份正在执行", false); }
            let result = run_schedule(&state, &project, &current).await;
            if let Ok(conn) = state.db.lock() { match result {
                Ok(ref backup) => { let _ = set_run_status(&conn, &project.id, None, "success", &format!("备份完成：{}", backup.id), true); }
                Err(ref error) => { let _ = set_run_status(&conn, &project.id, None, "failed", &error.to_string(), true); }
            }}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trip_preserves_object_kinds() {
        let manifest = BackupManifest {
            id: "backup-1".into(),
            project_id: "project-1".into(),
            service: "postgis".into(),
            database: "cis".into(),
            kind: "manual".into(),
            engine: "postgres".into(),
            server_version: "14.2".into(),
            server_platform: "PostgreSQL x86_64".into(),
            postgis_version: "3.3".into(),
            created_at: "2026-09-14T00:00:00Z".into(),
            dump_bytes: 10,
            dump_sha256: "a".repeat(64),
            globals_bytes: 2,
            globals_sha256: "b".repeat(64),
            objects: vec![BackupObject {
                schema: "public".into(),
                name: "roads".into(),
                kind: "table".into(),
            }],
            valid: true,
        };
        let json = serde_json::to_vec(&manifest).unwrap();
        let decoded: BackupManifest = serde_json::from_slice(&json).unwrap();
        assert_eq!(decoded.objects, manifest.objects);
    }

    #[test]
    fn parses_postgresql_major_versions() {
        assert_eq!(postgres_major("14.11"), Some(14));
        assert_eq!(postgres_major("9.6.24"), Some(9));
        assert_eq!(postgres_major("unknown"), None);
    }

    #[test]
    fn validates_daily_backup_time() {
        assert_eq!(validate_time("03:00").unwrap(), (3, 0));
        assert!(validate_time("24:00").is_err());
        assert!(validate_time("3:00").is_err());
    }
}
