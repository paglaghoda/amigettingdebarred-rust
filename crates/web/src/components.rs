use crate::{
    model::{AuthState, Page, SyncStatus},
    state::UiContext,
};
use chrono::{Datelike, NaiveDate};
use debarred_domain::{
    calendar::DayKind,
    slots::{slot_codes, TIME_SLOTS},
    CheckpointProjection, CourseProjection, GridDay, LeaveEntry, Timetable,
};
use leptos::prelude::*;

fn context() -> UiContext {
    use_context::<UiContext>().expect("UiContext must be provided by App")
}

#[component]
pub fn Shell() -> impl IntoView {
    let ctx = context();
    view! {
        <a class="skip-link" href="#main">"Skip to content"</a>
        {move || {
            match ctx.page.get() {
                Page::Privacy => return view! { <Privacy/> }.into_any(),
                Page::Terms => return view! { <Terms/> }.into_any(),
                _ => {}
            }
            let private = ctx.page.get() != Page::Welcome;
            match ctx.auth.get() {
                AuthState::SignedIn(_) | AuthState::Demo => view! { <Workspace/> }.into_any(),
                AuthState::Checking if private => view! {
                    <main id="main" class="loading card" aria-busy="true"><h1>"Getting your plan ready…"</h1><p role="status">"Checking your session. Your saved plan will not be overwritten."</p><a href="/">"Back to welcome"</a></main>
                }.into_any(),
                _ => view! { <Welcome/> }.into_any(),
            }
        }}
        <noscript><p class="banner">"Preview is visible without JavaScript; session checks and editing require JavaScript and WebAssembly."</p></noscript>
    }
}

#[component]
fn Welcome() -> impl IntoView {
    let ctx = context();
    let email = RwSignal::new("dev@vitattendance.test".to_string());
    let password = RwSignal::new(String::new());
    view! {
        <main id="main" class="welcome" data-testid="welcome">
            <section class="card welcome-copy">
                <span class="brand-mark" aria-hidden="true">"✓"</span>
                <p class="eyebrow">"VIT Bhopal · Attendance & leave planner"</p>
                <h1>"AmIGetting"<br/>"Debarred?"</h1>
                <p>"A little planning. A lot less attendance anxiety. Check where you stand before you book that trip."</p>
                <p class="semester">{move || ctx.calendar().config().label.clone()}</p>
                <Show when=move || !ctx.runtime.get().local_password_enabled>
                <Show when=move || ctx.runtime.get().auth_available fallback=move || view! {
                    <p class="banner warning">{move || {
                                            let mode = ctx.runtime.get().mode;
                                            if mode.is_empty() || mode == "loading" {
                                                "Checking sign-in availability…".to_string()
                                            } else {
                                                format!("Google sign-in is unavailable in {mode} mode. You can still try the device-only sandbox.")
                                            }
                                        }}</p>
                }>
                    <a class="button primary full" href="/auth/login" data-testid="google-sign-in">"Continue with Google"</a>
                    <p class="muted small">"Sign in to save your timetable and leave plan to your account."</p>
                </Show>
                </Show>
                <a class="button full" href="/demo" data-testid="try-sandbox">"Try local sandbox"</a>
                <p class="muted small">"No account needed. Sandbox changes stay on this device; they are not cloud saves."</p>
                <Show when=move || ctx.runtime.get().local_password_enabled>
                    <p class="muted small">"This local development environment uses email and password sign-in. Use the form below to sign in and save your plan to your local Supabase account."</p>
                    <details open class="local-login"><summary>"Local development sign-in"</summary>
                        <form on:submit=move |ev| { ev.prevent_default(); ctx.local_login(email.get_untracked(), password.get_untracked()); password.set(String::new()); }>
                            <label>"Email"<input type="email" autocomplete="username" required value=move || email.get() prop:value=move || email.get() on:input=move |ev| email.set(event_target_value(&ev))/></label>
                            <label>"Password"<input type="password" autocomplete="current-password" placeholder="Enter your local development password" required prop:value=move || password.get() on:input=move |ev| password.set(event_target_value(&ev))/></label>
                            <button type="submit" class="primary">"Sign in locally"</button>
                        </form>
                    </details>
                </Show>
                <Show when=move || ctx.deletion_pending.get()>
                    <div class="banner warning" role="status">"App data removed; identity deletion pending operator review. Your Google account was not deleted."</div>
                </Show>
                <Notice/>
            </section>
            <section class="trip-panel" aria-label="Before you book">
                <img src="/assets/images/trip-trek.jpg" alt="Turquoise high-altitude Himalayan lake" width="768" height="1024" fetchpriority="high"/>
                <div class="trip-copy"><p class="eyebrow">"Before you book"</p><h2>"Can you afford that Chandratal Lake trek?"</h2><p>"Pick your days off, see your projected attendance, and leave with a plan."</p></div>
                <span class="trip-badge">"Plan first."<br/>"Travel better."</span>
            </section>
        </main>
        <div class="welcome-footer"><SiteFooter/></div>
    }
}

#[component]
fn SupportContact() -> impl IntoView {
    let ctx = context();
    view! {
        <p class="support-contact">{move || match ctx.runtime.get().support_email {
            Some(email) if !email.is_empty() => view! {
                <span>"Support: "<a href=format!("mailto:{email}")>{email.clone()}</a></span>
            }.into_any(),
            _ => view! { <span>"Support contact must be configured before launch"</span> }.into_any(),
        }}</p>
    }
}

#[component]
fn SiteFooter() -> impl IntoView {
    view! {
        <footer class="site-footer stack">
            <p>"Unofficial planning tool · not an official college service. Projections assume you attend every future class except your selected leave. Check official attendance records and college rules before making plans."</p>
            <nav class="policy-links" aria-label="Privacy and terms"><a href="/privacy">"Privacy"</a><a href="/terms">"Terms"</a></nav>
            <SupportContact/>
        </footer>
    }
}

