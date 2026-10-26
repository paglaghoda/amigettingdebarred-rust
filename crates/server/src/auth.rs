use std::{
    collections::HashMap,
    sync::{Arc, Weak},
    time::Duration,
};

use aes_gcm::{
    aead::{Aead, Payload},
    Aes256Gcm, KeyInit, Nonce,
};
use axum::{
    extract::{DefaultBodyLimit, FromRequest, FromRequestParts, RawQuery, State},
    http::{header, request::Parts, HeaderMap, HeaderValue, Method, StatusCode},
    middleware,
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, TimeDelta, Utc};
use cookie::{Cookie, SameSite};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex, Semaphore};
use uuid::Uuid;

use crate::{
    config::Config,
    error::{AppError, Result},
    AppState,
};

const FLOW_TTL: i64 = 600;
const REFRESH_EARLY_SECONDS: i64 = 60;
const MAX_PROVIDER_BODY: usize = 64 * 1024;
const MAX_REFRESH_LOCKS: usize = 4096;

type RefreshLocks = Mutex<HashMap<Vec<u8>, Weak<Mutex<()>>>>;

/// The supplied client must have redirects disabled. Requests also have a hard timeout.
#[derive(Clone)]
pub struct AuthService {
    pool: PgPool,
    client: reqwest::Client,
    config: Arc<Config>,
    refresh_locks: Arc<RefreshLocks>,
    refresh_admission: Arc<Semaphore>,
}

// Deliberately no Debug: this type contains the browser's CSRF secret.
pub struct AuthUser {
    pub user_id: Uuid,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub csrf_token: String,
}

pub struct Authenticated(pub AuthUser);

impl FromRequestParts<AppState> for Authenticated {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self> {
        state
            .auth
            .authenticate(&parts.headers, &parts.method)
            .await
            .map(Self)
    }
}

#[derive(Serialize, Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: String,
    csrf_token: String,
}

#[derive(Deserialize)]
struct ProviderTokens {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
    expires_at: Option<i64>,
    user: ProviderUser,
}

#[derive(Deserialize)]
struct ProviderUser {
    id: Uuid,
    email: Option<String>,
    #[serde(default)]
    user_metadata: serde_json::Value,
}

impl ProviderTokens {
    fn expiry(&self) -> Result<DateTime<Utc>> {
        let now = Utc::now();
        // Supabase access tokens are short-lived. Reject absurd/overflowing lifetimes.
        if self.access_token.is_empty()
            || self.refresh_token.is_empty()
            || !(1..=604_800).contains(&self.expires_in)
        {
            return Err(AppError::Unauthorized);
        }
        let relative = now + TimeDelta::seconds(self.expires_in);
        let expiry = match self.expires_at {
            Some(seconds) => DateTime::from_timestamp(seconds, 0)
                .filter(|expiry| *expiry > now && *expiry <= relative + TimeDelta::seconds(60))
                .ok_or(AppError::Unauthorized)?,
            None => relative,
        };
        Ok(expiry)
    }
}

impl AuthService {
    pub fn new(pool: PgPool, client: reqwest::Client, config: Arc<Config>) -> Self {
        let refresh_limit = (config.database_max_connections / 4).max(1) as usize;
        Self {
            pool,
            client,
            config,
            refresh_locks: Arc::new(Mutex::new(HashMap::new())),
            refresh_admission: Arc::new(Semaphore::new(refresh_limit)),
        }
    }

