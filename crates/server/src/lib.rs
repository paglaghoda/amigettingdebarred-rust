pub mod account;
pub mod ai;
pub mod auth;
pub mod config;
pub mod db;
pub mod error;
mod frontend;
pub mod ops;

use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    extract::{ConnectInfo, DefaultBodyLimit, Path, Request, State},
    http::{HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{any, get, post},
    Json, Router,
};
use chrono::Utc;
use debarred_domain::{time::campus_date, Calendar, CheckpointProjection, Engine, Plan};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgPool;
use tokio::sync::Mutex;
use tower_http::timeout::TimeoutLayer;
use tracing::Instrument;
use uuid::Uuid;

use crate::{
    ai::AiService,
    auth::{AuthService, Authenticated},
    config::Config,
    db::{QuotaKind, Store},
    error::{AppError, Result},
};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub store: Store,
    pub auth: AuthService,
    pub ai: AiService,
    pub calendar: Arc<Calendar>,
}

impl AppState {
    pub fn new(config: Config, pool: PgPool) -> Result<Self> {
        config.validate()?;
        let config = Arc::new(config);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(4)
            .user_agent("debarred-server/0.1")
            .build()
            .map_err(|_| AppError::Internal)?;
        Ok(Self {
            auth: AuthService::new(pool.clone(), client.clone(), config.clone()),
            ai: AiService::new(client, config.clone()),
            config,
            store: Store::new(pool),
            calendar: Arc::new(Calendar::winter_2026()?),
        })
    }
}

pub fn router(state: AppState) -> Router {
    router_with_ops(state, ops::OpsConfig::default())
}

pub fn router_with_ops(state: AppState, ops_config: ops::OpsConfig) -> Router {
    let auth_limiter = Arc::new(AuthLimiter {
        clients: Mutex::new(BTreeMap::new()),
        trusted_proxies: state.config.trusted_proxy_ips.iter().copied().collect(),
        limit: state.config.auth_requests_per_minute,
    });
    let routes = Router::new()
        .merge(frontend::routes())
        .route("/api/runtime", get(runtime))
        .route(
            "/health/live",
            get(|| async { Json(json!({"status": "ok"})) }),
        )
        .route("/health/ready", get(ready))
        .route("/api/semesters", get(semesters))
        .route("/api/semesters/{semester_id}", get(semester))
        .route("/api/plans/{semester_id}", get(load_plan).put(save_plan))
        .route("/api/projections", post(preview))
        .route("/api/quota", get(quota))
        .merge(auth::routes())
        .merge(ai::routes())
        .merge(account::routes())
        .fallback(|| async { AppError::NotFound })
        .layer(DefaultBodyLimit::max(128 * 1024))
        .layer(middleware::from_fn_with_state(auth_limiter, limit_auth));
    ops::protect(routes, ops_config)
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(30),
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            response_policy,
        ))
        .with_state(state)
}

async fn runtime(State(state): State<AppState>) -> Result<Json<Value>> {
    let local =
        url::Url::parse(&state.config.public_origin).is_ok_and(|url| config::is_loopback(&url));
    runtime_json(
        if local { "local" } else { "production" },
        true,
        state.config.local_auth_enabled,
        state.config.ai.is_some(),
    )
}

fn runtime_json(mode: &str, auth: bool, local_password: bool, ai: bool) -> Result<Json<Value>> {
    Ok(Json(json!({"mode": mode, "auth_available": auth,
        "local_password_enabled": local_password, "ai_enabled": ai,
        "today": Some(campus_date(Utc::now())?),
        "support_email": debarred_web::sanitize_support_email(std::env::var("SUPPORT_EMAIL").ok().as_deref()),
        "policy_version": "2026-10-26"})))
}

/// Credential-free preview: no AppState, database, provider client, or private writes.
pub fn preview_router() -> Router {
    let calendar = Arc::new(Calendar::winter_2026().expect("bundled calendar is valid"));
    let list_calendar = calendar.clone();
    let mut router = Router::new()
        .merge(frontend::routes())
        .route("/api/runtime", get(|| async { runtime_json("preview", false, false, false) }))
        .route("/api/semesters", get(move || {
            let calendar = list_calendar.clone();
            async move {
                let config = calendar.config();
                Json(json!({"semesters": [{"id": config.id, "label": config.label, "revision": config.revision}]}))
            }
        }))
        .route("/api/semesters/{semester_id}", get(move |Path(id): Path<String>| {
            let calendar = calendar.clone();
            async move {
                if id != calendar.config().id {
                    return Err(AppError::BadRequest("unsupported semester".into()));
                }
                Ok(Json(json!({"config": calendar.config(), "days": calendar.days()})))
            }
        }))
        .route("/api/me", get(|| async { AppError::Unauthorized }));
    for path in [
        "/api/plans",
        "/api/plans/{semester_id}",
        "/api/projections",
        "/api/quota",
        "/api/helper",
        "/api/timetable/import",
        "/api/account/export",
        "/api/account/deletion",
        "/auth/login",
        "/auth/callback",
        "/auth/local/login",
        "/auth/logout",
    ] {
        router = router.route(
            path,
            any(|| async { AppError::Unavailable("service unavailable in preview mode") }),
        );
    }
    frontend::with_preview_policy(router.fallback(|| async { AppError::NotFound }))
}

