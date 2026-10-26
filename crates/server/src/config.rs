use std::{
    env,
    net::{IpAddr, SocketAddr},
};

use sqlx::postgres::{PgConnectOptions, PgSslMode};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use url::Url;

use crate::error::{AppError, Result};

/// Never derive Debug: configuration contains database and provider credentials.
#[derive(Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub public_origin: String,
    pub supabase_url: Url,
    pub supabase_anon_key: String,
    pub database_url: String,
    pub session_key: [u8; 32],
    pub secure_cookies: bool,
    pub local_auth_enabled: bool,
    pub session_ttl_seconds: i64,
    pub database_max_connections: u32,
    pub trusted_proxy_ips: Vec<IpAddr>,
    pub auth_requests_per_minute: u32,
    pub helper_daily_limit: i32,
    pub image_daily_limit: i32,
    pub ai_concurrency: usize,
    pub ai: Option<AiConfig>,
}

#[derive(Clone)]
pub struct AiConfig {
    /// OpenAI-compatible base URL, ending in a slash (e.g. https://api.openai.com/v1/).
    pub base_url: Url,
    pub api_key: String,
    pub model: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let required = |name: &str| {
            env::var(name)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .ok_or_else(|| AppError::BadRequest(format!("missing environment variable {name}")))
        };
        let get = |name: &str, default: &str| env::var(name).unwrap_or_else(|_| default.into());
        let origin = required("PUBLIC_ORIGIN")?;
        let origin_url = parse_url(&origin, "PUBLIC_ORIGIN")?;
        if origin_url.path() != "/"
            || origin_url.query().is_some()
            || origin_url.fragment().is_some()
        {
            return Err(AppError::BadRequest(
                "PUBLIC_ORIGIN must contain only scheme and authority".into(),
            ));
        }
        let public_origin = origin_url.origin().ascii_serialization();
        let secure_cookies = origin_url.scheme() == "https";
        let supabase_url = parse_url(&required("SUPABASE_URL")?, "SUPABASE_URL")?;
        if supabase_url.path() != "/"
            || supabase_url.query().is_some()
            || supabase_url.fragment().is_some()
        {
            return Err(AppError::BadRequest(
                "SUPABASE_URL must be the project origin".into(),
            ));
        }
        let decoded = STANDARD
            .decode(required("SESSION_ENCRYPTION_KEY")?)
            .map_err(|_| AppError::BadRequest("SESSION_ENCRYPTION_KEY must be base64".into()))?;
        let session_key: [u8; 32] = decoded.try_into().map_err(|_| {
            AppError::BadRequest("SESSION_ENCRYPTION_KEY must decode to 32 bytes".into())
        })?;
        let ai = match env::var("AI_API_KEY")
            .ok()
            .filter(|value| !value.trim().is_empty())
        {
            None => None,
            Some(api_key) => {
                let mut base_url = parse_url(
                    &get("AI_BASE_URL", "https://api.openai.com/v1/"),
                    "AI_BASE_URL",
                )?;
                if base_url.query().is_some() || base_url.fragment().is_some() {
                    return Err(AppError::BadRequest(
                        "AI_BASE_URL must not contain a query or fragment".into(),
                    ));
                }
                if !base_url.path().ends_with('/') {
                    base_url.set_path(&format!("{}/", base_url.path()));
                }
                let model = required("AI_MODEL")?;
                if model.len() > 128 {
                    return Err(AppError::BadRequest("AI_MODEL is too long".into()));
                }
                Some(AiConfig {
                    base_url,
                    api_key,
                    model,
                })
            }
        };
        let config = Self {
            bind: get("BIND_ADDRESS", "127.0.0.1:3000")
                .parse()
                .map_err(|_| AppError::BadRequest("invalid BIND_ADDRESS".into()))?,
            public_origin,
            supabase_url,
            supabase_anon_key: required("SUPABASE_ANON_KEY")?,
            database_url: required("DATABASE_URL")?,
            session_key,
            secure_cookies,
            local_auth_enabled: parse(&get("LOCAL_AUTH_ENABLED", "false"), "LOCAL_AUTH_ENABLED")?,
            session_ttl_seconds: parse(
                &get("SESSION_TTL_SECONDS", "2592000"),
                "SESSION_TTL_SECONDS",
            )?,
            database_max_connections: parse(
                &get("DATABASE_MAX_CONNECTIONS", "10"),
                "DATABASE_MAX_CONNECTIONS",
            )?,
            trusted_proxy_ips: get("TRUSTED_PROXY_IPS", "")
                .split(',')
                .filter(|s| !s.trim().is_empty())
                .map(|s| {
                    s.trim()
                        .parse()
                        .map_err(|_| AppError::BadRequest("invalid TRUSTED_PROXY_IPS".into()))
                })
                .collect::<Result<Vec<IpAddr>>>()?,
            auth_requests_per_minute: parse(
                &get("AUTH_REQUESTS_PER_MINUTE", "20"),
                "AUTH_REQUESTS_PER_MINUTE",
            )?,
            helper_daily_limit: parse(&get("HELPER_DAILY_LIMIT", "3"), "HELPER_DAILY_LIMIT")?,
            image_daily_limit: parse(&get("IMAGE_DAILY_LIMIT", "3"), "IMAGE_DAILY_LIMIT")?,
            ai_concurrency: parse(&get("AI_CONCURRENCY", "4"), "AI_CONCURRENCY")?,
            ai,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        validate_local_auth(
            self.local_auth_enabled,
            &self.public_origin,
            &self.supabase_url,
        )?;
        if !(60..=2_592_000).contains(&self.session_ttl_seconds)
            || !(1..=100).contains(&self.database_max_connections)
            || !(1..=1000).contains(&self.helper_daily_limit)
            || !(1..=1000).contains(&self.image_daily_limit)
            || !(1..=32).contains(&self.ai_concurrency)
            || !(1..=1000).contains(&self.auth_requests_per_minute)
            || self.trusted_proxy_ips.len() > 64
        {
            return Err(AppError::BadRequest(
                "configuration limits outside supported range".into(),
            ));
        }
        self.database_options()?;
        if self.session_key == [0; 32] {
            return Err(AppError::BadRequest(
                "SESSION_ENCRYPTION_KEY must be a securely generated key, not all zeroes".into(),
            ));
        }
        Ok(())
    }

