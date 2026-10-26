// Exercise the production middleware without a database or environment mutation.
use debarred_server::{error, ops};

use axum::{
    body::Body,
    http::{header, Method, Request, StatusCode},
    response::Response,
    routing::get,
    Router,
};
use http_body_util::BodyExt;
use ops::{protect, OpsConfig};
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use tower::ServiceExt;
use tower_http::timeout::TimeoutLayer;

const TOKEN: &str = "test-only-operator-bearer-not-a-real-secret";

fn config(limit: usize) -> OpsConfig {
    OpsConfig {
        max_in_flight: limit,
        metrics_token: Some(TOKEN.into()),
    }
}

async fn send(app: &Router, path: &str, token: Option<&str>) -> Response {
    let mut request = Request::builder().uri(path);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, token);
    }
    tokio::time::timeout(
        Duration::from_secs(3),
        app.clone().oneshot(request.body(Body::empty()).unwrap()),
    )
    .await
    .expect("request must not queue")
    .unwrap()
}

async fn body(response: Response) -> String {
    String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap()
}

async fn scrape(app: &Router) -> String {
    let response = send(app, "/internal/metrics", Some(&format!("Bearer {TOKEN}"))).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/plain; version=0.0.4; charset=utf-8"
    );
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    body(response).await
}

fn value(text: &str, prefix: &str) -> f64 {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix))
        .unwrap_or_else(|| panic!("missing metric {prefix}"))
        .trim()
        .parse()
        .unwrap()
}

fn blocking_app(limit: usize) -> (Router, Arc<Semaphore>, Arc<Semaphore>) {
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let handler_entered = entered.clone();
    let handler_release = release.clone();
    let app = Router::new()
        .route(
            "/hold",
            get(move || {
                let entered = handler_entered.clone();
                let release = handler_release.clone();
                async move {
                    entered.add_permits(1);
                    release.acquire().await.unwrap().forget();
                    StatusCode::OK
                }
            }),
        )
        .route("/health/live", get(|| async { StatusCode::OK }))
        .route("/health/ready", get(|| async { StatusCode::OK }))
        .route("/fast", get(|| async { StatusCode::OK }));
    (protect(app, config(limit)), entered, release)
}

async fn wait_entered(entered: &Semaphore, count: u32) {
    tokio::time::timeout(Duration::from_secs(3), entered.acquire_many(count))
        .await
        .unwrap()
        .unwrap()
        .forget();
}

#[test]
fn config_is_pure_strict_and_errors_do_not_contain_secrets() {
    // Check the startup API without reading or mutating ambient environment.
    let _from_env: fn() -> error::Result<OpsConfig> = OpsConfig::from_env;
    let default = OpsConfig::from_values(None, None).unwrap();
    assert_eq!(default.max_in_flight, 32);
    assert!(default.metrics_token.is_none());
    assert_eq!(OpsConfig::default().max_in_flight, 32);
    for limit in ["1", "32", "1024"] {
        assert!(OpsConfig::from_values(Some(limit), Some(TOKEN)).is_ok());
    }
    for limit in [
        "",
        "0",
        "1025",
        "-1",
        "+1",
        " 32",
        "32 ",
        "1.0",
        "9999999999999999999999999999999",
    ] {
        assert!(OpsConfig::from_values(Some(limit), None).is_err());
    }
    for token in [
        "".to_owned(),
        "x".repeat(31),
        "x".repeat(257),
        " ".repeat(32),
        "\n".repeat(32),
        "é".repeat(32),
        "\u{7f}".repeat(32),
    ] {
        assert!(OpsConfig::from_values(None, Some(&token)).is_err());
    }
    for length in [32, 256] {
        assert!(OpsConfig::from_values(None, Some(&"!".repeat(length))).is_ok());
    }
    let secret = "never-echo-this-secret-with-a-space ";
    let error = OpsConfig::from_values(None, Some(secret)).err().unwrap();
    assert!(!format!("{error:?} {error}").contains(secret));
}

#[tokio::test]
async fn three_handlers_saturate_without_queueing_but_liveness_is_available() {
    let (app, entered, release) = blocking_app(3);
    let mut tasks = Vec::new();
    for _ in 0..3 {
        let app = app.clone();
        tasks.push(tokio::spawn(async move { send(&app, "/hold", None).await }));
    }
    wait_entered(&entered, 3).await;
    for path in ["/hold", "/health/ready", "/internal/metrics"] {
        let response = send(&app, path, None).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body(response).await).unwrap(),
            serde_json::json!({"error":{"code":"unavailable","message":"server busy, retry shortly"}})
        );
    }
    assert_eq!(
        send(&app, "/health/live?probe=1", None).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        send(&app, "/health/live/not-live", None).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    release.add_permits(3);
    for task in tasks {
        assert_eq!(task.await.unwrap().status(), StatusCode::OK);
    }
    assert_eq!(
        send(&app, "/health/ready", None).await.status(),
        StatusCode::OK
    );
    let text = scrape(&app).await;
    assert!(text.contains("admission_rejected_total{method=\"GET\",route=\"/hold\"} 1"));
    assert!(text.contains("requests_total{method=\"GET\",route=\"/hold\",status_class=\"5xx\"} 1"));
    // The metrics request itself is admitted; previous permits have all gone.
    assert_eq!(value(&text, "requests_in_flight "), 1.0);
}

