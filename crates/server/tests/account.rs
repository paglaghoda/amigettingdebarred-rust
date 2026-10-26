//! These tests deliberately mount the scoped module without changing lib.rs.
use debarred_server::{auth, error, AppState};
#[path = "../src/account.rs"]
mod account;

use aes_gcm::{
    aead::{Aead, Payload},
    Aes256Gcm, KeyInit, Nonce,
};
use axum::{
    body::Body,
    http::{HeaderMap, Method, Request, StatusCode},
    response::Response,
    routing::post,
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use debarred_domain::{load_semester, AttendanceSnapshot, Plan, Timetable};
use debarred_server::{
    ai::AiService,
    auth::AuthService,
    config::Config,
    db::{QuotaKind, Store, MIGRATOR},
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::{sync::Arc, time::Duration};
use tokio::sync::OnceCell;
use tower::ServiceExt;
use uuid::Uuid;

static INITIALIZED: OnceCell<()> = OnceCell::const_new();
const CONFIRM: &str = r#"{"confirmation":"DELETE MY ACCOUNT"}"#;

async fn database() -> PgPool {
    let url = std::env::var("TEST_DATABASE_URL").expect("disposable TEST_DATABASE_URL required");
    let pool = PgPoolOptions::new()
        .max_connections(12)
        .connect(&url)
        .await
        .unwrap();
    INITIALIZED.get_or_init(|| async {
        sqlx::raw_sql("CREATE SCHEMA IF NOT EXISTS auth; CREATE TABLE IF NOT EXISTS auth.users (id uuid PRIMARY KEY);")
            .execute(&pool).await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
    }).await;
    pool
}

struct Fixture {
    pool: PgPool,
    state: AppState,
    app: Router,
    id: Uuid,
    other: Uuid,
    cookie: String,
    hash: Vec<u8>,
    provider: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.provider.abort();
    }
}
impl Fixture {
    async fn new() -> Self {
        Self::with_pool(database().await).await
    }

    async fn with_pool(pool: PgPool) -> Self {
        let id = Uuid::new_v4();
        let other = Uuid::new_v4();
        sqlx::query("INSERT INTO auth.users (id) VALUES ($1), ($2)")
            .bind(id)
            .bind(other)
            .execute(&pool)
            .await
            .unwrap();
        let provider_app = Router::new().route("/auth/v1/token", post(move || async move {
            Json(json!({"access_token": "mock-provider-access-secret", "refresh_token": "mock-provider-refresh-secret",
                "expires_in": 3600, "user": {"id": id, "email": "owner@example.test", "user_metadata": {"name": "Owner"}}}))
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let provider = tokio::spawn(async move {
            axum::serve(listener, provider_app).await.unwrap();
        });
        let config = Arc::new(Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            public_origin: "http://localhost:3000".into(),
            supabase_url: url.parse().unwrap(),
            supabase_anon_key: "mock-anon-secret".into(),
            database_url: String::new(),
            session_key: [7; 32],
            secure_cookies: false,
            local_auth_enabled: true,
            session_ttl_seconds: 7200,
            database_max_connections: 12,
            trusted_proxy_ips: vec![],
            auth_requests_per_minute: 20,
            helper_daily_limit: 3,
            image_daily_limit: 3,
            ai_concurrency: 2,
            ai: None,
        });
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap();
        let state = AppState {
            config: config.clone(),
            store: Store::new(pool.clone()),
            auth: AuthService::new(pool.clone(), client.clone(), config.clone()),
            ai: AiService::new(client, config),
            calendar: Arc::new(load_semester("winter-2026").unwrap()),
        };
        let app = account::routes()
            .merge(auth::routes())
            .with_state(state.clone());
        let (cookie, hash) = seed_session(&pool, id).await;
        seed_session(&pool, id).await;
        seed_session(&pool, other).await;
        for owner in [id, other] {
            state.store.save_plan(owner, 0, &plan()).await.unwrap();
            state
                .store
                .consume_quota(owner, QuotaKind::Helper, Utc::now().date_naive(), 3)
                .await
                .unwrap();
        }
        Self {
            pool,
            state,
            app,
            id,
            other,
            cookie,
            hash,
            provider,
        }
    }
    fn headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("cookie", self.cookie.parse().unwrap());
        headers.insert("origin", "http://localhost:3000".parse().unwrap());
        headers.insert("x-csrf-token", "mock-csrf-secret".parse().unwrap());
        headers.insert("x-session-user", self.id.to_string().parse().unwrap());
        headers
    }
    async fn request(
        &self,
        path: &str,
        method: Method,
        body: &str,
        headers: HeaderMap,
    ) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap();
        request.headers_mut().extend(headers);
        self.app.clone().oneshot(request).await.unwrap()
    }
    async fn delete(&self) -> Response {
        self.request(
            "/api/account/deletion",
            Method::POST,
            CONFIRM,
            self.headers(),
        )
        .await
    }
    async fn count(&self, table: &str, id: Uuid) -> i64 {
        sqlx::query_scalar(&format!(
            "SELECT count(*) FROM app_private.{table} WHERE user_id = $1"
        ))
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }
    async fn cleanup(&self) {
        sqlx::query("DELETE FROM auth.users WHERE id IN ($1, $2)")
            .bind(self.id)
            .bind(self.other)
            .execute(&self.pool)
            .await
            .unwrap();
    }
}

async fn seed_session(pool: &PgPool, id: Uuid) -> (String, Vec<u8>) {
    let mut bytes = Vec::from(*Uuid::new_v4().as_bytes());
    bytes.extend_from_slice(Uuid::new_v4().as_bytes());
    let token = URL_SAFE_NO_PAD.encode(bytes);
    let hash = Sha256::digest(token.as_bytes()).to_vec();
    let nonce = *Uuid::new_v4().as_bytes();
    let plaintext = serde_json::to_vec(&json!({"access_token": "seed-access-secret", "refresh_token": "seed-refresh-secret", "csrf_token": "mock-csrf-secret"})).unwrap();
    let cipher = Aes256Gcm::new_from_slice(&[7; 32]).unwrap();
    let mut encrypted = nonce[..12].to_vec();
    encrypted.extend(
        cipher
            .encrypt(
                Nonce::from_slice(&nonce[..12]),
                Payload {
                    msg: &plaintext,
                    aad: &hash,
                },
            )
            .unwrap(),
    );
    sqlx::query("INSERT INTO app_private.auth_sessions (token_hash,user_id,email,display_name,encrypted_tokens,access_expires_at,expires_at,authenticated_at)
        VALUES ($1,$2,'owner@example.test','Owner',$3,now()+interval '1 hour',now()+interval '2 hours',clock_timestamp())")
        .bind(&hash).bind(id).bind(encrypted).execute(pool).await.unwrap();
    (format!("session={token}"), hash)
}
fn plan() -> Plan {
    let calendar = load_semester("winter-2026").unwrap();
    Plan {
        schema_version: debarred_domain::model::PLAN_SCHEMA_VERSION,
        semester_id: calendar.config().id.clone(),
        calendar_revision: calendar.config().revision,
        timetable: Timetable::default(),
        attendance: AttendanceSnapshot {
            as_of: calendar.config().start_date - chrono::Duration::days(1),
            attended: Default::default(),
        },
        leave: vec![],
    }
}
async fn body(response: Response) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL"]
async fn lifecycle_schema_keeps_rls_and_private_table_function_privileges() {
    let pool = database().await;
    let rls: bool = sqlx::query_scalar(
        "SELECT relrowsecurity FROM pg_class WHERE oid = 'app_private.account_deletion_requests'::regclass",
    ).fetch_one(&pool).await.unwrap();
    assert!(rls);
    let table_grants: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
         CROSS JOIN LATERAL aclexplode(COALESCE(c.relacl, acldefault('r', c.relowner))) a
         WHERE n.nspname = 'app_private' AND c.relkind = 'r'
           AND (a.grantee = 0 OR a.grantee IN (SELECT oid FROM pg_roles WHERE rolname IN ('anon','authenticated')))",
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(table_grants, 0);
    let function_grants: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
         CROSS JOIN LATERAL aclexplode(COALESCE(p.proacl, acldefault('f', p.proowner))) a
         WHERE n.nspname = 'app_private'
           AND (a.grantee = 0 OR a.grantee IN (SELECT oid FROM pg_roles WHERE rolname IN ('anon','authenticated')))",
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(function_grants, 0);
    let default: String = sqlx::query_scalar(
        "SELECT column_default FROM information_schema.columns WHERE table_schema = 'app_private'
         AND table_name = 'auth_sessions' AND column_name = 'authenticated_at'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(default, "now()");
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL and permission to create/set test roles"]
async fn pending_guard_cannot_be_hidden_by_runtime_role_rls() {
    let pool = database().await;
    let role = format!("account_guard_{}", Uuid::new_v4().simple());
    let pending = Uuid::new_v4();
    let active = Uuid::new_v4();
    // Role, grants, policies, and fixture rows all roll back together. Even a
    // failed assertion drops this transaction without leaving a persistent role.
    let mut tx = pool.begin().await.unwrap();
    let (definer, settings): (bool, Vec<String>) = sqlx::query_as(
        "SELECT prosecdef, proconfig FROM pg_proc
         WHERE oid = 'app_private.lock_account_write(uuid)'::regprocedure",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert!(definer);
    assert_eq!(settings, vec!["search_path=pg_catalog, app_private"]);
    sqlx::query("INSERT INTO auth.users (id) VALUES ($1), ($2)")
        .bind(pending)
        .bind(active)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO app_private.account_deletion_requests (user_id) VALUES ($1)")
        .bind(pending)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::raw_sql(&format!(
        "CREATE ROLE {role} NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOBYPASSRLS;
         GRANT USAGE ON SCHEMA app_private TO {role};
         GRANT SELECT ON app_private.account_deletion_requests TO {role};
         GRANT SELECT, INSERT ON app_private.student_plans, app_private.auth_sessions TO {role};
         GRANT EXECUTE ON FUNCTION app_private.lock_account_write(uuid) TO {role};
         CREATE POLICY {role} ON app_private.student_plans TO {role} USING (true) WITH CHECK (true);
         CREATE POLICY {role} ON app_private.auth_sessions TO {role} USING (true) WITH CHECK (true);
         SET LOCAL ROLE {role};"
    ))
    .execute(&mut *tx)
    .await
    .unwrap();
    // SELECT is permitted, but there is deliberately no queue RLS policy.
    let visible: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM app_private.account_deletion_requests WHERE user_id = $1",
    )
    .bind(pending)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(visible, 0);
    let statements = [
        "INSERT INTO app_private.student_plans (user_id,semester_id,plan,revision) VALUES ($1,'winter-2026','{}',1)",
        "INSERT INTO app_private.auth_sessions (user_id,token_hash,encrypted_tokens,access_expires_at,expires_at)
         VALUES ($1,'\\x01','\\x02',now()+interval '1 hour',now()+interval '2 hours')",
    ];
    for statement in statements {
        // Prove the restricted role really can insert for a non-pending user,
        // including through the invoker trigger calling the definer helper.
        sqlx::query(statement)
            .bind(active)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("SAVEPOINT pending_insert")
            .execute(&mut *tx)
            .await
            .unwrap();
        let error = sqlx::query(statement)
            .bind(pending)
            .execute(&mut *tx)
            .await
            .unwrap_err();
        let database_error = error.as_database_error().unwrap();
        assert_eq!(database_error.code().as_deref(), Some("42501"));
        assert_eq!(
            database_error.message(),
            "account identity deletion is pending"
        );
        sqlx::query("ROLLBACK TO SAVEPOINT pending_insert")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("RELEASE SAVEPOINT pending_insert")
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx.rollback().await.unwrap();
    let role_exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_roles WHERE rolname = $1)")
            .bind(role)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!role_exists);
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL"]
async fn export_is_allowlisted_owner_scoped_and_no_store() {
    let f = Fixture::new().await;
    let response = f
        .request("/api/account/export", Method::GET, "", f.headers())
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let value = body(response).await;
    assert_eq!(value.as_object().unwrap().len(), 6);
    assert_eq!(value["migration_history"], json!([]));
    assert_eq!(value["version"], 1);
    assert!(value["generated_at"].as_str().is_some());
    assert_eq!(
        value["profile"],
        json!({"user_id": f.id, "email": "owner@example.test", "display_name": "Owner"})
    );
    assert_eq!(value["plans"].as_array().unwrap().len(), 1);
    assert_eq!(
        value["plans"][0]["plan"],
        serde_json::to_value(plan()).unwrap()
    );
    assert_eq!(value["plans"][0].as_object().unwrap().len(), 3);
    assert_eq!(value["ai_usage"].as_array().unwrap().len(), 1);
    assert_eq!(value["ai_usage"][0].as_object().unwrap().len(), 3);
    let text = value.to_string();
    for forbidden in [
        "csrf",
        "token",
        "cookie",
        "ciphertext",
        "encrypted",
        "secret",
        "provider",
        &f.other.to_string(),
    ] {
        assert!(
            !text.contains(forbidden),
            "export contains forbidden field/value"
        );
    }
    let mut headers = f.headers();
    headers.insert("x-session-user", f.other.to_string().parse().unwrap());
    assert_eq!(
        f.request("/api/account/export", Method::GET, "", headers)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.request("/api/account/export", Method::GET, "", HeaderMap::new())
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL"]
async fn migration_history_is_owner_scoped_and_erased_without_changing_shared_batch() {
    let f = Fixture::new().await;
    let batch_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO app_private.migration_batches
         (batch_id, report_version, fingerprint, source_sha256, mapping_sha256, row_count, status, applied_at)
         VALUES ($1, 1, repeat('a',64), repeat('b',64), repeat('c',64), 2, 'applied', '2026-01-10T12:00:00Z')",
    ).bind(batch_id).execute(&f.pool).await.unwrap();
    for (owner, storage_hash) in [(f.id, "e".repeat(64)), (f.other, "f".repeat(64))] {
        sqlx::query(
            "INSERT INTO app_private.migration_rows
             (batch_id, user_id, semester_id, inserted_revision, plan_sha256, plan_storage_sha256)
             VALUES ($1, $2, 'winter-2026', 1, repeat('d',64), $3)",
        )
        .bind(batch_id)
        .bind(owner)
        .bind(storage_hash)
        .execute(&f.pool)
        .await
        .unwrap();
    }
    let batch_before: Value = sqlx::query_scalar(
        "SELECT to_jsonb(b) FROM app_private.migration_batches b WHERE batch_id = $1",
    )
    .bind(batch_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    let response = f
        .request("/api/account/export", Method::GET, "", f.headers())
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let export = body(response).await;
    assert_eq!(
        export["migration_history"],
        json!([{
            "semester_id": "winter-2026", "inserted_revision": 1,
            "plan_storage_sha256": "e".repeat(64),
            "applied_at": "2026-01-10T12:00:00Z", "status": "applied",
        }])
    );
    let text = export.to_string();
    for forbidden in [
        "row_count",
        "source_sha256",
        "mapping_sha256",
        "fingerprint",
        "plan_sha256",
        "batch_id",
        &batch_id.to_string(),
        &f.other.to_string(),
        &"f".repeat(64),
    ] {
        assert!(
            !text.contains(forbidden),
            "export disclosed non-owned or global migration metadata"
        );
    }
    let response = f.delete().await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert!(!response.headers().contains_key("set-cookie"));
    assert_eq!(
        body(response).await,
        json!({"status": "pending_identity_deletion", "app_data_deleted": true})
    );
    assert_eq!(f.count("migration_rows", f.id).await, 0);
    assert_eq!(f.count("migration_rows", f.other).await, 1);
    let other_hash: String = sqlx::query_scalar(
        "SELECT plan_storage_sha256 FROM app_private.migration_rows WHERE batch_id = $1 AND user_id = $2",
    ).bind(batch_id).bind(f.other).fetch_one(&f.pool).await.unwrap();
    assert_eq!(other_hash, "f".repeat(64));
    let batch_after: Value = sqlx::query_scalar(
        "SELECT to_jsonb(b) FROM app_private.migration_batches b WHERE batch_id = $1",
    )
    .bind(batch_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        batch_after, batch_before,
        "shared batch audit must remain unchanged"
    );
    f.cleanup().await;
    sqlx::query("DELETE FROM app_private.migration_batches WHERE batch_id = $1")
        .bind(batch_id)
        .execute(&f.pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL"]
async fn deletion_requires_confirmation_origin_csrf_identity_and_recent_interactive_login() {
    let f = Fixture::new().await;
    for payload in [
        r#"{"confirmation":"delete"}"#,
        r#"{"confirmation":"DELETE MY ACCOUNT","user_id":"ignored"}"#,
    ] {
        assert!(f
            .request("/api/account/deletion", Method::POST, payload, f.headers())
            .await
            .status()
            .is_client_error());
    }
    for header in ["origin", "x-csrf-token", "x-session-user"] {
        let mut headers = f.headers();
        headers.insert(header, "wrong".parse().unwrap());
        assert!(f
            .request("/api/account/deletion", Method::POST, CONFIRM, headers)
            .await
            .status()
            .is_client_error());
    }
    sqlx::query("UPDATE app_private.auth_sessions SET authenticated_at = now() - interval '6 minutes', access_expires_at = now() WHERE token_hash = $1")
        .bind(&f.hash).execute(&f.pool).await.unwrap();
    // This calls the real refresh path with the mock provider. Age must not move.
    f.state
        .auth
        .authenticate(&f.headers(), &Method::GET)
        .await
        .unwrap();
    let old: bool = sqlx::query_scalar("SELECT authenticated_at < now() - interval '5 minutes' FROM app_private.auth_sessions WHERE token_hash = $1")
        .bind(&f.hash).fetch_one(&f.pool).await.unwrap();
    assert!(old);
    let response = f.delete().await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body(response).await["error"]["message"]
        .as_str()
        .unwrap()
        .starts_with("Sign out and sign in again within five minutes"));
    assert_eq!(f.count("account_deletion_requests", f.id).await, 0);
    assert_eq!(f.count("student_plans", f.id).await, 1);
    assert_eq!(f.count("auth_sessions", f.id).await, 2);
    // Legacy migration rows and expired sessions must also fail closed.
    sqlx::query(
        "UPDATE app_private.auth_sessions SET authenticated_at = '-infinity' WHERE token_hash = $1",
    )
    .bind(&f.hash)
    .execute(&f.pool)
    .await
    .unwrap();
    assert!(matches!(
        f.state.auth.require_recent(&f.headers()).await,
        Err(error::AppError::BadRequest(_))
    ));
    sqlx::query("UPDATE app_private.auth_sessions SET expires_at = now() - interval '1 second' WHERE token_hash = $1")
        .bind(&f.hash).execute(&f.pool).await.unwrap();
    assert!(matches!(
        f.state.auth.require_recent(&f.headers()).await,
        Err(error::AppError::Unauthorized)
    ));
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL"]
async fn deletion_erases_owned_data_revokes_all_sessions_and_blocks_recreation_and_login() {
    let f = Fixture::new().await;
    let response = f.delete().await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert!(!response.headers().contains_key("set-cookie"));
    assert_eq!(
        body(response).await,
        json!({"status":"pending_identity_deletion","app_data_deleted":true})
    );
    for table in ["student_plans", "ai_usage", "auth_sessions"] {
        assert_eq!(f.count(table, f.id).await, 0);
        assert_eq!(f.count(table, f.other).await, 1);
    }
    assert_eq!(f.count("account_deletion_requests", f.id).await, 1);
    let identity: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM auth.users WHERE id = $1)")
            .bind(f.id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert!(identity);
    assert!(matches!(
        f.state.store.save_plan(f.id, 0, &plan()).await,
        Err(error::AppError::Forbidden)
    ));
    assert!(matches!(
        f.state
            .store
            .consume_quota(f.id, QuotaKind::Helper, Utc::now().date_naive(), 3)
            .await,
        Err(error::AppError::Forbidden)
    ));
    // Use real interactive login helper: provider success must not mint a session.
    let response = f
        .request(
            "/auth/local/login",
            Method::POST,
            r#"{"email":"owner@example.test","password":"mock-password"}"#,
            f.headers(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(!response.headers().contains_key("set-cookie"));
    let text = body(response).await.to_string();
    assert!(!text.contains("mock-") && !text.contains("seed-") && !text.contains("provider"));
    assert_eq!(f.count("auth_sessions", f.id).await, 0);
    // Raw inserts are also guarded by the migration, not just Store.
    for statement in [
        "INSERT INTO app_private.ai_usage (user_id,usage_day,kind,used) VALUES ($1,CURRENT_DATE,'helper',1)",
        "INSERT INTO app_private.student_plans (user_id,semester_id,plan,revision) VALUES ($1,'winter-2026','{}',1)",
        "INSERT INTO app_private.auth_sessions (user_id,token_hash,encrypted_tokens,access_expires_at,expires_at) VALUES ($1,'\\x01','\\x02',now(),now())",
    ] {
        let error = sqlx::query(statement).bind(f.id).execute(&f.pool).await.unwrap_err();
        assert_eq!(error.as_database_error().unwrap().code().as_deref(), Some("42501"));
    }
    f.cleanup().await;
}

async fn wait_for_deletion_advisory_lock(pool: &PgPool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname = current_database()
                 AND wait_event_type = 'Lock' AND wait_event = 'advisory'
                 AND query LIKE 'SELECT pg_advisory_xact_lock(hashtextextended%')",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            if waiting {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("deletion must be waiting on the writer's advisory lock");
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL"]
async fn deletion_overrides_repeatable_read_default_before_waiting_for_writer() {
    // Bootstrap migrations on the ordinary pool, then exercise the real handler
    // on a separate pool whose every connection defaults to REPEATABLE READ.
    let bootstrap = database().await;
    let url = std::env::var("TEST_DATABASE_URL").unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(12)
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::query("SET default_transaction_isolation = 'repeatable read'")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    let f = Fixture::with_pool(pool).await;
    let default: String = sqlx::query_scalar("SHOW default_transaction_isolation")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(default, "repeatable read");
    // Keep existing sessions for authentication, but make plan/usage rows new
    // inserts invisible to any snapshot taken before the writer commits.
    for table in ["student_plans", "ai_usage"] {
        sqlx::query(&format!(
            "DELETE FROM app_private.{table} WHERE user_id = $1"
        ))
        .bind(f.id)
        .execute(&f.pool)
        .await
        .unwrap();
    }
    let mut writer = f.pool.begin().await.unwrap();
    sqlx::query("SELECT app_private.lock_account_write($1)")
        .bind(f.id)
        .execute(&mut *writer)
        .await
        .unwrap();
    sqlx::query("INSERT INTO app_private.student_plans (user_id,semester_id,plan,revision) VALUES ($1,'winter-2026',$2,1)")
        .bind(f.id).bind(serde_json::to_value(plan()).unwrap()).execute(&mut *writer).await.unwrap();
    sqlx::query("INSERT INTO app_private.ai_usage (user_id,usage_day,kind,used) VALUES ($1,CURRENT_DATE,'helper',1)")
        .bind(f.id).execute(&mut *writer).await.unwrap();
    sqlx::query("INSERT INTO app_private.auth_sessions (user_id,token_hash,encrypted_tokens,access_expires_at,expires_at)
        VALUES ($1,$2,$3,now()+interval '1 hour',now()+interval '2 hours')")
        .bind(f.id).bind(Uuid::new_v4().as_bytes().to_vec()).bind(vec![1u8; 32])
        .execute(&mut *writer).await.unwrap();
    let release = async {
        wait_for_deletion_advisory_lock(&bootstrap).await;
        writer.commit().await.unwrap();
    };
    let (response, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(f.delete(), release)
    })
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert!(!response.headers().contains_key("set-cookie"));
    for table in ["student_plans", "ai_usage", "auth_sessions"] {
        assert_eq!(
            f.count(table, f.id).await,
            0,
            "late writer left rows in {table}"
        );
    }
    assert_eq!(f.count("account_deletion_requests", f.id).await, 1);
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL"]
async fn delayed_deletion_does_not_clear_another_accounts_new_login_cookie() {
    let a = Fixture::new().await;
    let b = Fixture::new().await;
    let mut writer = a.pool.begin().await.unwrap();
    sqlx::query("SELECT app_private.lock_account_write($1)")
        .bind(a.id)
        .execute(&mut *writer)
        .await
        .unwrap();
    let login_b_while_a_waits = async {
        wait_for_deletion_advisory_lock(&a.pool).await;
        let response = b
            .request(
                "/auth/local/login",
                Method::POST,
                r#"{"email":"owner@example.test","password":"mock-password"}"#,
                b.headers(),
            )
            .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let cookie = response.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let mut headers = b.headers();
        headers.insert("cookie", cookie.parse().unwrap());
        assert_eq!(
            b.state
                .auth
                .authenticate(&headers, &Method::GET)
                .await
                .unwrap()
                .user_id,
            b.id
        );
        writer.commit().await.unwrap();
        headers
    };
    let (response, b_headers) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(a.delete(), login_b_while_a_waits)
    })
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    // No response cookie mutation can overwrite B's newly installed cookie.
    assert!(!response.headers().contains_key("set-cookie"));
    assert_eq!(
        b.state
            .auth
            .authenticate(&b_headers, &Method::GET)
            .await
            .unwrap()
            .user_id,
        b.id
    );
    assert!(matches!(
        a.state.auth.authenticate(&a.headers(), &Method::GET).await,
        Err(error::AppError::Unauthorized)
    ));
    assert_eq!(a.count("auth_sessions", a.id).await, 0);
    a.cleanup().await;
    b.cleanup().await;
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL"]
async fn export_keeps_one_repeatable_read_snapshot_across_tables() {
    let f = Fixture::new().await;
    let mut writer = f.pool.begin().await.unwrap();
    sqlx::query("SELECT app_private.lock_account_write($1)")
        .bind(f.id)
        .execute(&mut *writer)
        .await
        .unwrap();
    // Let export read plans, then block its usage SELECT until both tables change.
    sqlx::query("LOCK TABLE app_private.ai_usage IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *writer)
        .await
        .unwrap();
    let export = f.request("/api/account/export", Method::GET, "", f.headers());
    let change = async {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let waiting: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname = current_database()
                     AND wait_event_type = 'Lock' AND query LIKE 'SELECT usage_day, kind, used%')")
                    .fetch_one(&f.pool).await.unwrap();
                if waiting {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        sqlx::query("UPDATE app_private.student_plans SET revision = 2 WHERE user_id = $1")
            .bind(f.id)
            .execute(&mut *writer)
            .await
            .unwrap();
        sqlx::query("UPDATE app_private.ai_usage SET used = 2 WHERE user_id = $1")
            .bind(f.id)
            .execute(&mut *writer)
            .await
            .unwrap();
        writer.commit().await.unwrap();
    };
    let (response, ()) = tokio::join!(export, change);
    assert_eq!(response.status(), StatusCode::OK);
    let value = body(response).await;
    assert_eq!(value["plans"][0]["revision"], 1);
    assert_eq!(value["ai_usage"][0]["used"], 1);
    let current = body(
        f.request("/api/account/export", Method::GET, "", f.headers())
            .await,
    )
    .await;
    assert_eq!(current["plans"][0]["revision"], 2);
    assert_eq!(current["ai_usage"][0]["used"], 2);
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL"]
async fn refresh_row_update_does_not_invert_deletion_lock_order() {
    let f = Fixture::new().await;
    let mut refresh = f.pool.begin().await.unwrap();
    sqlx::query(
        "SELECT token_hash FROM app_private.auth_sessions WHERE token_hash = $1 FOR UPDATE",
    )
    .bind(&f.hash)
    .fetch_one(&mut *refresh)
    .await
    .unwrap();
    let deletion = f.delete();
    let finish_refresh = async {
        // Wait until deletion owns the advisory lock and is blocked on this row.
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let waiting: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname = current_database()
                     AND wait_event_type = 'Lock' AND query = 'DELETE FROM app_private.auth_sessions WHERE user_id = $1')")
                    .fetch_one(&f.pool).await.unwrap();
                if waiting { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        sqlx::query("UPDATE app_private.auth_sessions SET access_expires_at = now() + interval '1 hour' WHERE token_hash = $1")
            .bind(&f.hash).execute(&mut *refresh).await.unwrap();
        refresh.commit().await.unwrap();
    };
    let (response, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(deletion, finish_refresh)
    })
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(f.count("auth_sessions", f.id).await, 0);
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL"]
async fn successful_interactive_login_records_current_time() {
    let f = Fixture::new().await;
    let response = f
        .request(
            "/auth/local/login",
            Method::POST,
            r#"{"email":"owner@example.test","password":"mock-password"}"#,
            f.headers(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let cookie = response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    let mut headers = f.headers();
    headers.insert("cookie", cookie.parse().unwrap());
    // Read CSRF through the normal same-origin /api/me endpoint.
    let response = f.request("/api/me", Method::GET, "", headers.clone()).await;
    let me = body(response).await;
    headers.insert(
        "x-csrf-token",
        me["csrf_token"].as_str().unwrap().parse().unwrap(),
    );
    f.state.auth.require_recent(&headers).await.unwrap();
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires disposable TEST_DATABASE_URL"]
async fn concurrent_writes_and_deletion_use_advisory_before_row_lock() {
    let f = Fixture::new().await;
    // Hold a real server-ordered write transaction: deletion must wait, not deadlock.
    let mut writer = f.pool.begin().await.unwrap();
    sqlx::query("SELECT app_private.lock_account_write($1)")
        .bind(f.id)
        .execute(&mut *writer)
        .await
        .unwrap();
    sqlx::query("UPDATE app_private.student_plans SET revision = revision + 1 WHERE user_id = $1")
        .bind(f.id)
        .execute(&mut *writer)
        .await
        .unwrap();
    sqlx::query("UPDATE app_private.ai_usage SET used = used + 1 WHERE user_id = $1")
        .bind(f.id)
        .execute(&mut *writer)
        .await
        .unwrap();
    let deletion = f.delete();
    let release = async {
        tokio::time::sleep(Duration::from_millis(150)).await;
        writer.commit().await.unwrap();
    };
    let (response, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(deletion, release)
    })
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(f.count("student_plans", f.id).await, 0);
    assert_eq!(f.count("ai_usage", f.id).await, 0);
    f.cleanup().await;

    // Opposite order: an in-flight handler that authenticated before deletion
    // cannot recreate app data after the deletion commits.
    let f = Fixture::new().await;
    let mut deleting = f.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text,0))")
        .bind(f.id.to_string())
        .execute(&mut *deleting)
        .await
        .unwrap();
    sqlx::query("INSERT INTO app_private.account_deletion_requests (user_id) VALUES ($1)")
        .bind(f.id)
        .execute(&mut *deleting)
        .await
        .unwrap();
    for table in ["student_plans", "ai_usage", "auth_sessions"] {
        sqlx::query(&format!(
            "DELETE FROM app_private.{table} WHERE user_id = $1"
        ))
        .bind(f.id)
        .execute(&mut *deleting)
        .await
        .unwrap();
    }
    let p = plan();
    let save = f.state.store.save_plan(f.id, 0, &p);
    let quota = f
        .state
        .store
        .consume_quota(f.id, QuotaKind::Image, Utc::now().date_naive(), 3);
    let release = async {
        tokio::time::sleep(Duration::from_millis(150)).await;
        deleting.commit().await.unwrap();
    };
    let (save, quota, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(save, quota, release)
    })
    .await
    .unwrap();
    assert!(matches!(save, Err(error::AppError::Forbidden)));
    assert!(matches!(quota, Err(error::AppError::Forbidden)));
    assert_eq!(f.count("student_plans", f.id).await, 0);
    assert_eq!(f.count("ai_usage", f.id).await, 0);
    f.cleanup().await;
}
