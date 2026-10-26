//! Local-only executor. Run via scripts/benchmark.py, never against a shared database.
use aes_gcm::{
    aead::{Aead, Payload},
    Aes256Gcm, KeyInit, Nonce,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use debarred_domain::{Calendar, Engine, Plan};
use debarred_server::{config::Config, db::MIGRATOR, AppState};
use rand::{rngs::OsRng, RngCore};
use reqwest::{Client, Method};
use serde_json::{json, Value};
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions, PgSslMode},
    ConnectOptions, PgPool,
};
use std::{
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};
use uuid::Uuid;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const WARMUP: usize = 10;
const SAMPLES: usize = 100;

fn database_url() -> Result<String> {
    let value = std::env::var("BENCH_DATABASE_URL").map_err(|_| "BENCH_DATABASE_URL required")?;
    let url = url::Url::parse(&value).map_err(|_| "invalid benchmark database URL")?;
    let host = url.host_str().unwrap_or("").trim_matches(['[', ']']);
    if !matches!(url.scheme(), "postgres" | "postgresql")
        || !host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path().len() < 2
        || url.port() == Some(0)
    {
        return Err(
            "benchmark requires numeric loopback PostgreSQL; no query/fragment overrides".into(),
        );
    }
    // Check the driver interpretation too, before any connection is possible.
    let options: PgConnectOptions = value.parse().map_err(|_| "invalid database options")?;
    if options.get_socket().is_some()
        || !options
            .get_host()
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    {
        return Err("database driver host is not numeric loopback".into());
    }
    Ok(value)
}

fn summary(
    name: &str,
    mut samples: Vec<f64>,
    errors: usize,
    concurrency: usize,
    bytes: usize,
) -> Value {
    samples.sort_by(f64::total_cmp);
    let percentile = |p: usize| samples[(samples.len() * p).div_ceil(100).saturating_sub(1)];
    json!({"scenario": name, "unit": "ms", "sample_count": samples.len(),
        "warmup_count": WARMUP, "concurrency": concurrency, "error_count": errors,
        "p50": percentile(50), "p95": percentile(95), "max": samples.last(),
        "response_body_bytes_total": bytes})
}

struct HttpBench {
    client: Client,
    origin: String,
    cookie: String,
    csrf: String,
    user: Uuid,
}
impl HttpBench {
    async fn request(
        &self,
        path: &str,
        body: Option<Value>,
        authenticated: bool,
        gzip: bool,
    ) -> Result<(Value, usize)> {
        let method = if path == "/api/projections" {
            Method::POST
        } else if body.is_some() {
            Method::PUT
        } else {
            Method::GET
        };
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.origin));
        if authenticated {
            request = request
                .header("Cookie", &self.cookie)
                .header("X-CSRF-Token", &self.csrf)
                .header("X-Session-User", self.user.to_string())
                .header("Origin", &self.origin);
        }
        if gzip {
            request = request.header("Accept-Encoding", "gzip");
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|_| "local request failed")?;
        if response.status() != reqwest::StatusCode::OK {
            return Err("unexpected local HTTP status".into());
        }
        if gzip
            && response
                .headers()
                .get("content-encoding")
                .and_then(|v| v.to_str().ok())
                != Some("gzip")
        {
            return Err("WASM response was not gzip encoded".into());
        }
        let data = response
            .bytes()
            .await
            .map_err(|_| "local response body failed")?;
        let value = if authenticated {
            serde_json::from_slice(&data)?
        } else {
            Value::Null
        };
        Ok((value, data.len()))
    }

    async fn scenario(
        &self,
        name: &str,
        path: &str,
        body: Option<Value>,
        auth: bool,
        gzip: bool,
        concurrency: usize,
    ) -> Value {
        let mut errors = 0;
        for _ in 0..WARMUP {
            if self.request(path, body.clone(), auth, gzip).await.is_err() {
                errors += 1;
            }
        }
        let mut samples = Vec::new();
        let mut bytes = 0;
        // JoinSet bounds concurrent requests and includes full body consumption in latency.
        let mut tasks = tokio::task::JoinSet::new();
        for start in (0..SAMPLES).step_by(concurrency) {
            for _ in start..(start + concurrency).min(SAMPLES) {
                let client = self.client.clone();
                let bench = Self {
                    client,
                    origin: self.origin.clone(),
                    cookie: self.cookie.clone(),
                    csrf: self.csrf.clone(),
                    user: self.user,
                };
                let path = path.to_owned();
                let body = body.clone();
                tasks.spawn(async move {
                    let now = Instant::now();
                    let result = bench.request(&path, body, auth, gzip).await;
                    (now.elapsed().as_secs_f64() * 1000.0, result)
                });
            }
            while let Some(result) = tasks.join_next().await {
                match result {
                    Ok((ms, result)) => {
                        samples.push(ms);
                        match result {
                            Ok((_, size)) => bytes += size,
                            Err(_) => errors += 1,
                        }
                    }
                    Err(_) => {
                        errors += 1;
                        samples.push(5000.0);
                    }
                }
            }
        }
        summary(name, samples, errors, concurrency, bytes)
    }
}

