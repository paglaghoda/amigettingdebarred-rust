use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Utc};
use debarred_domain::{calendar::DEFAULT_SEMESTER_ID, load_semester};
use debarred_server::{
    ai::AiService,
    auth::{self, AuthService},
    config::Config,
    db::Store,
    error::AppError,
    AppState,
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{postgres::PgPoolOptions, PgPool, Row};
use tokio::{
    sync::{Notify, OnceCell, Semaphore},
    task::JoinHandle,
};
use tower::ServiceExt;
use uuid::Uuid;

static INITIALIZED: OnceCell<()> = OnceCell::const_new();

async fn database() -> PgPool {
    let url = std::env::var("TEST_DATABASE_URL")
        .expect("TEST_DATABASE_URL must point to a disposable PostgreSQL database");
    let pool = PgPoolOptions::new()
        .max_connections(12)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&url)
        .await
        .expect("connect to TEST_DATABASE_URL");
    INITIALIZED
        .get_or_init(|| async {
            sqlx::raw_sql(
                "CREATE SCHEMA IF NOT EXISTS auth;
            CREATE TABLE IF NOT EXISTS auth.users (id uuid PRIMARY KEY);",
            )
            .execute(&pool)
            .await
            .expect("bootstrap test auth.users");
            sqlx::migrate!("./migrations")
                .run(&pool)
                .await
                .expect("run migrations");
        })
        .await;
    pool
}

struct Recorded {
    grant: String,
    body: Value,
}

struct RefreshGate {
    entered: Notify,
    release: Semaphore,
}

#[derive(Clone)]
struct ProviderState {
    replies: Arc<Mutex<VecDeque<(StatusCode, String)>>>,
    requests: Arc<Mutex<Vec<Recorded>>>,
    first_refresh_gate: Arc<Mutex<Option<Arc<RefreshGate>>>>,
}