#[component]
fn Account() -> impl IntoView {
    let ctx = context();
    let confirmation = RwSignal::new(String::new());
    let deletion_disabled = Signal::derive(move || {
        confirmation.get() != "DELETE MY ACCOUNT"
            || ctx.account_busy.get()
            || matches!(ctx.sync.get(), SyncStatus::Expired)
            || !matches!(ctx.auth.get(), AuthState::SignedIn(_))
    });
    view! {
        <section class="page-hero"><p class="eyebrow">"Settings"</p><h1>"Account & privacy"</h1><p>"Manage your app data and device copies."</p></section>
        <div class="actions"><button type="button" on:click=move |_| ctx.navigate(Page::Setup)>"Back to timetable"</button></div>
        <Show when=move || ctx.is_demo() fallback=move || view! {
            <Show when=move || matches!(ctx.auth.get(), AuthState::SignedIn(_))>
                <section class="card stack account-profile" aria-labelledby="account-profile-title">
                    <h2 id="account-profile-title">"Signed-in profile"</h2>
                    {move || match ctx.auth.get() {
                        AuthState::SignedIn(user) => view! {
                            <dl><dt>"Display name"</dt><dd>{user.display_name.unwrap_or_else(|| "Not provided".into())}</dd>
                                <dt>"Email"</dt><dd>{user.email.unwrap_or_else(|| "Not provided".into())}</dd></dl>
                        }.into_any(),
                        _ => ().into_any(),
                    }}
                    <p class="muted small">"This profile comes from your signed-in account. It is not shared with other users by this page."</p>
                </section>
                <section class="card stack" aria-labelledby="account-export-title" aria-busy=move || if ctx.account_busy.get() { "true" } else { "false" }>
                    <h2 id="account-export-title">"Export your app data"</h2>
                    <p>"Download a JSON copy of the app data held for your account. This account export is separate from device drafts; export any unsaved device draft separately before requesting deletion."</p>
                    <div class="actions"><button type="button" aria-label="Download my app data" disabled=move || ctx.account_busy.get() on:click=move |_| {
                        if !ctx.account_busy.get_untracked() && matches!(ctx.auth.get_untracked(), AuthState::SignedIn(_)) {
                            ctx.export_account_data();
                        }
                    }>"Download my app data"</button><button type="button" on:click=move |_| ctx.download_draft()>"Export current device draft"</button></div>
                </section>
                <section class="card stack account-deletion" aria-labelledby="account-deletion-title" aria-busy=move || if ctx.account_busy.get() { "true" } else { "false" }>
                    <h2 id="account-deletion-title">"Request account deletion"</h2>
                    <p>"Export first if you want to keep a copy. A successful request immediately erases your app data and revokes all app sessions. This browser also clears this user's device drafts across documents and notifies other open tabs."</p>
                    <p>"Your Supabase sign-in identity is queued for manual deletion by an operator; it is not automatically deleted by this request. Your Google account is not deleted, and you are not forced to sign out of Google."</p>
                    <p id="deletion-auth-hint">"The server requires an interactive sign-in within the last 5 minutes. Refreshing this page or refreshing a session does not count. If the server asks you to reauthenticate, sign out of this app and sign in again, then return here."</p>
                    <Show when=move || matches!(ctx.sync.get(), SyncStatus::Expired)>
                        <p class="banner warning" role="status">"Your app session has expired. Sign out of this app and sign in again before requesting deletion."</p>
                    </Show>
                    <form class="stack" on:submit=move |ev| ev.prevent_default()>
                        <label>"Type DELETE MY ACCOUNT exactly"
                            <input type="text" aria-label="Deletion confirmation" aria-describedby="deletion-auth-hint" autocomplete="off" spellcheck="false" autocapitalize="off" disabled=move || ctx.account_busy.get() || matches!(ctx.sync.get(), SyncStatus::Expired) prop:value=move || confirmation.get() on:input=move |ev| confirmation.set(event_target_value(&ev))/>
                        </label>
                        <ConfirmButton label="Request account deletion" editable=false
                            explanation="Remove your app data now, revoke all app sessions, and queue your Supabase identity for manual operator deletion? This cannot be undone. Your Google account will not be deleted."
                            disabled=deletion_disabled
                            action=Callback::new(move |_| {
                                if !deletion_disabled.get_untracked() {
                                    ctx.request_account_deletion(confirmation.get_untracked());
                                    confirmation.set(String::new());
                                }
                            })
                        />
                    </form>
                </section>
            </Show>
        }>
            <section class="card stack" aria-labelledby="device-account-title">
                <h2 id="device-account-title">"Device-only sandbox"</h2>
                <p>"You are not signed in. This sandbox has no real account, no cloud saves, and does not use account export or account deletion endpoints. Its drafts stay in this browser on this device."</p>
                <div class="actions"><button type="button" on:click=move |_| ctx.download_draft()>"Export device draft"</button><button type="button" disabled aria-label="Request account deletion unavailable in sandbox">"Request account deletion"</button></div>
                <p class="muted small">"To remove sandbox copies, clear this site's storage in your browser settings. Export anything you need first; clearing site storage also removes other app drafts stored in this browser."</p>
            </section>
        </Show>
        <section class="card stack" aria-labelledby="device-privacy-title">
            <h2 id="device-privacy-title">"Device copies & optional AI"</h2>
            <p>"Each document keeps its own device draft. Downloaded exports are separate files: deleting app data does not remove files you downloaded or copies on other devices. Manage those copies yourself."</p>
            <p>"AI is optional. Only use image import or send a helper question if you want that image or question shared with the configured external AI provider. Normal timetable editing and attendance calculations do not require AI."</p>
            <p><a href="/privacy">"Read the privacy information"</a>" for storage, retention, and deletion details."</p>
        </section>
    }
}

#[component]
fn Privacy() -> impl IntoView {
    view! {
        <main id="main" class="policy-page stack">
            <a href="/">"← Back to welcome"</a>
            <section class="page-hero"><p class="eyebrow">"Privacy · draft 2026-10"</p><h1>"Your data & privacy"</h1><p>"Policy information · operator review required before publishing"</p></section>
            <section class="card stack">
                <h2>"What the app stores"</h2>
                <p>"Signed-in plans store your timetable and course details, attendance snapshot (date and counts), and selected leave. Supabase stores your account identity, including email and profile information supplied at sign-in. App sessions are stored on the server with encrypted session credentials; the browser uses a session cookie to access your account."</p>
                <h2>"Device copies & tabs"</h2>
                <p>"The browser keeps device drafts so work can survive a reload. Each document keeps a separate copy; other tabs can retain drafts and conflicts. The sandbox is device-only, not a cloud account. Clearing browser site storage removes local copies, but also removes other app drafts in that browser. Downloaded files and copies on other devices must be managed separately."</p>
                <h2>"Optional AI & external sharing"</h2>
                <p>"AI is optional. Image import sends the selected image to the configured external AI provider; crop out student IDs or other private information first. Natural-language questions routed to AI share the question, course codes, snapshot date, current date and calendar context. Structured status and maximum-skip checks do not use an external AI service. Attendance arithmetic always runs in Rust. Provider processing is outside this app's storage; no provider retention period is promised here."</p>
                <p>"AI usage is quota-limited. Usage records track feature use and quota consumption, including failed provider requests once admitted. Routine cleanup removes usage records older than 90 calendar days, subject to maintenance timing; this does not mean all account data is automatically deleted after 90 days."</p>
                <h2>"Request logs & analytics"</h2>
                <p>"The service uses operational request logs for running and troubleshooting the app. The current app code does not include analytics tracking. Operational logs are distinct from saved plans and AI usage records; no log-retention guarantee is made here."</p>
                <h2>"Exports & deletion"</h2>
                <p>"Signed-in users can download their app account data as JSON from Settings / Account and export device drafts separately. Requesting account deletion requires the exact phrase DELETE MY ACCOUNT, a second confirmation, and an interactive sign-in within the last 5 minutes, checked by the server. A refresh is not a new interactive sign-in."</p>
                <p>"A successful deletion request immediately erases app data and revokes all app sessions. This browser clears this user's drafts across documents and notifies other open tabs. Supabase identity deletion remains pending manual operator review and action; the request does not claim that the sign-in identity has already been deleted. Your Google account is not deleted, and this does not force a Google sign-out."</p>
                <p>"Operator-reviewed imports may retain audit metadata such as the semester, import time and a plan fingerprint, not an extra plan copy. Account exports include your linked import history. Deletion removes those user-linked audit rows; anonymous batch audit remains. Downloaded files and operator backups have separate handling: before launch, the operator must define retention and replay deletion requests when restoring a backup."</p>
                <h2>"Your controls & contact"</h2>
                <p>"You can use the sandbox without signing in, avoid optional AI, export your data, clear device storage, sign out of the app, or request account deletion from Settings / Account. For questions or help with pending identity deletion, contact the operator using the support contact below."</p>
                <SupportContact/>
                <p class="banner warning">"Pre-launch release check: the operator must manually review and approve this policy information and configure a support contact before publishing."</p>
            </section>
            <SiteFooter/>
        </main>
    }
}

