//! Per-router admission control and operator-only, bounded Prometheus metrics.
use std::{
    collections::BTreeMap,
    fmt::Write,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

use axum::{
    extract::{MatchedPath, Request},
    http::{header, HeaderMap, HeaderValue},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::{AppError, Result};

/// Deliberately not Debug: the bearer token is a secret.
#[derive(Clone)]
pub struct OpsConfig {
    pub max_in_flight: usize,
    pub metrics_token: Option<String>,
}

impl Default for OpsConfig {
    fn default() -> Self {
        Self {
            max_in_flight: 32,
            metrics_token: None,
        }
    }
}

impl OpsConfig {
    /// Reads only process environment; never reads a dotenv file.
    pub fn from_env() -> Result<Self> {
        fn optional(name: &str) -> Result<Option<String>> {
            match std::env::var(name) {
                Ok(value) => Ok(Some(value)),
                Err(std::env::VarError::NotPresent) => Ok(None),
                Err(_) => Err(AppError::BadRequest(format!(
                    "{name} must be valid Unicode"
                ))),
            }
        }
        let limit = optional("HTTP_MAX_IN_FLIGHT")?;
        let token = optional("METRICS_BEARER_TOKEN")?;
        Self::from_values(limit.as_deref(), token.as_deref())
    }

    /// Pure parser, also suitable for tests without mutating process environment.
    pub fn from_values(max_in_flight: Option<&str>, metrics_token: Option<&str>) -> Result<Self> {
        let config = Self {
            max_in_flight: match max_in_flight {
                None => 32,
                Some(value) if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) => {
                    value.parse().map_err(|_| invalid_limit())?
                }
                Some(_) => return Err(invalid_limit()),
            },
            metrics_token: metrics_token.map(str::to_owned),
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if !(1..=1024).contains(&self.max_in_flight) {
            return Err(invalid_limit());
        }
        if self.metrics_token.as_ref().is_some_and(|token| {
            !(32..=256).contains(&token.len()) || !token.bytes().all(|b| (0x21..=0x7e).contains(&b))
        }) {
            return Err(AppError::BadRequest(
                "METRICS_BEARER_TOKEN must contain 32..=256 visible ASCII bytes".into(),
            ));
        }
        Ok(())
    }
}

fn invalid_limit() -> AppError {
    AppError::BadRequest("HTTP_MAX_IN_FLIGHT must be an integer in 1..=1024".into())
}

const MAX_KEYS: usize = 128;
const BOUNDS: [f64; 10] = [
    0.005,
    0.010,
    0.025,
    0.050,
    0.100,
    0.250,
    0.500,
    1.0,
    5.0,
    f64::INFINITY,
];
const BOUND_LABELS: [&str; 10] = [
    "0.005", "0.01", "0.025", "0.05", "0.1", "0.25", "0.5", "1", "5", "+Inf",
];
type Key = (String, String);

#[derive(Clone, Default)]
struct Samples {
    statuses: [u64; 5],
    rejected: u64,
    cancelled: u64,
    buckets: [u64; 10],
    count: u64,
    sum: f64,
}

struct OpsState {
    semaphore: Arc<Semaphore>,
    token_hash: Option<[u8; 32]>,
    in_flight: AtomicUsize,
    samples: Mutex<BTreeMap<Key, Samples>>,
}

impl OpsState {
    fn key(&self, request: &Request) -> Key {
        let method = match request.method().as_str() {
            "GET" => "GET",
            "HEAD" => "HEAD",
            "POST" => "POST",
            "PUT" => "PUT",
            "PATCH" => "PATCH",
            "DELETE" => "DELETE",
            "OPTIONS" => "OPTIONS",
            "CONNECT" => "CONNECT",
            "TRACE" => "TRACE",
            _ => "OTHER",
        };
        // Only server-defined templates, never URI/path/query/header values.
        let route = request
            .extensions()
            .get::<MatchedPath>()
            .map(MatchedPath::as_str)
            .filter(|s| s.len() <= 256)
            .unwrap_or("unmatched");
        let key = (method.to_owned(), route.to_owned());
        let mut samples = self.samples.lock().unwrap_or_else(|e| e.into_inner());
        if samples.contains_key(&key) {
            return key;
        }
        // Reserve one key for overflow, including arbitrary extension methods.
        let key = if samples.len() < MAX_KEYS - 1 {
            key
        } else {
            ("OTHER".into(), "overflow".into())
        };
        samples.entry(key.clone()).or_default();
        key
    }

    fn observe(&self, key: &Key, elapsed: f64, status: Option<u16>, rejected: bool) {
        let mut samples = self.samples.lock().unwrap_or_else(|e| e.into_inner());
        let sample = samples.get_mut(key).expect("registered metric key");
        if let Some(status) = status {
            let class = usize::from(status / 100);
            if (1..=5).contains(&class) {
                sample.statuses[class - 1] += 1;
            }
        } else {
            sample.cancelled += 1;
        }
        sample.rejected += u64::from(rejected);
        sample.count += 1;
        sample.sum += elapsed;
        for (bucket, bound) in sample.buckets.iter_mut().zip(BOUNDS) {
            if elapsed <= bound {
                *bucket += 1;
            }
        }
    }

    fn render(&self) -> String {
        // Clone a bounded snapshot; do not hold the lock while formatting or awaiting.
        let samples = self
            .samples
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut text = String::from(
            "# HELP requests_total Completed HTTP responses, including rejected admission.\n# TYPE requests_total counter\n\
             # HELP admission_rejected_total Requests refused without queueing.\n# TYPE admission_rejected_total counter\n\
             # HELP requests_cancelled_total Dropped handler futures without a response.\n# TYPE requests_cancelled_total counter\n\
             # HELP request_duration_seconds Time to response headers or cancellation, in seconds.\n# TYPE request_duration_seconds histogram\n\
             # HELP requests_in_flight Admitted handler futures, excluding liveness bypass.\n# TYPE requests_in_flight gauge\n");
        writeln!(
            text,
            "requests_in_flight {}",
            self.in_flight.load(Ordering::Relaxed)
        )
        .unwrap();
        for ((method, route), sample) in samples {
            let labels = format!(
                "method=\"{}\",route=\"{}\"",
                escape(&method),
                escape(&route)
            );
            for (index, count) in sample.statuses.iter().enumerate() {
                writeln!(
                    text,
                    "requests_total{{{labels},status_class=\"{}xx\"}} {count}",
                    index + 1
                )
                .unwrap();
            }
            writeln!(
                text,
                "admission_rejected_total{{{labels}}} {}",
                sample.rejected
            )
            .unwrap();
            writeln!(
                text,
                "requests_cancelled_total{{{labels}}} {}",
                sample.cancelled
            )
            .unwrap();
            for (bound, count) in BOUND_LABELS.iter().zip(sample.buckets) {
                writeln!(
                    text,
                    "request_duration_seconds_bucket{{{labels},le=\"{bound}\"}} {count}"
                )
                .unwrap();
            }
            writeln!(
                text,
                "request_duration_seconds_count{{{labels}}} {}",
                sample.count
            )
            .unwrap();
            writeln!(
                text,
                "request_duration_seconds_sum{{{labels}}} {}",
                sample.sum
            )
            .unwrap();
        }
        text
    }
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}

struct Observation {
    state: Arc<OpsState>,
    key: Key,
    start: Instant,
    permit: Option<OwnedSemaphorePermit>,
    completed: bool,
}

impl Drop for Observation {
    fn drop(&mut self) {
        if !self.completed {
            self.state
                .observe(&self.key, self.start.elapsed().as_secs_f64(), None, false);
        }
        if self.permit.is_some() {
            self.state.in_flight.fetch_sub(1, Ordering::Relaxed);
        }
        // Owned permit is released even if the enclosing timeout drops this future.
    }
}

/// Adds GET /internal/metrics and admission/metrics middleware without changing S.
/// Panics on invalid explicit configuration; startup should call from_env/validate first.
/// Apply request timeout and response/security policy OUTSIDE this returned router.
/// Do not put user authentication or CORS middleware around /internal/metrics.
pub fn protect<S: Clone + Send + Sync + 'static>(
    router: Router<S>,
    config: OpsConfig,
) -> Router<S> {
    assert!(config.validate().is_ok(), "invalid OpsConfig");
    let state = Arc::new(OpsState {
        semaphore: Arc::new(Semaphore::new(config.max_in_flight)),
        token_hash: config
            .metrics_token
            .map(|token| Sha256::digest(token.as_bytes()).into()),
        in_flight: AtomicUsize::new(0),
        samples: Mutex::new(BTreeMap::new()),
    });
    let metrics_state = state.clone();
    router
        .route(
            "/internal/metrics",
            get(move |headers: HeaderMap| {
                let state = metrics_state.clone();
                async move { metrics(&state, &headers) }
            }),
        )
        .layer(middleware::from_fn(move |request: Request, next: Next| {
            let state = state.clone();
            async move { admit(state, request, next).await }
        }))
}

fn metrics(state: &OpsState, headers: &HeaderMap) -> Response {
    let response = match state.token_hash {
        None => AppError::NotFound.into_response(),
        Some(expected) => {
            let mut values = headers.get_all(header::AUTHORIZATION).iter();
            let token = values
                .next()
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .filter(|v| {
                    (32..=256).contains(&v.len()) && v.bytes().all(|b| (0x21..=0x7e).contains(&b))
                });
            let valid = token.is_some_and(|token| {
                let actual: [u8; 32] = Sha256::digest(token.as_bytes()).into();
                bool::from(expected.ct_eq(&actual))
            }) && values.next().is_none();
            if valid {
                (
                    [(
                        header::CONTENT_TYPE,
                        "text/plain; version=0.0.4; charset=utf-8",
                    )],
                    state.render(),
                )
                    .into_response()
            } else {
                AppError::Forbidden.into_response()
            }
        }
    };
    let mut response = response;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn admit(state: Arc<OpsState>, request: Request, next: Next) -> Response {
    let start = Instant::now();
    let key = state.key(&request);
    // Exact public liveness path only; readiness and metrics share normal admission.
    let live = request.uri().path() == "/health/live";
    let permit = if live {
        None
    } else {
        match state.semaphore.clone().try_acquire_owned() {
            Ok(permit) => {
                state.in_flight.fetch_add(1, Ordering::Relaxed);
                Some(permit)
            }
            Err(_) => {
                state.observe(&key, start.elapsed().as_secs_f64(), Some(503), true);
                let mut response =
                    AppError::Unavailable("server busy, retry shortly").into_response();
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
                return response;
            }
        }
    };
    let mut observation = Observation {
        state,
        key,
        start,
        permit,
        completed: false,
    };
    let response = next.run(request).await;
    observation.state.observe(
        &observation.key,
        start.elapsed().as_secs_f64(),
        Some(response.status().as_u16()),
        false,
    );
    observation.completed = true;
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_boundaries_are_cumulative_and_escape_is_prometheus_safe() {
        let state = OpsState {
            semaphore: Arc::new(Semaphore::new(1)),
            token_hash: None,
            in_flight: AtomicUsize::new(0),
            samples: Mutex::new(BTreeMap::new()),
        };
        let key = ("GET".into(), "/quoted\"\\\n".into());
        state
            .samples
            .lock()
            .unwrap()
            .insert(key.clone(), Samples::default());
        for elapsed in [
            0.005, 0.010, 0.025, 0.050, 0.100, 0.250, 0.500, 1.0, 5.0, 6.0,
        ] {
            state.observe(&key, elapsed, Some(200), false);
        }
        let samples = state.samples.lock().unwrap();
        let sample = samples.get(&key).unwrap();
        assert_eq!(sample.buckets, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        assert_eq!(sample.count, 10);
        assert!((sample.sum - 12.94).abs() < 1e-9);
        drop(samples);
        assert!(state.render().contains("route=\"/quoted\\\"\\\\\\n\""));
    }
}
