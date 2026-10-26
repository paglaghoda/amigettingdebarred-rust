use std::{net::SocketAddr, time::Duration};

use aes_gcm::{
    aead::{Aead, KeyInit, Payload},
    Aes256Gcm, Nonce,
};
use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode},
    response::Response,
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Utc};
use debarred_domain::{time::campus_date, Calendar, CheckpointProjection, Engine, Plan};
use debarred_server::{
    ai::{deterministic_helper, HelperAction, HelperIntent},
    config::Config,
    db::MIGRATOR,
    router, AppState,
};
use http_body_util::BodyExt;
use rand::{rngs::OsRng, RngCore};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{postgres::PgPoolOptions, PgPool};
use tokio::sync::OnceCell;
use tower::ServiceExt;
use uuid::Uuid;

const ORIGIN: &str = "http://localhost:3000";
const PLAN_PATH: &str = "/api/plans/winter-2026";
const CSRF: &str = "http-integration-test-csrf";
const JSON_LIMIT: usize = 128 * 1024;
const IMPORT_LIMIT: usize = 3 * 1024 * 1024;

fn config() -> Config {
    Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        public_origin: ORIGIN.into(),
        // Fresh seeded access tokens must never contact a provider.
        supabase_url: "http://127.0.0.1:1".parse().unwrap(),
        supabase_anon_key: "test-anon-key".into(),
        database_url: "postgres://localhost/http_tests_unused".into(),
        session_key: [7; 32],
        secure_cookies: false,
        local_auth_enabled: false,
        session_ttl_seconds: 7200,
        database_max_connections: 12,
        trusted_proxy_ips: Vec::new(),
        auth_requests_per_minute: 20,
        helper_daily_limit: 3,
        image_daily_limit: 3,
        ai_concurrency: 2,
        ai: None,
    }
}

async fn closed_pool() -> PgPool {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://localhost/http_tests_unused")
        .unwrap();
    pool.close().await;
    pool
}

async fn offline_app() -> Router {
    router(AppState::new(config(), closed_pool().await).unwrap())
}

async fn request(
    app: &Router,
    method: Method,
    path: &str,
    headers: &HeaderMap,
    body: String,
) -> Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .body(Body::from(body))
        .unwrap();
    *request.headers_mut() = headers.clone();
    app.clone().oneshot(request).await.unwrap()
}

async fn response_json(response: Response, status: StatusCode) -> Value {
    assert_eq!(response.status(), status);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn error(response: Response, status: StatusCode, code: &str, message: &str) {
    assert_policy(&response, false);
    assert_eq!(
        response_json(response, status).await,
        json!({"error": {"code": code, "message": message}})
    );
}

fn assert_policy(response: &Response, secure: bool) {
    let headers = response.headers();
    for (name, value) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("x-frame-options", "DENY"),
        (
            "content-security-policy",
            "default-src 'none'; frame-ancestors 'none'; base-uri 'none'",
        ),
    ] {
        assert_eq!(headers[name], value, "{name}");
    }
    Uuid::parse_str(headers["x-request-id"].to_str().unwrap()).unwrap();
    for name in [
        "access-control-allow-origin",
        "access-control-allow-credentials",
        "access-control-allow-methods",
        "access-control-allow-headers",
    ] {
        assert!(!headers.contains_key(name), "unexpected CORS header {name}");
    }
    if secure {
        assert_eq!(
            headers[header::STRICT_TRANSPORT_SECURITY],
            "max-age=31536000"
        );
    } else {
        assert!(!headers.contains_key(header::STRICT_TRANSPORT_SECURITY));
    }
}

fn json_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers
}

fn plan() -> Plan {
    serde_json::from_str(include_str!(
        "../../domain/tests/fixtures/winter-plan.v1.json"
    ))
    .unwrap()
}

fn save_body(plan: &Plan, revision: i64) -> String {
    json!({"expected_revision": revision, "plan": plan}).to_string()
}

fn assert_stored(value: &Value, plan: &Plan, revision: i64) {
    assert_eq!(value.as_object().unwrap().len(), 3);
    assert_eq!(value["plan"], serde_json::to_value(plan).unwrap());
    assert_eq!(value["revision"], revision);
    DateTime::parse_from_rfc3339(value["updated_at"].as_str().unwrap()).unwrap();
}

