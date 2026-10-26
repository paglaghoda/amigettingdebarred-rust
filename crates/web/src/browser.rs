//! Browser I/O only. The shared domain crate still owns every attendance calculation.

use gloo_net::http::Request;
use leptos::prelude::*;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use wasm_bindgen::{closure::Closure, JsCast, JsValue};
use wasm_bindgen_futures::{spawn_local, JsFuture};

use crate::{
    model::*,
    state::{empty_plan, UiContext},
};

#[derive(Debug)]
struct ApiError {
    status: u16,
    message: String,
}

async fn api(
    ctx: UiContext,
    method: &str,
    url: &str,
    body: Option<Value>,
) -> Result<Value, ApiError> {
    let controller = web_sys::AbortController::new().map_err(|_| network_error())?;
    let abort = controller.clone();
    let _timeout = gloo_timers::callback::Timeout::new(35_000, move || abort.abort());
    let mut builder = match method {
        "POST" => Request::post(url),
        "PUT" => Request::put(url),
        _ => Request::get(url),
    }
    .credentials(web_sys::RequestCredentials::SameOrigin)
    .abort_signal(Some(&controller.signal()));
    if url != "/api/me" {
        if let AuthState::SignedIn(user) = ctx.auth.get_untracked() {
            builder = builder.header("X-Session-User", &user.user_id);
            if method != "GET" {
                builder = builder.header("X-CSRF-Token", &user.csrf_token);
            }
        }
    }
    let request = match body {
        Some(body) => builder.json(&body).map_err(|_| network_error())?,
        None => builder.build().map_err(|_| network_error())?,
    };
    let response = request.send().await.map_err(|_| network_error())?;
    let status = response.status();
    if status == 204 {
        return Ok(Value::Null);
    }
    let text = response.text().await.map_err(|_| network_error())?;
    if !response.ok() {
        let message = match status {
            401 => "Your sign-in expired or the account changed in another tab. Sign in again; your draft is preserved.".into(),
            403 => "The session security check failed. Reload your session before retrying.".into(),
            409 => "A newer cloud plan exists. Export your draft, then reload to review it.".into(),
            413 => "This upload or plan is too large.".into(),
            429 => "The request limit was reached. Wait before trying again.".into(),
            503 => "The service is unavailable. Your saved plan has not been replaced.".into(),
            _ => serde_json::from_str::<Value>(&text).ok()
                .and_then(|v| v.get("error")?.get("message")?.as_str().map(|s| s.chars().take(240).collect()))
                .unwrap_or_else(|| format!("Request failed ({status}). Check your input and try again.")),
        };
        return Err(ApiError { status, message });
    }
    serde_json::from_str(&text).map_err(|_| ApiError {
        status: 502,
        message: "The server returned an unreadable response. No local edits were discarded."
            .into(),
    })
}

fn network_error() -> ApiError {
    ApiError { status: 0, message: "Could not reach the server. Check your connection and retry; your device draft is preserved when storage is available.".into() }
}
fn decode<T: DeserializeOwned>(value: Value) -> Result<T, ApiError> {
    serde_json::from_value(value).map_err(|_| ApiError {
        status: 502,
        message: "The server response did not match this app version. Reload before saving.".into(),
    })
}
fn window() -> web_sys::Window {
    web_sys::window().expect("browser window")
}
fn storage() -> Result<web_sys::Storage, ()> {
    window().local_storage().map_err(|_| ())?.ok_or(())
}
fn claim_identity(ctx: UiContext) -> Result<(), ()> {
    crate::drafts::claim(ctx)
}
fn key(ctx: UiContext) -> Option<String> {
    crate::drafts::key(ctx)
}
fn read_draft(ctx: UiContext) -> Option<Draft> {
    crate::drafts::read(ctx)
}
pub fn persist(ctx: UiContext) {
    crate::drafts::persist(ctx);
}