/// Checks the required schema, permissions, and deletion guards without reading
/// personal rows. The same gate runs at startup and on readiness probes.
pub async fn verify_storage(pool: &PgPool) -> Result<()> {
    sqlx::query(
        "SELECT p.revision, s.authenticated_at, u.used, d.requested_at, o.expires_at,
               r.plan_storage_sha256, b.applied_at
        FROM app_private.student_plans p, app_private.auth_sessions s,
             app_private.ai_usage u, app_private.account_deletion_requests d,
             app_private.oauth_flows o, app_private.migration_rows r,
             app_private.migration_batches b LIMIT 0",
    )
    .execute(pool)
    .await?;
    let guard_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_trigger t
        JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = 'app_private' AND t.tgenabled IN ('O', 'A')
          AND (c.relname, t.tgname) IN (
            ('student_plans', 'student_plans_account_guard'),
            ('ai_usage', 'ai_usage_account_guard'),
            ('auth_sessions', 'auth_sessions_account_guard'))",
    )
    .fetch_one(pool)
    .await?;
    let audit_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_class c
        JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 'app_private'
        AND c.relkind = 'r' AND c.relname IN ('migration_batches', 'migration_rows')",
    )
    .fetch_one(pool)
    .await?;
    let helper_valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_proc
        WHERE oid = to_regprocedure('app_private.lock_account_write(uuid)') AND prosecdef)
        AND has_function_privilege(current_user, 'app_private.lock_account_write(uuid)', 'EXECUTE')")
        .fetch_one(pool).await?;
    let erase_allowed: bool = sqlx::query_scalar(
        "SELECT has_table_privilege(current_user, 'app_private.migration_rows', 'DELETE')",
    )
    .fetch_one(pool)
    .await?;
    if guard_count != 3 || audit_count != 2 || !helper_valid || !erase_allowed {
        return Err(AppError::Unavailable(
            "database schema or account guards are not ready; apply migrations and runtime grants",
        ));
    }
    Ok(())
}

async fn ready(State(state): State<AppState>) -> Result<Json<Value>> {
    verify_storage(state.store.pool()).await?;
    Ok(Json(json!({"status": "ready"})))
}

async fn semesters(State(state): State<AppState>) -> Json<Value> {
    let config = state.calendar.config();
    Json(
        json!({"semesters": [{"id": config.id, "label": config.label, "revision": config.revision}]}),
    )
}

async fn semester(State(state): State<AppState>, Path(id): Path<String>) -> Result<Json<Value>> {
    check_semester(&state, &id)?;
    Ok(Json(
        json!({"config": state.calendar.config(), "days": state.calendar.days()}),
    ))
}

fn check_semester(state: &AppState, id: &str) -> Result<()> {
    if id != state.calendar.config().id {
        return Err(AppError::BadRequest("unsupported semester".into()));
    }
    Ok(())
}