struct Provider {
    state: ProviderState,
    url: url::Url,
    task: JoinHandle<()>,
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Provider {
    async fn start() -> Self {
        let state = ProviderState {
            replies: Arc::new(Mutex::new(VecDeque::new())),
            requests: Arc::new(Mutex::new(Vec::new())),
            first_refresh_gate: Arc::new(Mutex::new(None)),
        };
        async fn token(
            State(state): State<ProviderState>,
            Query(query): Query<std::collections::HashMap<String, String>>,
            headers: HeaderMap,
            Json(body): Json<Value>,
        ) -> Response {
            assert_eq!(headers["apikey"], "test-anon-key");
            let grant = query.get("grant_type").unwrap().clone();
            let gate = if grant == "refresh_token" {
                state.first_refresh_gate.lock().unwrap().take()
            } else {
                None
            };
            state
                .requests
                .lock()
                .unwrap()
                .push(Recorded { grant, body });
            let (status, body) = state.replies.lock().unwrap().pop_front().unwrap_or((
                StatusCode::INTERNAL_SERVER_ERROR,
                "unexpected provider request".into(),
            ));
            if let Some(gate) = gate {
                gate.entered.notify_one();
                gate.release.acquire().await.unwrap().forget();
            }
            (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
        }
        let app = Router::new()
            .route("/auth/v1/token", post(token))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { state, url, task }
    }

    fn reply(&self, status: StatusCode, body: impl Into<String>) {
        self.state
            .replies
            .lock()
            .unwrap()
            .push_back((status, body.into()));
    }

    fn success(&self, user_id: Uuid, generation: usize) {
        self.reply(
            StatusCode::OK,
            json!({
                "access_token": format!("access-secret-{generation}"),
                "refresh_token": format!("refresh-secret-{generation}"),
                "expires_in": 3600,
                "expires_at": Utc::now().timestamp() + 3600,
                "user": {"id": user_id, "email": "student@example.test",
                    "user_metadata": {"full_name": "Test Student"}},
            })
            .to_string(),
        );
    }

    fn calls(&self) -> usize {
        self.state.requests.lock().unwrap().len()
    }

    fn block_first_refresh(&self) -> Arc<RefreshGate> {
        let gate = Arc::new(RefreshGate {
            entered: Notify::new(),
            release: Semaphore::new(0),
        });
        *self.state.first_refresh_gate.lock().unwrap() = Some(gate.clone());
        gate
    }
}

struct Fixture {
    pool: PgPool,
    provider: Provider,
    auth: AuthService,
    app: Router,
    server: Router,
    user_id: Uuid,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_local_auth(false).await
    }

    async fn with_local_auth(local_auth_enabled: bool) -> Self {
        let pool = database().await;
        let provider = Provider::start().await;
        let user_id = Uuid::new_v4();
        sqlx::query("INSERT INTO auth.users (id) VALUES ($1)")
            .bind(user_id)
            .execute(&pool)
            .await
            .unwrap();
        let config = Arc::new(Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            public_origin: "http://localhost:3000".into(),
            supabase_url: provider.url.clone(),
            supabase_anon_key: "test-anon-key".into(),
            database_url: String::new(),
            session_key: [7; 32],
            secure_cookies: false,
            local_auth_enabled,
            session_ttl_seconds: 7200,
            database_max_connections: 12,
            trusted_proxy_ips: Vec::new(),
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
        let auth = AuthService::new(pool.clone(), client.clone(), config.clone());
        let state = AppState {
            config: config.clone(),
            store: Store::new(pool.clone()),
            auth: auth.clone(),
            ai: AiService::new(client, config),
            calendar: Arc::new(load_semester(DEFAULT_SEMESTER_ID).unwrap()),
        };
        let app = auth::routes().with_state(state.clone());
        let server = debarred_server::router(state);
        Self {
            pool,
            provider,
            auth,
            app,
            server,
            user_id,
        }
    }

    async fn request(&self, method: Method, path: &str, headers: &HeaderMap) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap();
        *request.headers_mut() = headers.clone();
        self.app.clone().oneshot(request).await.unwrap()
    }

    async fn begin(&self) -> (String, String) {
        let response = self
            .request(Method::GET, "/auth/login", &HeaderMap::new())
            .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let location: url::Url = response.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(location.path(), "/auth/v1/authorize");
        let params: std::collections::HashMap<_, _> = location.query_pairs().into_owned().collect();
        assert_eq!(params["provider"], "google");
        assert_eq!(params["redirect_to"], "http://localhost:3000/auth/callback");
        assert_eq!(params["code_challenge_method"], "s256");
        let cookie = response_cookie(&response, "oauth-flow");
        let hash = hash_cookie(&cookie);
        let encrypted: Vec<u8> = sqlx::query_scalar(
            "SELECT encrypted_verifier FROM app_private.oauth_flows WHERE token_hash = $1",
        )
        .bind(hash)
        .fetch_one(&self.pool)
        .await
        .unwrap();
        assert!(encrypted.len() > 28);
        (cookie, params["code_challenge"].clone())
    }

    async fn login(&self) -> (String, String) {
        self.login_for(self.user_id).await
    }

    async fn login_for(&self, user_id: Uuid) -> (String, String) {
        let previous_calls = self.provider.calls();
        let (flow, challenge) = self.begin().await;
        self.provider.success(user_id, 1);
        let headers = cookie_headers(&flow);
        let response = self
            .request(Method::GET, "/auth/callback?code=valid-code", &headers)
            .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/setup");
        assert_flow_cleared(&response);
        let session = response_cookie(&response, "session");
        assert_eq!(
            URL_SAFE_NO_PAD
                .decode(session.split_once('=').unwrap().1)
                .unwrap()
                .len(),
            32
        );
        {
            let calls = self.provider.state.requests.lock().unwrap();
            let exchange = &calls[previous_calls];
            assert_eq!(exchange.grant, "pkce");
            assert_eq!(exchange.body["auth_code"], "valid-code");
            let verifier = exchange.body["code_verifier"].as_str().unwrap();
            assert_eq!(URL_SAFE_NO_PAD.decode(verifier).unwrap().len(), 64);
            assert_eq!(
                URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
                challenge
            );
        }
        let row = sqlx::query("SELECT encrypted_tokens, expires_at FROM app_private.auth_sessions WHERE token_hash = $1")
            .bind(hash_cookie(&session)).fetch_one(&self.pool).await.unwrap();
        let encrypted: Vec<u8> = row.get("encrypted_tokens");
        assert!(!encrypted
            .windows(b"refresh-secret".len())
            .any(|w| w == b"refresh-secret"));
        let expires_at: DateTime<Utc> = row.get("expires_at");
        assert!((expires_at - Utc::now()).num_seconds() >= 7190);
        assert!((expires_at - Utc::now()).num_seconds() <= 7200);
        let response = self
            .request(Method::GET, "/api/me", &cookie_headers(&session))
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let me = response_json(response).await;
        assert_eq!(me["user_id"], user_id.to_string());
        assert_eq!(me["email"], "student@example.test");
        assert_eq!(me["display_name"], "Test Student");
        assert_eq!(me.as_object().unwrap().len(), 4);
        let serialized = me.to_string();
        assert!(!serialized.contains("access-secret"));
        assert!(!serialized.contains("refresh-secret"));
        let csrf = me["csrf_token"].as_str().unwrap().to_owned();
        assert_eq!(
            self.provider.calls(),
            previous_calls + 1,
            "fresh sessions must not contact provider"
        );
        (session, csrf)
    }

    async fn near_expiry(&self, session: &str) {
        sqlx::query("UPDATE app_private.auth_sessions SET access_expires_at = now() + interval '10 seconds' WHERE token_hash = $1")
            .bind(hash_cookie(session)).execute(&self.pool).await.unwrap();
    }

    async fn exists(&self, session: &str) -> bool {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM app_private.auth_sessions WHERE token_hash = $1)",
        )
        .bind(hash_cookie(session))
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn cleanup(&self) {
        sqlx::query("DELETE FROM auth.users WHERE id = $1")
            .bind(self.user_id)
            .execute(&self.pool)
            .await
            .unwrap();
    }
}