pub fn boot(ctx: UiContext) {
    if matches!(ctx.page.get_untracked(), Page::Privacy | Page::Terms) {
        // Public policy reads must not load private drafts or trigger cloud saves.
        spawn_local(async move {
            if let Ok(runtime) = api(ctx, "GET", "/api/runtime", None)
                .await
                .and_then(decode::<RuntimeInfo>)
            {
                ctx.runtime.set(runtime);
            }
        });
        return;
    }
    if claim_identity(ctx).is_err() {
        ctx.notice.set(Some(
            "Device draft storage is unavailable. Export your edits before closing this tab."
                .into(),
        ));
    }
    // Restore before enabling any sandbox input; network latency must not let
    // the SSR sample overwrite a previously customized device draft.
    if ctx.is_demo() {
        if let Some(draft) = read_draft(ctx) {
            ctx.plan.set(draft.plan);
        }
        ctx.saves.update(|s| s.loaded = true);
        ctx.sync.set(if let Some(error) = ctx.validation_error() {
            SyncStatus::Invalid(error)
        } else {
            SyncStatus::LocalOnly
        });
        if ctx.identity.get_value().is_none() {
            ctx.sync.set(SyncStatus::Failed(
                "Device storage unavailable; export your draft.".into(),
            ));
        } else if key(ctx)
            .and_then(|key| storage().ok()?.get_item(&key).ok())
            .is_some_and(|entry| entry.is_none())
        {
            // Initialize only a missing, owned tab key, after restoration. Never
            // replace unreadable existing bytes or the legacy shared draft.
            persist(ctx);
        }
    }
    install_lifecycle(ctx);
    spawn_local(async move {
        match api(ctx, "GET", "/api/runtime", None)
            .await
            .and_then(decode::<RuntimeInfo>)
        {
            Ok(runtime) => ctx.runtime.set(runtime),
            Err(_) => {
                ctx.runtime.set(RuntimeInfo {
                    mode: "offline".into(),
                    ..Default::default()
                });
                ctx.notice.set(Some("Could not check server availability. The sandbox remains usable; cloud editing waits for a successful load.".into()));
            }
        }
        if ctx.is_demo() {
            return;
        }
        check_session(ctx).await;
    });
}

async fn check_session(ctx: UiContext) {
    let generation = ctx.saves.get_untracked().generation;
    match api(ctx, "GET", "/api/me", None)
        .await
        .and_then(decode::<User>)
    {
        Ok(user) if ctx.saves.get_untracked().generation == generation => {
            ctx.auth.set(AuthState::SignedIn(user));
            ctx.account_busy.set(false);
            ctx.deletion_pending.set(false);
            ctx.saves.update(SaveState::reset);
            if ctx.page.get_untracked() == Page::Welcome {
                ctx.navigate(Page::Setup);
            }
            load_cloud(ctx, true).await;
        }
        Err(error) if ctx.saves.get_untracked().generation == generation => {
            ctx.auth.set(AuthState::SignedOut);
            if error.status != 401 {
                ctx.notice.set(Some(error.message));
            }
        }
        _ => (),
    }
}

async fn load_cloud(ctx: UiContext, restore_draft: bool) {
    let generation = ctx.saves.get_untracked().generation;
    ctx.sync.set(SyncStatus::Loading);
    let cached = if restore_draft { read_draft(ctx) } else { None };
    let result = match api(ctx, "GET", "/api/plans/winter-2026", None).await {
        Ok(value) => decode::<StoredPlan>(value),
        Err(error) if error.status == 404 => {
            let mut plan = empty_plan(&ctx.calendar());
            if let Some(yesterday) = ctx.runtime.get_untracked().today.and_then(|d| d.pred_opt()) {
                plan.attendance.as_of = yesterday;
            }
            Ok(StoredPlan { plan, revision: 0 })
        }
        Err(error) => Err(error),
    };
    if ctx.saves.get_untracked().generation != generation {
        return;
    }
    match result {
        Ok(stored) => {
            if stored.revision < 0
                || debarred_domain::Engine::new(&ctx.calendar(), &stored.plan).is_err()
            {
                ctx.sync.set(SyncStatus::Failed(
                    "The cloud plan failed validation. It has not been treated as an empty plan."
                        .into(),
                ));
                return;
            }
            let restored = restore_plan(stored, cached);
            if restored.dirty {
                ctx.notice.set(Some("Restored your unsaved device draft. It was not silently replaced by the cloud copy.".into()));
            }
            batch(|| {
                ctx.plan.set(restored.plan);
                ctx.saves.update(|s| {
                    s.loaded = true;
                    s.revision = restored.revision;
                    s.edited = u64::from(restored.dirty);
                    s.saved = 0;
                    s.blocked = restored.conflict;
                    s.in_flight = None;
                });
                ctx.sync.set(if restored.conflict {
                    SyncStatus::Conflict
                } else if let Some(error) = ctx.validation_error() {
                    SyncStatus::Invalid(error)
                } else if restored.dirty {
                    SyncStatus::Unsaved
                } else {
                    SyncStatus::Saved
                });
            });
            persist(ctx);
            schedule_save(ctx);
            refresh_quota(ctx);
        }
        Err(error) => {
            if let Some(draft) = cached {
                ctx.plan.set(draft.plan);
                ctx.saves.update(|s| {
                    s.revision = draft.base_revision;
                    s.edited = u64::from(draft.dirty);
                    s.saved = 0;
                });
            }
            ctx.saves.update(|s| {
                s.loaded = false;
                s.blocked = true;
            });
            ctx.sync.set(if error.status == 401 {
                SyncStatus::Expired
            } else {
                SyncStatus::Failed(error.message)
            });
            crate::drafts::refresh(ctx);
        }
    }
}

