use crate::models::{DeployedJar, LoadedImage, Project, Version};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS projects (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL DEFAULT '',
    directory TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS versions (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    version_no INTEGER NOT NULL,
    label TEXT NOT NULL,
    note TEXT NOT NULL DEFAULT '',
    backup_path TEXT NOT NULL,
    images_json TEXT NOT NULL DEFAULT '[]',
    is_current INTEGER NOT NULL DEFAULT 0,
    kind TEXT NOT NULL DEFAULT 'update',
    created_at TEXT NOT NULL,
    FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE CASCADE,
    UNIQUE(project_id, version_no)
);

CREATE INDEX IF NOT EXISTS idx_versions_project ON versions(project_id, version_no);

CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
    token TEXT PRIMARY KEY,
    user_id TEXT NOT NULL,
    last_seen TEXT NOT NULL,
    created_at TEXT NOT NULL,
    FOREIGN KEY(user_id) REFERENCES users(id) ON DELETE CASCADE
);
"#;

pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)
        .with_context(|| format!("open sqlite {}", path.display()))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.execute_batch(SCHEMA)?;
    let _ = conn.execute(
        "ALTER TABLE versions ADD COLUMN jars_json TEXT NOT NULL DEFAULT '[]'",
        [],
    );
    Ok(conn)
}

fn map_project(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        directory: row.get(3)?,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
        current_version_no: row.get(6)?,
        current_version_id: row.get(7)?,
        version_count: row.get::<_, i64>(8)?,
    })
}

const PROJECT_SELECT: &str = r#"
SELECT
    p.id, p.name, p.description, p.directory, p.created_at, p.updated_at,
    (SELECT v.version_no FROM versions v WHERE v.project_id = p.id AND v.is_current = 1),
    (SELECT v.id FROM versions v WHERE v.project_id = p.id AND v.is_current = 1),
    (SELECT COUNT(*) FROM versions v WHERE v.project_id = p.id)
FROM projects p
"#;

pub fn list_projects(conn: &Connection) -> Result<Vec<Project>> {
    let mut stmt = conn.prepare(&format!("{PROJECT_SELECT} ORDER BY p.updated_at DESC"))?;
    let rows = stmt.query_map([], map_project)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn get_project(conn: &Connection, id: &str) -> Result<Option<Project>> {
    let mut stmt = conn.prepare(&format!("{PROJECT_SELECT} WHERE p.id = ?1"))?;
    stmt.query_row(params![id], map_project)
        .optional()
        .map_err(Into::into)
}

pub fn insert_project(conn: &Connection, p: &Project) -> Result<()> {
    conn.execute(
        "INSERT INTO projects (id, name, description, directory, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![p.id, p.name, p.description, p.directory, p.created_at, p.updated_at],
    )?;
    Ok(())
}

pub fn update_project(
    conn: &Connection,
    id: &str,
    name: &str,
    description: &str,
    directory: &str,
    updated_at: &str,
) -> Result<bool> {
    let n = conn.execute(
        "UPDATE projects SET name = ?1, description = ?2, directory = ?3, updated_at = ?4 WHERE id = ?5",
        params![name, description, directory, updated_at, id],
    )?;
    Ok(n > 0)
}

pub fn delete_project(conn: &Connection, id: &str) -> Result<bool> {
    let n = conn.execute("DELETE FROM projects WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

pub fn next_version_no(conn: &Connection, project_id: &str) -> Result<i64> {
    let n: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version_no), 0) FROM versions WHERE project_id = ?1",
        params![project_id],
        |r| r.get(0),
    )?;
    Ok(n + 1)
}

pub fn insert_version(conn: &Connection, v: &Version) -> Result<()> {
    conn.execute(
        "INSERT INTO versions
            (id, project_id, version_no, label, note, backup_path, images_json, is_current, kind, created_at, jars_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            v.id,
            v.project_id,
            v.version_no,
            v.label,
            v.note,
            v.backup_path,
            serde_json::to_string(&v.images).unwrap_or_else(|_| "[]".into()),
            if v.is_current { 1 } else { 0 },
            v.kind,
            v.created_at,
            serde_json::to_string(&v.jars).unwrap_or_else(|_| "[]".into()),
        ],
    )?;
    Ok(())
}

pub fn mark_current(conn: &Connection, project_id: &str, version_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE versions SET is_current = 0 WHERE project_id = ?1",
        params![project_id],
    )?;
    conn.execute(
        "UPDATE versions SET is_current = 1 WHERE id = ?1 AND project_id = ?2",
        params![version_id, project_id],
    )?;
    conn.execute(
        "UPDATE projects SET updated_at = ?1 WHERE id = ?2",
        params![now_rfc3339(), project_id],
    )?;
    Ok(())
}