#[tokio::test]
async fn public_health_and_calendars_work_without_a_database() {
    let app = offline_app().await;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::ORIGIN,
        HeaderValue::from_static("https://other.example"),
    );
    let calendar = Calendar::winter_2026().unwrap();
    for (path, expected) in [
        ("/health/live", json!({"status": "ok"})),
        (
            "/api/semesters",
            json!({"semesters": [{
                "id": calendar.config().id,
                "label": calendar.config().label,
                "revision": calendar.config().revision,
            }]}),
        ),
        (
            "/api/semesters/winter-2026",
            json!({"config": calendar.config(), "days": calendar.days()}),
        ),
    ] {
        let response = request(&app, Method::GET, path, &headers, String::new()).await;
        assert_policy(&response, false);
        assert_eq!(response_json(response, StatusCode::OK).await, expected);
    }
    error(
        request(
            &app,
            Method::GET,
            "/api/semesters/unknown",
            &headers,
            String::new(),
        )
        .await,
        StatusCode::BAD_REQUEST,
        "invalid_input",
        "unsupported semester",
    )
    .await;
    let response = request(
        &app,
        Method::OPTIONS,
        "/api/projections",
        &headers,
        String::new(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_policy(&response, false);
}

#[tokio::test]
async fn ui_routes_are_public_ssr_with_strict_external_asset_csp() {
    let app = offline_app().await;
    for path in [
        "/",
        "/setup",
        "/planner",
        "/demo",
        "/demo/setup",
        "/demo/planner",
    ] {
        let response = request(&app, Method::GET, path, &HeaderMap::new(), String::new()).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response.headers()[header::CONTENT_SECURITY_POLICY],
            "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'self'; frame-ancestors 'none'; form-action 'self'");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains("/assets/boot.js"));
        assert!(body.contains("/assets/app.css"));
        assert!(!body.contains("test-anon-key"));
        assert!(!body.contains("csrf_token"));
    }
    for path in [
        "/unknown-ui",
        "/api/unknown",
        "/assets/missing.js",
        "/assets/../.env",
        "/assets/%2e%2e/Cargo.toml",
        "/assets/%2e%2e%2f.env",
    ] {
        let response = request(&app, Method::GET, path, &HeaderMap::new(), String::new()).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        assert_ne!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/html; charset=utf-8")
        );
    }
}

#[tokio::test]
async fn runtime_reports_only_public_capabilities() {
    for (origin, mode, local_password) in [
        (ORIGIN, "local", false),
        ("http://127.3.2.1:3000", "local", true),
        ("https://attendance.example", "production", false),
    ] {
        let mut config = config();
        config.public_origin = origin.into();
        config.local_auth_enabled = local_password;
        config.secure_cookies = origin.starts_with("https:");
        let app = router(AppState::new(config, closed_pool().await).unwrap());
        let response = request(
            &app,
            Method::GET,
            "/api/runtime",
            &HeaderMap::new(),
            String::new(),
        )
        .await;
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            response_json(response, StatusCode::OK).await,
            json!({
                "mode": mode, "auth_available": true, "local_password_enabled": local_password,
                "ai_enabled": false, "today": campus_date(Utc::now()).unwrap(),
                "policy_version": "2026-10-26",
                "support_email": debarred_web::sanitize_support_email(std::env::var("SUPPORT_EMAIL").ok().as_deref()),
            })
        );
    }
}