    async fn refresh_lock(locks: &RefreshLocks, hash: &[u8]) -> Result<Arc<Mutex<()>>> {
        let mut locks = locks.lock().await;
        if let Some(lock) = locks.get(hash).and_then(Weak::upgrade) {
            return Ok(lock);
        }
        // Weak entries do not retain idle sessions. Prune on capacity pressure so
        // unrelated refreshes cannot grow the registry without bound.
        if locks.len() >= MAX_REFRESH_LOCKS {
            locks.retain(|_, lock| lock.strong_count() > 0);
            if locks.len() >= MAX_REFRESH_LOCKS {
                return Err(AppError::RateLimited);
            }
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(hash.to_vec(), Arc::downgrade(&lock));
        Ok(lock)
    }

    pub async fn authenticate(&self, headers: &HeaderMap, method: &Method) -> Result<AuthUser> {
        self.authenticate_inner(headers, method, true).await
    }

    /// Interactive sign-in age is independent of access-token refresh and expiry.
    pub async fn require_recent(&self, headers: &HeaderMap) -> Result<()> {
        let user = self
            .authenticate_inner(headers, &Method::POST, false)
            .await?;
        let hash = token_hash(&read_cookie(headers, self.session_cookie_name())?)?;
        let recent: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM app_private.auth_sessions
             WHERE token_hash = $1 AND user_id = $2 AND expires_at > clock_timestamp()
               AND authenticated_at >= clock_timestamp() - interval '5 minutes'
               AND authenticated_at <= clock_timestamp())",
        )
        .bind(hash)
        .bind(user.user_id)
        .fetch_one(&self.pool)
        .await?;
        if !recent {
            return Err(AppError::BadRequest(
                "Sign out and sign in again within five minutes before deleting your account."
                    .into(),
            ));
        }
        Ok(())
    }

    pub fn clear_session_cookie(&self, response: &mut Response) {
        set_cookie(
            response,
            self.session_cookie_name(),
            "",
            0,
            self.config.secure_cookies,
        );
    }

    async fn authenticate_inner(
        &self,
        headers: &HeaderMap,
        method: &Method,
        refresh: bool,
    ) -> Result<AuthUser> {
        let token = read_cookie(headers, self.session_cookie_name())?;
        let hash = token_hash(&token)?;
        let row = sqlx::query(
            "SELECT user_id, email, display_name, encrypted_tokens, access_expires_at, expires_at
             FROM app_private.auth_sessions WHERE token_hash = $1",
        )
        .bind(&hash)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::Unauthorized)?;
        let expires_at: DateTime<Utc> = row.try_get("expires_at")?;
        if expires_at <= Utc::now() {
            // Maintenance removes expired rows. An unauthenticated request must
            // not acquire a connection for a DELETE blocked by an in-flight refresh.
            return Err(AppError::Unauthorized);
        }
        let user_id: Uuid = row.try_get("user_id")?;
        verify_session_user(headers, user_id)?;
        let tokens = self.session_tokens(&row, &hash, headers, method)?;
        let access_expires_at: DateTime<Utc> = row.try_get("access_expires_at")?;
        if !refresh || access_expires_at > Utc::now() + TimeDelta::seconds(REFRESH_EARLY_SECONDS) {
            let user = AuthUser {
                user_id,
                email: row.try_get("email")?,
                display_name: row.try_get("display_name")?,
                csrf_token: tokens.csrf_token,
            };
            if expires_at <= Utc::now() {
                return Err(AppError::Unauthorized);
            }
            return Ok(user);
        }

        // The initial SELECT has released its connection. Same-session waiters
        // must coalesce before admission or acquiring any transaction connection.
        let session_lock = Self::refresh_lock(&self.refresh_locks, &hash).await?;
        let _session_guard = session_lock.lock().await;
        let _refresh_permit = self
            .refresh_admission
            .try_acquire()
            .map_err(|_| AppError::RateLimited)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET LOCAL lock_timeout = '2s'")
            .execute(&mut *tx)
            .await?;
        // Re-read after acquiring the row lock: another request may
        // have rotated the credentials or logout may have removed the session.
        let row = sqlx::query(
            "SELECT user_id, email, display_name, encrypted_tokens, access_expires_at, expires_at
             FROM app_private.auth_sessions WHERE token_hash = $1 FOR UPDATE",
        )
        .bind(&hash)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::Unauthorized)?;
        let expires_at: DateTime<Utc> = row.try_get("expires_at")?;
        if expires_at <= Utc::now() {
            sqlx::query("DELETE FROM app_private.auth_sessions WHERE token_hash = $1")
                .bind(&hash)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Err(AppError::Unauthorized);
        }
        let user_id: Uuid = row.try_get("user_id")?;
        verify_session_user(headers, user_id)?;
        let mut tokens = self.session_tokens(&row, &hash, headers, method)?;
        let access_expires_at: DateTime<Utc> = row.try_get("access_expires_at")?;
        if refresh && access_expires_at <= Utc::now() + TimeDelta::seconds(REFRESH_EARLY_SECONDS) {
            let replacement = self
                .exchange(
                    "refresh_token",
                    serde_json::json!({
                        "refresh_token": tokens.refresh_token,
                    }),
                )
                .await;
            let validated = replacement.and_then(|value| {
                let expiry = value.expiry()?;
                if value.user.id != user_id {
                    return Err(AppError::Unauthorized);
                }
                Ok((value, expiry))
            });
            let (replacement, expiry) = match validated {
                Ok(value) => value,
                Err(AppError::Unauthorized) => {
                    sqlx::query("DELETE FROM app_private.auth_sessions WHERE token_hash = $1")
                        .bind(&hash)
                        .execute(&mut *tx)
                        .await?;
                    tx.commit().await?;
                    return Err(AppError::Unauthorized);
                }
                Err(error) => return Err(error),
            };
            tokens.access_token = replacement.access_token;
            tokens.refresh_token = replacement.refresh_token;
            let encrypted = encrypt(
                &self.config.session_key,
                &hash,
                &serde_json::to_vec(&tokens).map_err(|_| AppError::Internal)?,
            )?;
            sqlx::query(
                "UPDATE app_private.auth_sessions SET encrypted_tokens = $2, access_expires_at = $3
                 WHERE token_hash = $1",
            )
            .bind(&hash)
            .bind(encrypted)
            .bind(expiry)
            .execute(&mut *tx)
            .await?;
        }
        // A slow provider exchange must not carry authentication past the absolute deadline.
        if expires_at <= Utc::now() {
            sqlx::query("DELETE FROM app_private.auth_sessions WHERE token_hash = $1")
                .bind(&hash)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Err(AppError::Unauthorized);
        }
        let user = AuthUser {
            user_id,
            email: row.try_get("email")?,
            display_name: row.try_get("display_name")?,
            csrf_token: tokens.csrf_token,
        };
        tx.commit().await?;
        Ok(user)
    }

    fn session_tokens(
        &self,
        row: &sqlx::postgres::PgRow,
        hash: &[u8],
        headers: &HeaderMap,
        method: &Method,
    ) -> Result<Tokens> {
        let ciphertext: Vec<u8> = row.try_get("encrypted_tokens")?;
        let tokens: Tokens =
            serde_json::from_slice(&decrypt(&self.config.session_key, hash, &ciphertext)?)
                .map_err(|_| AppError::Unauthorized)?;
        verify_mutation(
            headers,
            method,
            &self.config.public_origin,
            &tokens.csrf_token,
        )?;
        Ok(tokens)
    }

    async fn exchange(&self, grant: &str, body: serde_json::Value) -> Result<ProviderTokens> {
        let mut url = self
            .config
            .supabase_url
            .join("/auth/v1/token")
            .map_err(|_| AppError::Internal)?;
        url.query_pairs_mut().append_pair("grant_type", grant);
        let mut response = self
            .client
            .post(url)
            .header("apikey", &self.config.supabase_anon_key)
            .timeout(Duration::from_secs(20))
            .json(&body)
            .send()
            .await
            .map_err(|_| AppError::Unavailable("authentication provider unavailable"))?;
        let status = response.status();
        if status == StatusCode::BAD_REQUEST || status == StatusCode::UNAUTHORIZED {
            return Err(AppError::Unauthorized);
        }
        if !status.is_success() {
            return Err(AppError::Unavailable("authentication provider unavailable"));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_PROVIDER_BODY as u64)
        {
            return Err(AppError::Unavailable(
                "invalid authentication provider response",
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| AppError::Unavailable("authentication provider unavailable"))?
        {
            if chunk.len() > MAX_PROVIDER_BODY - bytes.len() {
                return Err(AppError::Unavailable(
                    "invalid authentication provider response",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| AppError::Unavailable("invalid authentication provider response"))
    }

    fn session_cookie_name(&self) -> &'static str {
        if self.config.secure_cookies {
            "__Host-session"
        } else {
            "session"
        }
    }

    fn flow_cookie_name(&self) -> &'static str {
        if self.config.secure_cookies {
            "__Host-oauth-flow"
        } else {
            "oauth-flow"
        }
    }

    async fn begin_login(&self) -> Result<Response> {
        let flow = random_token(32);
        let hash = token_hash(&flow)?;
        let verifier = random_token(64);
        let encrypted = encrypt(&self.config.session_key, &hash, verifier.as_bytes())?;
        sqlx::query(
            "INSERT INTO app_private.oauth_flows (token_hash, encrypted_verifier, expires_at)
             VALUES ($1, $2, $3)",
        )
        .bind(&hash)
        .bind(encrypted)
        .bind(Utc::now() + TimeDelta::seconds(FLOW_TTL))
        .execute(&self.pool)
        .await?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut url = self
            .config
            .supabase_url
            .join("/auth/v1/authorize")
            .map_err(|_| AppError::Internal)?;
        url.query_pairs_mut()
            .append_pair("provider", "google")
            .append_pair(
                "redirect_to",
                &format!("{}/auth/callback", self.config.public_origin),
            )
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "s256");
        let mut response = Redirect::to(url.as_str()).into_response();
        set_cookie(
            &mut response,
            self.flow_cookie_name(),
            &flow,
            FLOW_TTL,
            self.config.secure_cookies,
        );
        Ok(response)
    }

    async fn finish_login(&self, headers: &HeaderMap, query: Option<String>) -> Result<Response> {
        let flow = read_cookie(headers, self.flow_cookie_name())?;
        let hash = token_hash(&flow)?;
        // Consume even failed callbacks. No verifier or flow can be replayed after an exchange.
        let row = sqlx::query(
            "DELETE FROM app_private.oauth_flows WHERE token_hash = $1
             RETURNING encrypted_verifier, expires_at",
        )
        .bind(&hash)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::Unauthorized)?;
        let expires_at: DateTime<Utc> = row.try_get("expires_at")?;
        if expires_at <= Utc::now() {
            return Err(AppError::Unauthorized);
        }
        let code = callback_code(query.as_deref().unwrap_or(""))?;
        let encrypted: Vec<u8> = row.try_get("encrypted_verifier")?;
        let verifier = String::from_utf8(decrypt(&self.config.session_key, &hash, &encrypted)?)
            .map_err(|_| AppError::Unauthorized)?;
        let provider = self
            .exchange(
                "pkce",
                serde_json::json!({
                    "auth_code": code, "code_verifier": verifier,
                }),
            )
            .await?;
        self.persist_tokens(provider, Redirect::to("/setup").into_response())
            .await
    }

    async fn persist_tokens(
        &self,
        provider: ProviderTokens,
        mut response: Response,
    ) -> Result<Response> {
        let access_expiry = provider.expiry()?;
        let session = random_token(32);
        let session_hash = token_hash(&session)?;
        let tokens = Tokens {
            access_token: provider.access_token,
            refresh_token: provider.refresh_token,
            csrf_token: random_token(32),
        };
        let encrypted = encrypt(
            &self.config.session_key,
            &session_hash,
            &serde_json::to_vec(&tokens).map_err(|_| AppError::Internal)?,
        )?;
        let display_name = provider
            .user
            .user_metadata
            .get("full_name")
            .or_else(|| provider.user.user_metadata.get("name"))
            .and_then(|value| value.as_str())
            .map(str::to_owned);
        let mut tx = self.pool.begin().await?;
        // Check pending under the deletion lock, before insertion (and its FK locks).
        // The trigger repeats the guard to cover races and other insert callers.
        crate::db::lock_account_write(&mut tx, provider.user.id).await?;
        sqlx::query(
            "INSERT INTO app_private.auth_sessions
             (token_hash, user_id, email, display_name, encrypted_tokens, access_expires_at, expires_at, authenticated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, clock_timestamp())",
        ).bind(&session_hash).bind(provider.user.id).bind(provider.user.email)
            .bind(display_name).bind(encrypted).bind(access_expiry)
            .bind(Utc::now() + TimeDelta::seconds(self.config.session_ttl_seconds))
            .execute(&mut *tx).await?;
        tx.commit().await?;
        set_cookie(
            &mut response,
            self.session_cookie_name(),
            &session,
            self.config.session_ttl_seconds,
            self.config.secure_cookies,
        );
        Ok(response)
    }

    async fn revoke(&self, headers: &HeaderMap) -> Result<()> {
        // Local revocation is authoritative; do not depend on provider availability or refresh.
        self.authenticate_inner(headers, &Method::POST, false)
            .await?;
        let hash = token_hash(&read_cookie(headers, self.session_cookie_name())?)?;
        // Share refresh coordination: logout waiters must not hold pool connections
        // while a refresh owns this session, including after its absolute deadline.
        let session_lock = Self::refresh_lock(&self.refresh_locks, &hash).await?;
        let _session_guard = session_lock.lock().await;
        let _refresh_permit = self
            .refresh_admission
            .try_acquire()
            .map_err(|_| AppError::RateLimited)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET LOCAL lock_timeout = '2s'")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM app_private.auth_sessions WHERE token_hash = $1")
            .bind(hash)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/login", get(login))
        .route(
            "/auth/local/login",
            post(local_login).layer(DefaultBodyLimit::max(8 * 1024)),
        )
        .route("/auth/callback", get(callback))
        .route("/auth/logout", post(logout))
        .route("/api/me", get(me))
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

// Deliberately not Debug: request credentials must never enter logs.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalCredentials {
    email: String,
    password: String,
}

