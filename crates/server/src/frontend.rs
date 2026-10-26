//! Public SSR shells contain no session state. The browser fetches /api/me.
use std::path::PathBuf;

use axum::{
    extract::Request,
    http::{header, HeaderValue},
    middleware::{self, Next},
    response::{Html, Response},
    routing::get,
    Router,
};
use tower_http::services::ServeDir;

pub const HTML_CSP: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'self'; frame-ancestors 'none'; form-action 'self'";
pub const API_CSP: &str = "default-src 'none'; frame-ancestors 'none'; base-uri 'none'";

pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    routes_at(
        std::env::var_os("FRONTEND_DIST")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("crates/web/dist")),
    )
}

fn routes_at<S: Clone + Send + Sync + 'static>(dist: PathBuf) -> Router<S> {
    let mut router = Router::new();
    for path in [
        "/",
        "/setup",
        "/planner",
        "/account",
        "/privacy",
        "/terms",
        "/demo",
        "/demo/setup",
        "/demo/planner",
        "/demo/account",
    ] {
        router = router.route(
            path,
            get(move || async move { Html(debarred_web::render_document(path)) }),
        );
    }
    // Only generated public assets are served, never the workspace or dist's parent.
    // ServeDir rejects parent components rather than falling back to an HTML shell.
    router.nest_service(
        "/assets",
        ServeDir::new(dist.join("assets")).precompressed_gzip(),
    )
}

pub fn security_headers(response: &mut Response, secure: bool) {
    let html = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/html"));
    let headers = response.headers_mut();
    headers
        .entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(if html { HTML_CSP } else { API_CSP }),
    );
    if secure {
        headers.insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000"),
        );
    }
}

pub async fn preview_policy(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    security_headers(&mut response, false);
    response
}

pub fn with_preview_policy(router: Router) -> Router {
    router.layer(middleware::from_fn(preview_policy))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[test]
    fn wasm_compilation_is_allowed_only_for_html_without_javascript_eval() {
        use axum::response::IntoResponse;
        for (mut response, html) in [
            (Html("<!doctype html><html></html>").into_response(), true),
            (
                axum::Json(serde_json::json!({"status": "ok"})).into_response(),
                false,
            ),
        ] {
            security_headers(&mut response, false);
            let csp = response.headers()[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap();
            assert_eq!(csp.contains("'wasm-unsafe-eval'"), html);
            assert!(!csp.contains("'unsafe-eval'"));
            assert!(!csp.contains("'unsafe-inline'"));
            if html {
                assert!(csp.contains("script-src 'self' 'wasm-unsafe-eval'"));
            } else {
                assert_eq!(
                    csp,
                    "default-src 'none'; frame-ancestors 'none'; base-uri 'none'"
                );
            }
        }
    }

    #[tokio::test]
    async fn serves_precompressed_gzip_with_original_mime_and_uncompressed_fallback() {
        let dir = std::env::temp_dir().join(format!("debarred-gzip-{}", uuid::Uuid::new_v4()));
        let assets = dir.join("assets");
        std::fs::create_dir_all(&assets).unwrap();
        // Valid gzip stream containing an empty body; no runtime compressor is needed.
        let gzip = [
            31, 139, 8, 0, 0, 0, 0, 0, 0, 3, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let files = [
            ("app_bg.wasm", "application/wasm"),
            ("app.js", "text/javascript"),
            ("app.css", "text/css"),
            ("font.woff2", "font/woff2"),
            ("icon.svg", "image/svg+xml"),
        ];
        for (file, _) in files {
            std::fs::write(assets.join(file), b"original asset").unwrap();
            std::fs::write(assets.join(format!("{file}.gz")), gzip).unwrap();
        }
        std::fs::write(assets.join("boot.js"), b"uncompressed fallback").unwrap();
        std::fs::write(dir.join(".env.gz"), b"SECRET_OUTSIDE_PUBLIC_ASSETS").unwrap();
        let app = with_preview_policy(routes_at(dir.clone()));
        for (file, mime) in files.into_iter().chain([("boot.js", "text/javascript")]) {
            for accepts_gzip in [true, false] {
                let mut request = Request::builder().uri(format!("/assets/{file}"));
                if accepts_gzip {
                    request = request.header(header::ACCEPT_ENCODING, "gzip");
                }
                let response = app
                    .clone()
                    .oneshot(request.body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(response.headers()[header::CONTENT_TYPE], mime);
                let compressed = accepts_gzip && file != "boot.js";
                assert_eq!(
                    response
                        .headers()
                        .get(header::CONTENT_ENCODING)
                        .and_then(|v| v.to_str().ok()),
                    compressed.then_some("gzip")
                );
                let body = response.into_body().collect().await.unwrap().to_bytes();
                let expected: &[u8] = if compressed {
                    &gzip
                } else if file == "boot.js" {
                    b"uncompressed fallback"
                } else {
                    b"original asset"
                };
                assert_eq!(body.as_ref(), expected);
            }
        }
        for path in [
            "/assets/../.env",
            "/assets/%2e%2e/.env",
            "/assets/%2e%2e%2f.env",
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header(header::ACCEPT_ENCODING, "gzip")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert!(!String::from_utf8_lossy(&body).contains("SECRET_OUTSIDE_PUBLIC_ASSETS"));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn serves_only_public_assets_with_correct_types_and_no_html_fallback() {
        let dir = std::env::temp_dir().join(format!("debarred-assets-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join(".env"), "SECRET_OUTSIDE_PUBLIC_ASSETS").unwrap();
        for (file, body) in [
            ("app.js", "export {}"),
            ("boot.js", "export {}"),
            ("app.css", "body {}"),
            ("app_bg.wasm", "wasm-test"),
        ] {
            std::fs::write(dir.join("assets").join(file), body).unwrap();
        }
        let app = with_preview_policy(routes_at(dir.clone()));
        for (path, status, content_type) in [
            ("/assets/app.js", StatusCode::OK, Some("text/javascript")),
            ("/assets/boot.js", StatusCode::OK, Some("text/javascript")),
            ("/assets/app.css", StatusCode::OK, Some("text/css")),
            (
                "/assets/app_bg.wasm",
                StatusCode::OK,
                Some("application/wasm"),
            ),
            ("/assets/missing.js", StatusCode::NOT_FOUND, None),
            ("/assets/../.env", StatusCode::NOT_FOUND, None),
            ("/assets/%2e%2e/.env", StatusCode::NOT_FOUND, None),
            ("/assets/%2e%2e%2f.env", StatusCode::NOT_FOUND, None),
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), status, "{path}");
            assert_eq!(
                response
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok()),
                content_type,
                "{path}"
            );
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            assert!(!String::from_utf8_lossy(&bytes).contains("SECRET_OUTSIDE_PUBLIC_ASSETS"));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
