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
        .route("/projects/{id}/environments", post(pages::manage::create_environment))
        .route("/projects/{id}/environments/new", get(pages::manage::new_environment))
        .route("/environments/{id}", get(pages::projects::environment))
        .route("/environments/{id}/settings", get(pages::manage::environment_settings).post(pages::manage::update_environment))
        .route("/environments/{id}/delete", post(pages::manage::delete_environment))
        .route("/environments/{id}/services", post(pages::service_settings::create_service))
        .route("/environments/{id}/services/new", get(pages::service_settings::new_service))
        .route("/services/{id}", get(pages::services::service))
        .route("/services/{id}/settings", get(pages::service_settings::settings))
        .route("/services/{id}/settings/general", post(pages::service_settings::update_general))
        .route("/services/{id}/settings/source", post(pages::service_settings::update_source))
        .route("/services/{id}/settings/runtime", post(pages::service_settings::update_runtime))
        .route("/services/{id}/delete", post(pages::service_settings::delete))
        .route("/services/{id}/deployments", get(pages::services::deployments))
        .route("/services/{id}/deploy", post(pages::services::deploy))
        .route("/services/{id}/rollback", post(pages::services::rollback))
        .route("/deployments/{id}", get(pages::deployments::deployment))
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
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CONTENT_SECURITY_POLICY));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("same-origin"));
    headers.entry(header::CACHE_CONTROL).or_insert(HeaderValue::from_static("no-store"));
    response
}