#[component]
fn Terms() -> impl IntoView {
    view! {
        <main id="main" class="policy-page stack">
            <a href="/">"← Back to welcome"</a>
            <section class="page-hero"><p class="eyebrow">"Terms · draft 2026-10"</p><h1>"Using this planning tool"</h1><p>"Policy information · operator review required before publishing"</p></section>
            <section class="card stack">
                <h2>"Unofficial planning tool"</h2>
                <p>"AmIGettingDebarred? is an independent, unofficial planning tool, not an official college service or an official source of attendance eligibility. Forecasts are informational; they do not grant permission to miss classes or determine a college's decisions."</p>
                <h2>"Check your inputs & official records"</h2>
                <p>"You are responsible for checking your timetable, attendance snapshot date and counts, leave selections, and the calendar used by the app. Projections assume attendance at every future class except selected leave. Calendar changes, cancelled or extra classes, and college rules can change the outcome. Verify eligibility and requirements with official college records before relying on a forecast."</p>
                <h2>"Optional features & your copies"</h2>
                <p>"AI image import and helper answers are optional and can be wrong. Review imported timetables before applying them. Keep exports of work you need; a device draft is not a cloud save. Do not submit other people's private information or misuse the service. AI features are subject to usage quotas and availability."</p>
                <h2>"Account controls & privacy"</h2>
                <p>"Settings / Account provides app data export and a confirmed deletion request. App data removal is immediate after a successful request, while Supabase identity deletion requires manual operator action. Your Google account is not deleted. See the privacy information for device copies, optional external AI sharing, retention, and support."</p>
                <p><a href="/privacy">"Read the privacy information"</a></p>
                <SupportContact/>
                <p class="banner warning">"Pre-launch release check: the operator must manually review and approve this draft information and configure a support contact before publishing."</p>
            </section>
            <SiteFooter/>
        </main>
    }
}

#[component]
fn Notice() -> impl IntoView {
    let ctx = context();
    view! { {move || ctx.notice.get().map(|text| view! {
        <div class="banner notice" role="status"><span>{text}</span><button aria-label="Dismiss notice" on:click=move |_| ctx.notice.set(None)>"Dismiss"</button></div>
    })} }
}

#[component]
fn Workspace() -> impl IntoView {
    let ctx = context();
    let helper_trigger = NodeRef::<leptos::html::Button>::new();
    view! {
        <header class="site-header"><div class="header-inner">
            <a class="brand" href="/">"AmIGettingDebarred?"</a>
            <span class="semester">{move || ctx.calendar().config().label.clone()}</span>
            <span class="account">{move || match ctx.auth.get() { AuthState::SignedIn(user) => user.display_name.or(user.email).unwrap_or_else(|| "Signed in".into()), _ => "Sandbox".into() }}</span>
            <button aria-label="Settings / Account" aria-current=move || if ctx.page.get() == Page::Account { Some("page") } else { None } on:click=move |_| ctx.navigate(Page::Account)>"Settings / Account"</button>
            <Show when=move || !ctx.is_demo()><button on:click=move |_| ctx.logout()>"Sign out"</button></Show>
            <Show when=move || ctx.is_demo()><a class="button" href="/">"Sign in instead"</a></Show>
        </div></header>
        <div class="workspace">
            <Show when=move || ctx.is_demo()><div class="banner sandbox" data-testid="sandbox-banner">"Device-only sandbox · not signed in · no cloud saves"</div></Show>
            <Show when=move || ctx.page.get() != Page::Account>
            <nav class="tabs" aria-label="Planner steps">
                <button aria-label="Timetable & attendance" data-testid="setup-tab" class:active=move || ctx.page.get() != Page::Planner aria-current=move || if ctx.page.get() != Page::Planner { Some("page") } else { None } on:click=move |_| ctx.navigate(Page::Setup)>"1 · Timetable & attendance"</button>
                <button aria-label="Plan leaves" data-testid="planner-tab" class:active=move || ctx.page.get() == Page::Planner aria-current=move || if ctx.page.get() == Page::Planner { Some("page") } else { None } on:click=move |_| ctx.navigate(Page::Planner)>"2 · Plan leaves"</button>
            </nav>
            <SaveStatus/>
            <Notice/>
            <DeviceDraftShelf/>
            {move || ctx.validation_error().map(|error| view! {
                <div class="banner danger" role="alert"><h2>"This plan needs attention"</h2><p>{error}</p><p>"Correct the timetable, snapshot date, or attendance below. Export a draft before clearing anything you want to keep."</p><div class="actions"><button on:click=move |_| ctx.download_draft()>"Export draft"</button><ConfirmButton label="Clear leave selections" explanation="Remove all planned leave?" action=Callback::new(move |_| ctx.clear_leave())/><ConfirmButton label="Clear timetable" explanation="Clear the timetable and its attendance? Export a draft first if you need a copy." action=Callback::new(move |_| ctx.clear_timetable())/></div></div>
            })}
            </Show>
            <Show when=move || ctx.page.get() == Page::Account><Notice/></Show>
            <main id="main" class="app-layout" class:account-layout=move || ctx.page.get() == Page::Account>
                <div class="main-column">{move || match ctx.page.get() {
                    Page::Account => view! { <Account/> }.into_any(),
                    Page::Planner => view! { <Planner/> }.into_any(),
                    Page::NotFound => view! { <section class="card"><h1>"Page not found"</h1><button on:click=move |_| ctx.navigate(Page::Setup)>"Back to timetable"</button></section> }.into_any(),
                    _ => view! { <Setup/> }.into_any(),
                }}</div>
                <Show when=move || ctx.page.get() != Page::Account>
                    <aside class="sidebar" aria-label="Attendance overview"><Standings/><div class="desktop-helper"><HelperBody/></div></aside>
                </Show>
            </main>
            <SiteFooter/>
        </div>
        <Show when=move || ctx.page.get() != Page::Account>
        <button node_ref=helper_trigger class="mobile-helper-toggle primary" aria-haspopup="dialog" aria-expanded=move || if ctx.helper_open.get() { "true" } else { "false" } on:click=move |_| ctx.helper_open.set(true)>"Attendance helper"</button>
        <Show when=move || ctx.helper_open.get()>
            <MobileHelper trigger=helper_trigger/>
        </Show>
        </Show>
    }
}