#[tokio::test]
async fn cancellation_and_outer_timeout_release_admission_and_gauge() {
    let (app, entered, _) = blocking_app(1);
    let clone = app.clone();
    let task = tokio::spawn(async move { send(&clone, "/hold", None).await });
    wait_entered(&entered, 1).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(send(&app, "/fast", None).await.status(), StatusCode::OK);
    let text = scrape(&app).await;
    assert!(text.contains("requests_cancelled_total{method=\"GET\",route=\"/hold\"} 1"));
    assert!(text.contains("requests_in_flight 1\n"));

    let timed = app.layer(TimeoutLayer::with_status_code(
        StatusCode::REQUEST_TIMEOUT,
        Duration::from_millis(20),
    ));
    assert_eq!(
        send(&timed, "/hold", None).await.status(),
        StatusCode::REQUEST_TIMEOUT
    );
    assert_eq!(send(&timed, "/fast", None).await.status(), StatusCode::OK);
    let text = scrape(&timed).await;
    assert!(text.contains("requests_cancelled_total{method=\"GET\",route=\"/hold\"} 2"));
    assert!(text.contains("requests_in_flight 1\n"));
}

#[tokio::test]
async fn error_responses_release_permits_and_record_status_classes() {
    let app = protect(
        Router::new().route("/error", get(|| async { error::AppError::Internal })),
        config(1),
    );
    for _ in 0..3 {
        assert_eq!(
            send(&app, "/error", None).await.status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
    let text = scrape(&app).await;
    assert!(text.contains("requests_total{method=\"GET\",route=\"/error\",status_class=\"5xx\"} 3"));
    assert!(text.contains("requests_in_flight 1\n"));
}

#[tokio::test]
async fn metrics_are_closed_by_default_and_only_exact_operator_bearer_is_accepted() {
    let closed = protect(Router::new(), OpsConfig::default());
    for token in [None, Some(TOKEN)] {
        let response = send(&closed, "/internal/metrics", token).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }
    let app = protect(Router::new(), config(32));
    for token in [
        None,
        Some(TOKEN),
        Some("Bearer wrong"),
        Some("bearer test-only-operator-bearer-not-a-real-secret"),
        Some("Bearer test-only-operator-bearer-not-a-real-secreu"),
    ] {
        let response = send(&app, "/internal/metrics", token).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let text = body(response).await;
        assert!(!text.contains(TOKEN));
        assert!(!text.contains("wrong"));
    }
    let request = Request::builder()
        .uri("/internal/metrics")
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let text = scrape(&app).await;
    assert!(!text.contains(TOKEN));
    let request = Request::builder()
        .uri("/internal/metrics")
        .method(Method::OPTIONS)
        .header(header::ORIGIN, "https://attacker.invalid")
        .header("access-control-request-method", "GET")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert!(!response
        .headers()
        .contains_key("access-control-allow-origin"));
}

#[tokio::test]
async fn templates_not_path_values_and_unknown_methods_are_bounded() {
    let app = protect(
        Router::new().route("/plans/{id}", get(|| async { StatusCode::OK })),
        config(32),
    );
    for i in 0..200 {
        let response = send(
            &app,
            &format!("/plans/private-user-{i}?token=hidden-{i}"),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        send(&app, &format!("/unknown-private-{i}"), None).await;
        let request = Request::builder()
            .uri("/plans/secret")
            .method(format!("CUSTOM{i}").as_str())
            .body(Body::empty())
            .unwrap();
        app.clone().oneshot(request).await.unwrap();
    }
    let text = scrape(&app).await;
    assert!(text
        .contains("requests_total{method=\"GET\",route=\"/plans/{id}\",status_class=\"2xx\"} 200"));
    assert!(text.contains("route=\"unmatched\""));
    assert!(text.contains("method=\"OTHER\""));
    for private in [
        "private-user",
        "unknown-private",
        "hidden-",
        "secret",
        "CUSTOM",
    ] {
        assert!(!text.contains(private));
    }
    assert_eq!(
        text.lines()
            .filter(|l| l.starts_with("request_duration_seconds_count{"))
            .count(),
        4
    );
}

#[tokio::test]
async fn many_server_templates_overflow_at_128_keys_and_escape_labels() {
    let mut router = Router::new();
    for i in 0..160 {
        router = router.route(
            &format!("/template-{i}/{{id}}"),
            get(|| async { StatusCode::OK }),
        );
    }
    // Quote and backslash are server-defined template characters, not user values.
    router = router.route("/quoted\"\\/{id}", get(|| async { StatusCode::OK }));
    let app = protect(router, config(32));
    send(&app, "/quoted\"\\/private-value", None).await;
    for i in 0..160 {
        send(&app, &format!("/template-{i}/private-value"), None).await;
    }
    let text = scrape(&app).await;
    assert_eq!(
        text.lines()
            .filter(|l| l.starts_with("request_duration_seconds_count{"))
            .count(),
        128
    );
    assert!(text.contains("method=\"OTHER\",route=\"overflow\""));
    assert!(text.contains("route=\"/quoted\\\"\\\\/{id}\""));
    assert!(!text.contains("private-value"));
}

#[tokio::test]
async fn protect_preserves_generic_router_state() {
    let router: Router<Arc<String>> = Router::new().route(
        "/state",
        get(
            |axum::extract::State(state): axum::extract::State<Arc<String>>| async move {
                state.as_str().to_owned()
            },
        ),
    );
    let app = protect(router, OpsConfig::default()).with_state(Arc::new("unchanged".to_owned()));
    assert_eq!(body(send(&app, "/state", None).await).await, "unchanged");
}
