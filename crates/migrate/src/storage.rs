//! Privileged, insert-only migration transactions. Never runs schema migrations.
use crate::{
    canonical_plan_sha256, plan_storage_sha256, Error, PreparedBatch, Result, REPORT_VERSION,
};
use debarred_domain::Plan;
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions, PgSslMode},
    PgPool, Postgres, Row, Transaction,
};
use std::{collections::BTreeSet, str::FromStr, time::Duration};
use uuid::Uuid;

fn db(_: sqlx::Error) -> Error {
    Error("database_operation_failed_transaction_aborted")
}

/// Check the parsed/effective host, not the URL's spelling. Remote connections
/// always override any URL sslmode to VerifyFull. No DATABASE_URL fallback.
pub fn connection_options(url: &str, allow_remote: bool) -> Result<PgConnectOptions> {
    let options =
        PgConnectOptions::from_str(url).map_err(|_| Error("invalid_migration_database_url"))?;
    let host = options.get_host();
    let local = matches!(host, "localhost" | "127.0.0.1" | "::1") || host.starts_with('/');
    if !local && !allow_remote {
        return Err(Error("remote_database_requires_allow_remote"));
    }
    Ok(if local {
        options
    } else {
        options.ssl_mode(PgSslMode::VerifyFull)
    })
}

pub async fn connect(url: &str, allow_remote: bool) -> Result<PgPool> {
    let options = connection_options(url, allow_remote)?;
    PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(15))
        .connect_with(options)
        .await
        .map_err(db)
}

async fn begin(pool: &PgPool) -> Result<Transaction<'_, Postgres>> {
    let mut tx = pool.begin().await.map_err(db)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("SET LOCAL lock_timeout = '15s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("SET LOCAL statement_timeout = '60s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    Ok(tx)
}

async fn lock_users(tx: &mut Transaction<'_, Postgres>, users: BTreeSet<Uuid>) -> Result<()> {
    for user in &users {
        // Same key and ordering as 0002. The pending check is a separate SQL
        // statement so READ COMMITTED refreshes its snapshot after a wait.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text, 0))")
            .bind(user.to_string())
            .execute(&mut **tx)
            .await
            .map_err(db)?;
    }
    for user in users {
        let pending: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM app_private.account_deletion_requests WHERE user_id = $1)")
            .bind(user).fetch_one(&mut **tx).await.map_err(db)?;
        if pending {
            return Err(Error("account_deletion_pending"));
        }
        // Prevent auth deletion (and its cascades) through commit, even if an
        // external auth administrator doesn't participate in advisory locking.
        let exists = sqlx::query("SELECT id FROM auth.users WHERE id = $1 FOR KEY SHARE")
            .bind(user)
            .fetch_optional(&mut **tx)
            .await
            .map_err(db)?;
        if exists.is_none() {
            return Err(Error("target_account_missing"));
        }
    }
    Ok(())
}

pub async fn apply(pool: &PgPool, batch: &PreparedBatch, confirm_digest: &str) -> Result<Uuid> {
    if confirm_digest != batch.digest() {
        return Err(Error("confirmation_digest_mismatch"));
    }
    if batch.is_empty() {
        return Err(Error("empty_batch"));
    }
    let mut tx = begin(pool).await?;
    lock_users(
        &mut tx,
        batch.rows.iter().map(|r| r.target_user_id).collect(),
    )
    .await?;
    for row in &batch.rows {
        // Conservative reconciliation: a target with any existing semester plan
        // requires human review, not automated merging into another semester.
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM app_private.student_plans WHERE user_id = $1)",
        )
        .bind(row.target_user_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
        if exists {
            return Err(Error("target_already_has_plan"));
        }
    }
    let batch_id = Uuid::new_v4();
    sqlx::query("INSERT INTO app_private.migration_batches (batch_id, report_version, fingerprint, source_sha256, mapping_sha256, row_count, status) VALUES ($1,$2,$3,$4,$5,$6,'applied')")
        .bind(batch_id).bind(REPORT_VERSION as i32).bind(batch.digest()).bind(&batch.source_sha256)
        .bind(&batch.mapping_sha256).bind(batch.len() as i64).execute(&mut *tx).await.map_err(db)?;
    for row in &batch.rows {
        sqlx::query("INSERT INTO app_private.student_plans (user_id, semester_id, plan, revision) VALUES ($1,$2,$3,1)")
            .bind(row.target_user_id).bind(&row.semester_id).bind(sqlx::types::Json(&row.plan))
            .execute(&mut *tx).await.map_err(db)?;
        sqlx::query("INSERT INTO app_private.migration_rows (batch_id,user_id,semester_id,inserted_revision,plan_sha256,plan_storage_sha256) VALUES ($1,$2,$3,1,$4,$5)")
            .bind(batch_id).bind(row.target_user_id).bind(&row.semester_id).bind(&row.plan_sha256).bind(&row.plan_storage_sha256)
            .execute(&mut *tx).await.map_err(db)?;
    }
    tx.commit().await.map_err(db)?;
    Ok(batch_id)
}