impl LocalCredentials {
    fn validate(&self) -> Result<()> {
        if self.email.is_empty()
            || self.email.len() > 320
            || self.email.trim() != self.email
            || !self.email.contains('@')
            || self.email.chars().any(char::is_control)
            || self.password.is_empty()
            || self.password.len() > 1024
        {
            return Err(AppError::BadRequest(
                "invalid email or password bounds".into(),
            ));
        }
        Ok(())
    }
}

async fn local_login(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: axum::extract::Request,
) -> Result<Response> {
    if !state.config.local_auth_enabled {
        return Err(AppError::NotFound);
    }
    verify_origin(&headers, &state.config.public_origin)?;
    let Json(credentials) = match Json::<LocalCredentials>::from_request(request, &state).await {
        Ok(credentials) => credentials,
        Err(rejection) => return Ok(rejection.into_response()),
    };
    credentials.validate()?;
    let provider = state
        .auth
        .exchange(
            "password",
            serde_json::json!({
                "email": credentials.email, "password": credentials.password,
            }),
        )
        .await?;
    state
        .auth
        .persist_tokens(provider, StatusCode::NO_CONTENT.into_response())
        .await
}

async fn login(State(state): State<AppState>) -> Result<Response> {
    state.auth.begin_login().await
}

async fn callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let mut response = match state.auth.finish_login(&headers, query).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    };
    set_cookie(
        &mut response,
        state.auth.flow_cookie_name(),
        "",
        0,
        state.auth.config.secure_cookies,
    );
    response
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let result = state.auth.revoke(&headers).await;
    // A stale tab must not clear the shared cookie belonging to the newly logged-in user.
    let clear = result.is_ok()
        || (!headers.contains_key("x-session-user")
            && matches!(&result, Err(AppError::Unauthorized)));
    let mut response = match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error.into_response(),
    };
    if clear {
        state.auth.clear_session_cookie(&mut response);
    }
    response
}

