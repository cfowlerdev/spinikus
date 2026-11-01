//! Errors as pages: a missing or hidden thing is a 404 page, a missing session a trip to the
//! sign-in page, and internal errors are logged and never shown.

use axum::{
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use maud::html;
use spinikus_control::ControlError;

use crate::layout;

#[derive(Debug)]
pub enum WebError {
    Control(ControlError),
    /// No session: sign in, then come back to `next`.
    SignIn {
        next: Option<String>,
        htmx: bool,
    },
    /// A request that changes something came from another site, or without its CSRF token.
    CrossSite,
    NoPage,
}

impl From<ControlError> for WebError {
    fn from(error: ControlError) -> Self {
        match error {
            ControlError::Unauthenticated => Self::SignIn { next: None, htmx: false },
            error => Self::Control(error),
        }
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let (status, title, message) = match self {
            Self::SignIn { next, htmx } => return sign_in(next.as_deref(), htmx),
            Self::NoPage | Self::Control(ControlError::NotFound { .. }) => {
                (StatusCode::NOT_FOUND, "Not found", "There is nothing here, or you don't have access to it.".to_string())
            }
            Self::CrossSite => (
                StatusCode::FORBIDDEN,
                "Request refused",
                "This request didn't come from a Spinikus page. Reload the page and try again.".to_string(),
            ),
            Self::Control(ControlError::Forbidden) => {
                (StatusCode::FORBIDDEN, "Not allowed", "You don't have permission to do this.".to_string())
            }
            Self::Control(ControlError::Internal(error)) => {
                tracing::error!(error = %error, "a page failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong", "Spinikus could not show this page.".to_string())
            }
            Self::Control(error) => (StatusCode::BAD_REQUEST, "Can't do that", error.to_string()),
        };
        let page = layout::bare(
            title,
            html! {
                div class="w-full max-w-md text-center" {
                    p class="text-sm font-semibold text-indigo-600 dark:text-indigo-400" { (status.as_u16()) }
                    h1 class="mt-2 text-2xl font-semibold tracking-tight" { (title) }
                    p class="mt-3 text-zinc-600 dark:text-zinc-400" { (message) }
                    div class="mt-6" { a href="/" class=(layout::BUTTON_SECONDARY) { "Back to the overview" } }
                }
            },
        );
        (status, page).into_response()
    }
}

/// To the sign-in page; HTMX gets `HX-Redirect`, so the whole page goes there.
fn sign_in(next: Option<&str>, htmx: bool) -> Response {
    let location = match next {
        Some(next) if next != "/" => format!("/login?next={}", encode(next)),
        _ => "/login".to_string(),
    };
    if htmx {
        let value = HeaderValue::from_str(&location).expect("an encoded path is a valid header value");
        return (StatusCode::UNAUTHORIZED, [(header::HeaderName::from_static("hx-redirect"), value)]).into_response();
    }
    Redirect::to(&location).into_response()
}

/// Percent-encodes a query parameter's value.
pub fn encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => encoded.push(byte as char),
            byte => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}