#[component]
fn MobileHelper(trigger: NodeRef<leptos::html::Button>) -> impl IntoView {
    let ctx = context();
    let dialog = NodeRef::<leptos::html::Dialog>::new();
    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        if let Some(element) = dialog.get() {
            if !element.open() && element.show_modal().is_err() {
                ctx.notice.set(Some(
                    "The attendance helper could not open. Please try again.".into(),
                ));
                ctx.helper_open.set(false);
            }
        }
    });
    let close = Callback::new(move |()| {
        #[cfg(feature = "hydrate")]
        {
            if let Some(element) = dialog.get_untracked() {
                element.close();
            }
            if let Some(button) = trigger.get_untracked() {
                let _ = button.focus();
            }
        }
        ctx.helper_open.set(false);
    });
    #[cfg(not(feature = "hydrate"))]
    let _ = trigger;
    view! {
        <dialog node_ref=dialog class="mobile-helper" aria-label="Attendance helper"
            on:cancel=move |ev: leptos::ev::Event| { ev.prevent_default(); close.run(()); }
            on:close=move |_| {
                ctx.helper_open.set(false);
                #[cfg(feature = "hydrate")]
                if let Some(button) = trigger.get_untracked() { let _ = button.focus(); }
            }
        >
            <button class="helper-close" autofocus aria-label="Close attendance helper" on:click=move |_| close.run(())>"Close ×"</button>
            <HelperBody/>
        </dialog>
    }
}

#[component]
fn SaveStatus() -> impl IntoView {
    let ctx = context();
    view! {
        <section class="save-status" aria-label="Save status" data-testid="save-status">
            <p role="status" aria-live="polite" aria-atomic="true"><span class="status-dot" aria-hidden="true"></span>{move || match ctx.sync.get() {
                SyncStatus::Loading => "Loading saved plan…".to_string(),
                SyncStatus::Saved => "All changes saved".into(),
                SyncStatus::Unsaved => "Changes not yet saved".into(),
                SyncStatus::Saving => "Saving changes…".into(),
                SyncStatus::LocalOnly => "Device-only · no cloud saves".into(),
                SyncStatus::Failed(error) => format!("Save or load failed: {error}"),
                SyncStatus::Conflict => "Conflict · cloud plan changed elsewhere".into(),
                SyncStatus::Expired => "Session expired · your draft has not been saved to the cloud".into(),
                SyncStatus::Invalid(error) => format!("Invalid plan · {error}"),
            }}</p>
            {move || match ctx.sync.get() {
                SyncStatus::Conflict => view! { <div class="banner warning" role="alert"><h2>"Keep your draft safe"</h2><p>"Another session saved a newer cloud plan. This draft will not overwrite it. Export your draft before reloading; reloading discards local edits."</p><div class="actions"><button on:click=move |_| ctx.download_draft()>"Export draft"</button><ConfirmButton label="Reload cloud" explanation="Discard local edits and load the latest cloud plan?" editable=false action=Callback::new(move |_| ctx.reload_cloud())/></div></div> }.into_any(),
                SyncStatus::Failed(_) => view! { <div class="actions"><button on:click=move |_| ctx.retry_save()>"Retry save / load"</button><button on:click=move |_| ctx.download_draft()>"Export draft"</button></div> }.into_any(),
                SyncStatus::Expired => view! { <div class="actions"><button on:click=move |_| ctx.download_draft()>"Export draft"</button><a href="/">"Sign in again"</a></div> }.into_any(),
                _ => ().into_any(),
            }}
        </section>
    }
}

#[component]
fn DeviceDraftShelf() -> impl IntoView {
    let ctx = context();
    view! {
        <Show when=move || ctx.device_drafts.get().iter().any(|entry| !entry.current)>
            <details class="card stack device-drafts">
                <summary>"Device drafts"</summary>
                <p class="small muted">"Each document keeps a separate device copy. Restore an archived draft explicitly; cloud conflicts remain blocked. Other copies are not overwritten."</p>
                <ul class="stack">
                    <For each=move || ctx.device_drafts.get() key=|entry| entry.key.clone()
                        children=move |entry| view! { <DeviceDraftRow draft_key=entry.key/> }
                    />
                </ul>
            </details>
        </Show>
    }
}

#[component]
fn DeviceDraftRow(draft_key: String) -> impl IntoView {
    let ctx = context();
    let key = StoredValue::new(draft_key);
    let current = Memo::new(move |_| {
        ctx.device_drafts
            .get()
            .iter()
            .any(|entry| entry.key == key.get_value() && entry.current)
    });
    view! {
        <li class="stack" data-testid="device-draft">
            <span>{move || ctx.device_drafts.get().into_iter().find(|entry| entry.key == key.get_value()).map(|entry| entry.label).unwrap_or_default()}</span>
            <Show when=move || current.get()><strong class="small">"Current device copy"</strong></Show>
            <Show when=move || !current.get()>
                <ConfirmButton label="Restore draft"
                    explanation="Replace the current plan with this archived device draft? Export your current draft first if you want to keep a copy. Cloud conflicts remain blocked."
                    action=Callback::new(move |_| ctx.restore_device_draft(key.get_value()))
                />
            </Show>
        </li>
    }
}

#[component]
fn ConfirmButton(
    label: &'static str,
    explanation: &'static str,
    action: Callback<()>,
    #[prop(default = true)] editable: bool,
    #[prop(optional, into)] disabled: Signal<bool>,
) -> impl IntoView {
    let ctx = context();
    let confirming = RwSignal::new(false);
    view! {
        <div class="confirm-action">
            <button type="button" disabled=move || disabled.get() || (editable && !ctx.can_edit()) on:click=move |_| confirming.set(true)>{label}</button>
            <Show when=move || confirming.get()>
                <div class="confirmation" role="group" aria-label=label><p>{explanation}</p><div class="actions">
                    <button type="button" class="danger-button" disabled=move || disabled.get() || (editable && !ctx.can_edit()) on:click=move |_| {
                        if !disabled.get_untracked() && (!editable || ctx.can_edit()) {
                            action.run(());
                            confirming.set(false);
                        }
                    }>"Confirm "{label}</button>
                    <button type="button" on:click=move |_| confirming.set(false)>"Cancel"</button>
                </div></div>
            </Show>
        </div>
    }
}