async fn measure(pool: &PgPool, user: Uuid, database: String) -> Result<Value> {
    let mut key = [0u8; 32];
    let mut cookie = [0u8; 32];
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut key);
    OsRng.fill_bytes(&mut cookie);
    OsRng.fill_bytes(&mut nonce);
    let cookie = URL_SAFE_NO_PAD.encode(cookie);
    let csrf = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(cookie.as_bytes()).to_vec();
    let tokens = serde_json::to_vec(
        &json!({"access_token": "benchmark-fake-access", "refresh_token": "benchmark-fake-refresh", "csrf_token": csrf}),
    )?;
    let encrypted = Aes256Gcm::new_from_slice(&key)
        .map_err(|_| "cipher init")?
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &tokens,
                aad: &hash,
            },
        )
        .map_err(|_| "session encryption")?;
    let packed = [nonce.as_slice(), encrypted.as_slice()].concat();
    sqlx::query("INSERT INTO auth.users (id) VALUES ($1)")
        .bind(user)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO app_private.auth_sessions (token_hash, user_id, encrypted_tokens, access_expires_at, expires_at, authenticated_at) VALUES ($1,$2,$3,now()+interval '1 hour',now()+interval '1 hour',now())")
        .bind(hash).bind(user).bind(packed).execute(pool).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let origin = format!("http://{address}");
    let config = Config {
        bind: address,
        public_origin: origin.clone(),
        supabase_url: "http://127.0.0.1:1".parse()?,
        supabase_anon_key: "benchmark-not-a-provider-key".into(),
        database_url: database,
        session_key: key,
        secure_cookies: false,
        local_auth_enabled: false,
        session_ttl_seconds: 3600,
        database_max_connections: 12,
        trusted_proxy_ips: vec![],
        auth_requests_per_minute: 20,
        helper_daily_limit: 3,
        image_daily_limit: 3,
        ai_concurrency: 4,
        ai: None,
    };
    let state = AppState::new(config, pool.clone())?;
    let plan = Plan::from_json(include_str!(
        "../../domain/tests/fixtures/winter-plan.v1.json"
    ))?;
    state.store.save_plan(user, 0, &plan).await?;
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            debarred_server::router(state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
    });
    // Abort on every return path, including timeout cancellation.
    struct ServerGuard(tokio::task::JoinHandle<std::io::Result<()>>);
    impl Drop for ServerGuard {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _guard = ServerGuard(server);
    let bench = HttpBench {
        client: Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()?,
        origin,
        cookie: format!("session={cookie}"),
        csrf,
        user,
    };
    let mut scenarios = vec![];
    scenarios.push(
        bench
            .scenario(
                "authenticated_plan_get",
                "/api/plans/winter-2026",
                None,
                true,
                false,
                8,
            )
            .await,
    );
    let mut revision = 1;
    let mut samples = vec![];
    let mut errors = 0;
    let mut bytes = 0;
    for i in 0..WARMUP + SAMPLES {
        let now = Instant::now();
        let result = bench
            .request(
                "/api/plans/winter-2026",
                Some(json!({"expected_revision": revision, "plan": plan})),
                true,
                false,
            )
            .await;
        let elapsed = now.elapsed().as_secs_f64() * 1000.0;
        match result {
            Ok((value, size)) if value["revision"].as_i64() == Some(revision + 1) => {
                revision += 1;
                if i >= WARMUP {
                    bytes += size;
                }
            }
            _ => {
                errors += 1;
            }
        }
        if i >= WARMUP {
            samples.push(elapsed);
        }
    }
    scenarios.push(summary("authenticated_plan_put", samples, errors, 1, bytes));
    scenarios.push(
        bench
            .scenario(
                "authenticated_projection_post",
                "/api/projections",
                Some(json!({"plan": plan})),
                true,
                false,
                1,
            )
            .await,
    );
    scenarios.push(
        bench
            .scenario("public_ssr_get", "/", None, false, false, 1)
            .await,
    );
    scenarios.push(
        bench
            .scenario(
                "gzip_wasm_transfer",
                "/assets/app_bg.wasm",
                None,
                false,
                true,
                1,
            )
            .await,
    );
    let calendar = Calendar::winter_2026()?;
    for project in [false, true] {
        let engine = Engine::new(&calendar, &plan)?;
        let mut samples = vec![];
        for i in 0..WARMUP + SAMPLES {
            let start = Instant::now();
            if project {
                std::hint::black_box(engine.project_all());
            } else {
                std::hint::black_box(Engine::new(&calendar, &plan)?);
            }
            if i >= WARMUP {
                samples.push(start.elapsed().as_secs_f64() * 1000.0);
            }
        }
        scenarios.push(summary(
            if project {
                "engine_project_all"
            } else {
                "engine_build"
            },
            samples,
            0,
            1,
            0,
        ));
    }
    Ok(
        json!({"version": 1, "metadata": {"platform": std::env::consts::OS, "arch": std::env::consts::ARCH,
        "build_mode": "release", "debug_assertions": cfg!(debug_assertions), "measured_at": Utc::now(),
        "database": "local PostgreSQL (not hosted Supabase)", "provider_auth_measured": false,
        "ai_calls": false, "request_timeout_seconds": 5, "executor_timeout_seconds": 60}, "scenarios": scenarios}),
    )
}