async fn me(Authenticated(user): Authenticated) -> Json<serde_json::Value> {
    // Only /api/me deliberately discloses the CSRF token to same-origin JavaScript.
    Json(serde_json::json!({
        "user_id": user.user_id, "email": user.email,
        "display_name": user.display_name, "csrf_token": user.csrf_token,
    }))
}

fn callback_code(query: &str) -> Result<String> {
    if query.len() > 8192 {
        return Err(AppError::Unauthorized);
    }
    let mut code = None;
    let mut failed = false;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "code" if code.is_none() && !value.is_empty() => code = Some(value.into_owned()),
            "error" | "error_code" | "error_description" => failed = true,
            _ => return Err(AppError::Unauthorized),
        }
    }
    if failed {
        return Err(AppError::Unauthorized);
    }
    code.ok_or(AppError::Unauthorized)
}

// This is a stale-page precondition, never an alternate source of identity.
// Check it for reads, writes, and logout before any refresh or private operation.
fn verify_session_user(headers: &HeaderMap, user_id: Uuid) -> Result<()> {
    let mut values = headers.get_all("x-session-user").iter();
    if let Some(value) = values.next() {
        let expected = value
            .to_str()
            .ok()
            .and_then(|value| Uuid::parse_str(value).ok());
        if values.next().is_some() || expected != Some(user_id) {
            return Err(AppError::Unauthorized);
        }
    }
    Ok(())
}

