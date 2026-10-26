use chrono::{DateTime, NaiveDate, Utc};
use debarred_domain::{load_semester, Engine, Plan};
use serde::Serialize;
use sqlx::{postgres::PgRow, PgPool, Row};
use uuid::Uuid;

use crate::error::{AppError, Result};

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
}

#[derive(Debug, Serialize)]
pub struct StoredPlan {
    pub plan: Plan,
    pub revision: i64,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Copy, Clone)]
pub enum QuotaKind {
    Helper,
    Image,
}

impl QuotaKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Helper => "helper",
            Self::Image => "image",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Quota {
    pub limit: i32,
    pub used: i32,
    pub remaining: i32,
}

impl Store {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn load_plan(&self, user_id: Uuid, semester_id: &str) -> Result<Option<StoredPlan>> {
        let row = sqlx::query(
            "SELECT plan, revision, updated_at FROM app_private.student_plans
             WHERE user_id = $1 AND semester_id = $2",
        )
        .bind(user_id)
        .bind(semester_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| stored_plan(row, semester_id)).transpose()
    }

    pub async fn save_plan(
        &self,
        user_id: Uuid,
        expected_revision: i64,
        plan: &Plan,
    ) -> Result<StoredPlan> {
        if !(0..i64::MAX).contains(&expected_revision) {
            return Err(AppError::BadRequest("invalid expected revision".into()));
        }
        validate_plan(plan)?;
        let json = serde_json::to_value(plan)
            .map_err(|_| AppError::BadRequest("plan cannot be serialized".into()))?;
        let mut tx = self.pool.begin().await?;
        lock_account_write(&mut tx, user_id).await?;
        let row = if expected_revision == 0 {
            sqlx::query(
                "INSERT INTO app_private.student_plans (user_id, semester_id, plan, revision)
                 VALUES ($1, $2, $3, 1)
                 ON CONFLICT (user_id, semester_id) DO NOTHING
                 RETURNING plan, revision, updated_at",
            )
            .bind(user_id)
            .bind(&plan.semester_id)
            .bind(json)
            .fetch_optional(&mut *tx)
            .await?
        } else {
            sqlx::query(
                "UPDATE app_private.student_plans
                 SET plan = $3, revision = revision + 1, updated_at = clock_timestamp()
                 WHERE user_id = $1 AND semester_id = $2 AND revision = $4
                 RETURNING plan, revision, updated_at",
            )
            .bind(user_id)
            .bind(&plan.semester_id)
            .bind(json)
            .bind(expected_revision)
            .fetch_optional(&mut *tx)
            .await?
        };
        let stored = stored_plan(row.ok_or(AppError::Conflict)?, &plan.semester_id)?;
        tx.commit().await?;
        Ok(stored)
    }

    pub async fn consume_quota(
        &self,
        user_id: Uuid,
        kind: QuotaKind,
        day: NaiveDate,
        limit: i32,
    ) -> Result<Quota> {
        validate_limit(limit)?;
        let mut tx = self.pool.begin().await?;
        lock_account_write(&mut tx, user_id).await?;
        let used: Option<i32> = sqlx::query_scalar(
            "INSERT INTO app_private.ai_usage (user_id, usage_day, kind, used)
             VALUES ($1, $2, $3, 1)
             ON CONFLICT (user_id, usage_day, kind) DO UPDATE
             SET used = app_private.ai_usage.used + 1, updated_at = clock_timestamp()
             WHERE app_private.ai_usage.user_id = $1
               AND app_private.ai_usage.usage_day = $2
               AND app_private.ai_usage.kind = $3
               AND app_private.ai_usage.used < $4
             RETURNING used",
        )
        .bind(user_id)
        .bind(day)
        .bind(kind.as_str())
        .bind(limit)
        .fetch_optional(&mut *tx)
        .await?;
        let quota = quota(limit, used.ok_or(AppError::RateLimited)?)?;
        tx.commit().await?;
        Ok(quota)
    }

    pub async fn read_quota(
        &self,
        user_id: Uuid,
        kind: QuotaKind,
        day: NaiveDate,
        limit: i32,
    ) -> Result<Quota> {
        validate_limit(limit)?;
        let used: Option<i32> = sqlx::query_scalar(
            "SELECT used FROM app_private.ai_usage
             WHERE user_id = $1 AND usage_day = $2 AND kind = $3",
        )
        .bind(user_id)
        .bind(day)
        .bind(kind.as_str())
        .fetch_optional(&self.pool)
        .await?;
        quota(limit, used.unwrap_or(0))
    }
}

/// Always acquire this before row locks in server plan/quota/session-insert paths.
/// Trigger checks remain defense in depth, not a substitute for this lock order.
pub(crate) async fn lock_account_write(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: Uuid,
) -> Result<()> {
    // Do not inherit a database/role default of REPEATABLE READ: after waiting
    // for deletion, the pending check must observe its newly committed marker.
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut **tx)
        .await?;
    sqlx::query("SET LOCAL lock_timeout = '22s'")
        .execute(&mut **tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout = '25s'")
        .execute(&mut **tx)
        .await?;
    sqlx::query("SELECT app_private.lock_account_write($1)")
        .bind(user_id)
        .execute(&mut **tx)
        .await
        .map_err(|error| {
            if error.as_database_error().and_then(|e| e.code()).as_deref() == Some("42501") {
                AppError::Forbidden
            } else {
                error.into()
            }
        })?;
    Ok(())
}

fn validate_plan(plan: &Plan) -> debarred_domain::Result<()> {
    let calendar = load_semester(&plan.semester_id)?;
    Engine::new(&calendar, plan)?;
    Ok(())
}

fn stored_plan(row: PgRow, semester_id: &str) -> Result<StoredPlan> {
    let json: serde_json::Value = row.try_get("plan")?;
    let plan: Plan = serde_json::from_value(json)
        .map_err(|_| AppError::Unavailable("stored plan is invalid"))?;
    if plan.semester_id != semester_id || validate_plan(&plan).is_err() {
        return Err(AppError::Unavailable("stored plan is invalid"));
    }
    let revision = row.try_get("revision")?;
    if revision <= 0 {
        return Err(AppError::Unavailable("stored plan revision is invalid"));
    }
    Ok(StoredPlan {
        plan,
        revision,
        updated_at: row.try_get("updated_at")?,
    })
}

fn validate_limit(limit: i32) -> Result<()> {
    if !(1..=1000).contains(&limit) {
        return Err(AppError::BadRequest(
            "quota limit must be between 1 and 1000".into(),
        ));
    }
    Ok(())
}

fn quota(limit: i32, used: i32) -> Result<Quota> {
    if used < 0 {
        return Err(AppError::Unavailable("stored quota is invalid"));
    }
    Ok(Quota {
        limit,
        used,
        remaining: (limit - used).max(0),
    })
}
