//! Recoverable device copies. Each document has a unique writer key: duplicated
//! tabs may copy sessionStorage pointers, but can never take another writer's key.
use leptos::prelude::*;

use crate::{model::*, state::UiContext};

const TAB_KEY: &str = "vitattendance:tab-id:v1";
fn window() -> web_sys::Window {
    web_sys::window().expect("browser window")
}
fn storage() -> Result<web_sys::Storage, ()> {
    window().local_storage().map_err(|_| ())?.ok_or(())
}
fn session() -> Result<web_sys::Storage, ()> {
    window().session_storage().map_err(|_| ())?.ok_or(())
}
fn base(ctx: UiContext) -> Option<String> {
    match ctx.auth.get_untracked() {
        AuthState::Demo => Some("vitattendance:v1:sandbox:winter-2026".into()),
        AuthState::SignedIn(user) => Some(format!(
            "vitattendance:v1:draft:{}:winter-2026",
            user.user_id
        )),
        _ => None,
    }
}

pub fn claim(ctx: UiContext) -> Result<(), ()> {
    let session = session()?;
    storage()?;
    let crypto = window().crypto().map_err(|_| ())?;
    let tab_id = session
        .get_item(TAB_KEY)
        .map_err(|_| ())?
        .filter(|id| id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
        .unwrap_or_else(|| crypto.random_uuid());
    session.set_item(TAB_KEY, &tab_id).map_err(|_| ())?;
    // Always new, even on reload or a browser's Duplicate Tab action. Never
    // overwrite a shared lease or a previous document's persistent snapshot.
    ctx.identity.set_value(Some(DraftIdentity {
        tab_id,
        instance_id: crypto.random_uuid(),
        updated_ms: 0,
    }));
    Ok(())
}

pub fn key(ctx: UiContext) -> Option<String> {
    let identity = ctx.identity.get_value()?;
    Some(format!(
        "{}:{}:{}",
        base(ctx)?,
        identity.tab_id,
        identity.instance_id
    ))
}

fn parse(ctx: UiContext, value: &str) -> Option<Draft> {
    if value.len() > 512 * 1024 {
        return None;
    }
    let draft: Draft = serde_json::from_str(value).ok()?;
    (draft.plan.semester_id == "winter-2026"
        && draft.plan.schema_version == 1
        && draft.plan.calendar_revision == ctx.calendar().config().revision
        && draft.base_revision >= 0)
        .then_some(draft)
}

fn read_key(ctx: UiContext, name: &str) -> Option<Draft> {
    let base = base(ctx)?;
    if name != base && (!name.starts_with(&format!("{base}:")) || name.ends_with(":updated")) {
        return None;
    }
    parse(ctx, &storage().ok()?.get_item(name).ok()??)
}

fn candidates(ctx: UiContext) -> Vec<(String, Draft, i64)> {
    let (Some(base), Ok(storage)) = (base(ctx), storage()) else {
        return Vec::new();
    };
    let prefix = format!("{base}:");
    let mut entries = Vec::new();
    for index in 0..storage.length().unwrap_or(0) {
        let Some(name) = storage.key(index).ok().flatten() else {
            continue;
        };
        if name != base && (!name.starts_with(&prefix) || name.ends_with(":updated")) {
            continue;
        }
        if let Some(draft) = read_key(ctx, &name) {
            let updated = storage
                .get_item(&format!("{name}:updated"))
                .ok()
                .flatten()
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or(0);
            entries.push((name, draft, updated));
        }
    }
    entries.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    entries
}

fn remember(ctx: UiContext, name: &str) -> Option<Draft> {
    let draft = read_key(ctx, name)?;
    let updated = storage()
        .ok()?
        .get_item(&format!("{name}:updated"))
        .ok()?
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(0);
    if let Some(mut identity) = ctx.identity.get_value() {
        identity.updated_ms = updated;
        ctx.identity.set_value(Some(identity));
    }
    Some(draft)
}

pub fn touch(ctx: UiContext) {
    if let Some(mut identity) = ctx.identity.get_value() {
        identity.updated_ms = js_sys::Date::now() as i64;
        ctx.identity.set_value(Some(identity));
    }
}

pub fn read(ctx: UiContext) -> Option<Draft> {
    if is_erased(ctx) {
        return None;
    }
    let base = base(ctx)?;
    let previous = session()
        .ok()?
        .get_item(&format!("{base}:last-draft"))
        .ok()?;
    if let Some(draft) = previous.as_deref().and_then(|name| remember(ctx, name)) {
        return Some(draft);
    }
    // A fresh sandbox tab recovers the latest device copy after old tabs close.
    // A fresh authenticated tab instead loads the server; archived unsaved
    // account copies are available through explicit shelf restoration only.
    if ctx.is_demo() {
        if let Some((name, _, _)) = candidates(ctx).into_iter().next() {
            ctx.notice.set(Some("Recovered the latest sandbox device copy. Other copies are available under Device drafts.".into()));
            return remember(ctx, &name);
        }
    }
    remember(ctx, &base)
}

pub fn refresh(ctx: UiContext) {
    if is_erased(ctx) {
        ctx.device_drafts.set(Vec::new());
        return;
    }
    let current = key(ctx);
    let entries = candidates(ctx)
        .into_iter()
        .map(|(key, draft, updated)| {
            let date = chrono::DateTime::from_timestamp_millis(updated)
                .filter(|_| updated > 0)
                .map(|d| d.format("%d %b %Y %H:%M:%S UTC").to_string())
                .unwrap_or_else(|| "Recovered earlier copy".into());
            let first = draft
                .plan
                .timetable
                .courses
                .first()
                .map(|c| format!("{} {}", c.code, c.name.chars().take(64).collect::<String>()))
                .unwrap_or_else(|| "Empty timetable".into());
            DeviceDraft {
                current: current.as_deref() == Some(key.as_str()),
                key,
                label: format!(
                    "{date} · {} courses · {first}",
                    draft.plan.timetable.courses.len()
                ),
            }
        })
        .collect();
    ctx.device_drafts.set(entries);
}

pub fn persist(ctx: UiContext) {
    if is_erased(ctx) {
        return;
    }
    let state = ctx.saves.get_untracked();
    if !state.loaded {
        return;
    }
    let result = (|| -> Result<(), ()> {
        let name = key(ctx).ok_or(())?;
        let base = base(ctx).ok_or(())?;
        let draft = Draft {
            plan: ctx.plan.get_untracked(),
            base_revision: state.revision,
            dirty: state.dirty(),
        };
        let text = serde_json::to_string(&draft).map_err(|_| ())?;
        let storage = storage()?;
        storage.set_item(&name, &text).map_err(|_| ())?;
        let updated = ctx.identity.get_value().ok_or(())?.updated_ms;
        storage
            .set_item(&format!("{name}:updated"), &updated.to_string())
            .map_err(|_| ())?;
        session()?
            .set_item(&format!("{base}:last-draft"), &name)
            .map_err(|_| ())?;
        Ok(())
    })();
    if result.is_err() {
        ctx.notice.set(Some("Device storage is unavailable or full. Export a draft before closing this page; cloud save status is shown separately.".into()));
        if ctx.is_demo() {
            ctx.sync.set(SyncStatus::Failed(
                "Device storage unavailable; export your draft.".into(),
            ));
        }
    }
    refresh(ctx);
}

pub fn restore(ctx: UiContext, name: String) {
    if !ctx.can_edit() || is_erased(ctx) {
        return;
    }
    let Some(draft) = read_key(ctx, &name) else {
        ctx.notice.set(Some(
            "That device copy is no longer available or has an unsupported format.".into(),
        ));
        return;
    };
    let previous = ctx.saves.get_untracked();
    let conflict = !ctx.is_demo() && (previous.blocked || previous.revision != draft.base_revision);
    ctx.saves.update(|state| {
        state.reset();
        state.loaded = true;
        state.revision = draft.base_revision;
        state.edited = 1;
        state.saved = 0;
        state.blocked = conflict;
    });
    ctx.plan.set(draft.plan);
    ctx.sync.set(if conflict {
        SyncStatus::Conflict
    } else if let Some(error) = ctx.validation_error() {
        SyncStatus::Invalid(error)
    } else if ctx.is_demo() {
        SyncStatus::LocalOnly
    } else {
        SyncStatus::Unsaved
    });
    ctx.notice.set(Some("Restored a device draft into this document. Other saved copies were not overwritten; cloud conflicts remain blocked.".into()));
    touch(ctx);
    persist(ctx);
    crate::browser::schedule_save(ctx);
}

pub fn delete(ctx: UiContext, name: String) {
    if !ctx.can_edit()
        || key(ctx).as_deref() == Some(name.as_str())
        || read_key(ctx, &name).is_none()
    {
        return;
    }
    if let Ok(storage) = storage() {
        let _ = storage.remove_item(&name);
        let _ = storage.remove_item(&format!("{name}:updated"));
    }
    refresh(ctx);
}

pub fn deletion_marker(user_id: &str) -> String {
    format!("vitattendance:v1:account-deleted:{user_id}")
}

fn is_erased(ctx: UiContext) -> bool {
    match ctx.auth.get_untracked() {
        AuthState::SignedIn(user) => storage()
            .ok()
            .and_then(|store| {
                store
                    .get_item(&deletion_marker(&user.user_id))
                    .ok()
                    .flatten()
            })
            .is_some(),
        _ => false,
    }
}

/// A durable marker goes first, preventing frozen tabs or late callbacks from
/// recreating copies after erasure. Never touches another account or sandbox.
pub fn erase_user(user_id: &str, broadcast: bool) -> Result<(), ()> {
    let storage = storage()?;
    if broadcast {
        storage
            .set_item(
                &deletion_marker(user_id),
                &(js_sys::Date::now() as i64).to_string(),
            )
            .map_err(|_| ())?;
    }
    let prefix = format!("vitattendance:v1:draft:{user_id}:");
    let mut names = Vec::new();
    for index in 0..storage.length().map_err(|_| ())? {
        if let Some(name) = storage.key(index).map_err(|_| ())? {
            if name.starts_with(&prefix) {
                names.push(name);
            }
        }
    }
    for name in names {
        storage.remove_item(&name).map_err(|_| ())?;
    }
    let session = session()?;
    let mut pointers = Vec::new();
    for index in 0..session.length().map_err(|_| ())? {
        if let Some(name) = session.key(index).map_err(|_| ())? {
            if name.starts_with(&prefix) {
                pointers.push(name);
            }
        }
    }
    for name in pointers {
        session.remove_item(&name).map_err(|_| ())?;
    }
    Ok(())
}

pub fn clear_current(ctx: UiContext) {
    if let (Some(name), Ok(storage)) = (key(ctx), storage()) {
        let _ = storage.remove_item(&name);
        let _ = storage.remove_item(&format!("{name}:updated"));
    }
    if let (Some(base), Ok(session)) = (base(ctx), session()) {
        let _ = session.remove_item(&format!("{base}:last-draft"));
    }
    ctx.device_drafts.set(Vec::new());
}