fn verify_mutation(headers: &HeaderMap, method: &Method, origin: &str, csrf: &str) -> Result<()> {
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return Ok(());
    }
    verify_origin(headers, origin)?;
    let csrf_values = headers.get_all("x-csrf-token");
    let mut values = csrf_values.iter();
    let provided = values.next().ok_or(AppError::Forbidden)?;
    if values.next().is_some() || !bool::from(csrf.as_bytes().ct_eq(provided.as_bytes())) {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

fn verify_origin(headers: &HeaderMap, origin: &str) -> Result<()> {
    let origins = headers.get_all(header::ORIGIN);
    let mut origins = origins.iter();
    if origins.next().and_then(|value| value.to_str().ok()) != Some(origin)
        || origins.next().is_some()
    {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

fn random_token(bytes: usize) -> String {
    let mut value = vec![0; bytes];
    OsRng.fill_bytes(&mut value);
    URL_SAFE_NO_PAD.encode(value)
}

fn token_hash(token: &str) -> Result<Vec<u8>> {
    let decoded = URL_SAFE_NO_PAD
        .decode(token)
        .map_err(|_| AppError::Unauthorized)?;
    if decoded.len() != 32 {
        return Err(AppError::Unauthorized);
    }
    Ok(Sha256::digest(token.as_bytes()).to_vec())
}

fn read_cookie(headers: &HeaderMap, name: &str) -> Result<String> {
    let mut found = None;
    for header in headers.get_all(header::COOKIE) {
        let header = header.to_str().map_err(|_| AppError::Unauthorized)?;
        for part in header.split(';') {
            let cookie = Cookie::parse(part.trim()).map_err(|_| AppError::Unauthorized)?;
            if cookie.name() == name {
                if found.is_some() {
                    return Err(AppError::Unauthorized);
                }
                found = Some(cookie.value().to_owned());
            }
        }
    }
    found.ok_or(AppError::Unauthorized)
}

fn set_cookie(response: &mut Response, name: &'static str, value: &str, ttl: i64, secure: bool) {
    let cookie = Cookie::build((name.to_owned(), value.to_owned()))
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Lax)
        .path("/")
        .max_age(cookie::time::Duration::seconds(ttl))
        .build();
    // All cookie values are locally generated base64url, not provider-controlled.
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie.to_string()).expect("valid generated cookie"),
    );
}

