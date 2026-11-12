//! PostgreSQL administration through kubectl exec, sharing the project editor.
use crate::{error::AppError, state::AppState};
use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/k3s/db/{namespace}/{pod}/meta", get(db_meta))
        .route("/api/k3s/db/{namespace}/{pod}/databases", get(db_databases))
        .route("/api/k3s/db/{namespace}/{pod}/schemas", get(db_schemas))
        .route("/api/k3s/db/{namespace}/{pod}/objects", get(db_objects))
        .route("/api/k3s/db/{namespace}/{pod}/rows", get(db_rows))
        .route("/api/k3s/db/{namespace}/{pod}/query", post(db_query))
        .route(
            "/api/k3s/db/{namespace}/{pod}/row",
            post(db_update_row).delete(db_delete_row),
        )
}
#[derive(Deserialize)]
struct DbParams {
    service: String,
    engine: Option<String>,
    database: Option<String>,
    schema: Option<String>,
    name: Option<String>,
    offset: Option<u32>,
    limit: Option<u32>,
    filter_col: Option<String>,
    filter_op: Option<String>,
    filter_value: Option<String>,
}

async fn db_meta(
    State(state): State<AppState>,
    Path((namespace, pod)): Path<(String, String)>,
    Query(q): Query<DbParams>,
) -> Result<Json<crate::dbadmin::DbMeta>, AppError> {
    crate::dbadmin::require_engine(q.engine.as_deref())?;
    let dir = crate::dbadmin::k3s_directory(&namespace, &pod)?;
    Ok(Json(
        crate::dbadmin::meta(&state.docker, &dir, &q.service).await?,
    ))
}

async fn db_databases(
    State(state): State<AppState>,
    Path((namespace, pod)): Path<(String, String)>,
    Query(q): Query<DbParams>,
) -> Result<Json<crate::dbadmin::NameList>, AppError> {
    crate::dbadmin::require_engine(q.engine.as_deref())?;
    let dir = crate::dbadmin::k3s_directory(&namespace, &pod)?;
    Ok(Json(
        crate::dbadmin::databases(&state.docker, &dir, &q.service).await?,
    ))
}

async fn db_schemas(
    State(state): State<AppState>,
    Path((namespace, pod)): Path<(String, String)>,
    Query(q): Query<DbParams>,
) -> Result<Json<crate::dbadmin::NameList>, AppError> {
    crate::dbadmin::require_engine(q.engine.as_deref())?;
    let database = q
        .database
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::bad("请选择数据库"))?;
    let dir = crate::dbadmin::k3s_directory(&namespace, &pod)?;
    Ok(Json(
        crate::dbadmin::schemas(&state.docker, &dir, &q.service, database).await?,
    ))
}

async fn db_objects(
    State(state): State<AppState>,
    Path((namespace, pod)): Path<(String, String)>,
    Query(q): Query<DbParams>,
) -> Result<Json<crate::dbadmin::ObjectList>, AppError> {
    crate::dbadmin::require_engine(q.engine.as_deref())?;
    let database = q
        .database
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::bad("请选择数据库"))?;
    let schema = q
        .schema
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::bad("请选择 schema"))?;
    let dir = crate::dbadmin::k3s_directory(&namespace, &pod)?;
    Ok(Json(
        crate::dbadmin::objects(&state.docker, &dir, &q.service, database, schema).await?,
    ))
}

async fn db_rows(
    State(state): State<AppState>,
    Path((namespace, pod)): Path<(String, String)>,
    Query(q): Query<DbParams>,
) -> Result<Json<crate::dbadmin::RowPage>, AppError> {
    crate::dbadmin::require_engine(q.engine.as_deref())?;
    let database = q
        .database
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::bad("请选择数据库"))?;
    let schema = q
        .schema
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::bad("请选择 schema"))?;
    let name = q
        .name
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::bad("请选择表或视图"))?;
    let dir = crate::dbadmin::k3s_directory(&namespace, &pod)?;
    let filter = match (
        q.filter_col.as_deref().filter(|s| !s.is_empty()),
        q.filter_value.as_deref().filter(|s| !s.trim().is_empty()),
    ) {
        (Some(col), Some(val)) => Some(crate::dbadmin::RowFilter {
            column: col.to_string(),
            op: q.filter_op.as_deref().unwrap_or("contains").to_string(),
            value: val.to_string(),
        }),
        _ => None,
    };
    Ok(Json(
        crate::dbadmin::rows(
            &state.docker,
            &dir,
            &q.service,
            database,
            schema,
            name,
            q.offset.unwrap_or(0),
            crate::dbadmin::clamp_limit(q.limit),
            filter,
        )
        .await?,
    ))
}

async fn db_update_row(
    State(state): State<AppState>,
    Path((namespace, pod)): Path<(String, String)>,
    Json(body): Json<crate::dbadmin::UpdateRowBody>,
) -> Result<Json<crate::dbadmin::UpdateRowResult>, AppError> {
    crate::dbadmin::require_engine(body.engine.as_deref())?;
    let dir = crate::dbadmin::k3s_directory(&namespace, &pod)?;
    Ok(Json(
        crate::dbadmin::update_row(&state.docker, &dir, &body).await?,
    ))
}

async fn db_delete_row(
    State(state): State<AppState>,
    Path((namespace, pod)): Path<(String, String)>,
    Json(body): Json<crate::dbadmin::DeleteRowBody>,
) -> Result<Json<crate::dbadmin::DeleteRowResult>, AppError> {
    crate::dbadmin::require_engine(body.engine.as_deref())?;
    let dir = crate::dbadmin::k3s_directory(&namespace, &pod)?;
    Ok(Json(
        crate::dbadmin::delete_row(&state.docker, &dir, &body).await?,
    ))
}

async fn db_query(
    State(state): State<AppState>,
    Path((namespace, pod)): Path<(String, String)>,
    Json(body): Json<crate::dbadmin::QueryBody>,
) -> Result<Json<crate::dbadmin::QueryResult>, AppError> {
    crate::dbadmin::require_engine(body.engine.as_deref())?;
    let dir = crate::dbadmin::k3s_directory(&namespace, &pod)?;
    Ok(Json(
        crate::dbadmin::query(
            &state.docker,
            &dir,
            &body.service,
            &body.database,
            &body.sql,
        )
        .await?,
    ))
}