#[component]
fn Setup() -> impl IntoView {
    let ctx = context();
    view! {
        <section class="page-hero"><p class="eyebrow">"Step 1 of 2"</p><h1>"Your timetable & attendance"</h1><p>"Add your weekly classes, then tell us what you have attended so far."</p></section>
        <section class="card stack" aria-labelledby="timetable-title">
            <div class="section-heading"><div><h2 id="timetable-title">"Your FFCS timetable"</h2><p class="muted small">"Enter course codes. New codes are registered automatically. Leave free slots empty."</p></div></div>
            <div class="actions"><ConfirmButton label="Load sample timetable" explanation="Replace your current plan with the sample timetable and attendance?" action=Callback::new(move |_| ctx.load_sample())/><ConfirmButton label="Clear timetable" explanation="Clear your timetable and attendance? Export a draft first if needed." action=Callback::new(move |_| ctx.clear_timetable())/><button on:click=move |_| ctx.download_draft()>"Export draft"</button></div>
            <p class="small muted" id="grid-hint">"On a small screen, swipe the timetable sideways. Changes commit when you leave a field."</p>
            <div class="table-scroll" tabindex="0" role="region" aria-label="Weekly timetable" aria-describedby="grid-hint">
                <table class="timetable"><caption class="sr-only">"Six-day, seven-slot weekly timetable"</caption><thead><tr><th scope="col">"Day"</th>{TIME_SLOTS.iter().enumerate().map(|(i, slot)| view! { <th scope="col">{format!("Slot {}", i + 1)}<small>{format!("{}–{}", slot.start, slot.end)}</small></th> }).collect_view()}</tr></thead>
                    <tbody>{[GridDay::Mon, GridDay::Tue, GridDay::Wed, GridDay::Thu, GridDay::Fri, GridDay::Sat].into_iter().map(|day| view! {
                        <tr><th scope="row">{format!("{day:?}")}</th>{(0..7).map(move |index| view! {
                            <td><span class="slot-code">{slot_codes(day)[index]}</span><input type="text" maxlength="32" autocapitalize="characters" spellcheck="false" aria-label=format!("Course code {day:?} slot {}", index + 1) disabled=move || !ctx.can_edit() value=move || ctx.plan.get().timetable.grid.get(&day).and_then(|row| row[index].clone()).unwrap_or_default() prop:value=move || ctx.plan.get().timetable.grid.get(&day).and_then(|row| row[index].clone()).unwrap_or_default() on:change=move |ev| ctx.set_cell(day, index, event_target_value(&ev))/></td>
                        }).collect_view()}</tr>
                    }).collect_view()}</tbody>
                </table>
            </div>
            <ImageImport/>
        </section>
        <section class="card stack" aria-labelledby="attendance-title">
            <h2 id="attendance-title">"Attendance so far"</h2>
            <label class="snapshot">"Attendance completed through"<input type="date" aria-describedby="snapshot-rules" disabled=move || !ctx.can_edit() value=move || ctx.plan.get().attendance.as_of.to_string() prop:value=move || ctx.plan.get().attendance.as_of.to_string() on:change=move |ev| match NaiveDate::parse_from_str(&event_target_value(&ev), "%Y-%m-%d") { Ok(date) => ctx.set_snapshot(date), Err(_) => ctx.notice.set(Some("Choose a valid completed date.".into())) }/></label>
            <p id="snapshot-rules" class="small muted">"Inclusive end-of-day snapshot: enter attendance through a fully completed date. Dates on or before it cannot be selected as future leave. This date never advances automatically at midnight."</p>
            <Show when=move || ctx.plan.get().timetable.courses.is_empty()><p class="empty">"No courses yet. Enter a course code in the timetable, or load the sample."</p></Show>
            <div class="course-list"><For each=move || ctx.plan.get().timetable.courses key=|course| course.code.clone() children=move |course| view! { <AttendanceCourse code=course.code/> }/></div>
            <button class="primary" on:click=move |_| ctx.navigate(Page::Planner)>"Plan leaves →"</button>
        </section>
    }
}

#[component]
fn AttendanceCourse(code: String) -> impl IntoView {
    let ctx = context();
    let code = StoredValue::new(code);
    let remove = RwSignal::new(false);
    view! {
        <article class="attendance-course">
            <strong>{code.get_value()}</strong>
            <label>"Course name"<input type="text" maxlength="160" disabled=move || !ctx.can_edit() value=move || ctx.plan.get().timetable.courses.iter().find(|c| c.code == code.get_value()).map(|c| c.name.clone()).unwrap_or_default() prop:value=move || ctx.plan.get().timetable.courses.iter().find(|c| c.code == code.get_value()).map(|c| c.name.clone()).unwrap_or_default() on:change=move |ev| ctx.set_course_name(code.get_value(), event_target_value(&ev))/></label>
            <label>"Classes attended"
                            <input
                                type="number" min="0" step="1"
                                aria-label=format!("Classes attended {}", code.get_value())
                                max={move || ctx.held_for(&code.get_value())}
                                disabled={move || !ctx.can_edit() || ctx.held_for(&code.get_value()) == 0}
                                value={move || ctx.plan.get().attendance.attended.get(&code.get_value()).copied().unwrap_or(0).to_string()}
                                prop:value={move || ctx.plan.get().attendance.attended.get(&code.get_value()).copied().unwrap_or(0).to_string()}
                                on:change={move |ev| match event_target_value(&ev).parse::<u32>() {
                                    Ok(value) => ctx.set_attended(code.get_value(), value),
                                    Err(_) => ctx.notice.set(Some("Attendance must be a whole, non-negative number.".into())),
                                }}
                            />
                            <span class="small muted">{move || format!("of {} classes held", ctx.held_for(&code.get_value()))}</span>
                        </label>
            <button aria-label=format!("Remove course {}", code.get_value()) disabled=move || !ctx.can_edit() on:click=move |_| remove.set(true)>"Remove"</button>
            <Show when=move || remove.get()><div class="confirmation"><p>"Remove this course and its timetable entries and attendance?"</p><div class="actions"><button class="danger-button" disabled=move || !ctx.can_edit() on:click=move |_| { ctx.remove_course(code.get_value()); remove.set(false); }>"Confirm remove course"</button><button on:click=move |_| remove.set(false)>"Cancel"</button></div></div></Show>
        </article>
    }
}

