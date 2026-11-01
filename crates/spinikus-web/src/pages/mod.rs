//! Page handlers, by area.

pub mod deployments;
pub mod overview;
pub mod projects;
pub mod services;
pub mod session;

use axum::{
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Redirect, Response},
};

use spinikus_control::ControlError;
use spinikus_core::{environment::Environment, project::Project};

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

/// Projects, then the project, then the environment if given.
fn crumbs<'a>(project: &'a Project, environment: Option<&'a Environment>) -> Vec<(&'a str, String)> {
    let mut crumbs = vec![("Projects", "/".to_string()), (project.name.as_str(), format!("/projects/{}", project.id))];
    if let Some(environment) = environment {
        crumbs.push((environment.name.as_str(), format!("/environments/{}", environment.id)));
    }
    crumbs
}

/// Why an action was refused, to show on the page, for errors the user can do something
/// about; `None` for the rest, which become error pages.
fn refused(error: &ControlError) -> Option<String> {
    let message = match error {
        ControlError::Invalid { problem, .. } | ControlError::Rejected { problem, .. } => problem.clone(),
        ControlError::Conflict { .. }
        | ControlError::HasChildren { .. }
        | ControlError::RevisionMismatch { .. }
        | ControlError::InvalidTransition(_) => error.to_string(),
        _ => return None,
    };
    let mut chars = message.chars();
    let first = chars.next().map(|first| first.to_uppercase().collect::<String>()).unwrap_or_default();
    Some(format!("{first}{}.", chars.as_str().trim_end_matches('.')))
}