async fn execute() -> Result<Value> {
    if cfg!(debug_assertions) {
        return Err("benchmark must run with --release".into());
    }
    let database = database_url()?;
    let options: PgConnectOptions = database.parse()?;
    let pool = PgPoolOptions::new()
        .max_connections(12)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::query("SET search_path TO app_private, public")
                    .execute(&mut *connection)
                    .await?;
                sqlx::query("SET statement_timeout = '5s'")
                    .execute(&mut *connection)
                    .await?;
                sqlx::query("SET lock_timeout = '5s'")
                    .execute(&mut *connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(
            options
                .ssl_mode(PgSslMode::Disable)
                .disable_statement_logging(),
        )
        .await?;
    let user = Uuid::new_v4();
    // Reserve cleanup time inside the whole-executor 60 second deadline.
    let result = tokio::time::timeout(Duration::from_secs(48), async {
        sqlx::raw_sql("CREATE SCHEMA IF NOT EXISTS auth; CREATE TABLE IF NOT EXISTS auth.users (id uuid PRIMARY KEY);").execute(&pool).await?;
        // Keep SQLx's migration ledger in public on both first and repeat runs.
        // app_private does not exist before the first migration.
        let mut connection = pool.acquire().await?;
        sqlx::query("SET search_path TO public").execute(&mut *connection).await?;
        MIGRATOR.run(&mut *connection).await?;
        sqlx::query("SET search_path TO app_private, public").execute(&mut *connection).await?;
        drop(connection);
        measure(&pool, user, database).await
    }).await;
    let cleanup = tokio::time::timeout(
        Duration::from_secs(6),
        sqlx::query("DELETE FROM auth.users WHERE id = $1")
            .bind(user)
            .execute(&pool),
    )
    .await;
    if !matches!(cleanup, Ok(Ok(_))) {
        return Err("benchmark cleanup failed; inspect disposable database".into());
    }
    result.map_err(|_| "benchmark workload timed out")?
}

#[tokio::main]
async fn main() {
    match tokio::time::timeout(Duration::from_secs(60), execute()).await {
        Ok(Ok(report)) => println!("{report}"),
        _ => {
            eprintln!("benchmark failed (details suppressed); requires release build, disposable numeric-loopback BENCH_DATABASE_URL, current migrations and built gzip WASM. Check disposable DB for interrupted-run users.");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn percentiles_use_nearest_rank() {
        let result = summary("test", (1..=100).map(f64::from).collect(), 0, 1, 0);
        assert_eq!(result["p50"], 50.0);
        assert_eq!(result["p95"], 95.0);
        assert_eq!(result["max"], 100.0);
    }
}