#[component]
fn ImageImport() -> impl IntoView {
    let ctx = context();
    view! {
        <details class="image-import"><summary>"Import a timetable image"</summary>
            <Show when=move || !ctx.is_demo() fallback=|| view! { <p class="banner warning">"Sign in to import timetable images. Image import is unavailable in the device-only sandbox; you can still enter courses manually."</p> }>
                <p class="small muted">"Upload a readable image of the full weekly grid. The image is sent to the configured image service. Review the extracted timetable before applying; extraction does not change your plan."</p>
                <label>"Timetable image"<input type="file" accept="image/png,image/jpeg,image/webp" disabled=move || !ctx.can_edit() || ctx.import_busy.get() on:change=move |ev| {
                    #[cfg(feature = "hydrate")]
                    {
                        let input = event_target::<web_sys::HtmlInputElement>(&ev);
                        if let Some(file) = input.files().and_then(|files| files.get(0)) { ctx.import_file(file); }
                        input.set_value("");
                    }
                    #[cfg(not(feature = "hydrate"))]
                    let _ = ev;
                }/></label>
                {move || ctx.quotas.get().map(|q| view! { <p class="small muted">{format!("{} of {} image imports remaining", q.image.remaining, q.image.limit)}</p> })}
                <Show when=move || ctx.import_busy.get()><p role="status">"Reading your timetable image… Please wait for the review."</p></Show>
                {move || ctx.import_draft.get().map(|draft| view! {
                    <section class="import-review stack" aria-label="Review imported timetable"><h3>"Review extracted timetable"</h3><p>"Check every course and slot. Apply replaces the timetable, then follows the normal save process."</p><TimetablePreview timetable=draft/><div class="actions"><button class="primary" disabled=move || !ctx.can_edit() || ctx.import_busy.get() on:click=move |_| ctx.apply_import()>"Apply imported timetable"</button><button on:click=move |_| ctx.cancel_import()>"Cancel import"</button></div></section>
                })}
            </Show>
        </details>
    }
}

#[component]
fn TimetablePreview(timetable: Timetable) -> impl IntoView {
    view! { <div class="table-scroll" tabindex="0" role="region" aria-label="Imported timetable preview"><table class="timetable"><thead><tr><th>"Day"</th>{(1..=7).map(|i| view! { <th>{format!("Slot {i}")}</th> }).collect_view()}</tr></thead><tbody>{timetable.grid.into_iter().map(|(day, row)| view! { <tr><th scope="row">{format!("{day:?}")}</th>{row.into_iter().map(|code| view! { <td>{code.unwrap_or_else(|| "—".into())}</td> }).collect_view()}</tr> }).collect_view()}</tbody></table><ul>{timetable.courses.into_iter().map(|course| view! { <li>{format!("{} · {}", course.code, course.name)}</li> }).collect_view()}</ul></div> }
}

