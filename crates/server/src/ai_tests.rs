use super::*;

use axum::{
    body::Body,
    http::{Request, StatusCode},
    response::IntoResponse,
};
use debarred_domain::{AttendanceSnapshot, Course};
use http_body_util::BodyExt;
use sqlx::postgres::PgPoolOptions;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::TcpListener,
    sync::Mutex,
};
use tower::ServiceExt;

fn date(value: &str) -> NaiveDate {
    value.parse().unwrap()
}

#[test]
fn quota_day_rolls_over_at_ist_midnight_not_utc_midnight() {
    let before: chrono::DateTime<Utc> = "2026-11-16T18:29:59Z".parse().unwrap();
    let midnight: chrono::DateTime<Utc> = "2026-11-16T18:30:00Z".parse().unwrap();
    let after: chrono::DateTime<Utc> = "2026-11-17T00:00:00Z".parse().unwrap();
    assert_eq!(before.date_naive(), midnight.date_naive());
    assert_eq!(quota_day(before).unwrap(), date("2026-11-16"));
    assert_eq!(quota_day(midnight).unwrap(), date("2026-11-17"));
    assert_eq!(quota_day(after).unwrap(), quota_day(midnight).unwrap());
    assert!(quota_day(chrono::DateTime::<Utc>::MAX_UTC).is_err());
}

fn fixture() -> (Calendar, Plan) {
    let mut config = Calendar::winter_2026().unwrap().config().clone();
    config.id = "ai-test".into();
    config.start_date = date("2026-11-16");
    config.end_date = date("2026-11-23");
    config.last_instructional_day = date("2026-11-20");
    config.minimum_percentage = 75;
    config.holidays.clear();
    config.day_notes.clear();
    config.working_saturdays.clear();
    config.exam_windows.clear();
    config.checkpoints = vec![
        debarred_domain::Checkpoint {
            id: "past".into(),
            label: "Past".into(),
            start_date: date("2026-11-17"),
        },
        debarred_domain::Checkpoint {
            id: "final".into(),
            label: "Final".into(),
            start_date: date("2026-11-23"),
        },
    ];
    let calendar = Calendar::new(config).unwrap();
    let plan = Plan {
        schema_version: 1,
        semester_id: calendar.config().id.clone(),
        calendar_revision: calendar.config().revision,
        timetable: Timetable {
            courses: vec![
                Course {
                    code: "A".into(),
                    name: "Private course name".into(),
                },
                Course {
                    code: "B".into(),
                    name: "Other".into(),
                },
            ],
            grid: [
                GridDay::Mon,
                GridDay::Tue,
                GridDay::Wed,
                GridDay::Thu,
                GridDay::Fri,
            ]
            .into_iter()
            .map(|day| {
                (
                    day,
                    std::array::from_fn(|i| match i {
                        0 => Some("A".into()),
                        1 => Some("B".into()),
                        _ => None,
                    }),
                )
            })
            .collect(),
        },
        attendance: AttendanceSnapshot {
            as_of: date("2026-11-17"),
            attended: BTreeMap::from([("A".into(), 2), ("B".into(), 1)]),
        },
        leave: vec![],
    };
    (calendar, plan)
}

fn intent(action: HelperAction) -> HelperIntent {
    HelperIntent {
        action,
        course_codes: vec![],
        checkpoint: None,
        dates: vec![],
    }
}

fn config(url: Option<url::Url>) -> Arc<Config> {
    Arc::new(Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        public_origin: "http://localhost".into(),
        supabase_url: "http://localhost/".parse().unwrap(),
        supabase_anon_key: "unused".into(),
        database_url: "postgres://unused:unused@127.0.0.1:1/unused".into(),
        session_key: [7; 32],
        secure_cookies: false,
        local_auth_enabled: false,
        session_ttl_seconds: 60,
        database_max_connections: 1,
        trusted_proxy_ips: Vec::new(),
        auth_requests_per_minute: 20,
        helper_daily_limit: 3,
        image_daily_limit: 3,
        ai_concurrency: 1,
        ai: url.map(|base_url| crate::config::AiConfig {
            base_url,
            api_key: "secret-provider-key".into(),
            model: "test-model".into(),
        }),
    })
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap()
}

