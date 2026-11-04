//! The web dashboard: pages rendered on the server with Maud, styled with Tailwind and made
//! interactive with HTMX, over the same service layer in `spinikus-control` as the API, so
//! the UI never does something the API can't.
//!
//! Conventions: handlers stay thin (extract, call `Control`, render). Signed-in pages take
//! [`auth::Signed`], which turns the session cookie into a principal or redirects to the
//! sign-in page. Every request that changes something passes [`auth::protect`]: it must come
//! from this site, and with a session it must carry the session's CSRF token (the `_csrf`
//! form field or the `X-CSRF-Token` header, which HTMX sends). Styles are Tailwind classes in
//! the Rust source; `cargo xtask css` rebuilds `assets/app.css` after changing them.

mod assets;
mod auth;
mod charts;
mod error;
mod forms;
mod layout;
mod pages;

use axum::{
    Router,
    extract::Request,
    http::{HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
};
use spinikus_control::Control;
use tower_http::trace::TraceLayer;

pub use auth::SESSION_COOKIE;
use error::WebError;

/// The dashboard's routes, to merge with the API's into the server's router.
pub fn router(control: Control) -> Router {
    Router::new()
        .route("/", get(pages::overview::overview))
        .route("/setup", get(pages::session::setup_page).post(pages::session::setup))
        .route("/login", get(pages::session::login_page).post(pages::session::login))
        .route("/logout", post(pages::session::logout))
        .route("/projects", post(pages::manage::create_project))
        .route("/projects/new", get(pages::manage::new_project))
        .route("/projects/{id}", get(pages::projects::project))
        .route("/projects/{id}/settings", get(pages::manage::project_settings).post(pages::manage::update_project))
        .route("/projects/{id}/delete", post(pages::manage::delete_project))
        .route(
            "/projects/{id}/variables",
            get(pages::variables::project_variables).post(pages::variables::create_project_variable),
        )
        .route("/projects/{id}/environments", post(pages::manage::create_environment))
        .route("/projects/{id}/environments/new", get(pages::manage::new_environment))
        .route("/environments/{id}", get(pages::projects::environment))
        .route("/environments/{id}/settings", get(pages::manage::environment_settings).post(pages::manage::update_environment))
        .route("/environments/{id}/settings/previews", post(pages::manage::update_previews))
        .route("/environments/{id}/delete", post(pages::manage::delete_environment))
        .route(
            "/environments/{id}/variables",
            get(pages::variables::environment_variables).post(pages::variables::create_environment_variable),
        )
        .route("/environments/{id}/services", post(pages::service_settings::create_service))
        .route("/environments/{id}/services/new", get(pages::service_settings::new_service))
        .route("/environments/{id}/databases", post(pages::databases::create_database))
        .route("/environments/{id}/databases/new", get(pages::databases::new_database))
        .route("/services/{id}", get(pages::services::service))
        .route("/services/{id}/settings", get(pages::service_settings::settings))
        .route("/services/{id}/settings/general", post(pages::service_settings::update_general))
        .route("/services/{id}/settings/source", post(pages::service_settings::update_source))
        .route("/services/{id}/settings/runtime", post(pages::service_settings::update_runtime))
        .route("/services/{id}/settings/placement", post(pages::service_settings::update_placement))
        .route("/services/{id}/settings/webhook", post(pages::service_settings::create_webhook))
        .route("/services/{id}/settings/webhook/delete", post(pages::service_settings::delete_webhook))
        .route("/services/{id}/settings/volumes", post(pages::service_settings::create_volume))
        .route("/volumes/{id}", post(pages::service_settings::update_volume))
        .route("/volumes/{id}/delete", post(pages::service_settings::delete_volume))
        .route("/services/{id}/delete", post(pages::service_settings::delete))
        .route(
            "/services/{id}/variables",
            get(pages::variables::service_variables).post(pages::variables::create_service_variable),
        )
        .route("/services/{id}/logs", get(pages::logs::logs))
        .route("/services/{id}/logs/stream", get(pages::logs::stream))
        .route("/services/{id}/logs/download", get(pages::logs::download))
        .route("/services/{id}/terminal", get(pages::terminal::terminal))
        .route("/services/{id}/terminal/socket", get(pages::terminal::socket))
        .route("/services/{id}/domains", get(pages::domains::domains).post(pages::domains::create))
        .route("/variables/{id}/edit", get(pages::variables::edit).post(pages::variables::update))
        .route("/variables/{id}/delete", post(pages::variables::delete))
        .route("/variables/{id}/reveal", post(pages::variables::reveal))
        .route("/domains/{id}", post(pages::domains::update))
        .route("/domains/{id}/delete", post(pages::domains::delete))
        .route("/services/{id}/backups", get(pages::backups::backups).post(pages::backups::back_up))
        .route("/services/{id}/backups/activity", get(pages::backups::activity))
        .route("/services/{id}/backup-schedules", post(pages::backups::create_schedule))
        .route("/backup-schedules/{id}", post(pages::backups::update_schedule))
        .route("/backup-schedules/{id}/delete", post(pages::backups::delete_schedule))
        .route("/backups/{id}/restore", post(pages::backups::restore))
        .route("/backups/{id}/delete", post(pages::backups::delete_backup))
        .route("/palette", get(pages::palette::palette))
        .route("/settings", get(pages::settings::index))
        .route("/settings/general", get(pages::settings::general))
        .route("/settings/general/{section}", post(pages::settings::update_general))
        .route("/settings/servers", get(pages::servers::servers).post(pages::servers::create))
        .route("/settings/servers/{id}", get(pages::servers::server))
        .route("/settings/servers/{id}/join-token", post(pages::servers::join_token))
        .route("/settings/servers/{id}/revoke", post(pages::servers::revoke))
        .route("/settings/users", get(pages::users::users).post(pages::users::create))
        .route("/settings/users/{id}", get(pages::users::user).post(pages::users::update))
        .route("/settings/users/{id}/password", post(pages::users::reset_password))
        .route("/settings/users/{id}/disable", post(pages::users::disable))
        .route("/settings/users/{id}/enable", post(pages::users::enable))
        .route("/settings/users/{id}/tokens/{token}/revoke", post(pages::users::revoke_token))
        .route("/settings/account", get(pages::users::account))
        .route("/settings/account/password", post(pages::users::change_password))
        .route("/settings/audit-log", get(pages::audit::instance))
        .route("/projects/{id}/audit-log", get(pages::audit::project))
        .route("/projects/{id}/members", get(pages::members::project_members).post(pages::members::add_to_project))
        .route("/environments/{id}/members", get(pages::members::environment_members).post(pages::members::add_to_environment))
        .route("/memberships/{id}", post(pages::members::update))
        .route("/memberships/{id}/delete", post(pages::members::remove))
        .route("/settings/tokens", get(pages::tokens::tokens).post(pages::tokens::create))
        .route("/settings/tokens/{id}/revoke", post(pages::tokens::revoke))
        .route("/settings/storage", get(pages::storage::storage).post(pages::storage::create))
        .route("/settings/storage/{id}", get(pages::storage::edit).post(pages::storage::update))
        .route("/settings/storage/{id}/test", post(pages::storage::test))
        .route("/settings/storage/{id}/delete", post(pages::storage::delete))
        .route("/settings/notifications", get(pages::notifications::notifications).post(pages::notifications::create))
        .route("/settings/notifications/{id}", get(pages::notifications::edit).post(pages::notifications::update))
        .route("/settings/notifications/{id}/test", post(pages::notifications::test))
        .route("/settings/notifications/{id}/delete", post(pages::notifications::delete))
        .route("/settings/backups", get(pages::instance_backups::page).post(pages::instance_backups::create))
        .route("/settings/backups/schedule", post(pages::instance_backups::set_schedule))
        .route("/settings/backups/activity", get(pages::instance_backups::activity))
        .route("/settings/backups/{id}/delete", post(pages::instance_backups::delete))
        .route("/services/{id}/metrics", get(pages::metrics::service))
        .route("/services/{id}/deployments", get(pages::services::deployments))
        .route("/services/{id}/deploy", post(pages::services::deploy))
        .route("/services/{id}/rollback", post(pages::services::rollback))
        .route("/services/{id}/restart", post(pages::services::restart))
        .route("/services/{id}/stop", post(pages::services::stop))
        .route("/services/{id}/start", post(pages::services::start))
        .route("/deployments/{id}", get(pages::deployments::deployment))
        .route("/deployments/{id}/log", get(pages::deployments::download_log))
        .route("/deployments/{id}/summary", get(pages::deployments::summary))
        .route("/deployments/{id}/stream", get(pages::deployments::stream))
        .route("/deployments/{id}/cancel", post(pages::deployments::cancel))
        .route("/assets/{name}", get(assets::serve))
        .fallback(|| async { WebError::NoPage })
        .layer(middleware::from_fn(auth::protect))
        .layer(middleware::from_fn(keep_url_on_refusal))
        .layer(middleware::from_fn(security_headers))
        .layer(TraceLayer::new_for_http())
        .with_state(control)
}

/// What the browser may do with a page: only this site's scripts, styles and forms, never
/// in a frame, and nothing kept in caches unless the response says so (assets do).
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
     connect-src 'self'; font-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

/// A boosted form that is refused gets its page again with the problem, so the address bar
/// keeps the page's URL rather than the URL the form posted to, which may have no page.
async fn keep_url_on_refusal(request: Request, next: Next) -> Response {
    let refusable = request.method() != Method::GET && auth::is_htmx(request.headers());
    let mut response = next.run(request).await;
    if refusable && response.status() == StatusCode::UNPROCESSABLE_ENTITY {
        response.headers_mut().insert("hx-push-url", HeaderValue::from_static("false"));
    }
    response
}

async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    // A page may have its own (the terminal's).
    headers.entry(header::CONTENT_SECURITY_POLICY).or_insert(HeaderValue::from_static(CONTENT_SECURITY_POLICY));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("same-origin"));
    headers.entry(header::CACHE_CONTROL).or_insert(HeaderValue::from_static("no-store"));
    response
}