pub fn schedule_save(ctx: UiContext) {
    if ctx.is_demo() || ctx.validation_error().is_some() {
        return;
    }
    let state = ctx.saves.get_untracked();
    if !state.loaded || state.blocked || !state.dirty() {
        return;
    }
    spawn_local(async move {
        gloo_timers::future::TimeoutFuture::new(650).await;
        let latest = ctx.saves.get_untracked();
        if latest.generation != state.generation || latest.edited != state.edited {
            return;
        }
        save(ctx).await;
    });
}

async fn save(ctx: UiContext) {
    if ctx.is_demo() || ctx.validation_error().is_some() {
        return;
    }
    let mut ticket = None;
    ctx.saves.update(|s| ticket = s.begin());
    let Some(ticket) = ticket else {
        return;
    };
    let plan = ctx.plan.get_untracked();
    ctx.sync.set(SyncStatus::Saving);
    match api(
        ctx,
        "PUT",
        "/api/plans/winter-2026",
        Some(json!({"expected_revision": ticket.expected_revision, "plan": plan})),
    )
    .await
    .and_then(decode::<StoredPlan>)
    {
        Ok(stored) => {
            let mut accepted = false;
            ctx.saves
                .update(|s| accepted = s.finish(ticket, stored.revision));
            if !accepted {
                if ctx.saves.get_untracked().generation == ticket.generation {
                    ctx.sync.set(SyncStatus::Conflict);
                }
                return;
            }
            ctx.sync.set(if ctx.saves.get_untracked().dirty() {
                SyncStatus::Unsaved
            } else {
                SyncStatus::Saved
            });
            persist(ctx);
            if ctx.saves.get_untracked().dirty() {
                schedule_save(ctx);
            }
        }
        Err(error) => {
            let mut accepted = false;
            ctx.saves.update(|s| accepted = s.fail(ticket));
            if !accepted {
                return;
            }
            ctx.sync.set(match error.status {
                409 => SyncStatus::Conflict,
                401 => SyncStatus::Expired,
                _ => SyncStatus::Failed(error.message),
            });
            persist(ctx);
        }
    }
}

pub fn reload_cloud(ctx: UiContext) {
    if ctx.is_demo() {
        return;
    }
    // Called only after the UI's explicit discard confirmation.
    ctx.saves.update(SaveState::reset);
    spawn_local(load_cloud(ctx, false));
}

pub fn retry_save(ctx: UiContext) {
    if ctx.is_demo() {
        persist(ctx);
        return;
    }
    if !ctx.saves.get_untracked().loaded {
        ctx.saves.update(SaveState::reset);
        spawn_local(load_cloud(ctx, true));
    } else {
        ctx.saves.update(|s| s.blocked = false);
        schedule_save(ctx);
    }
}

pub fn navigate(ctx: UiContext, page: Page) {
    let path = match (ctx.is_demo(), page) {
        (_, Page::Welcome) => "/",
        (_, Page::Privacy) => "/privacy",
        (_, Page::Terms) => "/terms",
        (true, Page::Account) => "/demo/account",
        (false, Page::Account) => "/account",
        (true, Page::Planner) => "/demo/planner",
        (true, _) => "/demo/setup",
        (false, Page::Planner) => "/planner",
        _ => "/setup",
    };
    if let Ok(history) = window().history() {
        let _ = history.push_state_with_url(&JsValue::NULL, "", Some(path));
    }
}