async fn load_plan(
    State(state): State<AppState>,
    Authenticated(user): Authenticated,
    Path(id): Path<String>,
) -> Result<Json<db::StoredPlan>> {
    check_semester(&state, &id)?;
    state
        .store
        .load_plan(user.user_id, &id)
        .await?
        .map(Json)
        .ok_or(AppError::NotFound)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavePlanRequest {
    pub expected_revision: i64,
    pub plan: Plan,
}

async fn save_plan(
    State(state): State<AppState>,
    Authenticated(user): Authenticated,
    Path(id): Path<String>,
    Json(request): Json<SavePlanRequest>,
) -> Result<Json<db::StoredPlan>> {
    check_semester(&state, &id)?;
    if request.plan.semester_id != id {
        return Err(AppError::BadRequest(
            "plan semester does not match route".into(),
        ));
    }
    Engine::new(&state.calendar, &request.plan)?;
    state
        .store
        .save_plan(user.user_id, request.expected_revision, &request.plan)
        .await
        .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewRequest {
    pub plan: Plan,
}

async fn preview(
    State(state): State<AppState>,
    Authenticated(_user): Authenticated,
    Json(request): Json<PreviewRequest>,
) -> Result<Json<Value>> {
    let engine = Engine::new(&state.calendar, &request.plan)?;
    let projections = engine.project_all();
    let mut allowances = Vec::new();
    for projection in &projections {
        if let CheckpointProjection::Upcoming {
            checkpoint,
            courses,
        } = projection
        {
            for course in courses {
                allowances.push(json!({
                    "checkpoint": checkpoint.id, "course_code": course.course_code,
                    "allowance": engine.skip_allowance(&course.course_code, &checkpoint.id)?,
                }));
            }
        }
    }
    Ok(Json(json!({"as_of": request.plan.attendance.as_of,
        "projections": projections, "allowances": allowances, "leave": engine.leave_summary()})))
}

async fn quota(
    State(state): State<AppState>,
    Authenticated(user): Authenticated,
) -> Result<Json<Value>> {
    let day = campus_date(Utc::now())?;
    let (helper, image) = tokio::try_join!(
        state.store.read_quota(
            user.user_id,
            QuotaKind::Helper,
            day,
            state.config.helper_daily_limit
        ),
        state.store.read_quota(
            user.user_id,
            QuotaKind::Image,
            day,
            state.config.image_daily_limit
        ),
    )?;
    Ok(Json(
        json!({"day": day, "timezone": "Asia/Kolkata", "helper": helper, "image": image}),
    ))
}

async fn response_policy(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let id = Uuid::new_v4().to_string();
    let path = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|path| path.as_str().to_owned())
        .unwrap_or_else(|| "unmatched".into());
    // Never log raw URIs: OAuth callbacks contain one-use authorization codes.
    let span =
        tracing::info_span!("http", request_id = %id, method = %request.method(), route = %path);
    let start = Instant::now();
    let mut response = async {
        let response = next.run(request).await;
        tracing::info!(
            status = response.status().as_u16(),
            elapsed_ms = start.elapsed().as_millis(),
            "request completed"
        );
        response
    }
    .instrument(span)
    .await;
    frontend::security_headers(&mut response, state.config.secure_cookies);
    let headers = response.headers_mut();

    headers.insert(
        "x-request-id",
        HeaderValue::from_str(&id).expect("UUID header"),
    );
    response
}

struct LoginWindow {
    start: Instant,
    requests: u32,
}

struct AuthLimiter {
    clients: Mutex<BTreeMap<IpAddr, LoginWindow>>,
    trusted_proxies: BTreeSet<IpAddr>,
    limit: u32,
}

/// Forwarded addresses are accepted only from explicitly trusted TCP peers.
/// Multiple replicas still need an edge-level distributed limit.
async fn limit_auth(
    State(limiter): State<Arc<AuthLimiter>>,
    request: Request,
    next: Next,
) -> Response {
    if (matches!(*request.method(), Method::GET | Method::HEAD)
        && matches!(request.uri().path(), "/auth/login" | "/auth/callback"))
        || (*request.method() == Method::POST && request.uri().path() == "/auth/local/login")
    {
        let ip = match client_ip(&request, &limiter.trusted_proxies) {
            Ok(ip) => ip,
            Err(error) => return error.into_response(),
        };
        let mut limits = limiter.clients.lock().await;
        limits.retain(|_, value| value.start.elapsed() < Duration::from_secs(60));
        if limits.len() >= 4096 && !limits.contains_key(&ip) {
            return AppError::RateLimited.into_response();
        }
        let window = limits.entry(ip).or_insert(LoginWindow {
            start: Instant::now(),
            requests: 0,
        });
        if window.requests >= limiter.limit {
            return AppError::RateLimited.into_response();
        }
        window.requests += 1;
    }
    next.run(request).await
}

fn client_ip(request: &Request, trusted: &BTreeSet<IpAddr>) -> Result<IpAddr> {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip())
        .unwrap_or(IpAddr::from([127, 0, 0, 1]));
    if !trusted.contains(&peer) {
        return Ok(peer);
    }
    let mut headers = request.headers().get_all("x-forwarded-for").iter();
    let value = headers
        .next()
        .and_then(|header| header.to_str().ok())
        .filter(|value| value.len() <= 1024)
        .ok_or_else(|| {
            AppError::BadRequest("trusted proxy must provide a valid X-Forwarded-For".into())
        })?;
    if headers.next().is_some() {
        return Err(AppError::BadRequest("duplicate X-Forwarded-For".into()));
    }
    let addresses: Vec<IpAddr> = value
        .split(',')
        .take(17)
        .map(|value| {
            value
                .trim()
                .parse()
                .map_err(|_| AppError::BadRequest("invalid forwarded address".into()))
        })
        .collect::<Result<_>>()?;
    if addresses.is_empty() || addresses.len() > 16 {
        return Err(AppError::BadRequest(
            "invalid forwarded chain length".into(),
        ));
    }
    // The nearest untrusted hop is authoritative; ignore spoofable entries to its left.
    Ok(addresses
        .into_iter()
        .rev()
        .find(|ip| !trusted.contains(ip))
        .unwrap_or(peer))
}

/// Called by the executable on a bounded interval, never on the request path.
pub async fn cleanup_expired(store: &Store) -> Result<()> {
    sqlx::query("DELETE FROM app_private.oauth_flows WHERE expires_at < now()")
        .execute(store.pool())
        .await?;
    sqlx::query("DELETE FROM app_private.auth_sessions WHERE expires_at < now()")
        .execute(store.pool())
        .await?;
    sqlx::query("DELETE FROM app_private.ai_usage WHERE usage_day < CURRENT_DATE - 90")
        .execute(store.pool())
        .await?;
    Ok(())
}
