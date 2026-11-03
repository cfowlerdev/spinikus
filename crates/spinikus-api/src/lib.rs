//! The JSON API under `/api/v1`, and inbound webhooks under `/hooks`: a thin HTTP layer over the service layer in
//! `spinikus-control`, which does the validation, permission checks and auditing.
//!
//! Conventions: errors are RFC 9457 problem details with a stable `code`; callers
//! authenticate with `Authorization: Bearer spk_…`; single resources carry their revision
//! as an `ETag`, and updates need it back in `If-Match`; lists are cursor pages
//! (`?after=<id>&limit=<n>`, answered with `next_cursor`).

mod error;
mod extract;
mod openapi;
mod response;
mod routes;

use axum::Router;
use spinikus_control::Control;
use tower_http::trace::TraceLayer;

pub use error::ApiError;
pub use openapi::{document as openapi, json as openapi_json};

/// The API's routes, to merge into the server's router.
pub fn router(control: Control) -> Router {
    let (v1, _) = routes::v1().split_for_parts();
    let v1 = v1.fallback(|| async { ApiError::NoRoute }).method_not_allowed_fallback(|| async { ApiError::MethodNotAllowed });
    let (hooks, _) = routes::hooks().split_for_parts();
    let hooks =
        hooks.fallback(|| async { ApiError::NoRoute }).method_not_allowed_fallback(|| async { ApiError::MethodNotAllowed });
    Router::new().nest("/api/v1", v1).nest("/hooks", hooks).layer(TraceLayer::new_for_http()).with_state(control)
}
