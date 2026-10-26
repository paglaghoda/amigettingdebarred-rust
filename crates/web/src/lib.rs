#![recursion_limit = "256"]

mod components;
pub mod model;
pub mod state;

#[cfg(all(feature = "hydrate", target_arch = "wasm32"))]
mod browser;
#[cfg(all(feature = "hydrate", target_arch = "wasm32"))]
mod drafts;

use leptos::prelude::*;
use state::UiContext;

#[component]
pub fn App(
    initial_path: String,
    #[prop(optional_no_strip)] initial_support_email: Option<String>,
) -> impl IntoView {
    let context = UiContext::new(&initial_path);
    context
        .runtime
        .update(|runtime| runtime.support_email = initial_support_email);
    provide_context(context);
    Effect::new(move |_| untrack(move || context.boot()));
    view! { <components::Shell/> }
}

#[cfg(feature = "ssr")]
pub fn render_document(path: &str) -> String {
    render_document_with_contact(
        path,
        sanitize_support_email(std::env::var("SUPPORT_EMAIL").ok().as_deref()),
    )
}

#[cfg(feature = "ssr")]
fn render_document_with_contact(path: &str, contact: Option<String>) -> String {
    let owner = Owner::new();
    let contact = sanitize_support_email(contact.as_deref());
    let contact_meta = contact
        .as_ref()
        .map(|email| format!("<meta name=\"support-email\" content=\"{email}\">"))
        .unwrap_or_default();
    let body = owner.with(|| {
        view! { <App initial_path=path.to_owned() initial_support_email=contact/> }.to_html()
    });
    format!("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><meta name=\"theme-color\" content=\"#192333\">{contact_meta}<meta name=\"description\" content=\"Plan your VIT Bhopal attendance, timetable and leave before CAT-1, CAT-2 and final exams.\"><title>AmIGettingDebarred? — VIT attendance planner</title><link rel=\"icon\" href=\"/assets/favicon.svg\" type=\"image/svg+xml\"><link rel=\"stylesheet\" href=\"/assets/app.css\"><link rel=\"preload\" href=\"/assets/fonts/space-grotesk-semibold.ttf\" as=\"font\" type=\"font/ttf\" crossorigin></head><body>{body}<script type=\"module\" src=\"/assets/boot.js\"></script></body></html>")
}

#[cfg(all(feature = "hydrate", target_arch = "wasm32"))]
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn hydrate() {
    let path = web_sys::window()
        .and_then(|window| window.location().pathname().ok())
        .unwrap_or_else(|| "/".into());
    let contact = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| {
            document
                .query_selector("meta[name='support-email']")
                .ok()
                .flatten()
        })
        .and_then(|meta| meta.get_attribute("content"));
    let contact = sanitize_support_email(contact.as_deref());
    leptos::mount::hydrate_body(
        move || view! { <App initial_path=path.clone() initial_support_email=contact.clone()/> },
    );
}

/// Public contact metadata is intentionally narrow and safe in HTML attributes.
/// This is not proof that the mailbox exists or is monitored; release review is required.
pub fn sanitize_support_email(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.len() > 254 || !value.is_ascii() {
        return None;
    }
    let (local, domain) = value.split_once('@')?;
    if local.is_empty()
        || local.len() > 64
        || domain.contains('@')
        || !domain.contains('.')
        || !local
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
        || !domain
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
        || domain.starts_with('.')
        || domain.ends_with('.')
        || domain.contains("..")
    {
        return None;
    }
    Some(value.to_owned())
}

#[cfg(all(test, feature = "ssr"))]
mod tests {
    use super::*;

    #[test]
    fn document_is_real_html_with_only_same_origin_assets() {
        let html = render_document("/");
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("Try local sandbox"));
        assert!(html.contains("/assets/boot.js"));
        assert!(html.contains("/assets/app.css"));
        assert!(!html.contains("fonts.googleapis.com"));
        assert!(!html.contains("<script>"));
        assert!(!html.contains("csrf_token"));
    }

    #[test]
    fn public_contact_is_safe_and_visible_without_javascript() {
        for invalid in [
            "javascript:alert(1)",
            "x@a.test\r\nBcc: other@a.test",
            "x@a.test\" onclick=\"bad",
            "<script>@a.test",
            "x@@a.test",
        ] {
            assert!(sanitize_support_email(Some(invalid)).is_none());
        }
        let html =
            render_document_with_contact("/privacy", Some("support@operator.example".into()));
        assert!(html.contains("<meta name=\"support-email\" content=\"support@operator.example\">"));
        assert!(html.contains("mailto:support@operator.example"));
        assert!(!html.contains("Support contact must be configured"));
        assert_eq!(
            sanitize_support_email(Some("  support+app@operator.example  ")),
            Some("support+app@operator.example".into())
        );
    }

    #[test]
    fn sandbox_ssr_includes_the_actual_timetable() {
        let html = render_document("/demo");
        assert!(html.contains("Device-only sandbox"));
        assert!(html.contains("CSE1001"));
        assert!(html.contains("Classes attended"));
    }
}