#[tokio::test]
async fn preview_has_real_calendar_but_no_auth_or_private_services() {
    let app = debarred_server::preview_router();
    let headers = HeaderMap::new();
    let response = request(&app, Method::GET, "/api/runtime", &headers, String::new()).await;
    assert_eq!(
        response_json(response, StatusCode::OK).await,
        json!({
            "mode": "preview", "auth_available": false, "local_password_enabled": false,
            "ai_enabled": false, "today": campus_date(Utc::now()).unwrap(),
            "policy_version": "2026-10-26",
            "support_email": debarred_web::sanitize_support_email(std::env::var("SUPPORT_EMAIL").ok().as_deref()),
        })
    );
    let response = request(
        &app,
        Method::GET,
        "/api/semesters/winter-2026",
        &headers,
        String::new(),
    )
    .await;
    let calendar = Calendar::winter_2026().unwrap();
    assert_eq!(
        response_json(response, StatusCode::OK).await,
        json!({"config": calendar.config(), "days": calendar.days()})
    );
    for (method, path, status) in [
        (Method::GET, "/demo/planner", StatusCode::OK),
        (Method::GET, "/api/me", StatusCode::UNAUTHORIZED),
        (Method::GET, PLAN_PATH, StatusCode::SERVICE_UNAVAILABLE),
        (Method::PUT, PLAN_PATH, StatusCode::SERVICE_UNAVAILABLE),
        (Method::GET, "/auth/login", StatusCode::SERVICE_UNAVAILABLE),
        (
            Method::POST,
            "/auth/local/login",
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (
            Method::POST,
            "/auth/logout",
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (Method::POST, "/api/helper", StatusCode::SERVICE_UNAVAILABLE),
        (
            Method::POST,
            "/api/projections",
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (Method::GET, "/unknown", StatusCode::NOT_FOUND),
        (Method::GET, "/api/unknown", StatusCode::NOT_FOUND),
        (Method::GET, "/assets/missing.wasm", StatusCode::NOT_FOUND),
    ] {
        assert_eq!(
            request(&app, method, path, &headers, String::new())
                .await
                .status(),
            status,
            "{path}"
        );
    }
}

#[tokio::test]
async fn local_login_requires_flag_origin_bounds_and_shares_auth_budget() {
    let headers = json_headers();
    let app = offline_app().await;
    assert_eq!(
        request(
            &app,
            Method::POST,
            "/auth/local/login",
            &headers,
            "{}".into()
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    let mut config = config();
    config.local_auth_enabled = true;
    let app = router(AppState::new(config, closed_pool().await).unwrap());
    for origin in [
        None,
        Some("null"),
        Some("https://evil.example"),
        Some("http://localhost:3000/"),
    ] {
        let mut headers = headers.clone();
        if let Some(origin) = origin {
            headers.insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
        }
        assert_eq!(
            request(
                &app,
                Method::POST,
                "/auth/local/login",
                &headers,
                "{}".into()
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
    }
    let mut headers = headers;
    headers.insert(header::ORIGIN, HeaderValue::from_static(ORIGIN));
    for body in [
        json!({"email":"", "password":"p"}),
        json!({"email":"a@b", "password":""}),
        json!({"email":"a@b", "password":"x".repeat(1025)}),
        json!({"email":"x".repeat(321), "password":"p"}),
    ] {
        assert_eq!(
            request(
                &app,
                Method::POST,
                "/auth/local/login",
                &headers,
                body.to_string()
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        request(
            &app,
            Method::POST,
            "/auth/local/login",
            &headers,
            json!({"email":"a@b", "password":"x".repeat(9000)}).to_string()
        )
        .await
        .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    // Nine local attempts above consumed the same anonymous budget as Google login.
    for _ in 0..11 {
        assert_eq!(
            request(
                &app,
                Method::HEAD,
                "/auth/login",
                &HeaderMap::new(),
                String::new()
            )
            .await
            .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
    assert_eq!(
        request(
            &app,
            Method::POST,
            "/auth/local/login",
            &headers,
            "{}".into()
        )
        .await
        .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn https_responses_include_hsts() {
    let mut config = config();
    config.public_origin = "https://localhost:3000".into();
    config.secure_cookies = true;
    let app = router(AppState::new(config, closed_pool().await).unwrap());
    let response = request(
        &app,
        Method::GET,
        "/health/live",
        &HeaderMap::new(),
        String::new(),
    )
    .await;
    assert_policy(&response, true);
    assert_eq!(
        response_json(response, StatusCode::OK).await,
        json!({"status": "ok"})
    );
}

#[tokio::test]
async fn protected_routes_reject_missing_and_bad_cookies_before_json_or_database() {
    let app = offline_app().await;
    for cookie in [
        None,
        Some("session=not-base64!".to_owned()),
        Some(format!("session={}", URL_SAFE_NO_PAD.encode([1; 31]))),
        Some(format!(
            "session={0}; session={0}",
            URL_SAFE_NO_PAD.encode([1; 32])
        )),
    ] {
        let mut headers = json_headers();
        if let Some(cookie) = cookie {
            headers.insert(header::COOKIE, cookie.parse().unwrap());
        }
        for (method, path) in [
            (Method::GET, PLAN_PATH),
            (Method::PUT, PLAN_PATH),
            (Method::POST, "/api/projections"),
            (Method::POST, "/api/helper"),
            (Method::POST, "/api/timetable/import"),
            (Method::GET, "/api/quota"),
            (Method::GET, "/api/me"),
        ] {
            error(
                request(&app, method, path, &headers, "not JSON".into()).await,
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "authentication required",
            )
            .await;
        }
    }
}

#[tokio::test]
async fn unknown_routes_are_404_and_closed_database_is_503() {
    let app = offline_app().await;
    error(
        request(
            &app,
            Method::GET,
            "/does-not-exist",
            &HeaderMap::new(),
            String::new(),
        )
        .await,
        StatusCode::NOT_FOUND,
        "not_found",
        "not found",
    )
    .await;
    error(
        request(
            &app,
            Method::GET,
            "/health/ready",
            &HeaderMap::new(),
            String::new(),
        )
        .await,
        StatusCode::SERVICE_UNAVAILABLE,
        "unavailable",
        "database temporarily unavailable",
    )
    .await;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::COOKIE,
        format!("session={}", URL_SAFE_NO_PAD.encode([1; 32]))
            .parse()
            .unwrap(),
    );
    error(
        request(&app, Method::GET, PLAN_PATH, &headers, String::new()).await,
        StatusCode::SERVICE_UNAVAILABLE,
        "unavailable",
        "database temporarily unavailable",
    )
    .await;
}

#[tokio::test]
async fn auth_limit_allows_twenty_per_ip_and_ignores_forwarded_headers() {
    let app = offline_app().await;
    let peer: SocketAddr = "192.0.2.1:12345".parse().unwrap();
    for attempt in 0..22 {
        let path = if attempt % 2 == 0 {
            "/auth/login"
        } else {
            "/auth/callback"
        };
        let request = Request::builder()
            .uri(path)
            .header("x-forwarded-for", format!("198.51.100.{attempt}"))
            .extension(ConnectInfo(peer))
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        if attempt < 20 {
            assert_eq!(
                response.status(),
                if attempt % 2 == 0 {
                    StatusCode::SERVICE_UNAVAILABLE
                } else {
                    StatusCode::UNAUTHORIZED
                }
            );
            assert_policy(&response, false);
        } else {
            error(
                response,
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "request limit reached",
            )
            .await;
        }
    }
    let other_peer_request = Request::builder()
        .uri("/auth/login")
        .extension(ConnectInfo(
            "192.0.2.2:12345".parse::<SocketAddr>().unwrap(),
        ))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone()
            .oneshot(other_peer_request)
            .await
            .unwrap()
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        request(
            &app,
            Method::GET,
            "/health/live",
            &HeaderMap::new(),
            String::new()
        )
        .await
        .status(),
        StatusCode::OK
    );
}

async fn peer_auth_request(
    app: &Router,
    method: Method,
    path: &str,
    peer: &str,
    forwarded: &[&str],
) -> Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .extension(ConnectInfo(peer.parse::<SocketAddr>().unwrap()))
        .body(Body::empty())
        .unwrap();
    for value in forwarded {
        request
            .headers_mut()
            .append("x-forwarded-for", value.parse().unwrap());
    }
    app.clone().oneshot(request).await.unwrap()
}

async fn proxy_app() -> Router {
    let mut config = config();
    config.auth_requests_per_minute = 2;
    config.trusted_proxy_ips = vec![
        "192.0.2.10".parse().unwrap(),
        "192.0.2.11".parse().unwrap(),
        "2001:db8::10".parse().unwrap(),
    ];
    router(AppState::new(config, closed_pool().await).unwrap())
}

#[tokio::test]
async fn get_and_head_share_a_configurable_budget_across_auth_routes() {
    // Exercise both orderings: neither HEAD nor GET gets a separate window.
    for methods in [[Method::GET, Method::HEAD], [Method::HEAD, Method::GET]] {
        let mut config = config();
        config.auth_requests_per_minute = 2;
        let app = router(AppState::new(config, closed_pool().await).unwrap());
        for (method, path, expected) in [
            (
                methods[0].clone(),
                "/auth/login",
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                methods[1].clone(),
                "/auth/callback",
                StatusCode::UNAUTHORIZED,
            ),
            (Method::HEAD, "/auth/login", StatusCode::TOO_MANY_REQUESTS),
            (Method::GET, "/auth/callback", StatusCode::TOO_MANY_REQUESTS),
        ] {
            let response = peer_auth_request(&app, method, path, "192.0.2.1:12345", &[]).await;
            assert_eq!(response.status(), expected);
            assert_policy(&response, false);
        }
    }
}

#[tokio::test]
async fn head_only_auth_requests_are_limited_on_the_twenty_first_request() {
    for path in ["/auth/login", "/auth/callback"] {
        let app = offline_app().await;
        for attempt in 1..=21 {
            let response =
                peer_auth_request(&app, Method::HEAD, path, "192.0.2.1:12345", &[]).await;
            assert_eq!(
                response.status(),
                if attempt == 21 {
                    StatusCode::TOO_MANY_REQUESTS
                } else if path == "/auth/login" {
                    StatusCode::SERVICE_UNAVAILABLE
                } else {
                    StatusCode::UNAUTHORIZED
                },
                "{path}, HEAD request {attempt}"
            );
            assert_policy(&response, false);
            if attempt <= 20 {
                assert!(response
                    .into_body()
                    .collect()
                    .await
                    .unwrap()
                    .to_bytes()
                    .is_empty());
            }
        }
    }
}

#[tokio::test]
async fn trusted_proxy_clients_have_independent_budgets_and_left_spoofing_cannot_reset_them() {
    let app = proxy_app().await;
    // Trusted suffixes can contain several proxies, including the TCP peer.
    for client in ["198.51.100.1", "198.51.100.2"] {
        for spoofed in ["203.0.113.1", "203.0.113.2"] {
            let chain = format!("{spoofed}, {client}, 192.0.2.11, 192.0.2.10");
            let response = peer_auth_request(
                &app,
                Method::GET,
                "/auth/login",
                "192.0.2.10:12345",
                &[&chain],
            )
            .await;
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        }
    }
    for client in ["198.51.100.1", "198.51.100.2"] {
        let chain = format!("203.0.113.99, {client}, 192.0.2.11");
        error(
            peer_auth_request(
                &app,
                Method::GET,
                "/auth/login",
                "192.0.2.10:54321",
                &[&chain],
            )
            .await,
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "request limit reached",
        )
        .await;
    }
    // The same resolved client stays limited through a different trusted TCP peer.
    let response = peer_auth_request(
        &app,
        Method::HEAD,
        "/auth/callback",
        "[2001:db8::10]:12345",
        &["203.0.113.100, 198.51.100.1, 192.0.2.11"],
    )
    .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    // A new IPv6 client has its own budget; trusted suffixes are stripped by IP.
    let response = peer_auth_request(
        &app,
        Method::HEAD,
        "/auth/login",
        "[2001:db8::10]:12345",
        &["2001:db8::1, 2001:db8::10"],
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn trusted_proxy_rejects_invalid_forwarding_but_untrusted_peer_ignores_it() {
    let too_many_hops = vec!["198.51.100.1"; 17].join(",");
    let oversized = format!("{}198.51.100.1", " ".repeat(1024));
    for (forwarded, message) in [
        (vec![], "trusted proxy must provide a valid X-Forwarded-For"),
        (
            vec!["198.51.100.1", "198.51.100.2"],
            "duplicate X-Forwarded-For",
        ),
        (vec![""], "invalid forwarded address"),
        (vec!["not-an-ip"], "invalid forwarded address"),
        (vec!["198.51.100.1:1234"], "invalid forwarded address"),
        (
            vec!["198.51.100.1,,192.0.2.11"],
            "invalid forwarded address",
        ),
        (
            vec!["198.51.100.1, unknown, 192.0.2.11"],
            "invalid forwarded address",
        ),
        (
            vec![too_many_hops.as_str()],
            "invalid forwarded chain length",
        ),
        (
            vec![oversized.as_str()],
            "trusted proxy must provide a valid X-Forwarded-For",
        ),
    ] {
        let app = proxy_app().await;
        error(
            peer_auth_request(
                &app,
                Method::GET,
                "/auth/login",
                "192.0.2.10:12345",
                &forwarded,
            )
            .await,
            StatusCode::BAD_REQUEST,
            "invalid_input",
            message,
        )
        .await;
        let head = peer_auth_request(
            &app,
            Method::HEAD,
            "/auth/callback",
            "192.0.2.10:12345",
            &forwarded,
        )
        .await;
        assert_eq!(head.status(), StatusCode::BAD_REQUEST);
        assert_policy(&head, false);
        // Rejected headers must not consume the valid forwarded client's budget.
        for _ in 0..2 {
            assert_eq!(
                peer_auth_request(
                    &app,
                    Method::GET,
                    "/auth/login",
                    "192.0.2.10:12345",
                    &["198.51.100.1"],
                )
                .await
                .status(),
                StatusCode::SERVICE_UNAVAILABLE
            );
        }
        // Even an allowlisted address in XFF cannot make an untrusted TCP peer trusted.
        assert_eq!(
            peer_auth_request(
                &app,
                Method::GET,
                "/auth/login",
                "203.0.113.10:12345",
                &forwarded,
            )
            .await
            .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            peer_auth_request(
                &app,
                Method::HEAD,
                "/auth/callback",
                "203.0.113.10:54321",
                &["192.0.2.10"],
            )
            .await
            .status(),
            StatusCode::UNAUTHORIZED
        );
        error(
            peer_auth_request(
                &app,
                Method::GET,
                "/auth/login",
                "203.0.113.10:23456",
                &["198.51.100.99"],
            )
            .await,
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "request limit reached",
        )
        .await;
    }
}

#[tokio::test]
async fn trusted_proxy_accepts_forwarding_at_the_header_and_hop_limits() {
    let app = proxy_app().await;
    let chain = std::iter::once("198.51.100.1")
        .chain(std::iter::repeat_n("192.0.2.11", 15))
        .collect::<Vec<_>>()
        .join(", ");
    let padded = format!("{}{chain}", " ".repeat(1024 - chain.len()));
    assert_eq!(padded.len(), 1024);
    for forwarded in [&chain, &padded] {
        assert_eq!(
            peer_auth_request(
                &app,
                Method::GET,
                "/auth/login",
                "192.0.2.10:12345",
                &[forwarded],
            )
            .await
            .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
    error(
        peer_auth_request(
            &app,
            Method::GET,
            "/auth/login",
            "192.0.2.10:12345",
            &["198.51.100.1"],
        )
        .await,
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        "request limit reached",
    )
    .await;
}

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
            MIGRATOR.run(&pool).await.expect("run database migrations");
        })
        .await;
    pool
}

struct Session {
    user_id: Uuid,
    headers: HeaderMap,
}

struct Fixture {
    pool: PgPool,
    state: AppState,
    app: Router,
    sessions: Vec<Session>,
}

impl Fixture {
    async fn new() -> Self {
        let pool = database().await;
        let state = AppState::new(config(), pool.clone()).unwrap();
        Self {
            pool,
            app: router(state.clone()),
            state,
            sessions: Vec::new(),
        }
    }

    async fn session(&mut self) -> usize {
        let user_id = Uuid::new_v4();
        sqlx::query("INSERT INTO auth.users (id) VALUES ($1)")
            .bind(user_id)
            .execute(&self.pool)
            .await
            .unwrap();
        let mut raw = [0; 32];
        OsRng.fill_bytes(&mut raw);
        let cookie = URL_SAFE_NO_PAD.encode(raw);
        let hash = Sha256::digest(cookie.as_bytes()).to_vec();
        let plaintext = serde_json::to_vec(&json!({
            "access_token": "http-test-access-token",
            "refresh_token": "http-test-refresh-token",
            "csrf_token": CSRF,
        }))
        .unwrap();
        let mut nonce = [0; 12];
        OsRng.fill_bytes(&mut nonce);
        let ciphertext = Aes256Gcm::new_from_slice(&self.state.config.session_key)
            .unwrap()
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &plaintext,
                    aad: &hash,
                },
            )
            .unwrap();
        let mut packed = nonce.to_vec();
        packed.extend_from_slice(&ciphertext);
        sqlx::query(
            "INSERT INTO app_private.auth_sessions
             (token_hash, user_id, encrypted_tokens, access_expires_at, expires_at)
             VALUES ($1, $2, $3, now() + interval '1 hour', now() + interval '2 hours')",
        )
        .bind(hash)
        .bind(user_id)
        .bind(packed)
        .execute(&self.pool)
        .await
        .unwrap();
        let mut headers = json_headers();
        headers.insert(header::COOKIE, format!("session={cookie}").parse().unwrap());
        headers.insert(header::ORIGIN, HeaderValue::from_static(ORIGIN));
        headers.insert("x-csrf-token", HeaderValue::from_static(CSRF));
        self.sessions.push(Session { user_id, headers });
        self.sessions.len() - 1
    }

    async fn send(&self, session: usize, method: Method, path: &str, body: String) -> Response {
        request(
            &self.app,
            method,
            path,
            &self.sessions[session].headers,
            body,
        )
        .await
    }

    async fn cleanup(self) {
        for session in self.sessions {
            sqlx::query("DELETE FROM auth.users WHERE id = $1")
                .bind(session.user_id)
                .execute(&self.pool)
                .await
                .unwrap();
        }
        self.pool.close().await;
    }
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn authenticated_missing_plan_is_404_but_store_failure_is_503() {
    let mut f = Fixture::new().await;
    let user = f.session().await;
    error(
        f.send(user, Method::GET, PLAN_PATH, String::new()).await,
        StatusCode::NOT_FOUND,
        "not_found",
        "not found",
    )
    .await;
    assert_eq!(
        response_json(
            f.send(user, Method::GET, "/health/ready", String::new())
                .await,
            StatusCode::OK
        )
        .await,
        json!({"status": "ready"})
    );
    // Keep real authentication, but fail the plan store independently of auth.
    let mut failed = AppState::new(config(), closed_pool().await).unwrap();
    failed.auth = f.state.auth.clone();
    error(
        request(
            &router(failed),
            Method::GET,
            PLAN_PATH,
            &f.sessions[user].headers,
            String::new(),
        )
        .await,
        StatusCode::SERVICE_UNAVAILABLE,
        "unavailable",
        "database temporarily unavailable",
    )
    .await;
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn plan_create_update_and_stale_revision_preserve_the_winning_document() {
    let mut f = Fixture::new().await;
    let user = f.session().await;
    let original = plan();
    let created = response_json(
        f.send(user, Method::PUT, PLAN_PATH, save_body(&original, 0))
            .await,
        StatusCode::OK,
    )
    .await;
    assert_stored(&created, &original, 1);
    let mut updated = original.clone();
    updated.leave.clear();
    let saved = response_json(
        f.send(user, Method::PUT, PLAN_PATH, save_body(&updated, 1))
            .await,
        StatusCode::OK,
    )
    .await;
    assert_stored(&saved, &updated, 2);
    error(
        f.send(user, Method::PUT, PLAN_PATH, save_body(&original, 1))
            .await,
        StatusCode::CONFLICT,
        "revision_conflict",
        "plan was changed elsewhere; reload before saving",
    )
    .await;
    assert_eq!(
        response_json(
            f.send(user, Method::GET, PLAN_PATH, String::new()).await,
            StatusCode::OK
        )
        .await,
        saved
    );
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn plan_routes_validate_semester_and_reject_client_supplied_ownership() {
    let mut f = Fixture::new().await;
    let user = f.session().await;
    let mut mismatched = plan();
    mismatched.semester_id = "another-semester".into();
    error(
        f.send(user, Method::PUT, PLAN_PATH, save_body(&mismatched, 0))
            .await,
        StatusCode::BAD_REQUEST,
        "invalid_input",
        "plan semester does not match route",
    )
    .await;
    error(
        f.send(
            user,
            Method::PUT,
            "/api/plans/unknown",
            save_body(&plan(), 0),
        )
        .await,
        StatusCode::BAD_REQUEST,
        "invalid_input",
        "unsupported semester",
    )
    .await;
    for field in ["user_id", "owner_id"] {
        let mut body = json!({"expected_revision": 0, "plan": plan()});
        body[field] = json!(Uuid::new_v4());
        let response = f.send(user, Method::PUT, PLAN_PATH, body.to_string()).await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_policy(&response, false);
    }
    error(
        f.send(user, Method::GET, PLAN_PATH, String::new()).await,
        StatusCode::NOT_FOUND,
        "not_found",
        "not found",
    )
    .await;
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn accounts_cannot_read_or_update_each_others_plans() {
    let mut f = Fixture::new().await;
    let alice = f.session().await;
    let bob = f.session().await;
    let original = plan();
    let saved = response_json(
        f.send(alice, Method::PUT, PLAN_PATH, save_body(&original, 0))
            .await,
        StatusCode::OK,
    )
    .await;
    error(
        f.send(bob, Method::GET, PLAN_PATH, String::new()).await,
        StatusCode::NOT_FOUND,
        "not_found",
        "not found",
    )
    .await;
    let mut other = original.clone();
    other.leave.clear();
    error(
        f.send(bob, Method::PUT, PLAN_PATH, save_body(&other, 1))
            .await,
        StatusCode::CONFLICT,
        "revision_conflict",
        "plan was changed elsewhere; reload before saving",
    )
    .await;
    let own = response_json(
        f.send(bob, Method::PUT, PLAN_PATH, save_body(&other, 0))
            .await,
        StatusCode::OK,
    )
    .await;
    assert_stored(&own, &other, 1);
    assert_eq!(
        response_json(
            f.send(alice, Method::GET, PLAN_PATH, String::new()).await,
            StatusCode::OK
        )
        .await,
        saved
    );
    assert_eq!(
        response_json(
            f.send(bob, Method::GET, PLAN_PATH, String::new()).await,
            StatusCode::OK
        )
        .await,
        own
    );
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn stale_page_identity_cannot_read_write_or_revoke_a_different_cookie_user() {
    let mut f = Fixture::new().await;
    let old = f.session().await;
    let current = f.session().await;
    // Simulate another tab replacing the cookie. Even the new cookie's correct
    // CSRF token must not allow the old displayed user's draft to be saved.
    let mut headers = f.sessions[current].headers.clone();
    headers.insert(
        "x-session-user",
        f.sessions[old].user_id.to_string().parse().unwrap(),
    );
    for (method, path, body) in [
        (Method::PUT, PLAN_PATH, save_body(&plan(), 0)),
        (Method::GET, PLAN_PATH, String::new()),
        (Method::POST, "/auth/logout", String::new()),
    ] {
        let response = request(&f.app, method, path, &headers, body).await;
        assert!(!response.headers().contains_key(header::SET_COOKIE));
        error(
            response,
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "authentication required",
        )
        .await;
    }
    for user in [old, current] {
        assert!(f
            .state
            .store
            .load_plan(f.sessions[user].user_id, "winter-2026")
            .await
            .unwrap()
            .is_none());
    }
    // Missing header remains compatible, and /api/me can discover the new user.
    let me = response_json(
        f.send(current, Method::GET, "/api/me", String::new()).await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(me["user_id"], f.sessions[current].user_id.to_string());
    let current_id: HeaderValue = f.sessions[current].user_id.to_string().parse().unwrap();
    for value in [
        HeaderValue::from_static("invalid-uuid"),
        HeaderValue::from_static(""),
    ] {
        headers.insert("x-session-user", value);
        assert_eq!(
            request(&f.app, Method::GET, PLAN_PATH, &headers, String::new())
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    headers.insert("x-session-user", current_id.clone());
    headers.append("x-session-user", current_id.clone());
    assert_eq!(
        request(&f.app, Method::GET, PLAN_PATH, &headers, String::new())
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    headers.insert("x-session-user", current_id);
    let saved = response_json(
        request(
            &f.app,
            Method::PUT,
            PLAN_PATH,
            &headers,
            save_body(&plan(), 0),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_stored(&saved, &plan(), 1);
    assert_eq!(
        response_json(
            request(&f.app, Method::GET, PLAN_PATH, &headers, String::new()).await,
            StatusCode::OK
        )
        .await,
        saved
    );
    assert!(f
        .state
        .store
        .load_plan(f.sessions[old].user_id, "winter-2026")
        .await
        .unwrap()
        .is_none());
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn post_and_put_require_exact_origin_and_csrf_before_json_extraction() {
    let mut f = Fixture::new().await;
    let user = f.session().await;
    for (method, path) in [
        (Method::PUT, PLAN_PATH),
        (Method::POST, "/api/projections"),
        (Method::POST, "/api/helper"),
        (Method::POST, "/api/timetable/import"),
    ] {
        for (name, value) in [
            ("origin", None),
            ("origin", Some("https://evil.example")),
            ("origin", Some("http://localhost:3000/")),
            ("x-csrf-token", None),
            ("x-csrf-token", Some("wrong-token")),
        ] {
            let mut headers = f.sessions[user].headers.clone();
            headers.remove(name);
            if let Some(value) = value {
                headers.insert(name, value.parse().unwrap());
            }
            error(
                request(&f.app, method.clone(), path, &headers, "not JSON".into()).await,
                StatusCode::FORBIDDEN,
                "forbidden",
                "request not permitted",
            )
            .await;
        }
        for name in ["origin", "x-csrf-token"] {
            let mut headers = f.sessions[user].headers.clone();
            let value = headers[name].clone();
            headers.append(name, value);
            error(
                request(&f.app, method.clone(), path, &headers, "not JSON".into()).await,
                StatusCode::FORBIDDEN,
                "forbidden",
                "request not permitted",
            )
            .await;
        }
    }
    error(
        f.send(user, Method::GET, PLAN_PATH, String::new()).await,
        StatusCode::NOT_FOUND,
        "not_found",
        "not found",
    )
    .await;
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn projections_match_the_domain_and_never_save_the_preview() {
    let mut f = Fixture::new().await;
    let user = f.session().await;
    let plan = plan();
    let engine = Engine::new(&f.state.calendar, &plan).unwrap();
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
                    "allowance": engine.skip_allowance(&course.course_code, &checkpoint.id).unwrap(),
                }));
            }
        }
    }
    let expected = json!({"as_of": plan.attendance.as_of, "projections": projections,
        "allowances": allowances, "leave": engine.leave_summary()});
    let body = json!({"plan": plan}).to_string();
    assert_eq!(
        response_json(
            f.send(user, Method::POST, "/api/projections", body.clone())
                .await,
            StatusCode::OK
        )
        .await,
        expected
    );
    error(
        f.send(user, Method::GET, PLAN_PATH, String::new()).await,
        StatusCode::NOT_FOUND,
        "not_found",
        "not found",
    )
    .await;
    let mut stored = plan.clone();
    stored.leave.clear();
    let saved = response_json(
        f.send(user, Method::PUT, PLAN_PATH, save_body(&stored, 0))
            .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        response_json(
            f.send(user, Method::POST, "/api/projections", body).await,
            StatusCode::OK
        )
        .await,
        expected
    );
    assert_eq!(
        response_json(
            f.send(user, Method::GET, PLAN_PATH, String::new()).await,
            StatusCode::OK
        )
        .await,
        saved
    );
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn deterministic_helper_is_free_quota_uses_ist_and_disabled_images_are_503() {
    let mut f = Fixture::new().await;
    let user = f.session().await;
    let intent = HelperIntent {
        action: HelperAction::Status,
        course_codes: Vec::new(),
        checkpoint: None,
        dates: Vec::new(),
    };
    let plan = plan();
    let expected =
        serde_json::to_value(deterministic_helper(&f.state.calendar, &plan, &intent).unwrap())
            .unwrap();
    for _ in 0..4 {
        assert_eq!(
            response_json(
                f.send(
                    user,
                    Method::POST,
                    "/api/helper",
                    json!({"plan": plan, "intent": intent}).to_string()
                )
                .await,
                StatusCode::OK
            )
            .await,
            expected
        );
    }
    error(
        f.send(
            user,
            Method::POST,
            "/api/timetable/import",
            json!({"image_data_url": "data:image/png;base64,iVBORw0KGgo="}).to_string(),
        )
        .await,
        StatusCode::SERVICE_UNAVAILABLE,
        "unavailable",
        "AI provider is disabled",
    )
    .await;
    let before = campus_date(Utc::now()).unwrap();
    let quota = response_json(
        f.send(user, Method::GET, "/api/quota", String::new()).await,
        StatusCode::OK,
    )
    .await;
    let after = campus_date(Utc::now()).unwrap();
    assert!(quota["day"] == json!(before) || quota["day"] == json!(after));
    assert_eq!(
        quota,
        json!({"day": quota["day"], "timezone": "Asia/Kolkata",
        "helper": {"limit": 3, "used": 0, "remaining": 3},
        "image": {"limit": 3, "used": 0, "remaining": 3}})
    );
    let usage: i64 =
        sqlx::query_scalar("SELECT count(*) FROM app_private.ai_usage WHERE user_id = $1")
            .bind(f.sessions[user].user_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(usage, 0);
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn authenticated_json_limits_and_larger_import_override_are_enforced() {
    let mut f = Fixture::new().await;
    let user = f.session().await;
    for (method, path) in [
        (Method::PUT, PLAN_PATH),
        (Method::POST, "/api/projections"),
        (Method::POST, "/api/helper"),
    ] {
        let response = f
            .send(
                user,
                method,
                path,
                json!({"padding": "x".repeat(JSON_LIMIT)}).to_string(),
            )
            .await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_policy(&response, false);
    }
    // Valid small-image JSON padded with whitespace tests extraction, not provider I/O.
    let small = json!({"image_data_url": "data:image/png;base64,iVBORw0KGgo="}).to_string();
    let padded = format!("{small}{}", " ".repeat(JSON_LIMIT));
    assert!(padded.len() > JSON_LIMIT && padded.len() < IMPORT_LIMIT);
    error(
        f.send(user, Method::POST, "/api/timetable/import", padded)
            .await,
        StatusCode::SERVICE_UNAVAILABLE,
        "unavailable",
        "AI provider is disabled",
    )
    .await;
    // A large inline payload must reach image validation rather than the global 128 KiB limit.
    let invalid_image =
        json!({"image_data_url": format!("data:image/png;base64,{}", "A".repeat(JSON_LIMIT))})
            .to_string();
    error(
        f.send(user, Method::POST, "/api/timetable/import", invalid_image)
            .await,
        StatusCode::BAD_REQUEST,
        "invalid_input",
        "image signature does not match MIME type",
    )
    .await;
    let oversized = format!("{small}{}", " ".repeat(IMPORT_LIMIT));
    let response = f
        .send(user, Method::POST, "/api/timetable/import", oversized)
        .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_policy(&response, false);
    f.cleanup().await;
}