    pub fn database_options(&self) -> Result<PgConnectOptions> {
        database_options(&self.database_url)
    }
}

fn database_options(value: &str) -> Result<PgConnectOptions> {
    let options: PgConnectOptions = value
        .parse()
        .map_err(|_| AppError::BadRequest("invalid DATABASE_URL".into()))?;
    let host = options
        .get_host()
        .trim_start_matches('[')
        .trim_end_matches(']');
    let local = options.get_socket().is_some()
        || host == "localhost"
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    if !local && !matches!(options.get_ssl_mode(), PgSslMode::VerifyFull) {
        return Err(AppError::BadRequest(
            "remote DATABASE_URL requires sslmode=verify-full and a trusted CA".into(),
        ));
    }
    Ok(options)
}

fn parse<T: std::str::FromStr>(value: &str, name: &str) -> Result<T> {
    value
        .parse()
        .map_err(|_| AppError::BadRequest(format!("invalid {name}")))
}

pub fn is_loopback(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    })
}

fn validate_local_auth(enabled: bool, origin: &str, supabase: &Url) -> Result<()> {
    if enabled {
        let origin = parse_url(origin, "PUBLIC_ORIGIN")?;
        if origin.scheme() != "http"
            || !is_loopback(&origin)
            || supabase.scheme() != "http"
            || !is_loopback(supabase)
        {
            return Err(AppError::BadRequest(
                "LOCAL_AUTH_ENABLED requires HTTP loopback PUBLIC_ORIGIN and SUPABASE_URL".into(),
            ));
        }
    }
    Ok(())
}

fn parse_url(value: &str, name: &str) -> Result<Url> {
    let url = Url::parse(value).map_err(|_| AppError::BadRequest(format!("invalid {name}")))?;
    let local = is_loopback(&url);
    if !url.username().is_empty()
        || url.password().is_some()
        || !(url.scheme() == "https" || (url.scheme() == "http" && local))
    {
        return Err(AppError::BadRequest(format!(
            "{name} must use HTTPS (HTTP is allowed only for loopback development)"
        )));
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_database_requires_verified_tls_including_host_overrides() {
        for url in [
            "postgresql://u:p@db.example.com/app?sslmode=prefer",
            "postgresql://u:p@db.example.com/app?sslmode=disable",
            "postgresql://u:p@db.example.com/app?sslmode=require",
            "postgresql://u:p@localhost/app?host=db.example.com&sslmode=disable",
        ] {
            assert!(
                database_options(url).is_err(),
                "must reject insecure remote DB"
            );
        }
        for url in [
            "postgresql://u:p@db.example.com/app?sslmode=verify-full",
            "postgresql://u:p@localhost/app?sslmode=disable",
            "postgresql://u:p@127.0.0.1/app?sslmode=disable",
            "postgresql://u:p@[::1]/app?sslmode=disable",
        ] {
            assert!(
                database_options(url).is_ok(),
                "expected supported database transport"
            );
        }
    }

    #[test]
    fn local_password_auth_requires_both_http_loopback_origins() {
        for origin in [
            "http://localhost:3000",
            "http://127.2.3.4:3000",
            "http://[::1]:3000",
        ] {
            for provider in ["http://localhost:54321", "http://127.0.0.2", "http://[::1]"] {
                assert!(validate_local_auth(true, origin, &Url::parse(provider).unwrap()).is_ok());
            }
        }
        for remote in [
            "https://localhost",
            "https://example.com",
            "http://localhost.example.com",
            "http://192.168.1.1",
        ] {
            assert!(
                validate_local_auth(true, remote, &Url::parse("http://localhost").unwrap())
                    .is_err()
            );
            assert!(
                validate_local_auth(true, "http://localhost", &Url::parse(remote).unwrap())
                    .is_err()
            );
            assert!(validate_local_auth(
                false,
                "https://example.com",
                &Url::parse(remote).unwrap()
            )
            .is_ok());
        }
    }

    #[test]
    fn urls_reject_credentials_and_insecure_remote_hosts() {
        for url in [
            "http://example.com",
            "https://u:p@example.com",
            "ftp://localhost",
            "not-a-url",
        ] {
            assert!(parse_url(url, "TEST").is_err());
        }
        for url in [
            "http://localhost:3000",
            "http://127.0.0.1:54321",
            "https://example.com",
        ] {
            assert!(parse_url(url, "TEST").is_ok());
        }
    }
}