pub async fn rollback(pool: &PgPool, batch_id: Uuid, confirm_digest: &str) -> Result<()> {
    let mut tx = begin(pool).await?;
    let batch = sqlx::query("SELECT fingerprint, status, row_count, report_version FROM app_private.migration_batches WHERE batch_id = $1 FOR UPDATE")
        .bind(batch_id).fetch_optional(&mut *tx).await.map_err(db)?.ok_or(Error("batch_missing"))?;
    if batch.get::<String, _>("fingerprint") != confirm_digest {
        return Err(Error("confirmation_digest_mismatch"));
    }
    if batch.get::<String, _>("status") != "applied" {
        return Err(Error("batch_not_applied"));
    }
    if batch.get::<i32, _>("report_version") != REPORT_VERSION as i32 {
        return Err(Error("unsupported_report_version"));
    }
    let rows = sqlx::query("SELECT user_id, semester_id, plan_sha256, plan_storage_sha256, inserted_revision FROM app_private.migration_rows WHERE batch_id = $1 ORDER BY user_id, semester_id")
        .bind(batch_id).fetch_all(&mut *tx).await.map_err(db)?;
    // Cascaded audit rows indicate missing/deleted accounts: never treat the
    // surviving subset as the entire batch.
    if rows.len() as i64 != batch.get::<i64, _>("row_count") {
        return Err(Error("batch_audit_incomplete"));
    }
    lock_users(
        &mut tx,
        rows.iter().map(|r| r.get::<Uuid, _>("user_id")).collect(),
    )
    .await?;
    for row in &rows {
        let current = sqlx::query("SELECT revision, plan FROM app_private.student_plans WHERE user_id = $1 AND semester_id = $2 FOR UPDATE")
            .bind(row.get::<Uuid,_>("user_id")).bind(row.get::<String,_>("semester_id"))
            .fetch_optional(&mut *tx).await.map_err(db)?.ok_or(Error("imported_plan_missing"))?;
        if current.get::<i64, _>("revision") != 1 || row.get::<i64, _>("inserted_revision") != 1 {
            return Err(Error("imported_plan_modified"));
        }
        let value: sqlx::types::Json<serde_json::Value> = current.try_get("plan").map_err(db)?;
        // Semantic equivalence is insufficient: never discard representation-only
        // edits made without a revision bump. Check the raw stored JSON first.
        if plan_storage_sha256(&value.0)? != row.get::<String, _>("plan_storage_sha256") {
            return Err(Error("imported_plan_modified"));
        }
        let plan: Plan =
            serde_json::from_value(value.0).map_err(|_| Error("imported_plan_modified"))?;
        if canonical_plan_sha256(&plan).map_err(|_| Error("imported_plan_modified"))?
            != row.get::<String, _>("plan_sha256")
        {
            return Err(Error("imported_plan_modified"));
        }
    }
    for row in &rows {
        let result = sqlx::query("DELETE FROM app_private.student_plans WHERE user_id = $1 AND semester_id = $2 AND revision = 1")
            .bind(row.get::<Uuid,_>("user_id")).bind(row.get::<String,_>("semester_id"))
            .execute(&mut *tx).await.map_err(db)?;
        if result.rows_affected() != 1 {
            return Err(Error("imported_plan_changed"));
        }
    }
    sqlx::query("UPDATE app_private.migration_batches SET status = 'rolled_back', rolled_back_at = clock_timestamp() WHERE batch_id = $1")
        .bind(batch_id).execute(&mut *tx).await.map_err(db)?;
    tx.commit().await.map_err(db)
}