fn hash_cookie(cookie: &str) -> Vec<u8> {
    Sha256::digest(cookie.split_once('=').unwrap().1.as_bytes()).to_vec()
}

fn cookie_headers(cookie: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::COOKIE, HeaderValue::from_str(cookie).unwrap());
    headers
}

fn mutation_headers(session: &str, csrf: &str) -> HeaderMap {
    let mut headers = cookie_headers(session);
    headers.insert(
        header::ORIGIN,
        HeaderValue::from_static("http://localhost:3000"),
    );
    headers.insert("x-csrf-token", HeaderValue::from_str(csrf).unwrap());
    headers
}

fn response_cookie(response: &Response, name: &str) -> String {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .find_map(|header| {
            let cookie = cookie::Cookie::parse(header.to_str().unwrap()).unwrap();
            (cookie.name() == name).then(|| format!("{}={}", cookie.name(), cookie.value()))
        })
        .unwrap_or_else(|| panic!("missing {name} cookie"))
}

fn assert_flow_cleared(response: &Response) {
    let cleared = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .any(|header| {
            let cookie = cookie::Cookie::parse(header.to_str().unwrap()).unwrap();
            cookie.name() == "oauth-flow"
                && cookie.value().is_empty()
                && cookie.max_age().unwrap().whole_seconds() == 0
        });
    assert!(cleared);
}