#[component]
fn Planner() -> impl IntoView {
    let ctx = context();
    let calendar = ctx.calendar();
    let months: Vec<(i32, u32)> = calendar
        .days()
        .iter()
        .map(|d| (d.date.year(), d.date.month()))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let initial = months
        .iter()
        .position(|m| {
            *m == (
                ctx.plan.get_untracked().attendance.as_of.year(),
                ctx.plan.get_untracked().attendance.as_of.month(),
            )
        })
        .unwrap_or(0);
    let month = RwSignal::new(initial);
    let per_class = RwSignal::new(false);
    let selected = RwSignal::new(None::<NaiveDate>);
    let month_options = months.clone();
    view! {
        <section class="page-hero"><p class="eyebrow">"Step 2 of 2"</p><h1>"Plan your leaves"</h1><p>"Tap a class day to take it off. Your projections update straight away."</p><div class="hero-stats">{move || { let summary = ctx.leave_summary(); view! { <span><strong>{summary.days}</strong>" days with leave"</span><span><strong>{summary.classes}</strong>" classes missed"</span> } }}</div></section>
        <section class="card stack" aria-label="Leave calendar">
            <div class="section-heading"><label>"Month"<select prop:value=move || month.get().to_string() on:change=move |ev| { if let Ok(index) = event_target_value(&ev).parse::<usize>() { month.set(index); selected.set(None); } }>{month_options.into_iter().enumerate().map(|(i, (year, m))| view! { <option value=i.to_string() selected={move || month.get() == i}>{NaiveDate::from_ymd_opt(year, m, 1).map(|d| d.format("%B %Y").to_string()).unwrap_or_default()}</option> }).collect_view()}</select></label>
                <fieldset class="mode-switch"><legend>"Select leave by"</legend><label><input type="radio" name="leave-mode" checked=move || !per_class.get() prop:checked=move || !per_class.get() on:change=move |_| { per_class.set(false); selected.set(None); }/>"Whole days"</label><label><input type="radio" name="leave-mode" prop:checked=move || per_class.get() on:change=move |_| per_class.set(true)/>"Per classes"</label></fieldset>
            </div>
            <p class="small muted">"Only scheduled classes after your attendance snapshot can be selected. Partially selected days keep their individual class choices."</p>
            <div class="calendar-week" aria-hidden="true">{["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].into_iter().map(|day| view! { <span>{day}</span> }).collect_view()}</div>
            <div class="calendar-grid">{move || {
                let active = months.get(month.get()).copied();
                let days = ctx.calendar().days().iter().filter(|d| Some((d.date.year(), d.date.month())) == active).cloned().collect::<Vec<_>>();
                let padding = days.first().map(|d| d.date.weekday().num_days_from_monday()).unwrap_or(0);
                view! {
                    {(0..padding).map(|_| view! { <span class="calendar-padding" aria-hidden="true"></span> }).collect_view()}
                    {days.into_iter().map(|day| { let date = day.date; view! { <CalendarCell date=date kind=day.kind note=day.note per_class=per_class selected=selected/> } }).collect_view()}
                }
            }}</div>
            <ul class="legend" aria-label="Calendar legend"><li class="selected">"Selected"</li><li class="partial">"Some classes"</li><li class="holiday">"Holiday"</li><li class="exam">"Exam"</li><li class="working-saturday">"Working Saturday"</li><li>"Faded: past / no classes"</li></ul>
            {move || selected.get().map(|date| view! { <SlotPicker date=date/> })}
            <ConfirmButton label="Clear leave selections" explanation="Remove all whole-day and individual-class leave selections?" action=Callback::new(move |_| ctx.clear_leave())/>
        </section>
        <section class="stack" aria-labelledby="projections-title"><h2 id="projections-title">"Your checkpoint projections"</h2><p class="small muted">{move || format!("Minimum required: {}%. Counts use your timetable and the published calendar, not AI arithmetic.", ctx.calendar().config().minimum_percentage)}</p><Projections/></section>
    }
}

#[component]
fn CalendarCell(
    date: NaiveDate,
    kind: DayKind,
    note: Option<String>,
    per_class: RwSignal<bool>,
    selected: RwSignal<Option<NaiveDate>>,
) -> impl IntoView {
    let ctx = context();
    let note = StoredValue::new(note.unwrap_or_default());
    let kind_class = match kind {
        DayKind::Holiday => "holiday",
        DayKind::Exam => "exam",
        DayKind::WorkingSaturday => "working-saturday",
        _ => "",
    };
    view! {
        <button type="button" class=format!("calendar-day {kind_class}") class:selected=move || { let (n, total) = ctx.day_selection(date); total > 0 && n == total } class:partial=move || { let (n, total) = ctx.day_selection(date); n > 0 && n < total } aria-pressed=move || { let (n, total) = ctx.day_selection(date); if n > 0 && n < total { "mixed" } else if n > 0 { "true" } else { "false" } } aria-label=move || { let (n, total) = ctx.day_selection(date); format!("{}, {} of {} classes selected. {}", date.format("%A %d %B %Y"), n, total, note.get_value()) } title=note.get_value() disabled={move || !ctx.can_edit() || date <= ctx.plan.get().attendance.as_of || ctx.meetings(date).is_empty()} on:click=move |_| { if per_class.get_untracked() { selected.set(Some(date)); } else { ctx.toggle_day(date); } }>
            <strong>{date.day()}</strong><span class="day-count">{move || { let (n, total) = ctx.day_selection(date); if n > 0 { format!("{n}/{total} off") } else if total > 0 { format!("{total} classes") } else { "—".into() } }}</span><span class="day-note">{note.get_value()}</span>
        </button>
    }
}

#[component]
fn SlotPicker(date: NaiveDate) -> impl IntoView {
    let ctx = context();
    view! { <section class="slot-picker stack" aria-label=format!("Choose classes on {date}")><h3>{date.format("%A, %d %B").to_string()}</h3><p class="small muted">"Checked classes are planned leave. Uncheck one to keep the rest of a whole-day selection."</p>{move || ctx.meetings(date).into_iter().map(|meeting| {
        let slot = meeting.slot;
        view! { <label class="slot-option"><input type="checkbox" prop:checked=move || ctx.plan.get().leave.iter().any(|entry| match entry { LeaveEntry::Day { date: d } => *d == date, LeaveEntry::Slot { date: d, slot: s } => *d == date && *s == slot }) disabled={move || !ctx.can_edit() || date <= ctx.plan.get().attendance.as_of} on:change=move |_| ctx.toggle_slot(date, slot)/>{format!("Slot {} · {} · {}–{}", slot.get() + 1, meeting.course_code, TIME_SLOTS[slot.get()].start, TIME_SLOTS[slot.get()].end)}</label> }
    }).collect_view()}</section> }
}

#[component]
fn Projections() -> impl IntoView {
    let ctx = context();
    view! { {move || {
        if let Some(error) = ctx.validation_error() { return view! { <p class="banner warning">{format!("Fix the plan to see reliable projections: {error}")}</p> }.into_any(); }
        if ctx.plan.get().timetable.courses.is_empty() { return view! { <p class="empty">"Add your timetable and attendance to see checkpoint projections."</p> }.into_any(); }
        ctx.results().into_iter().map(|result| match result {
            CheckpointProjection::Historical { checkpoint } => view! { <section class="card historical"><h3>{checkpoint.label}</h3><p class="small muted">"Historical checkpoint · the current attendance snapshot cannot reconstruct eligibility at this earlier date."</p></section> }.into_any(),
            CheckpointProjection::Upcoming { checkpoint, courses } => {
                let checkpoint_id = checkpoint.id;
                view! { <section class="card checkpoint stack"><div class="section-heading"><h3>{checkpoint.label}</h3><span class="small muted">{format!("Before {}", checkpoint.start_date.format("%d %b %Y"))}</span></div><div class="projection-courses">{courses.into_iter().map(|course| view! { <ProjectionCourse course=course checkpoint_id=checkpoint_id.clone()/> }).collect_view()}</div></section> }.into_any()
            }
        }).collect_view().into_any()
    }} }
}

#[component]
fn ProjectionCourse(course: CourseProjection, checkpoint_id: String) -> impl IntoView {
    let ctx = context();
    let name = ctx
        .plan
        .get_untracked()
        .timetable
        .courses
        .iter()
        .find(|c| c.code == course.course_code)
        .map(|c| c.name.clone())
        .unwrap_or_default();
    let status = if !course.has_classes {
        "No classes"
    } else if course.safe {
        "Safe"
    } else {
        "Below threshold"
    };
    let tone = if !course.has_classes {
        "neutral"
    } else if course.safe {
        "safe"
    } else {
        "at-risk"
    };
    let allowance = ctx.allowance(&course.course_code, &checkpoint_id);
    view! { <article class=format!("projection-course {tone}")><div class="section-heading"><strong>{course.course_code.clone()}</strong><span class="status-chip">{status}</span></div><p class="small muted">{name}</p>
        <p class="percentage">{if course.has_classes { format!("{:.1}%", course.percentage) } else { "—".into() }}</p>
        <p class="small">{format!("{} attended / {} held at checkpoint", course.total_attended, course.total_held)}</p>
        <p class="small muted">{format!("So far: {} / {} · Future: {} classes, {} planned skips", course.attended_so_far, course.held_so_far, course.future_held, course.future_skipped)}</p>
        <p class="small">{format!("Minimum attended required: {}", course.minimum_attended_required)}</p>
        {allowance.map(|a| view! { <p class="allowance small">{if a.shortfall > 0 { format!("{} classes short under this plan", a.shortfall) } else { format!("{} additional skips available · {} already planned", a.additional, a.already_planned) }}</p> })}
    </article> }
}

#[component]
fn Standings() -> impl IntoView {
    let ctx = context();
    view! { <section class="card standings stack" aria-labelledby="standings-title"><div><p class="eyebrow">"Your attendance, at a glance"</p><h2 id="standings-title">"Where you stand"</h2></div><p class="small muted">{move || format!("Snapshot: {} · Required: {}%", ctx.plan.get().attendance.as_of, ctx.calendar().config().minimum_percentage)}</p><div class="summary-numbers">{move || { let s = ctx.leave_summary(); view! { <span><strong>{s.days}</strong>" days with leave"</span><span><strong>{s.classes}</strong>" classes missed"</span> } }}</div><StandingRows/><p class="small muted">"Checkpoint cards show course-by-course safety and additional skips. No classes is not evidence of eligibility."</p><button on:click=move |_| ctx.navigate(Page::Planner)>"View checkpoint projections"</button></section> }
}

#[component]
fn StandingRows() -> impl IntoView {
    let ctx = context();
    view! { {move || {
        if ctx.validation_error().is_some() { return view! { <p class="small warning banner">"Resolve plan errors before using these projections."</p> }.into_any(); }
        let next = ctx.results().into_iter().find_map(|result| match result {
            CheckpointProjection::Upcoming { checkpoint, courses } => Some((checkpoint, courses)),
            CheckpointProjection::Historical { .. } => None,
        });
        match next {
            Some((checkpoint, courses)) if !courses.is_empty() => {
                let id = checkpoint.id;
                view! { <div class="stack"><h3>{format!("At {}", checkpoint.label)}</h3>{courses.into_iter().map(|course| {
                    let allowance = ctx.allowance(&course.course_code, &id);
                    let status = if !course.has_classes { "No classes".into() } else { format!("{:.1}% · {}", course.percentage, if course.safe { "Safe" } else { "Below threshold" }) };
                    view! { <div class="standing-row"><strong>{course.course_code}</strong><span>{status}</span>{allowance.filter(|_| course.has_classes).map(|a| view! { <span class="muted">{if a.shortfall > 0 { format!("{} classes short", a.shortfall) } else { format!("{} more skips available", a.additional) }}</span> })}</div> }
                }).collect_view()}</div> }.into_any()
            }
            _ => view! { <p class="small muted">"No upcoming course projections. Add courses, or review historical checkpoints in the planner."</p> }.into_any(),
        }
    }} }
}

#[component]
fn HelperBody() -> impl IntoView {
    let ctx = context();
    let question = RwSignal::new(String::new());
    view! { <section class="card helper stack" aria-label="Ask about your attendance"><h2>"Ask about your attendance"</h2>
        <p class="small muted">{move || if !ctx.is_demo() && ctx.runtime.get().ai_enabled { "Ask a question in your own words. AI interprets your question; Rust computes and explains results." } else { "Local structured helper: check your status or maximum skips. Natural-language AI and image import require sign-in and an enabled service. No extra AI arithmetic." }}</p>
        <div class="helper-starters"><button disabled=move || ctx.helper_busy.get() on:click=move |_| ctx.ask("status".into())>"Check my status"</button><button disabled=move || ctx.helper_busy.get() on:click=move |_| ctx.ask("max_skips".into())>"Maximum skips"</button></div>
        <div class="messages" role="log" aria-live="polite" aria-relevant="additions text">{move || ctx.messages.get().into_iter().map(|message| view! { <p class=if message.error { "message error" } else if message.from_user { "message user" } else { "message answer" }><strong class="sr-only">{if message.from_user { "You: " } else { "Helper: " }}</strong>{message.text}</p> }).collect_view()}<Show when=move || ctx.helper_busy.get()><p role="status">"Working on your question…"</p></Show></div>
        <Show when=move || !ctx.is_demo() && ctx.runtime.get().ai_enabled>
            <form class="helper-form" on:submit=move |ev| { ev.prevent_default(); let text = question.get_untracked().trim().to_string(); if text.len() > 1000 { ctx.notice.set(Some("Questions must be at most 1,000 UTF-8 bytes.".into())); } else if !text.is_empty() && !ctx.helper_busy.get_untracked() { ctx.ask(text); question.set(String::new()); } }><label>"Your question"<textarea rows="2" maxlength="1000" placeholder="How many classes can I miss before the next checkpoint?" prop:value=move || question.get() on:input=move |ev| question.set(event_target_value(&ev)) disabled=move || ctx.helper_busy.get()/><span class="small muted">"Maximum 1,000 UTF-8 bytes; some characters use more than one byte."</span></label><button class="primary" type="submit" disabled=move || ctx.helper_busy.get() || question.get().trim().is_empty()>"Ask helper"</button></form>
        </Show>
        {move || ctx.quotas.get().map(|q| view! { <p class="small muted">{format!("{} of {} helper questions remaining", q.helper.remaining, q.helper.limit)}</p> })}
    </section> }
}

#[cfg(all(test, feature = "ssr"))]
mod tests {
    use super::*;
    use crate::model::User;

    fn signed_in() -> AuthState {
        AuthState::SignedIn(User {
            user_id: "private-user-id".into(),
            email: Some("private@example.test".into()),
            display_name: Some("<script>private-profile</script>".into()),
            csrf_token: "never-render-this-token".into(),
        })
    }

    fn render_shell(page: Page, auth: AuthState, pending: bool) -> String {
        let owner = Owner::new();
        owner.with(|| {
            let ctx = UiContext::new("/");
            ctx.page.set(page);
            ctx.auth.set(auth);
            ctx.deletion_pending.set(pending);
            provide_context(ctx);
            view! { <Shell/> }.to_html()
        })
    }

    #[test]
    fn legal_pages_render_before_auth_and_never_render_private_profile() {
        for page in [Page::Privacy, Page::Terms] {
            for auth in [
                AuthState::Checking,
                AuthState::SignedOut,
                AuthState::Demo,
                signed_in(),
            ] {
                let html = render_shell(page, auth, false);
                assert!(html.contains("id=\"main\""));
                assert!(html.contains("operator review required before publishing"));
                assert!(html.contains("Support contact must be configured before launch"));
                assert!(html.contains("href=\"/\""));
                assert!(html.contains("href=\"/privacy\""));
                assert!(html.contains("href=\"/terms\""));
                for secret in [
                    "private-user-id",
                    "private@example.test",
                    "private-profile",
                    "never-render-this-token",
                ] {
                    assert!(!html.contains(secret));
                }
            }
        }
    }

    #[test]
    fn native_privacy_document_contains_policy_without_javascript() {
        let html = crate::render_document_with_contact("/privacy", None);
        for fact in [
            "Supabase",
            "encrypted session credentials",
            "attendance snapshot",
            "separate copy",
            "external AI provider",
            "older than 90 calendar days",
            "request logs",
            "does not include analytics",
            "manual operator",
            "Google account",
            "Support contact must be configured before launch",
        ] {
            assert!(html.contains(fact), "missing policy fact: {fact}");
        }
    }

    #[test]
    fn sandbox_account_exposes_only_device_controls() {
        let html = render_shell(Page::Account, AuthState::Demo, false);
        assert!(html.contains("Device-only sandbox"));
        assert!(html.contains("Export device draft"));
        assert!(html.contains("Request account deletion unavailable in sandbox"));
        assert!(!html.contains("Download my app data"));
        assert!(!html.contains("Deletion confirmation"));
        assert!(!html.contains("Planner steps"));
    }

    #[test]
    fn signed_in_profile_is_escaped_and_account_actions_are_present() {
        let html = render_shell(Page::Account, signed_in(), false);
        assert!(html.contains("private@example.test"));
        assert!(html.contains("&lt;script&gt;private-profile&lt;/script&gt;"));
        assert!(!html.contains("<script>private-profile</script>"));
        assert!(!html.contains("never-render-this-token"));
        assert!(html.contains("Download my app data"));
        assert!(html.contains("Deletion confirmation"));
        assert!(html.contains("DELETE MY ACCOUNT"));
        assert!(html.contains("last 5 minutes"));
    }

    #[test]
    fn deletion_pending_welcome_does_not_claim_identity_deletion() {
        let html = render_shell(Page::Welcome, AuthState::SignedOut, true);
        assert!(html.contains("App data removed; identity deletion pending operator review. Your Google account was not deleted."));
        assert!(!html.contains("Signed-in profile"));
    }
}
