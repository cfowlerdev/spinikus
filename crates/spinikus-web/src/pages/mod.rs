//! Page handlers, by area.

pub mod overview;
pub mod projects;
pub mod session;

use axum::{
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Redirect, Response},
};

use crate::{auth::is_htmx, error::WebError};

/// A redirect after a form or to another page. HTMX would follow a plain redirect without
/// changing the address bar, so it gets `HX-Location`: it loads `to` and pushes its URL.
fn redirect(headers: &HeaderMap, to: &str) -> Response {
    match is_htmx(headers) {
        true => {
            let location = HeaderValue::from_str(to).expect("a local path is a valid header value");
            (StatusCode::OK, [(HeaderName::from_static("hx-location"), location)]).into_response()
        }
        false => Redirect::to(to).into_response(),
    }
}

/// An ID from the URL; one that doesn't parse is a page that doesn't exist.
fn parse_id<T: std::str::FromStr>(id: &str) -> Result<T, WebError> {
    id.parse().map_err(|_| WebError::NoPage)
}