fn encrypt(key: &[u8; 32], binding: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| AppError::Internal)?;
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: binding,
            },
        )
        .map_err(|_| AppError::Internal)?;
    let mut packed = nonce.to_vec();
    packed.extend_from_slice(&ciphertext);
    Ok(packed)
}

fn decrypt(key: &[u8; 32], binding: &[u8], packed: &[u8]) -> Result<Vec<u8>> {
    if packed.len() < 28 {
        return Err(AppError::Unauthorized);
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| AppError::Internal)?;
    cipher
        .decrypt(
            Nonce::from_slice(&packed[..12]),
            Payload {
                msg: &packed[12..],
                aad: binding,
            },
        )
        .map_err(|_| AppError::Unauthorized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn refresh_lock_registry_shares_live_locks_and_prunes_at_its_bound() {
        let registry = Arc::new(Mutex::new(HashMap::new()));
        let shared = registry.clone();
        let mut live = Vec::new();
        for index in 0..MAX_REFRESH_LOCKS {
            live.push(
                AuthService::refresh_lock(&registry, &index.to_be_bytes())
                    .await
                    .unwrap(),
            );
        }
        let same = AuthService::refresh_lock(&shared, &0usize.to_be_bytes())
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&same, &live[0]));
        assert_eq!(registry.lock().await.len(), MAX_REFRESH_LOCKS);
        assert!(matches!(
            AuthService::refresh_lock(&registry, b"another-session").await,
            Err(AppError::RateLimited)
        ));
        drop(same);
        drop(live);
        let replacement = AuthService::refresh_lock(&registry, b"another-session")
            .await
            .unwrap();
        assert_eq!(registry.lock().await.len(), 1);
        assert_eq!(
            Arc::strong_count(&replacement),
            1,
            "registry must retain only weak references"
        );
    }

    #[test]
    fn session_user_is_an_optional_single_valid_matching_uuid() {
        let user_id = Uuid::new_v4();
        let mut headers = HeaderMap::new();
        assert!(verify_session_user(&headers, user_id).is_ok());
        headers.insert("x-session-user", user_id.to_string().parse().unwrap());
        assert!(verify_session_user(&headers, user_id).is_ok());
        assert!(matches!(
            verify_session_user(&headers, Uuid::new_v4()),
            Err(AppError::Unauthorized)
        ));
        headers.append("x-session-user", user_id.to_string().parse().unwrap());
        assert!(matches!(
            verify_session_user(&headers, user_id),
            Err(AppError::Unauthorized)
        ));
        for value in [
            "".to_owned(),
            "invalid".to_owned(),
            format!("{user_id}, {user_id}"),
        ] {
            headers.insert("x-session-user", value.parse().unwrap());
            assert!(matches!(
                verify_session_user(&headers, user_id),
                Err(AppError::Unauthorized)
            ));
        }
        headers.insert("x-session-user", HeaderValue::from_bytes(&[0xff]).unwrap());
        assert!(matches!(
            verify_session_user(&headers, user_id),
            Err(AppError::Unauthorized)
        ));
    }

    #[test]
    fn local_credentials_have_strict_byte_bounds_and_no_extra_fields() {
        let credentials = |email: &str, password: &str| LocalCredentials {
            email: email.into(),
            password: password.into(),
        };
        assert!(credentials("dev@example.test", "password")
            .validate()
            .is_ok());
        assert!(
            credentials(&format!("{}@b", "a".repeat(318)), &"p".repeat(1024))
                .validate()
                .is_ok()
        );
        for (email, password) in [
            ("", "p"),
            ("a@b", ""),
            (" a@b", "p"),
            ("a@b\n", "p"),
            ("not-an-email", "p"),
        ] {
            assert!(credentials(email, password).validate().is_err());
        }
        assert!(credentials(&format!("{}@b", "a".repeat(319)), "p")
            .validate()
            .is_err());
        assert!(credentials("a@b", &"p".repeat(1025)).validate().is_err());
        assert!(
            serde_json::from_value::<LocalCredentials>(serde_json::json!({
                "email":"a@b", "password":"p", "user_id": Uuid::new_v4(),
            }))
            .is_err()
        );
    }

    #[test]
    fn encryption_is_random_authenticated_and_bound_to_row() {
        let key = [42; 32];
        let packed = encrypt(&key, b"row one", b"refresh secret").unwrap();
        assert_ne!(
            packed,
            encrypt(&key, b"row one", b"refresh secret").unwrap()
        );
        assert_eq!(
            decrypt(&key, b"row one", &packed).unwrap(),
            b"refresh secret"
        );
        assert!(decrypt(&key, b"row two", &packed).is_err());
        assert!(decrypt(&[43; 32], b"row one", &packed).is_err());
        for index in 0..packed.len() {
            let mut corrupt = packed.clone();
            corrupt[index] ^= 1;
            assert!(decrypt(&key, b"row one", &corrupt).is_err());
        }
        for length in 0..28 {
            assert!(decrypt(&key, b"row one", &packed[..length]).is_err());
        }
    }

    #[test]
    fn cookies_are_host_only_secure_and_clearable() {
        for secure in [false, true] {
            let name = if secure { "__Host-session" } else { "session" };
            let mut response = StatusCode::OK.into_response();
            set_cookie(&mut response, name, &random_token(32), 3600, secure);
            let cookie =
                Cookie::parse(response.headers()[header::SET_COOKIE].to_str().unwrap()).unwrap();
            assert_eq!(cookie.name(), name);
            assert_eq!(cookie.http_only(), Some(true));
            assert_eq!(cookie.secure().unwrap_or(false), secure);
            assert_eq!(cookie.same_site(), Some(SameSite::Lax));
            assert_eq!(cookie.path(), Some("/"));
            assert_eq!(cookie.domain(), None);
            assert_eq!(cookie.max_age().unwrap().whole_seconds(), 3600);
            let mut response = StatusCode::OK.into_response();
            set_cookie(&mut response, name, "", 0, secure);
            let cookie =
                Cookie::parse(response.headers()[header::SET_COOKIE].to_str().unwrap()).unwrap();
            assert_eq!(cookie.max_age().unwrap().whole_seconds(), 0);
        }
    }

    #[test]
    fn mutations_require_exact_origin_and_csrf() {
        let origin = "https://example.com";
        let mut headers = HeaderMap::new();
        assert!(verify_mutation(&headers, &Method::GET, origin, "secret").is_ok());
        for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            assert!(verify_mutation(&headers, &method, origin, "secret").is_err());
        }
        headers.insert("x-csrf-token", HeaderValue::from_static("secret"));
        for invalid in [
            "null",
            "https://example.com/",
            "https://example.com.evil",
            "http://example.com",
        ] {
            headers.insert(header::ORIGIN, HeaderValue::from_str(invalid).unwrap());
            assert!(verify_mutation(&headers, &Method::POST, origin, "secret").is_err());
        }
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://example.com"),
        );
        assert!(verify_mutation(&headers, &Method::POST, origin, "secret").is_ok());
        assert!(verify_mutation(&headers, &Method::POST, origin, "wrong").is_err());
        headers.append("x-csrf-token", HeaderValue::from_static("secret"));
        assert!(verify_mutation(&headers, &Method::POST, origin, "secret").is_err());
        headers.remove("x-csrf-token");
        assert!(verify_mutation(&headers, &Method::POST, origin, "secret").is_err());
    }

    #[test]
    fn provider_expiry_and_credentials_must_be_valid() {
        let make = || ProviderTokens {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_in: 3600,
            expires_at: None,
            user: ProviderUser {
                id: Uuid::new_v4(),
                email: None,
                user_metadata: serde_json::Value::Null,
            },
        };
        assert!(make().expiry().is_ok());
        for expires_in in [i64::MIN, -1, 0, 604_801, i64::MAX] {
            let mut value = make();
            value.expires_in = expires_in;
            assert!(value.expiry().is_err());
        }
        for expires_at in [
            i64::MIN,
            Utc::now().timestamp() - 1,
            Utc::now().timestamp() + 7200,
            i64::MAX,
        ] {
            let mut value = make();
            value.expires_at = Some(expires_at);
            assert!(value.expiry().is_err());
        }
        let mut value = make();
        value.access_token.clear();
        assert!(value.expiry().is_err());
        let mut value = make();
        value.refresh_token.clear();
        assert!(value.expiry().is_err());
    }

    #[test]
    fn callback_parameters_and_browser_tokens_are_strict() {
        assert_eq!(callback_code("code=abc").unwrap(), "abc");
        for invalid in [
            "",
            "code=",
            "code=a&code=b",
            "code=a&verifier=evil",
            "error=denied",
            "code=a&error_description=secret",
        ] {
            assert!(callback_code(invalid).is_err());
        }
        assert!(token_hash(&random_token(32)).is_ok());
        for invalid in ["", "abc", "not/a/token"] {
            assert!(token_hash(invalid).is_err());
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("session=a; session=b"),
        );
        assert!(read_cookie(&headers, "session").is_err());
    }
}