fn install_lifecycle(ctx: UiContext) {
    // These listeners belong to the single root App for the life of the document.
    let deletion = Closure::<dyn FnMut(web_sys::StorageEvent)>::new(
        move |event: web_sys::StorageEvent| {
            if event.new_value().is_none() {
                return;
            }
            if let AuthState::SignedIn(user) = ctx.auth.get_untracked() {
                if event.key().as_deref()
                    == Some(crate::drafts::deletion_marker(&user.user_id).as_str())
                {
                    let _ = crate::drafts::erase_user(&user.user_id, false);
                    finish_account_deletion(ctx, "Account deletion was requested in another tab. App data was removed; identity deletion remains pending operator review.");
                }
            }
        },
    );
    let _ = window().add_event_listener_with_callback("storage", deletion.as_ref().unchecked_ref());
    deletion.forget();
    let pop = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
        if let Ok(path) = window().location().pathname() {
            ctx.page.set(Page::from_path(&path));
        }
    });
    let _ = window().add_event_listener_with_callback("popstate", pop.as_ref().unchecked_ref());
    pop.forget();
    let unload = Closure::<dyn FnMut(web_sys::BeforeUnloadEvent)>::new(
        move |event: web_sys::BeforeUnloadEvent| {
            if (!ctx.is_demo() && ctx.saves.get_untracked().dirty())
                || (ctx.is_demo() && matches!(ctx.sync.get_untracked(), SyncStatus::Failed(_)))
            {
                event.prevent_default();
                event.set_return_value("Unsaved changes");
            }
        },
    );
    let _ =
        window().add_event_listener_with_callback("beforeunload", unload.as_ref().unchecked_ref());
    unload.forget();
    let focus = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
        if !matches!(ctx.auth.get_untracked(), AuthState::SignedIn(_)) {
            return;
        }
        let generation = ctx.saves.get_untracked().generation;
        spawn_local(async move {
            let result = api(ctx, "GET", "/api/me", None)
                .await
                .and_then(decode::<User>);
            if ctx.saves.get_untracked().generation != generation {
                return;
            }
            if let Ok(user) = result {
                if let AuthState::SignedIn(previous) = ctx.auth.get_untracked() {
                    if previous.user_id != user.user_id || previous.csrf_token != user.csrf_token {
                        persist(ctx);
                        ctx.saves.update(SaveState::reset);
                        ctx.plan.set(empty_plan(&ctx.calendar()));
                        ctx.messages.set(Vec::new());
                        ctx.device_drafts.set(Vec::new());
                        ctx.import_draft.set(None);
                        ctx.helper_busy.set(false);
                        ctx.import_busy.set(false);
                        ctx.account_busy.set(false);
                        ctx.deletion_pending.set(false);
                        ctx.auth.set(AuthState::SignedIn(user));
                        ctx.notice.set(Some("The browser session changed. Reloading the matching account; the previous account's draft remains separate.".into()));
                        load_cloud(ctx, true).await;
                    }
                }
            } else if matches!(result, Err(ApiError { status: 401, .. })) {
                ctx.saves.update(|s| s.blocked = true);
                ctx.sync.set(SyncStatus::Expired);
            }
        });
    });
    let _ = window().add_event_listener_with_callback("focus", focus.as_ref().unchecked_ref());
    focus.forget();
}

fn finish_account_deletion(ctx: UiContext, message: &str) {
    batch(|| {
        ctx.saves.update(SaveState::reset);
        ctx.plan.set(empty_plan(&ctx.calendar()));
        ctx.auth.set(AuthState::SignedOut);
        ctx.messages.set(Vec::new());
        ctx.device_drafts.set(Vec::new());
        ctx.import_draft.set(None);
        ctx.quotas.set(None);
        ctx.helper_busy.set(false);
        ctx.import_busy.set(false);
        ctx.helper_open.set(false);
        ctx.account_busy.set(false);
        ctx.deletion_pending.set(true);
        ctx.notice.set(Some(message.to_owned()));
    });
    ctx.navigate(Page::Welcome);
}

