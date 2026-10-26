use std::sync::Arc;

use chrono::{Duration, NaiveDate, Utc};
use debarred_domain::{load_semester, AttendanceSnapshot, Plan, Timetable};
use debarred_server::{
    db::{Quota, QuotaKind, Store, MIGRATOR},
    error::AppError,
};
use sqlx::{postgres::PgPoolOptions, PgPool};
use tokio::{
    sync::{Barrier, OnceCell},
    task::JoinSet,
};
use uuid::Uuid;

static INITIALIZED: OnceCell<()> = OnceCell::const_new();

async fn database() -> Store {
    let url = std::env::var("TEST_DATABASE_URL")
        .expect("TEST_DATABASE_URL must point to a disposable PostgreSQL database");
    let pool = PgPoolOptions::new()
        .max_connections(12)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(&url)
        .await
        .expect("connect to TEST_DATABASE_URL");
    INITIALIZED
        .get_or_init(|| async {
            // Supabase owns auth.users in production; only disposable tests bootstrap it.
            sqlx::raw_sql(
                "CREATE SCHEMA IF NOT EXISTS auth;
             CREATE TABLE IF NOT EXISTS auth.users (id uuid PRIMARY KEY);",
            )
            .execute(&pool)
            .await
            .expect("bootstrap test auth.users");
            MIGRATOR.run(&pool).await.expect("run database migrations");
        })
        .await;
    Store::new(pool)
}

async fn user(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO auth.users (id) VALUES ($1)")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn delete_user(pool: &PgPool, id: Uuid) {
    sqlx::query("DELETE FROM auth.users WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

fn plan() -> Plan {
    let calendar = load_semester("winter-2026").unwrap();
    Plan {
        schema_version: debarred_domain::model::PLAN_SCHEMA_VERSION,
        semester_id: calendar.config().id.clone(),
        calendar_revision: calendar.config().revision,
        timetable: Timetable::default(),
        attendance: AttendanceSnapshot {
            as_of: calendar.config().start_date - Duration::days(1),
            attended: Default::default(),
        },
        leave: Vec::new(),
    }
}

fn day() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 1, 10).unwrap()
}

