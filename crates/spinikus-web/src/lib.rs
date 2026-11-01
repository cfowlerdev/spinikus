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
mod layout;
mod pages;

use axum::{
    Router,
    extract::Request,
    http::{HeaderValue, header},
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
        .route("/projects/{id}", get(pages::projects::project))
        .route("/environments/{id}", get(pages::projects::environment))
        .route("/services/{id}", get(pages::projects::service))
        .route("/assets/{name}", get(assets::serve))
        .fallback(|| async { WebError::NoPage })
        .layer(middleware::from_fn(auth::protect))
        .layer(middleware::from_fn(security_headers))
        .layer(TraceLayer::new_for_http())
        .with_state(control)
}

/// What the browser may do with a page: only this site's scripts, styles and forms, never
/// in a frame, and nothing kept in caches unless the response says so (assets do).
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
     connect-src 'self'; font-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

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