pub fn export_account_data(ctx: UiContext) {
    if ctx.account_busy.get_untracked()
        || !matches!(ctx.auth.get_untracked(), AuthState::SignedIn(_))
    {
        return;
    }
    let generation = ctx.saves.get_untracked().generation;
    ctx.account_busy.set(true);
    spawn_local(async move {
        if ctx.saves.get_untracked().generation != generation {
            return;
        }
        let result = api(ctx, "GET", "/api/account/export", None).await;
        if ctx.saves.get_untracked().generation != generation {
            return;
        }
        ctx.account_busy.set(false);
        match result {
            Ok(value) => download_value(ctx, value, "vitattendance-data-export.json"),
            Err(error) => ctx.notice.set(Some(format!(
                "Export could not complete: {}",
                error.message
            ))),
        }
    });
}

pub fn request_account_deletion(ctx: UiContext, confirmation: String) {
    let AuthState::SignedIn(user) = ctx.auth.get_untracked() else {
        return;
    };
    if ctx.account_busy.get_untracked() || confirmation != "DELETE MY ACCOUNT" {
        return;
    }
    let generation = ctx.saves.get_untracked().generation;
    ctx.account_busy.set(true);
    spawn_local(async move {
        if ctx.saves.get_untracked().generation != generation {
            return;
        }
        let result = api(
            ctx,
            "POST",
            "/api/account/deletion",
            Some(json!({"confirmation": confirmation})),
        )
        .await;
        match result {
            Ok(value)
                if value.get("status").and_then(Value::as_str)
                    == Some("pending_identity_deletion")
                    && value.get("app_data_deleted").and_then(Value::as_bool) == Some(true) =>
            {
                // Clear the original account's copies even if a different account
                // became active while the server was processing this request.
                let erased = crate::drafts::erase_user(&user.user_id, true).is_ok();
                if ctx.saves.get_untracked().generation == generation {
                    let message = if erased {
                        "App data and this browser's account drafts were removed. Supabase identity deletion is pending operator review; your Google account was not deleted."
                    } else {
                        "Server app data was removed and identity deletion is pending. Browser cleanup could not be confirmed: clear this site's browser data manually. Your Google account was not deleted."
                    };
                    finish_account_deletion(ctx, message);
                }
            }
            result if ctx.saves.get_untracked().generation == generation => {
                ctx.account_busy.set(false);
                let message = match result {
                    Err(error) if error.status != 0 && error.status < 500 => error.message,
                    _ => "Could not confirm the deletion outcome. Sign in again or contact support; the server may already have accepted the request.".into(),
                };
                ctx.notice.set(Some(message));
            }
            _ => (),
        }
    });
}

pub fn logout(ctx: UiContext) {
    if ctx.saves.get_untracked().dirty() && !window().confirm_with_message("Cloud changes are not fully saved. Export a draft first if you need it. Sign out and clear this account's device draft?").unwrap_or(false) { return; }
    let generation = ctx.saves.get_untracked().generation;
    spawn_local(async move {
        if ctx.saves.get_untracked().generation != generation {
            return;
        }
        match api(ctx, "POST", "/auth/logout", None).await {
            Ok(_) if ctx.saves.get_untracked().generation == generation => {
                crate::drafts::clear_current(ctx);
                batch(|| {
                    ctx.saves.update(SaveState::reset);
                    ctx.plan.set(empty_plan(&ctx.calendar()));
                    ctx.auth.set(AuthState::SignedOut);
                    ctx.messages.set(Vec::new());
                    ctx.import_draft.set(None);
                    ctx.quotas.set(None);
                    ctx.helper_busy.set(false);
                    ctx.import_busy.set(false);
                    ctx.account_busy.set(false);
                    ctx.notice.set(Some(
                        "Signed out. This tab's account draft was cleared.".into(),
                    ));
                });
                ctx.navigate(Page::Welcome);
            }
            Err(error) if ctx.saves.get_untracked().generation == generation => ctx.notice.set(
                Some(format!("Sign-out did not complete: {}", error.message)),
            ),
            _ => (),
        }
    });
}

