use std::time::Duration;

use axum::{
    extract::{DefaultBodyLimit, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use tower_http::timeout::TimeoutLayer;

use crate::{
    auth::Authenticated,
    error::{AppError, Result},
    AppState,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/account/export", get(export))
        .route("/api/account/deletion", post(deletion))
        .layer(DefaultBodyLimit::max(1024))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(30),
        ))
        .layer(middleware::map_response(
            |mut response: Response| async move {
                response
                    .headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                response
                    .headers_mut()
                    .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
                response.headers_mut().insert(
                    header::REFERRER_POLICY,
                    HeaderValue::from_static("no-referrer"),
                );
                response
            },
        ))
}

#[derive(Serialize)]
struct ExportPlan {
    plan: Value,
    revision: i64,
    updated_at: DateTime<Utc>,
}

#[derive(Serialize)]
struct ExportUsage {
    day: NaiveDate,
    kind: String,
    used: i32,
}

#[derive(Serialize)]
struct ExportMigration {
    semester_id: String,
    inserted_revision: i64,
    plan_storage_sha256: String,
    applied_at: DateTime<Utc>,
    status: String,
}

async fn export(
    State(state): State<AppState>,
    Authenticated(user): Authenticated,
) -> Result<Json<Value>> {
    let mut tx = state.store.pool().begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout = '25s'")
        .execute(&mut *tx)
        .await?;
    let generated_at: DateTime<Utc> = sqlx::query_scalar("SELECT transaction_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    // Explicit allowlists: never serialize AuthUser or any session row/blob.
    let plans = sqlx::query(
        "SELECT plan, revision, updated_at FROM app_private.student_plans
         WHERE user_id = $1 ORDER BY semester_id",
    )
    .bind(user.user_id)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|row| {
        Ok(ExportPlan {
            plan: row.try_get("plan")?,
            revision: row.try_get("revision")?,
            updated_at: row.try_get("updated_at")?,
        })
    })
    .collect::<std::result::Result<Vec<_>, sqlx::Error>>()?;
    let usage = sqlx::query(
        "SELECT usage_day, kind, used FROM app_private.ai_usage
         WHERE user_id = $1 ORDER BY usage_day, kind",
    )
    .bind(user.user_id)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|row| {
        Ok(ExportUsage {
            day: row.try_get("usage_day")?,
            kind: row.try_get("kind")?,
            used: row.try_get("used")?,
        })
    })
    .collect::<std::result::Result<Vec<_>, sqlx::Error>>()?;
    let migration_history = sqlx::query(
        "SELECT r.semester_id, r.inserted_revision, r.plan_storage_sha256, b.applied_at, b.status
         FROM app_private.migration_rows r
         JOIN app_private.migration_batches b ON b.batch_id = r.batch_id
         WHERE r.user_id = $1 ORDER BY b.applied_at, b.batch_id, r.semester_id",
    )
    .bind(user.user_id)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|row| {
        Ok(ExportMigration {
            semester_id: row.try_get("semester_id")?,
            inserted_revision: row.try_get("inserted_revision")?,
            plan_storage_sha256: row.try_get("plan_storage_sha256")?,
            applied_at: row.try_get("applied_at")?,
            status: row.try_get("status")?,
        })
    })
    .collect::<std::result::Result<Vec<_>, sqlx::Error>>()?;
    tx.commit().await?;
    Ok(Json(json!({
        "version": 1, "generated_at": generated_at,
        "profile": {"user_id": user.user_id, "email": user.email, "display_name": user.display_name},
        "plans": plans, "ai_usage": usage, "migration_history": migration_history,
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeletionRequest {
    confirmation: String,
}

async fn deletion(
    State(state): State<AppState>,
    Authenticated(user): Authenticated,
    headers: HeaderMap,
    Json(request): Json<DeletionRequest>,
) -> Result<Response> {
    if request.confirmation != "DELETE MY ACCOUNT" {
        return Err(AppError::BadRequest(
            "confirmation must be DELETE MY ACCOUNT".into(),
        ));
    }
    state.auth.require_recent(&headers).await?;
    // Bound the entire transaction, not just individual statements. Dropping an
    // unfinished SQLx transaction rolls it back; never acknowledge partial erasure.
    tokio::time::timeout(Duration::from_secs(25), async {
        let mut tx = state.store.pool().begin().await?;
        // A lock wait must not pin a REPEATABLE READ snapshot that predates
        // writes committed by the account writer we are waiting for.
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SET LOCAL lock_timeout = '22s'")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SET LOCAL statement_timeout = '24s'")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text, 0))")
            .bind(user.user_id.to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO app_private.account_deletion_requests (user_id) VALUES ($1)
                     ON CONFLICT (user_id) DO NOTHING",
        )
        .bind(user.user_id)
        .execute(&mut *tx)
        .await?;
        for statement in [
            "DELETE FROM app_private.student_plans WHERE user_id = $1",
            "DELETE FROM app_private.ai_usage WHERE user_id = $1",
            "DELETE FROM app_private.migration_rows WHERE user_id = $1",
            "DELETE FROM app_private.auth_sessions WHERE user_id = $1",
        ] {
            sqlx::query(statement)
                .bind(user.user_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok::<_, AppError>(())
    })
    .await
    .map_err(|_| AppError::Unavailable("account deletion timed out; retry the request"))??;
    // Do not expire the shared browser cookie: a delayed response for this
    // account could otherwise clear a different account's newer login cookie.
    // Deleting every owned session above is the authoritative revocation.
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "pending_identity_deletion", "app_data_deleted": true,
        })),
    )
        .into_response())
}