fn assert_quota(quota: Quota, limit: i32, used: i32) {
    assert_eq!(quota.limit, limit);
    assert_eq!(quota.used, used);
    assert_eq!(quota.remaining, (limit - used).max(0));
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn plan_revisions_are_conditional_and_retries_do_not_write() {
    let store = database().await;
    let id = user(store.pool()).await;
    let plan = plan();
    assert!(store
        .load_plan(id, &plan.semester_id)
        .await
        .unwrap()
        .is_none());
    assert!(matches!(
        store.save_plan(id, 1, &plan).await,
        Err(AppError::Conflict)
    ));
    let first = store.save_plan(id, 0, &plan).await.unwrap();
    assert_eq!(first.revision, 1);
    assert_eq!(first.plan, plan);
    assert!(first.updated_at <= Utc::now());
    assert!(matches!(
        store.save_plan(id, 0, &plan).await,
        Err(AppError::Conflict)
    ));
    let unchanged = store
        .load_plan(id, &plan.semester_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.revision, first.revision);
    assert_eq!(unchanged.updated_at, first.updated_at);
    let second = store.save_plan(id, 1, &plan).await.unwrap();
    assert_eq!(second.revision, 2);
    assert!(second.updated_at >= first.updated_at);
    assert!(matches!(
        store.save_plan(id, 1, &plan).await,
        Err(AppError::Conflict)
    ));
    let unchanged = store
        .load_plan(id, &plan.semester_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.revision, second.revision);
    assert_eq!(unchanged.updated_at, second.updated_at);
    assert_eq!(unchanged.plan, second.plan);
    for revision in [-1, i64::MIN, i64::MAX] {
        assert!(matches!(
            store.save_plan(id, revision, &plan).await,
            Err(AppError::BadRequest(_))
        ));
    }
    sqlx::query("UPDATE app_private.student_plans SET revision = $3 WHERE user_id = $1 AND semester_id = $2")
        .bind(id).bind(&plan.semester_id).bind(i64::MAX - 1).execute(store.pool()).await.unwrap();
    let last = store.save_plan(id, i64::MAX - 1, &plan).await.unwrap();
    assert_eq!(last.revision, i64::MAX);
    assert!(matches!(
        store.save_plan(id, i64::MAX, &plan).await,
        Err(AppError::BadRequest(_))
    ));
    delete_user(store.pool(), id).await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn concurrent_creates_and_updates_have_exactly_one_winner() {
    let store = database().await;
    let id = user(store.pool()).await;
    for expected in [0, 1] {
        let barrier = Arc::new(Barrier::new(24));
        let mut tasks = JoinSet::new();
        for _ in 0..24 {
            let store = store.clone();
            let barrier = barrier.clone();
            tasks.spawn(async move {
                barrier.wait().await;
                store.save_plan(id, expected, &plan()).await
            });
        }
        let mut winners = 0;
        while let Some(result) = tasks.join_next().await {
            match result.unwrap() {
                Ok(saved) => {
                    winners += 1;
                    assert_eq!(saved.revision, expected + 1);
                }
                Err(AppError::Conflict) => {}
                other => panic!("unexpected save result: {other:?}"),
            }
        }
        assert_eq!(winners, 1);
        assert_eq!(
            store
                .load_plan(id, "winter-2026")
                .await
                .unwrap()
                .unwrap()
                .revision,
            expected + 1
        );
    }
    delete_user(store.pool(), id).await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn plans_are_account_and_semester_scoped() {
    let store = database().await;
    let a = user(store.pool()).await;
    let b = user(store.pool()).await;
    let original = plan();
    store.save_plan(a, 0, &original).await.unwrap();
    assert!(store
        .load_plan(b, &original.semester_id)
        .await
        .unwrap()
        .is_none());
    assert!(store
        .load_plan(a, "other-semester")
        .await
        .unwrap()
        .is_none());
    assert!(matches!(
        store.save_plan(b, 1, &original).await,
        Err(AppError::Conflict)
    ));
    let mut other = original.clone();
    other.attendance.as_of += Duration::days(1);
    store.save_plan(b, 0, &other).await.unwrap();
    store.save_plan(b, 1, &other).await.unwrap();
    let saved_a = store
        .load_plan(a, &original.semester_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved_a.plan, original);
    assert_eq!(saved_a.revision, 1);
    assert_eq!(
        store
            .load_plan(b, &other.semester_id)
            .await
            .unwrap()
            .unwrap()
            .plan,
        other
    );
    delete_user(store.pool(), a).await;
    delete_user(store.pool(), b).await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn invalid_input_is_rejected_and_corrupt_storage_is_unavailable() {
    let store = database().await;
    let id = user(store.pool()).await;
    let valid = plan();
    let mut schema = valid.clone();
    schema.schema_version += 1;
    let mut calendar = valid.clone();
    calendar.calendar_revision += 1;
    let mut semester = valid.clone();
    semester.semester_id = "unknown".into();
    let mut attendance = valid.clone();
    attendance.attendance.attended.insert("UNKNOWN".into(), 1);
    for invalid in [&schema, &calendar, &semester, &attendance] {
        assert!(matches!(
            store.save_plan(id, 0, invalid).await,
            Err(AppError::BadRequest(_))
        ));
        assert!(store
            .load_plan(id, &valid.semester_id)
            .await
            .unwrap()
            .is_none());
    }
    store.save_plan(id, 0, &valid).await.unwrap();
    for invalid in [
        serde_json::json!(null),
        serde_json::json!([]),
        serde_json::json!("invalid"),
        serde_json::json!(1),
    ] {
        let error = sqlx::query(
            "UPDATE app_private.student_plans SET plan = $3 WHERE user_id = $1 AND semester_id = $2",
        )
        .bind(id)
        .bind(&valid.semester_id)
        .bind(invalid)
        .execute(store.pool())
        .await
        .unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("23514")
        );
        assert_eq!(
            store
                .load_plan(id, &valid.semester_id)
                .await
                .unwrap()
                .unwrap()
                .plan,
            valid
        );
    }
    for invalid in [
        serde_json::json!({}),
        serde_json::to_value(schema).unwrap(),
        serde_json::to_value(calendar).unwrap(),
        serde_json::to_value(semester).unwrap(),
        serde_json::to_value(attendance).unwrap(),
    ] {
        sqlx::query("UPDATE app_private.student_plans SET plan = $3 WHERE user_id = $1 AND semester_id = $2")
            .bind(id).bind(&valid.semester_id).bind(invalid).execute(store.pool()).await.unwrap();
        assert!(matches!(
            store.load_plan(id, &valid.semester_id).await,
            Err(AppError::Unavailable(_))
        ));
    }
    delete_user(store.pool(), id).await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn concurrent_quota_consumption_has_exactly_limit_winners() {
    let store = database().await;
    let id = user(store.pool()).await;
    for kind in [QuotaKind::Helper, QuotaKind::Image] {
        let limit = 7;
        assert_quota(
            store.read_quota(id, kind, day(), limit).await.unwrap(),
            limit,
            0,
        );
        let barrier = Arc::new(Barrier::new(32));
        let mut tasks = JoinSet::new();
        for _ in 0..32 {
            let store = store.clone();
            let barrier = barrier.clone();
            tasks.spawn(async move {
                barrier.wait().await;
                store.consume_quota(id, kind, day(), limit).await
            });
        }
        let mut used = Vec::new();
        while let Some(result) = tasks.join_next().await {
            match result.unwrap() {
                Ok(quota) => {
                    assert_eq!(quota.remaining, limit - quota.used);
                    used.push(quota.used);
                }
                Err(AppError::RateLimited) => {}
                other => panic!("unexpected quota result: {other:?}"),
            }
        }
        used.sort_unstable();
        assert_eq!(used, (1..=limit).collect::<Vec<_>>());
        assert_quota(
            store.read_quota(id, kind, day(), limit).await.unwrap(),
            limit,
            limit,
        );
        assert!(matches!(
            store.consume_quota(id, kind, day(), limit).await,
            Err(AppError::RateLimited)
        ));
    }
    delete_user(store.pool(), id).await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn quotas_are_scoped_and_limit_changes_do_not_reset_usage() {
    let store = database().await;
    let a = user(store.pool()).await;
    let b = user(store.pool()).await;
    for _ in 0..3 {
        store
            .consume_quota(a, QuotaKind::Helper, day(), 3)
            .await
            .unwrap();
    }
    for (id, kind, date) in [
        (b, QuotaKind::Helper, day()),
        (a, QuotaKind::Image, day()),
        (a, QuotaKind::Helper, day() + Duration::days(1)),
    ] {
        assert_quota(store.read_quota(id, kind, date, 3).await.unwrap(), 3, 0);
        assert_quota(store.consume_quota(id, kind, date, 3).await.unwrap(), 3, 1);
    }
    assert_quota(
        store
            .read_quota(a, QuotaKind::Helper, day(), 1)
            .await
            .unwrap(),
        1,
        3,
    );
    assert!(matches!(
        store.consume_quota(a, QuotaKind::Helper, day(), 1).await,
        Err(AppError::RateLimited)
    ));
    assert_quota(
        store
            .consume_quota(a, QuotaKind::Helper, day(), 4)
            .await
            .unwrap(),
        4,
        4,
    );
    for limit in [i32::MIN, -1, 0, 1001, i32::MAX] {
        assert!(matches!(
            store.read_quota(a, QuotaKind::Helper, day(), limit).await,
            Err(AppError::BadRequest(_))
        ));
        assert!(matches!(
            store
                .consume_quota(a, QuotaKind::Helper, day(), limit)
                .await,
            Err(AppError::BadRequest(_))
        ));
    }
    assert_quota(
        store
            .read_quota(a, QuotaKind::Image, day(), 1000)
            .await
            .unwrap(),
        1000,
        1,
    );
    delete_user(store.pool(), a).await;
    delete_user(store.pool(), b).await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn missing_rows_and_database_failures_are_distinct() {
    let store = database().await;
    let id = user(store.pool()).await;
    assert!(store.load_plan(id, "winter-2026").await.unwrap().is_none());
    assert_quota(
        store
            .read_quota(id, QuotaKind::Helper, day(), 3)
            .await
            .unwrap(),
        3,
        0,
    );
    let absent_user = Uuid::new_v4();
    assert!(matches!(
        store.save_plan(absent_user, 0, &plan()).await,
        Err(AppError::Unavailable(_))
    ));
    assert!(matches!(
        store
            .consume_quota(absent_user, QuotaKind::Helper, day(), 3)
            .await,
        Err(AppError::Unavailable(_))
    ));
    delete_user(store.pool(), id).await;
    store.pool().close().await;
    assert!(matches!(
        store.load_plan(id, "winter-2026").await,
        Err(AppError::Unavailable(_))
    ));
    assert!(matches!(
        store.save_plan(id, 0, &plan()).await,
        Err(AppError::Unavailable(_))
    ));
    assert!(matches!(
        store.read_quota(id, QuotaKind::Helper, day(), 3).await,
        Err(AppError::Unavailable(_))
    ));
    assert!(matches!(
        store.consume_quota(id, QuotaKind::Helper, day(), 3).await,
        Err(AppError::Unavailable(_))
    ));
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn deleting_users_cascades_plans_sessions_and_usage() {
    let store = database().await;
    let id = user(store.pool()).await;
    let survivor = user(store.pool()).await;
    for user_id in [id, survivor] {
        store.save_plan(user_id, 0, &plan()).await.unwrap();
        store
            .consume_quota(user_id, QuotaKind::Helper, day(), 3)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO app_private.auth_sessions
             (token_hash, user_id, email, display_name, encrypted_tokens, access_expires_at, expires_at)
             VALUES ($1, $2, NULL, NULL, $3, now() + interval '1 hour', now() + interval '1 day')",
        ).bind(user_id.as_bytes().as_slice()).bind(user_id).bind(vec![1_u8, 2, 3])
            .execute(store.pool()).await.unwrap();
    }
    delete_user(store.pool(), id).await;
    for table in ["student_plans", "auth_sessions", "ai_usage"] {
        let query = format!("SELECT count(*) FROM app_private.{table} WHERE user_id = $1");
        let deleted: i64 = sqlx::query_scalar(&query)
            .bind(id)
            .fetch_one(store.pool())
            .await
            .unwrap();
        let retained: i64 = sqlx::query_scalar(&query)
            .bind(survivor)
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(deleted, 0, "{table}");
        assert_eq!(retained, 1, "{table}");
    }
    delete_user(store.pool(), survivor).await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn private_tables_have_rls_no_api_privileges_and_expiration_indexes() {
    let store = database().await;
    MIGRATOR
        .run(store.pool())
        .await
        .expect("migrations are idempotent");
    let tables: Vec<(String, bool)> = sqlx::query_as(
        "SELECT c.relname::text, c.relrowsecurity FROM pg_class c
         JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = 'app_private' AND c.relkind = 'r' ORDER BY c.relname",
    )
    .fetch_all(store.pool())
    .await
    .unwrap();
    assert_eq!(
        tables
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "account_deletion_requests",
            "ai_usage",
            "auth_sessions",
            "migration_batches",
            "migration_rows",
            "oauth_flows",
            "student_plans",
        ]
    );
    for (table, rls) in &tables {
        assert!(*rls, "RLS disabled for {table}");
    }
    let public_grants: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
         CROSS JOIN LATERAL aclexplode(COALESCE(c.relacl, acldefault('r', c.relowner))) a
         WHERE n.nspname = 'app_private' AND c.relkind = 'r' AND a.grantee = 0",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(public_grants, 0);
    let public_schema_grants: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_namespace n
         CROSS JOIN LATERAL aclexplode(COALESCE(n.nspacl, acldefault('n', n.nspowner))) a
         WHERE n.nspname = 'app_private' AND a.grantee = 0",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(public_schema_grants, 0);
    let roles: Vec<String> = sqlx::query_scalar(
        "SELECT rolname::text FROM pg_roles WHERE rolname IN ('anon', 'authenticated')",
    )
    .fetch_all(store.pool())
    .await
    .unwrap();
    for role in roles {
        let schema_access: bool =
            sqlx::query_scalar("SELECT has_schema_privilege($1, 'app_private', 'USAGE')")
                .bind(&role)
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert!(!schema_access, "{role} can access app_private");
        for (table, _) in &tables {
            let access: bool = sqlx::query_scalar(
                "SELECT has_table_privilege($1, $2, 'SELECT, INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER')",
            ).bind(&role).bind(format!("app_private.{table}"))
                .fetch_one(store.pool()).await.unwrap();
            assert!(!access, "{role} has privileges on {table}");
        }
    }
    let indexes: Vec<String> =
        sqlx::query_scalar("SELECT indexdef FROM pg_indexes WHERE schemaname = 'app_private'")
            .fetch_all(store.pool())
            .await
            .unwrap();
    for (table, column) in [
        ("auth_sessions", "expires_at"),
        ("auth_sessions", "user_id"),
        ("oauth_flows", "expires_at"),
    ] {
        assert!(indexes
            .iter()
            .any(|index| index.contains(table) && index.contains(&format!("({column})"))));
    }
    let token = Uuid::new_v4().as_bytes().to_vec();
    let blob = vec![0_u8, 255, 1, 128];
    sqlx::query("INSERT INTO app_private.oauth_flows (token_hash, encrypted_verifier, expires_at) VALUES ($1, $2, now() + interval '5 minutes')")
        .bind(&token).bind(&blob).execute(store.pool()).await.unwrap();
    let stored: Vec<u8> = sqlx::query_scalar(
        "SELECT encrypted_verifier FROM app_private.oauth_flows WHERE token_hash = $1",
    )
    .bind(&token)
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(stored, blob);
    sqlx::query("DELETE FROM app_private.oauth_flows WHERE token_hash = $1")
        .bind(&token)
        .execute(store.pool())
        .await
        .unwrap();
}