pub fn local_login(ctx: UiContext, email: String, password: String) {
    if !ctx.runtime.get_untracked().local_password_enabled
        || matches!(ctx.auth.get_untracked(), AuthState::Checking)
    {
        return;
    }
    ctx.saves.update(SaveState::reset);
    ctx.auth.set(AuthState::Checking);
    ctx.notice.set(Some("Signing in to local Supabase…".into()));
    let generation = ctx.saves.get_untracked().generation;
    spawn_local(async move {
        if ctx.saves.get_untracked().generation != generation {
            return;
        }
        match api(
            ctx,
            "POST",
            "/auth/local/login",
            Some(json!({"email": email, "password": password})),
        )
        .await
        {
            Ok(_) => {
                ctx.auth.set(AuthState::Checking);
                check_session(ctx).await;
            }
            Err(error) => {
                ctx.auth.set(AuthState::SignedOut);
                ctx.notice.set(Some(if error.status == 401 {
                    "Local Supabase did not accept that email/password.".into()
                } else {
                    error.message
                }));
            }
        }
    });
}

fn refresh_quota(ctx: UiContext) {
    let generation = ctx.saves.get_untracked().generation;
    spawn_local(async move {
        if ctx.saves.get_untracked().generation != generation {
            return;
        }
        if let Ok(quota) = api(ctx, "GET", "/api/quota", None)
            .await
            .and_then(decode::<Quotas>)
        {
            if ctx.saves.get_untracked().generation == generation {
                ctx.quotas.set(Some(quota));
            }
        }
    });
}

pub fn ask(ctx: UiContext, question: String) {
    if ctx.helper_busy.get_untracked() {
        return;
    }
    let question = question.trim().to_owned();
    if question.is_empty() || question.len() > 1000 {
        ctx.notice
            .set(Some("Use a question of 1–1,000 bytes.".into()));
        return;
    }
    if let Some(error) = ctx.validation_error() {
        ctx.notice.set(Some(error));
        return;
    }
    let action = match question.to_ascii_lowercase().as_str() {
        "status" => Some("status"),
        "max skips" | "max_skips" => Some("max_skips"),
        _ => None,
    };
    append_message(ctx, true, question.clone(), false);
    if ctx.is_demo() {
        let answer = if action.is_some() {
            demo_answer(ctx)
        } else {
            "The device-only sandbox supports Check my status and Maximum skips. Sign in with an enabled provider for natural-language questions.".into()
        };
        append_message(ctx, false, answer, false);
        return;
    }
    if !ctx.can_edit() {
        append_message(
            ctx,
            false,
            "Load your signed-in plan before asking.".into(),
            true,
        );
        return;
    }
    let generation = ctx.saves.get_untracked().generation;
    let body = if let Some(action) = action {
        json!({"plan": ctx.plan.get_untracked(), "intent": {"action": action, "course_codes": [], "checkpoint": null, "dates": []}})
    } else {
        json!({"plan": ctx.plan.get_untracked(), "question": question})
    };
    ctx.helper_busy.set(true);
    spawn_local(async move {
        if ctx.saves.get_untracked().generation != generation {
            return;
        }
        let result = api(ctx, "POST", "/api/helper", Some(body)).await;
        if ctx.saves.get_untracked().generation != generation {
            return;
        }
        ctx.helper_busy.set(false);
        match result {
            Ok(value) => {
                append_message(
                    ctx,
                    false,
                    value
                        .get("answer")
                        .and_then(Value::as_str)
                        .unwrap_or("No answer was returned.")
                        .into(),
                    false,
                );
                refresh_quota(ctx);
            }
            Err(error) => {
                append_message(ctx, false, error.message, true);
                refresh_quota(ctx);
            }
        }
    });
}

fn append_message(ctx: UiContext, from_user: bool, text: String, error: bool) {
    ctx.messages.update(|messages| {
        messages.push(ChatMessage {
            from_user,
            text,
            error,
        });
        if messages.len() > 40 {
            messages.drain(..messages.len() - 40);
        }
    });
}

fn demo_answer(ctx: UiContext) -> String {
    let mut lines = vec!["Sandbox projection only; assumes attendance at every unselected future meeting. Skip allowances are per checkpoint, not additive.".into()];
    for result in ctx.results() {
        match result {
            debarred_domain::CheckpointProjection::Historical { checkpoint } => {
                lines.push(format!(
                    "{}: historical; aggregate counts cannot reconstruct it.",
                    checkpoint.label
                ))
            }
            debarred_domain::CheckpointProjection::Upcoming {
                checkpoint,
                courses,
            } => {
                for course in courses {
                    let allowance = ctx.allowance(&course.course_code, &checkpoint.id);
                    lines.push(format!(
                        "{} at {}: {}/{} ({:.1}%), {}. Additional skips: {}; shortfall: {}.",
                        course.course_code,
                        checkpoint.label,
                        course.total_attended,
                        course.total_held,
                        course.percentage,
                        if !course.has_classes {
                            "no classes; not evidence of eligibility"
                        } else if course.safe {
                            "meets threshold"
                        } else {
                            "below threshold"
                        },
                        allowance.map(|a| a.additional).unwrap_or(0),
                        allowance.map(|a| a.shortfall).unwrap_or(0)
                    ));
                }
            }
        }
    }
    lines.join("\n")
}