fn lazy_store() -> Store {
    Store::new(
        PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(30))
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .unwrap(),
    )
}

fn completion(content: Value) -> String {
    json!({"choices":[{"finish_reason":"stop","message":{"content":content.to_string()}}]})
        .to_string()
}

struct Provider {
    url: url::Url,
    calls: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn provider(status: StatusCode, body: String, delay: Duration) -> Provider {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = calls.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(payload): Json<Value>| {
            let calls = recorded.clone();
            let body = body.clone();
            async move {
                calls.lock().unwrap().push(payload);
                tokio::time::sleep(delay).await;
                (status, body)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Provider { url, calls, task }
}

#[test]
fn deterministic_arithmetic_filtering_and_historical_checkpoints() {
    let (calendar, plan) = fixture();
    let response = deterministic_helper(&calendar, &plan, &intent(HelperAction::Status)).unwrap();
    assert!(response.quota.is_none());
    assert!(matches!(
        &response.projections[0],
        CheckpointProjection::Historical { .. }
    ));
    assert!(response
        .answer
        .contains("historical; aggregate attendance cannot reconstruct"));
    let CheckpointProjection::Upcoming { courses, .. } = &response.projections[1] else {
        panic!()
    };
    assert_eq!((courses[0].total_attended, courses[0].total_held), (5, 5));
    assert_eq!(
        (
            courses[1].total_attended,
            courses[1].minimum_attended_required
        ),
        (4, 4)
    );
    assert_eq!(response.allowances.len(), 2);
    assert_eq!(response.allowances[0].allowance.additional, 1);
    assert_eq!(response.allowances[1].allowance.additional, 0);
    let mut selected = intent(HelperAction::MaxSkips);
    selected.course_codes = vec!["B".into()];
    selected.checkpoint = Some("final".into());
    let filtered = deterministic_helper(&calendar, &plan, &selected).unwrap();
    assert_eq!(filtered.projections.len(), 1);
    assert_eq!(filtered.allowances.len(), 1);
    assert_eq!(filtered.allowances[0].course_code, "B");
    selected.checkpoint = Some("past".into());
    let past = deterministic_helper(&calendar, &plan, &selected).unwrap();
    assert!(past.allowances.is_empty());
}

#[test]
fn skip_dates_are_hypothetical_course_specific_and_preserve_existing_leave() {
    let (calendar, mut plan) = fixture();
    plan.leave.push(LeaveEntry::Slot {
        date: date("2026-11-19"),
        slot: debarred_domain::SlotIndex::new(0).unwrap(),
    });
    let before = plan.clone();
    let mut selected = intent(HelperAction::SkipDates);
    selected.course_codes = vec!["A".into()];
    selected.dates = vec![date("2026-11-18"), date("2026-11-19")];
    let response = deterministic_helper(&calendar, &plan, &selected).unwrap();
    assert_eq!(plan, before);
    assert_eq!(response.allowances[0].allowance.already_planned, 2);
    assert_eq!(response.allowances[0].allowance.shortfall, 1);
    assert!(response.answer.contains("Nothing has been saved"));
}

#[test]
fn rejects_invalid_intents_and_invalid_plans() {
    let (calendar, plan) = fixture();
    let mut cases = Vec::new();
    for checkpoint in ["unknown", ""] {
        let mut i = intent(HelperAction::Status);
        i.checkpoint = Some(checkpoint.into());
        cases.push(i);
    }
    for codes in [
        vec!["UNKNOWN".into()],
        vec!["A".into(), "A".into()],
        vec!["A".into(); 65],
    ] {
        let mut i = intent(HelperAction::Status);
        i.course_codes = codes;
        cases.push(i);
    }
    cases.push(intent(HelperAction::SkipDates));
    let mut status_dates = intent(HelperAction::Status);
    status_dates.dates.push(date("2026-11-18"));
    cases.push(status_dates);
    for dates in [
        vec![date("2026-11-17")],
        vec![date("2026-11-24")],
        vec![date("2026-11-21")],
        vec![date("2026-11-18"); 2],
        vec![date("2026-11-18"); 65],
    ] {
        let mut i = intent(HelperAction::SkipDates);
        i.dates = dates;
        cases.push(i);
    }
    for i in cases {
        assert!(deterministic_helper(&calendar, &plan, &i).is_err(), "{i:?}");
    }
    let mut bad_plan = plan;
    bad_plan.attendance.attended.insert("A".into(), 999);
    assert!(deterministic_helper(&calendar, &bad_plan, &intent(HelperAction::Status)).is_err());
}

#[test]
fn strict_deserialization_rejects_facts_and_unknown_fields() {
    for value in [
        json!({"action":"status","answer":"100%"}),
        json!({"action":"status","attendance":10}),
        json!({"action":"invent"}),
    ] {
        assert!(serde_json::from_value::<HelperIntent>(value).is_err());
    }
    let (_, plan) = fixture();
    assert!(serde_json::from_value::<HelperRequest>(
        json!({"plan":plan,"question":"status","facts":{}})
    )
    .is_err());
    assert!(serde_json::from_value::<ImportRequest>(
        json!({"image_data_url":"x","url":"http://example.com"})
    )
    .is_err());
}

#[test]
fn image_validation_checks_type_encoding_signature_and_decoded_size() {
    for (mime, bytes) in [
        ("png", b"\x89PNG\r\n\x1a\n".as_slice()),
        ("jpeg", &[0xff, 0xd8, 0xff]),
        ("webp", b"RIFFabcdWEBP"),
    ] {
        assert!(validate_image(&format!(
            "data:image/{mime};base64,{}",
            STANDARD.encode(bytes)
        ))
        .is_ok());
    }
    for input in [
        "https://example.com/image.png",
        "data:image/svg+xml;base64,PHN2Zz4=",
        "data:image/png;base64,!!!!",
        "data:image/png;base64,",
        "data:image/png;base64,/9j/",
    ] {
        assert!(validate_image(input).is_err(), "{input}");
    }
    let mut bytes = vec![0; MAX_IMAGE_BYTES];
    bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    assert!(validate_image(&format!(
        "data:image/png;base64,{}",
        STANDARD.encode(&bytes)
    ))
    .is_ok());
    bytes.push(0);
    assert!(validate_image(&format!("data:image/png;base64,{}", STANDARD.encode(bytes))).is_err());
}

#[test]
fn provider_decoder_rejects_prose_fences_truncation_and_extra_facts() {
    let good = completion(json!({"action":"status"}));
    assert!(decode_completion::<HelperIntent>(good.as_bytes()).is_ok());
    for body in ["not json".to_owned(), completion(json!({"action":"status","remaining":999})),
        json!({"choices":[]}).to_string(),
        json!({"choices":[{"finish_reason":"length","message":{"content":"{}"}}]}).to_string(),
        json!({"choices":[{"finish_reason":"stop","message":{"content":"```json\n{\"action\":\"status\"}\n```"}}]}).to_string()] {
        assert!(decode_completion::<HelperIntent>(body.as_bytes()).is_err());
    }
    for timetable in [
        json!({"courses":[],"grid":{"Mon":[null]}}),
        json!({"courses":[],"grid":{},"attendance":0}),
    ] {
        assert!(decode_completion::<Timetable>(completion(timetable).as_bytes()).is_err());
    }
    let invalid: Timetable = decode_completion(
        completion(json!({"courses":[],"grid":{"Mon":["UNKNOWN",null,null,null,null,null,null]}}))
            .as_bytes(),
    )
    .unwrap();
    assert!(invalid.validate().is_err());
}

#[test]
fn prompts_do_not_send_personal_attendance_or_ask_for_arithmetic() {
    let (calendar, plan) = fixture();
    let messages = intent_messages(
        &calendar,
        &plan,
        "Can I miss Wednesday?",
        date("2026-11-18"),
    );
    let user: Value = serde_json::from_str(messages[1]["content"].as_str().unwrap()).unwrap();
    let keys: BTreeSet<_> = user
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        BTreeSet::from([
            "calendar",
            "course_codes",
            "question",
            "snapshot_date",
            "today"
        ])
    );
    let prompt = messages.to_string();
    assert!(!prompt.contains("Private course name"));
    assert!(!prompt.contains("attended"));
    assert!(!prompt.contains("timetable"));
    assert!(prompt.contains("Never calculate"));
    let image = image_messages("data:image/png;base64,example");
    assert!(image[0]["content"].as_str().unwrap().contains("A11"));
    assert!(image[0]["content"].as_str().unwrap().contains("08:30"));
    assert_eq!(
        image[1]["content"][1]["image_url"]["url"],
        "data:image/png;base64,example"
    );
}

#[test]
fn intent_prompt_anchors_relative_dates_to_injected_today_not_stale_snapshot() {
    let (calendar, plan) = fixture();
    let today = date("2026-11-20");
    let messages = intent_messages(&calendar, &plan, "Can I skip next Friday?", today);
    let user: Value = serde_json::from_str(messages[1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(user["today"], "2026-11-20");
    assert_eq!(user["snapshot_date"], "2026-11-17");
    assert_ne!(user["today"], user["snapshot_date"]);
    assert!(messages[0]["content"].as_str().unwrap().contains(
        "Resolve relative dates against today (the current campus date), not snapshot_date"
    ));
}

#[tokio::test]
async fn deterministic_paths_work_disabled_and_invalid_requests_do_not_reach_db() {
    let service = AiService::new(client(), config(None));
    let (calendar, plan) = fixture();
    let store = lazy_store();
    for question in ["status", "max skips"] {
        let response = service
            .helper(
                &store,
                Uuid::nil(),
                &calendar,
                HelperRequest {
                    plan: plan.clone(),
                    intent: None,
                    question: Some(question.into()),
                },
            )
            .await
            .unwrap();
        assert!(response.quota.is_none());
    }
    let response = service
        .helper(
            &store,
            Uuid::nil(),
            &calendar,
            HelperRequest {
                plan: plan.clone(),
                intent: Some(intent(HelperAction::Status)),
                question: None,
            },
        )
        .await
        .unwrap();
    assert!(response.quota.is_none());
    for (i, q) in [
        (None, None),
        (Some(intent(HelperAction::Status)), Some("status".into())),
        (None, Some(" ".into())),
        (None, Some("é".repeat(501))),
    ] {
        assert!(matches!(
            service
                .helper(
                    &store,
                    Uuid::nil(),
                    &calendar,
                    HelperRequest {
                        plan: plan.clone(),
                        intent: i,
                        question: q
                    }
                )
                .await,
            Err(AppError::BadRequest(_))
        ));
    }
    assert!(matches!(
        service
            .helper(
                &store,
                Uuid::nil(),
                &calendar,
                HelperRequest {
                    plan,
                    intent: None,
                    question: Some("Can I skip classes?".into())
                }
            )
            .await,
        Err(AppError::Unavailable("AI provider is disabled"))
    ));
    assert!(matches!(
        service
            .import(
                &store,
                Uuid::nil(),
                ImportRequest {
                    image_data_url: "https://example.com".into()
                }
            )
            .await,
        Err(AppError::BadRequest(_))
    ));
}

#[tokio::test]
async fn concurrency_is_shared_by_clones_rejects_immediately_and_releases() {
    let service = AiService::new(
        client(),
        config(Some("http://localhost/v1/".parse().unwrap())),
    );
    let permit = service.admit().unwrap();
    let clone = service.clone();
    assert!(matches!(clone.admit(), Err(AppError::RateLimited)));
    let (calendar, plan) = fixture();
    let result = clone
        .helper(
            &lazy_store(),
            Uuid::nil(),
            &calendar,
            HelperRequest {
                plan,
                intent: None,
                question: Some("Can I skip classes?".into()),
            },
        )
        .await;
    assert!(matches!(result, Err(AppError::RateLimited)));
    drop(permit);
    assert!(clone.admit().is_ok());
}

#[tokio::test]
async fn provider_request_is_single_bounded_extraction_call() {
    let mock = provider(
        StatusCode::OK,
        completion(
            json!({"action":"max_skips","course_codes":["A"],"checkpoint":"final","dates":[]}),
        ),
        Duration::ZERO,
    )
    .await;
    let service = AiService::new(client(), config(Some(mock.url.clone())));
    let (calendar, plan) = fixture();
    let extracted: HelperIntent = service
        .complete(intent_messages(
            &calendar,
            &plan,
            "How many A classes can I miss?",
            date("2026-11-18"),
        ))
        .await
        .unwrap();
    let response = deterministic_helper(&calendar, &plan, &extracted).unwrap();
    assert_eq!(response.allowances[0].allowance.additional, 1);
    let calls = mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["max_tokens"], 2048);
    assert_eq!(calls[0]["stream"], false);
    assert_eq!(calls[0]["response_format"]["type"], "json_object");
}

#[tokio::test]
async fn provider_errors_and_advertised_caps_are_sanitized() {
    for (status, body) in [
        (
            StatusCode::BAD_REQUEST,
            "secret-provider-key private provider error".into(),
        ),
        (StatusCode::OK, "x".repeat(MAX_RESPONSE_BYTES + 1)),
        (StatusCode::OK, "not json".into()),
        (StatusCode::TEMPORARY_REDIRECT, String::new()),
    ] {
        let mock = provider(status, body, Duration::ZERO).await;
        let service = AiService::new(client(), config(Some(mock.url.clone())));
        let error = service
            .complete::<HelperIntent>(json!([]))
            .await
            .unwrap_err();
        assert!(matches!(error, AppError::Unavailable(_)));
        let text = error.to_string();
        assert!(!text.contains("secret-provider-key"));
        assert!(!text.contains("private provider error"));
        assert_eq!(mock.calls.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn provider_timeouts_are_sanitized() {
    let mock = provider(
        StatusCode::OK,
        completion(json!({"action":"status"})),
        Duration::from_millis(150),
    )
    .await;
    let short_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_millis(30))
        .build()
        .unwrap();
    let service = AiService::new(short_client, config(Some(mock.url.clone())));
    assert!(matches!(
        service.complete::<HelperIntent>(json!([])).await,
        Err(AppError::Unavailable(_))
    ));
}

#[tokio::test]
async fn streaming_response_cap_does_not_depend_on_content_length() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1/", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let thread = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = [0; 8192];
        let _ = socket.read(&mut request);
        let _ = socket.write_all(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        );
        for _ in 0..17 {
            if socket.write_all(b"1000\r\n").is_err() {
                break;
            }
            if socket.write_all(&[b'x'; 4096]).is_err() {
                break;
            }
            if socket.write_all(b"\r\n").is_err() {
                break;
            }
        }
        let _ = socket.write_all(b"0\r\n\r\n");
    });
    let service = AiService::new(client(), config(Some(url)));
    assert!(matches!(
        service.complete::<HelperIntent>(json!([])).await,
        Err(AppError::Unavailable(_))
    ));
    thread.join().unwrap();
}

#[tokio::test]
async fn routes_require_authentication() {
    let config = config(None);
    let client = client();
    let store = lazy_store();
    let state = AppState {
        auth: crate::auth::AuthService::new(store.pool().clone(), client.clone(), config.clone()),
        ai: AiService::new(client, config.clone()),
        config,
        store,
        calendar: Arc::new(fixture().0),
    };
    let response = routes()
        .with_state(state)
        .oneshot(
            Request::post("/api/helper")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(std::str::from_utf8(&body).unwrap().contains("unauthorized"));
    assert_eq!(
        AppError::RateLimited.into_response().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
#[ignore = "requires explicit TEST_DATABASE_URL pointing to disposable PostgreSQL"]
async fn billed_attempts_are_atomic_count_failures_and_import_does_not_save() {
    let url =
        std::env::var("TEST_DATABASE_URL").expect("set TEST_DATABASE_URL to a disposable database");
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .unwrap();
    sqlx::raw_sql("CREATE SCHEMA IF NOT EXISTS auth; CREATE TABLE IF NOT EXISTS auth.users (id uuid PRIMARY KEY);").execute(&pool).await.unwrap();
    crate::db::MIGRATOR.run(&pool).await.unwrap();
    let user = Uuid::new_v4();
    sqlx::query("INSERT INTO auth.users (id) VALUES ($1)")
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
    let store = Store::new(pool.clone());
    let day = debarred_domain::time::campus_date(Utc::now()).unwrap();
    let mock = provider(
        StatusCode::BAD_GATEWAY,
        "private error".into(),
        Duration::ZERO,
    )
    .await;
    let service = AiService::new(client(), config(Some(mock.url.clone())));
    let (calendar, plan) = fixture();
    for _ in 0..3 {
        assert!(matches!(
            service
                .helper(
                    &store,
                    user,
                    &calendar,
                    HelperRequest {
                        plan: plan.clone(),
                        intent: None,
                        question: Some("Can I miss Wednesday?".into())
                    }
                )
                .await,
            Err(AppError::Unavailable(_))
        ));
    }
    assert!(matches!(
        service
            .helper(
                &store,
                user,
                &calendar,
                HelperRequest {
                    plan: plan.clone(),
                    intent: None,
                    question: Some("Can I miss Wednesday?".into())
                }
            )
            .await,
        Err(AppError::RateLimited)
    ));
    assert_eq!(
        store
            .read_quota(user, QuotaKind::Helper, day, 3)
            .await
            .unwrap()
            .used,
        3
    );
    assert_eq!(mock.calls.lock().unwrap().len(), 3);
    let image_provider = provider(
        StatusCode::OK,
        completion(json!({"courses":[],"grid":{}})),
        Duration::ZERO,
    )
    .await;
    let image_service = AiService::new(client(), config(Some(image_provider.url.clone())));
    let imported = image_service
        .import(
            &store,
            user,
            ImportRequest {
                image_data_url: format!(
                    "data:image/png;base64,{}",
                    STANDARD.encode(b"\x89PNG\r\n\x1a\n")
                ),
            },
        )
        .await
        .unwrap();
    assert_eq!(imported.quota.used, 1);
    // Race the two remaining image quota slots; only two may succeed.
    let mut attempts = tokio::task::JoinSet::new();
    for _ in 0..12 {
        let store = store.clone();
        attempts.spawn(async move { store.consume_quota(user, QuotaKind::Image, day, 3).await });
    }
    let mut accepted = 0;
    while let Some(result) = attempts.join_next().await {
        match result.unwrap() {
            Ok(_) => accepted += 1,
            Err(AppError::RateLimited) => {}
            Err(error) => panic!("unexpected quota error: {error}"),
        }
    }
    assert_eq!(accepted, 2);
    assert_eq!(
        store
            .read_quota(user, QuotaKind::Image, day, 3)
            .await
            .unwrap()
            .used,
        3
    );
    assert!(store
        .load_plan(user, "winter-2026")
        .await
        .unwrap()
        .is_none());
    sqlx::query("DELETE FROM auth.users WHERE id = $1")
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
}