pub fn list_versions(conn: &Connection, project_id: &str) -> Result<Vec<Version>> {
    let mut stmt = conn.prepare(
        "SELECT id, project_id, version_no, label, note, backup_path, images_json, is_current, kind, created_at, jars_json
         FROM versions WHERE project_id = ?1 ORDER BY version_no DESC",
    )?;
    let rows = stmt.query_map(params![project_id], map_version)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn get_version(conn: &Connection, project_id: &str, version_id: &str) -> Result<Option<Version>> {
    let mut stmt = conn.prepare(
        "SELECT id, project_id, version_no, label, note, backup_path, images_json, is_current, kind, created_at, jars_json
         FROM versions WHERE project_id = ?1 AND id = ?2",
    )?;
    stmt.query_row(params![project_id, version_id], map_version)
        .optional()
        .map_err(Into::into)
}

fn map_version(row: &rusqlite::Row<'_>) -> rusqlite::Result<Version> {
    let images_json: String = row.get(6)?;
    let images: Vec<LoadedImage> = serde_json::from_str(&images_json).unwrap_or_default();
    let is_current: i64 = row.get(7)?;
    let jars_json: String = row.get(10).unwrap_or_else(|_| "[]".to_string());
    let jars: Vec<DeployedJar> = serde_json::from_str(&jars_json).unwrap_or_default();
    Ok(Version {
        id: row.get(0)?,
        project_id: row.get(1)?,
        version_no: row.get(2)?,
        label: row.get(3)?,
        note: row.get(4)?,
        backup_path: row.get(5)?,
        images,
        jars,
        is_current: is_current != 0,
        kind: row.get(8)?,
        created_at: row.get(9)?,
        app_bytes: 0,
        backup_bytes: 0,
        repo_bytes: 0,
    })
}

pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[derive(Debug, Clone)]
pub struct UserRow {
    pub id: String,
    pub username: String,
    pub password_hash: String,
}

pub fn user_count(conn: &Connection) -> Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
        .map_err(Into::into)
}

pub fn insert_user(conn: &Connection, id: &str, username: &str, password_hash: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO users (id, username, password_hash, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![id, username, password_hash, now_rfc3339()],
    )?;
    Ok(())
}

pub fn get_user_by_name(conn: &Connection, username: &str) -> Result<Option<UserRow>> {
    let mut stmt =
        conn.prepare("SELECT id, username, password_hash FROM users WHERE username = ?1")?;
    stmt.query_row(params![username], |r| {
        Ok(UserRow {
            id: r.get(0)?,
            username: r.get(1)?,
            password_hash: r.get(2)?,
        })
    })
    .optional()
    .map_err(Into::into)
}

pub fn get_user_by_id(conn: &Connection, id: &str) -> Result<Option<UserRow>> {
    let mut stmt = conn.prepare("SELECT id, username, password_hash FROM users WHERE id = ?1")?;
    stmt.query_row(params![id], |r| {
        Ok(UserRow {
            id: r.get(0)?,
            username: r.get(1)?,
            password_hash: r.get(2)?,
        })
    })
    .optional()
    .map_err(Into::into)
}

pub fn create_session(conn: &Connection, token: &str, user_id: &str) -> Result<()> {
    let now = now_rfc3339();
    conn.execute(
        "INSERT INTO sessions (token, user_id, last_seen, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![token, user_id, now, now],
    )?;
    Ok(())
}

pub fn session_user_id(conn: &Connection, token: &str) -> Result<Option<(String, String)>> {
    let mut stmt = conn.prepare("SELECT user_id, last_seen FROM sessions WHERE token = ?1")?;
    stmt.query_row(params![token], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()
        .map_err(Into::into)
}

pub fn touch_session(conn: &Connection, token: &str) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET last_seen = ?1 WHERE token = ?2",
        params![now_rfc3339(), token],
    )?;
    Ok(())
}

pub fn delete_session(conn: &Connection, token: &str) -> Result<()> {
    conn.execute("DELETE FROM sessions WHERE token = ?1", params![token])?;
    Ok(())
}

pub fn list_usernames(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT username FROM users ORDER BY username")?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn update_password_hash(conn: &Connection, user_id: &str, password_hash: &str) -> Result<bool> {
    let n = conn.execute(
        "UPDATE users SET password_hash = ?1 WHERE id = ?2",
        params![password_hash, user_id],
    )?;
    Ok(n > 0)
}

pub fn delete_sessions_for_user(conn: &Connection, user_id: &str) -> Result<()> {
    conn.execute("DELETE FROM sessions WHERE user_id = ?1", params![user_id])?;
    Ok(())
}


