//! Destructive only to synthetic UUIDs created by these tests. Point
//! TEST_DATABASE_URL at a disposable local PostgreSQL database, never production.
use debarred_migrate::{
    canonical_plan_sha256, plan_storage_sha256, storage, validate, Error, PreparedBatch,
};
use serde_json::json;
use sqlx::{postgres::PgPoolOptions, PgPool};
use tokio::sync::OnceCell;
use uuid::Uuid;

// Same files embedded by the server's MIGRATOR, without a server dependency.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../server/migrations");
static INITIALIZED: OnceCell<()> = OnceCell::const_new();
async fn database() -> PgPool {
    let url = std::env::var("TEST_DATABASE_URL").expect("requires disposable TEST_DATABASE_URL");
    let options = storage::connection_options(&url, false).expect("tests require a local database");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await
        .expect("local test connection");
    INITIALIZED.get_or_init(|| async {
        sqlx::raw_sql("CREATE SCHEMA IF NOT EXISTS auth; CREATE TABLE IF NOT EXISTS auth.users (id uuid PRIMARY KEY);")
            .execute(&pool).await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
    }).await;
    pool
}
async fn fixture(pool: &PgPool, count: usize) -> (Vec<Uuid>, PreparedBatch) {
    fixture_with_plan(pool, count, json!({"courses":[],"grid":{}}), json!([])).await
}
async fn fixture_with_plan(
    pool: &PgPool,
    count: usize,
    timetable: serde_json::Value,
    leave_dates: serde_json::Value,
) -> (Vec<Uuid>, PreparedBatch) {
    let mut ids: Vec<_> = (0..count).map(|_| Uuid::new_v4()).collect();
    ids.sort();
    let mut records = vec![];
    let mut mappings = vec![];
    for (i, id) in ids.iter().enumerate() {
        sqlx::query("INSERT INTO auth.users (id) VALUES ($1)")
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        records.push(
            json!({"source_user_id":format!("user_synthetic_{i}"),"semester_id":"winter-2026",
            "timetable":timetable.clone(),"attended":{},"leave_dates":leave_dates.clone()}),
        );
        mappings.push(json!({"source_user_id":format!("user_synthetic_{i}"),"target_user_id":id,
            "semester_id":"winter-2026","snapshot_as_of":"2026-11-16",
            "verified_by":"Synthetic test operator","verification_reference":"Synthetic manual confirmation"}));
    }
    let batch = validate(
        &serde_json::to_vec(&json!({"source_kind":"Clerk","records":records})).unwrap(),
        &serde_json::to_vec(&json!({"mappings":mappings})).unwrap(),
    )
    .prepared
    .unwrap();
    (ids, batch)
}
async fn count(pool: &PgPool, ids: &[Uuid]) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM app_private.student_plans WHERE user_id = ANY($1)")
        .bind(ids)
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn wait_for_account_lock(pool: &PgPool, user: Uuid) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory' AND NOT granted AND classid::bigint = ((hashtextextended($1::text, 0) >> 32) & 4294967295) AND objid::bigint = (hashtextextended($1::text, 0) & 4294967295))")
                .bind(user.to_string()).fetch_one(pool).await.unwrap();
            if waiting { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.expect("migration must actually wait for the competing writer");
}
async fn cleanup(pool: &PgPool, ids: &[Uuid]) {
    sqlx::query("DELETE FROM auth.users WHERE id = ANY($1)")
        .bind(ids)
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn apply_existing_row_aborts_all_and_competing_applies_are_atomic() {
    let pool = database().await;
    let (ids, batch) = fixture(&pool, 2).await;
    let value = serde_json::to_value(&batch).unwrap()["rows"][1]["plan"].clone();
    // Simulate a server write winning the advisory lock while migration waits.
    let mut writer = pool.begin().await.unwrap();
    sqlx::query("SELECT app_private.lock_account_write($1)")
        .bind(ids[1])
        .execute(&mut *writer)
        .await
        .unwrap();
    let p = pool.clone();
    let digest = batch.digest().to_owned();
    let attempt = tokio::spawn(async move { storage::apply(&p, &batch, &digest).await });
    sqlx::query("INSERT INTO app_private.student_plans (user_id,semester_id,revision,plan) VALUES ($1,'winter-2026',1,$2)")
        .bind(ids[1]).bind(value).execute(&mut *writer).await.unwrap();
    wait_for_account_lock(&pool, ids[1]).await;
    writer.commit().await.unwrap();
    assert!(attempt.await.unwrap().is_err());
    assert_eq!(count(&pool, &ids).await, 1);
    cleanup(&pool, &ids).await;

    let (ids, batch) = fixture(&pool, 2).await;
    let digest = batch.digest();
    let (a, b) = tokio::join!(
        storage::apply(&pool, &batch, digest),
        storage::apply(&pool, &batch, digest)
    );
    assert_ne!(a.is_ok(), b.is_ok());
    assert_eq!(count(&pool, &ids).await, 2);
    let batch_id = a.or(b).unwrap();
    let audit: (String, i64, String) = sqlx::query_as(
        "SELECT fingerprint,row_count,status FROM app_private.migration_batches WHERE batch_id=$1",
    )
    .bind(batch_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audit, (digest.to_owned(), 2, "applied".to_owned()));
    let revisions: Vec<i64> = sqlx::query_scalar(
        "SELECT inserted_revision FROM app_private.migration_rows WHERE batch_id=$1",
    )
    .bind(batch_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(revisions, vec![1, 1]);
    for table in ["migration_batches", "migration_rows"] {
        let rls: bool = sqlx::query_scalar(
            "SELECT relrowsecurity FROM pg_class WHERE oid = ('app_private.' || $1)::regclass",
        )
        .bind(table)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(rls);
        let public_grant: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_class c, LATERAL aclexplode(COALESCE(c.relacl, acldefault('r',c.relowner))) a WHERE c.oid = ('app_private.' || $1)::regclass AND a.grantee = 0)")
            .bind(table).fetch_one(&pool).await.unwrap();
        assert!(!public_grant);
        for role in ["anon", "authenticated"] {
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname=$1)")
                    .bind(role)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            if exists {
                let access: bool = sqlx::query_scalar("SELECT has_table_privilege($1, 'app_private.' || $2, 'SELECT,INSERT,UPDATE,DELETE,TRUNCATE,REFERENCES,TRIGGER')")
                    .bind(role).bind(table).fetch_one(&pool).await.unwrap();
                assert!(!access);
            }
        }
    }
    assert!(storage::rollback(&pool, batch_id, "wrong").await.is_err());
    assert_eq!(count(&pool, &ids).await, 2);
    storage::rollback(&pool, batch_id, digest).await.unwrap();
    assert_eq!(count(&pool, &ids).await, 0);
    assert!(storage::rollback(&pool, batch_id, digest).await.is_err());
    let status: String =
        sqlx::query_scalar("SELECT status FROM app_private.migration_batches WHERE batch_id=$1")
            .bind(batch_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "rolled_back");
    cleanup(&pool, &ids).await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn pending_and_missing_accounts_abort_before_any_insert() {
    let pool = database().await;
    let (ids, batch) = fixture(&pool, 2).await;
    let mut deletion = pool.begin().await.unwrap();
    sqlx::query("SELECT app_private.lock_account_write($1)")
        .bind(ids[1])
        .execute(&mut *deletion)
        .await
        .unwrap();
    sqlx::query("INSERT INTO app_private.account_deletion_requests (user_id) VALUES ($1)")
        .bind(ids[1])
        .execute(&mut *deletion)
        .await
        .unwrap();
    let p = pool.clone();
    let digest = batch.digest().to_owned();
    let attempt = tokio::spawn(async move { storage::apply(&p, &batch, &digest).await });
    wait_for_account_lock(&pool, ids[1]).await;
    deletion.commit().await.unwrap();
    assert!(attempt.await.unwrap().is_err());
    assert_eq!(count(&pool, &ids).await, 0);
    cleanup(&pool, &ids).await;
    let (ids, batch) = fixture(&pool, 2).await;
    sqlx::query("DELETE FROM auth.users WHERE id=$1")
        .bind(ids[1])
        .execute(&pool)
        .await
        .unwrap();
    assert!(storage::apply(&pool, &batch, batch.digest()).await.is_err());
    assert_eq!(count(&pool, &ids).await, 0);
    cleanup(&pool, &ids).await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn rollback_refuses_representation_only_edits_without_revision_bump() {
    let pool = database().await;
    for change in ["course_order", "day_to_slots"] {
        let (ids, batch) = fixture_with_plan(&pool, 2, json!({
            "courses":[{"code":"CS101","name":"Synthetic one"},{"code":"CS102","name":"Synthetic two"}],
            "grid":{"Mon":["CS101","CS102",null,null,null,null,null]}
        }), json!(["2026-11-23"])).await;
        let batch_id = storage::apply(&pool, &batch, batch.digest()).await.unwrap();
        let original = serde_json::to_value(&batch).unwrap()["rows"][1]["plan"].clone();
        let mut edited = original.clone();
        match change {
            "course_order" => edited["timetable"]["courses"]
                .as_array_mut()
                .unwrap()
                .reverse(),
            "day_to_slots" => {
                edited["leave"] = json!([
                    {"kind":"slot","date":"2026-11-23","slot":0},
                    {"kind":"slot","date":"2026-11-23","slot":1}
                ])
            }
            _ => unreachable!(),
        }
        let original_plan = serde_json::from_value(original.clone()).unwrap();
        let edited_plan = serde_json::from_value(edited.clone()).unwrap();
        assert_eq!(
            canonical_plan_sha256(&original_plan).unwrap(),
            canonical_plan_sha256(&edited_plan).unwrap()
        );
        assert_ne!(
            plan_storage_sha256(&original).unwrap(),
            plan_storage_sha256(&edited).unwrap()
        );
        sqlx::query("UPDATE app_private.student_plans SET plan=$2 WHERE user_id=$1")
            .bind(ids[1])
            .bind(&edited)
            .execute(&pool)
            .await
            .unwrap();
        let before: Vec<(Uuid, serde_json::Value, i64)> = sqlx::query_as("SELECT user_id,plan,revision FROM app_private.student_plans WHERE user_id = ANY($1) ORDER BY user_id")
            .bind(&ids).fetch_all(&pool).await.unwrap();
        assert_eq!(before.len(), 2);
        assert!(before.iter().all(|r| r.2 == 1));
        assert_eq!(
            storage::rollback(&pool, batch_id, batch.digest()).await,
            Err(Error("imported_plan_modified")),
            "{change}"
        );
        let after: Vec<(Uuid, serde_json::Value, i64)> = sqlx::query_as("SELECT user_id,plan,revision FROM app_private.student_plans WHERE user_id = ANY($1) ORDER BY user_id")
            .bind(&ids).fetch_all(&pool).await.unwrap();
        assert_eq!(
            after, before,
            "all rows, including the manual edit, must survive {change}"
        );
        let status: String = sqlx::query_scalar(
            "SELECT status FROM app_private.migration_batches WHERE batch_id=$1",
        )
        .bind(batch_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "applied");
        cleanup(&pool, &ids).await;
    }
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn rollback_refuses_semantic_edits_revision_changes_missing_rows_and_deleted_accounts() {
    let pool = database().await;
    for change in [
        "semantic",
        "revision",
        "missing_plan",
        "missing_account",
        "pending",
    ] {
        let (ids, batch) = fixture(&pool, 2).await;
        let batch_id = storage::apply(&pool, &batch, batch.digest()).await.unwrap();
        match change {
            "semantic" => {
                sqlx::query("UPDATE app_private.student_plans SET plan=jsonb_set(plan,'{attendance,as_of}','\"2026-11-17\"') WHERE user_id=$1")
                    .bind(ids[1]).execute(&pool).await.unwrap();
            }
            "revision" => {
                sqlx::query("UPDATE app_private.student_plans SET revision=2 WHERE user_id=$1")
                    .bind(ids[1])
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            "missing_plan" => {
                sqlx::query("DELETE FROM app_private.student_plans WHERE user_id=$1")
                    .bind(ids[1])
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            "missing_account" => {
                sqlx::query("DELETE FROM auth.users WHERE id=$1")
                    .bind(ids[1])
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            "pending" => {
                sqlx::query(
                    "INSERT INTO app_private.account_deletion_requests (user_id) VALUES ($1)",
                )
                .bind(ids[1])
                .execute(&pool)
                .await
                .unwrap();
            }
            _ => unreachable!(),
        }
        let before = count(&pool, &ids).await;
        assert!(
            storage::rollback(&pool, batch_id, batch.digest())
                .await
                .is_err(),
            "{change}"
        );
        assert_eq!(
            count(&pool, &ids).await,
            before,
            "must not partially roll back {change}"
        );
        let status: String = sqlx::query_scalar(
            "SELECT status FROM app_private.migration_batches WHERE batch_id=$1",
        )
        .bind(batch_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "applied");
        cleanup(&pool, &ids).await;
    }
}
