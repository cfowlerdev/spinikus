//! Errors as RFC 9457 problem details, each with a stable `code` clients can match on.

use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use spinikus_control::ControlError;

#[derive(Debug)]
pub enum ApiError {
    Control(ControlError),
    /// A request the API could not read, such as malformed JSON or a bad query string.
    BadRequest {
        status: StatusCode,
        code: &'static str,
        detail: String,
    },
    /// An update without `If-Match`.
    PreconditionRequired,
    /// No route for the path, or the path has a malformed ID.
    NoRoute,
    MethodNotAllowed,
}

impl From<ControlError> for ApiError {
    fn from(error: ControlError) -> Self {
        Self::Control(error)
    }
}

impl ApiError {
    pub(crate) fn invalid(field: &'static str, problem: impl ToString) -> Self {
        Self::Control(ControlError::Invalid { field, problem: problem.to_string() })
    }
}

#[derive(Serialize)]
struct Problem<'a> {
    r#type: &'static str,
    title: &'static str,
    status: u16,
    detail: String,
    code: &'a str,
    /// The input field at fault, for `invalid`.
    #[serde(skip_serializing_if = "Option::is_none")]
    field: Option<&'static str>,
}

fn control_status(error: &ControlError) -> StatusCode {
    match error {
        ControlError::NotFound { .. } => StatusCode::NOT_FOUND,
        ControlError::Conflict { .. }
        | ControlError::HasChildren { .. }
        | ControlError::Rejected { .. }
        | ControlError::InvalidTransition(_)
        | ControlError::AlreadySetUp => StatusCode::CONFLICT,
        ControlError::RevisionMismatch { .. } => StatusCode::PRECONDITION_FAILED,
        ControlError::Invalid { .. } => StatusCode::UNPROCESSABLE_ENTITY,
        ControlError::InvalidCredentials | ControlError::Unauthenticated => StatusCode::UNAUTHORIZED,
        ControlError::Forbidden => StatusCode::FORBIDDEN,
        ControlError::TooManyAttempts { .. } => StatusCode::TOO_MANY_REQUESTS,
        ControlError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, detail, field) = match &self {
            Self::Control(error) => {
                let field = match error {
                    ControlError::Invalid { field, .. } => Some(*field),
                    _ => None,
                };
                (control_status(error), error.code(), error.to_string(), field)
            }
            Self::BadRequest { status, code, detail } => (*status, *code, detail.clone(), None),
            Self::PreconditionRequired => (
                StatusCode::PRECONDITION_REQUIRED,
                "precondition_required",
                "send the revision you are updating in an `If-Match` header".to_string(),
                None,
            ),
            Self::NoRoute => (StatusCode::NOT_FOUND, "not_found", "no such resource".to_string(), None),
            Self::MethodNotAllowed => (
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "the resource does not allow this method".to_string(),
                None,
            ),
        };
        let problem = Problem {
            r#type: "about:blank",
            title: status.canonical_reason().unwrap_or("Error"),
            status: status.as_u16(),
            detail,
            code,
            field,
        };
        let mut response = (status, Json(problem)).into_response();
        let headers = response.headers_mut();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/problem+json"));
        if status == StatusCode::UNAUTHORIZED {
            headers.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        if let Self::Control(ControlError::TooManyAttempts { retry_after }) = &self {
            headers.insert(
                header::RETRY_AFTER,
                HeaderValue::from(u64::try_from(retry_after.as_millis().div_ceil(1000)).unwrap_or(u64::MAX).max(1)),
            );
        }
        response
    }
}