async fn response_json(response: Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn local_password_login_uses_provider_and_encrypted_real_sessions() {
    let f = Fixture::with_local_auth(true).await;
    let send = |body: Value, origin: &'static str| {
        let app = f.server.clone();
        async move {
            app.oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/auth/local/login")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ORIGIN, origin)
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    };
    let credentials = json!({"email":"student@example.test", "password":"local-test-password"});
    let response = send(credentials.clone(), "http://evil.example").await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(f.provider.calls(), 0);
    f.provider
        .reply(StatusCode::BAD_REQUEST, r#"{"error":"invalid_grant"}"#);
    let response = send(credentials.clone(), "http://localhost:3000").await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(!response.headers().contains_key(header::SET_COOKIE));
    f.provider.success(f.user_id, 1);
    let response = send(credentials.clone(), "http://localhost:3000").await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert!(!response.headers().contains_key(header::LOCATION));
    let session = response_cookie(&response, "session");
    assert!(response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .is_empty());
    {
        let calls = f.provider.state.requests.lock().unwrap();
        assert_eq!(calls.len(), 2);
        for call in calls.iter() {
            assert_eq!(call.grant, "password");
            assert_eq!(call.body, credentials);
        }
    }
    let row = sqlx::query(
        "SELECT user_id, encrypted_tokens FROM app_private.auth_sessions WHERE token_hash = $1",
    )
    .bind(hash_cookie(&session))
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(row.get::<Uuid, _>("user_id"), f.user_id);
    let encrypted: Vec<u8> = row.get("encrypted_tokens");
    for secret in ["access-secret", "refresh-secret", "local-test-password"] {
        assert!(!encrypted
            .windows(secret.len())
            .any(|bytes| bytes == secret.as_bytes()));
    }
    let response = f
        .request(Method::GET, "/api/me", &cookie_headers(&session))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let me = response_json(response).await;
    assert_eq!(me["user_id"], f.user_id.to_string());
    assert!(me["csrf_token"]
        .as_str()
        .is_some_and(|value| !value.is_empty()));
    let response = f
        .request(
            Method::POST,
            "/auth/logout",
            &mutation_headers(&session, me["csrf_token"].as_str().unwrap()),
        )
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        f.request(Method::GET, "/api/me", &cookie_headers(&session))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn pkce_login_is_browser_bound_single_use_and_sessions_are_local() {
    let f = Fixture::new().await;
    let (session, csrf) = f.login().await;
    let headers = cookie_headers(&session);
    let user = f.auth.authenticate(&headers, &Method::GET).await.unwrap();
    assert_eq!(user.user_id, f.user_id);
    assert_eq!(user.csrf_token, csrf);
    assert_eq!(f.provider.calls(), 1);
    let response = f.request(Method::GET, "/api/me", &HeaderMap::new()).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let (flow, _) = f.begin().await;
    let response = f
        .request(
            Method::GET,
            "/auth/callback?code=valid-code",
            &HeaderMap::new(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_flow_cleared(&response);
    // A missing/wrong browser cookie cannot consume the legitimate browser's flow.
    f.provider.success(f.user_id, 2);
    let response = f
        .request(
            Method::GET,
            "/auth/callback?code=valid-code",
            &cookie_headers(&flow),
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let replay = f
        .request(
            Method::GET,
            "/auth/callback?code=valid-code",
            &cookie_headers(&flow),
        )
        .await;
    assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
    assert_flow_cleared(&replay);
    assert_eq!(f.provider.calls(), 2);
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn callbacks_reject_expiry_provider_errors_unknown_parameters_and_replays() {
    let f = Fixture::new().await;
    let (flow, _) = f.begin().await;
    sqlx::query("UPDATE app_private.oauth_flows SET expires_at = now() - interval '1 second' WHERE token_hash = $1")
        .bind(hash_cookie(&flow)).execute(&f.pool).await.unwrap();
    let response = f
        .request(
            Method::GET,
            "/auth/callback?code=valid",
            &cookie_headers(&flow),
        )
        .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_flow_cleared(&response);
    for path in [
        "/auth/callback?error=access_denied&error_description=provider-secret",
        "/auth/callback?code=valid&code_verifier=attacker-controlled",
        "/auth/callback?code=first&code=second",
    ] {
        let (flow, _) = f.begin().await;
        let response = f.request(Method::GET, path, &cookie_headers(&flow)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_flow_cleared(&response);
        assert!(!response_json(response)
            .await
            .to_string()
            .contains("provider-secret"));
        let replay = f
            .request(
                Method::GET,
                "/auth/callback?code=valid",
                &cookie_headers(&flow),
            )
            .await;
        assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
    }
    assert_eq!(f.provider.calls(), 0);
    let (flow, _) = f.begin().await;
    f.provider
        .reply(StatusCode::BAD_REQUEST, r#"{"error":"provider-secret"}"#);
    let response = f
        .request(
            Method::GET,
            "/auth/callback?code=invalid",
            &cookie_headers(&flow),
        )
        .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_flow_cleared(&response);
    assert!(!response_json(response)
        .await
        .to_string()
        .contains("provider-secret"));
    let response = f
        .request(
            Method::GET,
            "/auth/callback?code=invalid",
            &cookie_headers(&flow),
        )
        .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(f.provider.calls(), 1);
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn mutations_require_csrf_and_exact_origin_logout_revokes_even_during_provider_outage() {
    let f = Fixture::new().await;
    let (session, csrf) = f.login().await;
    f.near_expiry(&session).await;
    let valid = mutation_headers(&session, &csrf);
    for missing in [header::ORIGIN.as_str(), "x-csrf-token"] {
        let mut invalid = valid.clone();
        invalid.remove(missing);
        assert!(matches!(
            f.auth.authenticate(&invalid, &Method::POST).await,
            Err(AppError::Forbidden)
        ));
        let response = f.request(Method::POST, "/auth/logout", &invalid).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(response.headers().get(header::SET_COOKIE).is_none());
        assert!(f.exists(&session).await);
    }
    for origin in ["null", "http://localhost:3000/", "https://evil.test"] {
        let mut invalid = valid.clone();
        invalid.insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
        assert!(matches!(
            f.auth.authenticate(&invalid, &Method::DELETE).await,
            Err(AppError::Forbidden)
        ));
    }
    let mut invalid = valid.clone();
    invalid.insert("x-csrf-token", HeaderValue::from_static("wrong"));
    assert!(matches!(
        f.auth.authenticate(&invalid, &Method::PATCH).await,
        Err(AppError::Forbidden)
    ));
    assert_eq!(f.provider.calls(), 1, "rejected mutations must not refresh");
    let response = f.request(Method::POST, "/auth/logout", &valid).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(response_cookie(&response, "session"), "session=");
    assert!(!f.exists(&session).await);
    assert_eq!(
        f.provider.calls(),
        1,
        "logout must not depend on upstream refresh"
    );
    assert!(matches!(
        f.auth
            .authenticate(&cookie_headers(&session), &Method::GET)
            .await,
        Err(AppError::Unauthorized)
    ));
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn fresh_sessions_authenticate_without_waiting_for_a_row_lock() {
    let f = Fixture::new().await;
    let (session, csrf) = f.login().await;
    let mut tx = f.pool.begin().await.unwrap();
    sqlx::query(
        "SELECT token_hash FROM app_private.auth_sessions WHERE token_hash = $1 FOR UPDATE",
    )
    .bind(hash_cookie(&session))
    .fetch_one(&mut *tx)
    .await
    .unwrap();

    // An unrelated writer holding this row must not serialize ordinary API reads.
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        f.request(Method::GET, "/api/me", &cookie_headers(&session)),
    )
    .await;
    tx.rollback().await.unwrap();
    let response = result.expect("fresh authentication must not acquire a session row lock");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_json(response).await["csrf_token"], csrf);
    assert_eq!(f.provider.calls(), 1);
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn same_session_refresh_waiters_leave_pool_available_for_fresh_users_and_readiness() {
    let f = Fixture::new().await;
    let (session, csrf) = f.login().await;
    let other_user = Uuid::new_v4();
    sqlx::query("INSERT INTO auth.users (id) VALUES ($1)")
        .bind(other_user)
        .execute(&f.pool)
        .await
        .unwrap();
    let (fresh_session, _) = f.login_for(other_user).await;
    f.near_expiry(&session).await;
    f.provider.success(f.user_id, 2);
    let gate = f.provider.block_first_refresh();

    let mut tasks = tokio::task::JoinSet::new();
    let auth = f.auth.clone();
    let headers = cookie_headers(&session);
    tasks.spawn(async move { auth.authenticate(&headers, &Method::GET).await });
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .expect("refresh must reach the blocked provider");

    let waiters = f.pool.options().get_max_connections() as usize * 2;
    let barrier = Arc::new(tokio::sync::Barrier::new(waiters + 1));
    for _ in 0..waiters {
        let auth = f.auth.clone();
        let headers = cookie_headers(&session);
        let barrier = barrier.clone();
        tasks.spawn(async move {
            barrier.wait().await;
            auth.authenticate(&headers, &Method::GET).await
        });
    }
    barrier.wait().await;
    // Let the burst reach its session lock while the provider is still blocked.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut fresh_request = Request::builder()
        .uri("/api/me")
        .body(Body::empty())
        .unwrap();
    *fresh_request.headers_mut() = cookie_headers(&fresh_session);
    let ready_request = Request::builder()
        .uri("/health/ready")
        .body(Body::empty())
        .unwrap();
    let (fresh_result, ready_result) = tokio::join!(
        tokio::time::timeout(
            Duration::from_secs(2),
            f.server.clone().oneshot(fresh_request)
        ),
        tokio::time::timeout(
            Duration::from_secs(2),
            f.server.clone().oneshot(ready_request)
        ),
    );
    let calls_while_blocked = f.provider.calls();
    // Always unblock before assertions so a regression cannot strand request tasks.
    gate.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(result) = tasks.join_next().await {
            let user = result.unwrap().unwrap();
            assert_eq!(user.user_id, f.user_id);
            assert_eq!(user.csrf_token, csrf);
        }
    })
    .await
    .expect("coalesced refresh waiters must finish");

    let fresh_response = fresh_result
        .expect("fresh users must retain DB access")
        .unwrap();
    let ready_response = ready_result
        .expect("readiness must retain DB access")
        .unwrap();
    assert_eq!(fresh_response.status(), StatusCode::OK);
    assert_eq!(
        response_json(fresh_response).await["user_id"],
        other_user.to_string()
    );
    assert_eq!(ready_response.status(), StatusCode::OK);
    assert_eq!(calls_while_blocked, 3);
    assert_eq!(
        f.provider.calls(),
        3,
        "the burst must share one refresh exchange"
    );
    sqlx::query("DELETE FROM auth.users WHERE id = $1")
        .bind(other_user)
        .execute(&f.pool)
        .await
        .unwrap();
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn logout_burst_during_blocked_refresh_preserves_fresh_users_and_readiness() {
    let f = Fixture::new().await;
    let (session, csrf) = f.login().await;
    let other_user = Uuid::new_v4();
    sqlx::query("INSERT INTO auth.users (id) VALUES ($1)")
        .bind(other_user)
        .execute(&f.pool)
        .await
        .unwrap();
    let (fresh_session, _) = f.login_for(other_user).await;
    f.near_expiry(&session).await;
    f.provider.success(f.user_id, 2);
    let gate = f.provider.block_first_refresh();
    let auth = f.auth.clone();
    let headers = cookie_headers(&session);
    let refresh = tokio::spawn(async move { auth.authenticate(&headers, &Method::GET).await });
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .expect("refresh must reach the blocked provider");

    let waiters = f.pool.options().get_max_connections() as usize * 2;
    let barrier = Arc::new(tokio::sync::Barrier::new(waiters + 1));
    let mut logouts = tokio::task::JoinSet::new();
    for _ in 0..waiters {
        let app = f.app.clone();
        let headers = mutation_headers(&session, &csrf);
        let barrier = barrier.clone();
        logouts.spawn(async move {
            let mut request = Request::builder()
                .method(Method::POST)
                .uri("/auth/logout")
                .body(Body::empty())
                .unwrap();
            *request.headers_mut() = headers;
            barrier.wait().await;
            app.oneshot(request).await.unwrap()
        });
    }
    barrier.wait().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut fresh_request = Request::builder()
        .uri("/api/me")
        .body(Body::empty())
        .unwrap();
    *fresh_request.headers_mut() = cookie_headers(&fresh_session);
    let ready_request = Request::builder()
        .uri("/health/ready")
        .body(Body::empty())
        .unwrap();
    let (fresh_result, ready_result) = tokio::join!(
        tokio::time::timeout(
            Duration::from_secs(2),
            f.server.clone().oneshot(fresh_request)
        ),
        tokio::time::timeout(
            Duration::from_secs(2),
            f.server.clone().oneshot(ready_request)
        ),
    );
    gate.release.add_permits(1);
    let refreshed_user = refresh.await.unwrap().unwrap();
    assert_eq!(refreshed_user.user_id, f.user_id);
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(result) = logouts.join_next().await {
            let response = result.unwrap();
            assert!(matches!(
                response.status(),
                StatusCode::NO_CONTENT | StatusCode::UNAUTHORIZED
            ));
            assert_eq!(response_cookie(&response, "session"), "session=");
        }
    })
    .await
    .expect("logout waiters must finish after refresh releases the session");
    let fresh_response = fresh_result
        .expect("logout waiters must leave DB access for fresh users")
        .unwrap();
    assert_eq!(fresh_response.status(), StatusCode::OK);
    assert_eq!(
        response_json(fresh_response).await["user_id"],
        other_user.to_string()
    );
    assert_eq!(
        ready_result
            .expect("logout waiters must leave readiness responsive")
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert!(
        !f.exists(&session).await,
        "logout must remain authoritative"
    );
    assert_eq!(
        f.provider.calls(),
        3,
        "logout must not exchange provider credentials"
    );
    sqlx::query("DELETE FROM auth.users WHERE id = $1")
        .bind(other_user)
        .execute(&f.pool)
        .await
        .unwrap();
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn global_refresh_admission_rejects_excess_sessions_without_consuming_the_pool() {
    let f = Fixture::new().await;
    let mut sessions = Vec::new();
    for _ in 0..4 {
        let (session, _) = f.login().await;
        f.near_expiry(&session).await;
        sessions.push(session);
    }
    // The fixture's 12-connection budget permits at most three refresh owners.
    let mut tasks = tokio::task::JoinSet::new();
    let mut gates = Vec::new();
    for session in &sessions[..3] {
        f.provider.success(f.user_id, 2);
        let gate = f.provider.block_first_refresh();
        let auth = f.auth.clone();
        let headers = cookie_headers(session);
        tasks.spawn(async move { auth.authenticate(&headers, &Method::GET).await });
        tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
            .await
            .expect("admitted refresh must reach provider");
        gates.push(gate);
    }
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        f.request(Method::GET, "/api/me", &cookie_headers(&sessions[3])),
    )
    .await;
    for gate in gates {
        gate.release.add_permits(1);
    }
    while let Some(result) = tasks.join_next().await {
        result.unwrap().unwrap();
    }
    assert_eq!(
        response
            .expect("excess refresh must fail promptly")
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(f.provider.calls(), 7);
    assert!(
        f.exists(&sessions[3]).await,
        "admission rejection must preserve credentials"
    );
    f.provider.success(f.user_id, 2);
    f.auth
        .authenticate(&cookie_headers(&sessions[3]), &Method::GET)
        .await
        .unwrap();
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn cross_replica_refresh_row_lock_wait_is_bounded_and_preserves_the_session() {
    let f = Fixture::new().await;
    let (session, _) = f.login().await;
    f.near_expiry(&session).await;
    let mut tx = f.pool.begin().await.unwrap();
    sqlx::query(
        "SELECT token_hash FROM app_private.auth_sessions WHERE token_hash = $1 FOR UPDATE",
    )
    .bind(hash_cookie(&session))
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(4),
        f.auth.authenticate(&cookie_headers(&session), &Method::GET),
    )
    .await;
    tx.rollback().await.unwrap();
    assert!(matches!(
        result.expect("row lock wait must be bounded"),
        Err(AppError::Unavailable(_))
    ));
    assert!(f.exists(&session).await);
    assert_eq!(
        f.provider.calls(),
        1,
        "DB lock timeout must not exchange credentials"
    );
    f.provider.success(f.user_id, 2);
    f.auth
        .authenticate(&cookie_headers(&session), &Method::GET)
        .await
        .unwrap();
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn cross_replica_logout_delete_wait_is_bounded_and_can_be_retried() {
    let f = Fixture::new().await;
    let (session, csrf) = f.login().await;
    let mut tx = f.pool.begin().await.unwrap();
    sqlx::query(
        "SELECT token_hash FROM app_private.auth_sessions WHERE token_hash = $1 FOR UPDATE",
    )
    .bind(hash_cookie(&session))
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    let headers = mutation_headers(&session, &csrf);
    let result = tokio::time::timeout(
        Duration::from_secs(4),
        f.request(Method::POST, "/auth/logout", &headers),
    )
    .await;
    tx.rollback().await.unwrap();
    let response = result.expect("logout DELETE lock wait must be bounded");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(response.headers().get(header::SET_COOKIE).is_none());
    assert!(f.exists(&session).await);
    let retry = f.request(Method::POST, "/auth/logout", &headers).await;
    assert_eq!(retry.status(), StatusCode::NO_CONTENT);
    assert!(!f.exists(&session).await);
    assert_eq!(f.provider.calls(), 1);
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn expired_session_rejection_does_not_wait_for_a_locked_row_or_delete_it() {
    let f = Fixture::new().await;
    let (session, csrf) = f.login().await;
    sqlx::query("UPDATE app_private.auth_sessions SET expires_at = now() - interval '1 second' WHERE token_hash = $1")
        .bind(hash_cookie(&session)).execute(&f.pool).await.unwrap();
    let mut tx = f.pool.begin().await.unwrap();
    sqlx::query(
        "SELECT token_hash FROM app_private.auth_sessions WHERE token_hash = $1 FOR UPDATE",
    )
    .bind(hash_cookie(&session))
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    let headers = mutation_headers(&session, &csrf);
    let (auth_result, logout_result) = tokio::join!(
        tokio::time::timeout(
            Duration::from_secs(1),
            f.request(Method::GET, "/api/me", &headers)
        ),
        tokio::time::timeout(
            Duration::from_secs(1),
            f.request(Method::POST, "/auth/logout", &headers)
        ),
    );
    tx.rollback().await.unwrap();
    assert_eq!(
        auth_result
            .expect("expired authentication must not DELETE a locked row")
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        logout_result
            .expect("expired logout must not DELETE a locked row")
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(
        f.exists(&session).await,
        "expired rows are left to maintenance"
    );
    assert_eq!(f.provider.calls(), 1);
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn session_user_precondition_rejects_stale_context_before_refresh_and_allows_matching_refresh(
) {
    let f = Fixture::new().await;
    let (session, _) = f.login().await;
    f.near_expiry(&session).await;
    let mut headers = cookie_headers(&session);
    headers.insert(
        "x-session-user",
        Uuid::new_v4().to_string().parse().unwrap(),
    );
    assert!(matches!(
        f.auth.authenticate(&headers, &Method::GET).await,
        Err(AppError::Unauthorized)
    ));
    assert_eq!(
        f.provider.calls(),
        1,
        "stale context must not trigger provider refresh"
    );
    headers.insert("x-session-user", f.user_id.to_string().parse().unwrap());
    f.provider.success(f.user_id, 2);
    let user = f.auth.authenticate(&headers, &Method::GET).await.unwrap();
    assert_eq!(user.user_id, f.user_id);
    assert_eq!(f.provider.calls(), 2);
    let response = f
        .request(Method::GET, "/api/me", &cookie_headers(&session))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn concurrent_refreshes_serialize_rotation_and_do_not_extend_absolute_expiry() {
    let f = Fixture::new().await;
    let (session, csrf) = f.login().await;
    let original: DateTime<Utc> = sqlx::query_scalar(
        "SELECT expires_at FROM app_private.auth_sessions WHERE token_hash = $1",
    )
    .bind(hash_cookie(&session))
    .fetch_one(&f.pool)
    .await
    .unwrap();
    f.near_expiry(&session).await;
    f.provider.success(f.user_id, 2);
    let mut tasks = tokio::task::JoinSet::new();
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    for _ in 0..8 {
        let auth = f.auth.clone();
        let headers = cookie_headers(&session);
        let barrier = barrier.clone();
        tasks.spawn(async move {
            barrier.wait().await;
            auth.authenticate(&headers, &Method::GET).await
        });
    }
    while let Some(result) = tasks.join_next().await {
        let user = result.unwrap().unwrap();
        assert_eq!(user.user_id, f.user_id);
        assert_eq!(user.csrf_token, csrf);
    }
    assert_eq!(f.provider.calls(), 2);
    {
        let calls = f.provider.state.requests.lock().unwrap();
        assert_eq!(calls[1].grant, "refresh_token");
        assert_eq!(calls[1].body["refresh_token"], "refresh-secret-1");
    }
    f.near_expiry(&session).await;
    f.provider.success(f.user_id, 3);
    f.auth
        .authenticate(&cookie_headers(&session), &Method::GET)
        .await
        .unwrap();
    assert_eq!(
        f.provider.state.requests.lock().unwrap()[2].body["refresh_token"],
        "refresh-secret-2"
    );
    let after: DateTime<Utc> = sqlx::query_scalar(
        "SELECT expires_at FROM app_private.auth_sessions WHERE token_hash = $1",
    )
    .bind(hash_cookie(&session))
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(original, after);
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn provider_failures_preserve_sessions_but_invalid_refresh_and_wrong_user_revoke() {
    for status in [
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::BAD_REQUEST,
        StatusCode::UNAUTHORIZED,
        StatusCode::OK,
    ] {
        let f = Fixture::new().await;
        let (session, _) = f.login().await;
        f.near_expiry(&session).await;
        if status == StatusCode::OK {
            f.provider.success(Uuid::new_v4(), 2);
        } else {
            f.provider.reply(status, r#"{"error":"provider-secret"}"#);
        }
        let result = f
            .auth
            .authenticate(&cookie_headers(&session), &Method::GET)
            .await;
        let preserve = status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS;
        if preserve {
            assert!(matches!(result, Err(AppError::Unavailable(_))));
            assert!(f.exists(&session).await);
            f.provider.success(f.user_id, 3);
            f.auth
                .authenticate(&cookie_headers(&session), &Method::GET)
                .await
                .unwrap();
            assert_eq!(
                f.provider.state.requests.lock().unwrap()[2].body["refresh_token"],
                "refresh-secret-1"
            );
        } else {
            assert!(matches!(result, Err(AppError::Unauthorized)));
            assert!(!f.exists(&session).await);
        }
        f.cleanup().await;
    }
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn expired_sessions_and_tampered_or_swapped_ciphertexts_fail_closed() {
    let f = Fixture::new().await;
    let (session, _) = f.login().await;
    let hash = hash_cookie(&session);
    let packed: Vec<u8> = sqlx::query_scalar(
        "SELECT encrypted_tokens FROM app_private.auth_sessions WHERE token_hash = $1",
    )
    .bind(&hash)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    let mut corrupt = packed.clone();
    corrupt[12] ^= 1;
    sqlx::query("UPDATE app_private.auth_sessions SET encrypted_tokens = $2 WHERE token_hash = $1")
        .bind(&hash)
        .bind(corrupt)
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(matches!(
        f.auth
            .authenticate(&cookie_headers(&session), &Method::GET)
            .await,
        Err(AppError::Unauthorized)
    ));
    let mut other_raw = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut other_raw);
    let other = format!("session={}", URL_SAFE_NO_PAD.encode(other_raw));
    sqlx::query("INSERT INTO app_private.auth_sessions (token_hash,user_id,encrypted_tokens,access_expires_at,expires_at)
        VALUES ($1,$2,$3,now() + interval '1 hour',now() + interval '2 hours')")
        .bind(hash_cookie(&other)).bind(f.user_id).bind(&packed).execute(&f.pool).await.unwrap();
    assert!(matches!(
        f.auth
            .authenticate(&cookie_headers(&other), &Method::GET)
            .await,
        Err(AppError::Unauthorized)
    ));
    sqlx::query("UPDATE app_private.auth_sessions SET encrypted_tokens = $2, expires_at = now() - interval '1 second' WHERE token_hash = $1")
        .bind(&hash).bind(packed).execute(&f.pool).await.unwrap();
    assert!(matches!(
        f.auth
            .authenticate(&cookie_headers(&session), &Method::GET)
            .await,
        Err(AppError::Unauthorized)
    ));
    assert!(
        f.exists(&session).await,
        "maintenance, not authentication, removes expired rows"
    );
    assert_eq!(f.provider.calls(), 1);
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn oversized_malformed_redirect_and_invalid_expiry_provider_responses_fail_closed() {
    for (status, body) in [
        (StatusCode::OK, "x".repeat(65_537)),
        (StatusCode::OK, "not json".into()),
        (StatusCode::FOUND, String::new()),
        (
            StatusCode::OK,
            json!({"access_token":"a", "refresh_token":"r", "expires_in":0,
            "user":{"id":Uuid::new_v4()}})
            .to_string(),
        ),
    ] {
        let f = Fixture::new().await;
        let (flow, _) = f.begin().await;
        f.provider.reply(status, body);
        let response = f
            .request(
                Method::GET,
                "/auth/callback?code=valid",
                &cookie_headers(&flow),
            )
            .await;
        assert!(matches!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE | StatusCode::UNAUTHORIZED
        ));
        assert_flow_cleared(&response);
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM app_private.auth_sessions WHERE user_id = $1")
                .bind(f.user_id)
                .fetch_one(&f.pool)
                .await
                .unwrap();
        assert_eq!(count, 0);
        f.cleanup().await;
    }
}