pub fn import_file(ctx: UiContext, file: web_sys::File) {
    if ctx.is_demo() || !ctx.can_edit() || ctx.import_busy.get_untracked() {
        return;
    }
    let mime = file.type_();
    if !matches!(mime.as_str(), "image/png" | "image/jpeg" | "image/webp")
        || file.size() > 2.0 * 1024.0 * 1024.0
    {
        ctx.notice.set(Some(
            "Choose a PNG, JPEG or WebP image no larger than 2 MiB.".into(),
        ));
        return;
    }
    if !ctx.runtime.get_untracked().ai_enabled {
        ctx.notice.set(Some(
            "Image import is disabled. You can enter the timetable manually.".into(),
        ));
        return;
    }
    let generation = ctx.saves.get_untracked().generation;
    ctx.import_busy.set(true);
    spawn_local(async move {
        let result = async {
            let buffer = JsFuture::from(file.array_buffer())
                .await
                .map_err(|_| network_error())?;
            if ctx.saves.get_untracked().generation != generation {
                return Err(network_error());
            }
            let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
            use base64::Engine as _;
            let data = base64::engine::general_purpose::STANDARD.encode(bytes);
            let value = api(
                ctx,
                "POST",
                "/api/timetable/import",
                Some(json!({"image_data_url": format!("data:{mime};base64,{data}")})),
            )
            .await?;
            decode::<debarred_domain::Timetable>(
                value.get("timetable").cloned().unwrap_or(Value::Null),
            )
        }
        .await;
        if ctx.saves.get_untracked().generation != generation {
            return;
        }
        ctx.import_busy.set(false);
        match result {
            Ok(draft) if draft.validate().is_ok() => {
                ctx.import_draft.set(Some(draft));
                ctx.notice.set(Some("Review every extracted slot before applying. Your current plan has not changed.".into()));
            }
            Ok(_) => ctx.notice.set(Some(
                "The extracted timetable was invalid. Your plan has not changed.".into(),
            )),
            Err(error) => ctx.notice.set(Some(error.message)),
        }
        refresh_quota(ctx);
    });
}

pub fn download_draft(ctx: UiContext) {
    match serde_json::to_value(ctx.plan.get_untracked()) {
        Ok(value) => download_value(ctx, value, "vitattendance-draft.json"),
        Err(_) => ctx
            .notice
            .set(Some("The draft could not be serialized for export.".into())),
    }
}

fn download_value(ctx: UiContext, value: Value, filename: &str) {
    let result = (|| -> Result<(), JsValue> {
        let text = serde_json::to_string_pretty(&value).map_err(|_| JsValue::NULL)?;
        let options = web_sys::BlobPropertyBag::new();
        options.set_type("application/json");
        let blob = web_sys::Blob::new_with_str_sequence_and_options(
            &js_sys::Array::of1(&JsValue::from_str(&text)),
            &options,
        )?;
        let url = web_sys::Url::create_object_url_with_blob(&blob)?;
        let document = window().document().ok_or(JsValue::NULL)?;
        let anchor = document
            .create_element("a")?
            .dyn_into::<web_sys::HtmlAnchorElement>()?;
        anchor.set_href(&url);
        anchor.set_download(filename);
        let body = document.body().ok_or(JsValue::NULL)?;
        body.append_child(&anchor)?;
        anchor.click();
        anchor.remove();
        gloo_timers::callback::Timeout::new(1000, move || {
            let _ = web_sys::Url::revoke_object_url(&url);
        })
        .forget();
        Ok(())
    })();
    if result.is_err() {
        ctx.notice.set(Some(
            "The browser could not create the export. Allow downloads and retry.".into(),
        ));
    }
}
