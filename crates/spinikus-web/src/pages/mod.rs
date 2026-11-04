//! Page handlers, by area.

pub mod attention;
pub mod audit;
pub mod backups;
pub mod databases;
pub mod deployments;
pub mod domains;
pub mod instance_backups;
pub mod logs;
pub mod manage;
pub mod members;
pub mod metrics;
pub mod notifications;
pub mod overview;
pub mod palette;
pub mod projects;
pub mod servers;
pub mod service_settings;
pub mod services;
pub mod session;
pub mod settings;
pub mod storage;
pub mod terminal;
pub mod tokens;
pub mod users;
pub mod variables;

use axum::{
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Redirect, Response},
};
use maud::Markup;

use spinikus_control::ControlError;
use spinikus_core::{
    database::Engine,
    environment::Environment,
    membership::Role,
    project::Project,
    service::{Service, ServiceKind},
};

use crate::{auth::is_htmx, error::WebError, layout};

/// The pages of a project, an environment or a service, for their tabs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    /// The project's environments, the environment's services, the service's deployments.
    Main,
    Logs,
    Metrics,
    Variables,
    Domains,
    Backups,
    Members,
    AuditLog,
    Terminal,
    Settings,
}

/// A project's tabs. Its variables need a role in the project itself, its settings the admin role.
fn project_tabs(project: &Project, role: Option<Role>, current: Tab) -> Markup {
    let base = format!("/projects/{}", project.id);
    let mut tabs = vec![("Environments", base.clone(), current == Tab::Main)];
    if role.is_some() {
        tabs.push(("Variables", format!("{base}/variables"), current == Tab::Variables));
    }
    if role == Some(Role::Admin) {
        tabs.push(("Members", format!("{base}/members"), current == Tab::Members));
        tabs.push(("Audit log", format!("{base}/audit-log"), current == Tab::AuditLog));
        tabs.push(("Settings", format!("{base}/settings"), current == Tab::Settings));
    }
    layout::tabs(&tabs)
}

fn environment_tabs(environment: &Environment, role: Role, current: Tab) -> Markup {
    let base = format!("/environments/{}", environment.id);
    let mut tabs = vec![
        ("Services", base.clone(), current == Tab::Main),
        ("Variables", format!("{base}/variables"), current == Tab::Variables),
    ];
    if role >= Role::Admin {
        tabs.push(("Members", format!("{base}/members"), current == Tab::Members));
        tabs.push(("Settings", format!("{base}/settings"), current == Tab::Settings));
    }
    layout::tabs(&tabs)
}

/// A service's tabs; only apps have domains, and only databases backups.
fn service_tabs(service: &Service, role: Role, current: Tab) -> Markup {
    let base = format!("/services/{}", service.id);
    let mut tabs = vec![
        ("Deployments", base.clone(), current == Tab::Main),
        ("Logs", format!("{base}/logs"), current == Tab::Logs),
        ("Metrics", format!("{base}/metrics"), current == Tab::Metrics),
        ("Variables", format!("{base}/variables"), current == Tab::Variables),
    ];
    if service.kind == ServiceKind::App {
        tabs.push(("Domains", format!("{base}/domains"), current == Tab::Domains));
    }
    if Engine::of(&service.config).is_some() {
        tabs.push(("Backups", format!("{base}/backups"), current == Tab::Backups));
    }
    if role == Role::Admin {
        tabs.push(("Terminal", format!("{base}/terminal"), current == Tab::Terminal));
    }
    if role >= Role::Developer {
        tabs.push(("Settings", format!("{base}/settings"), current == Tab::Settings));
    }
    layout::tabs(&tabs)
}

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
        | ControlError::InvalidTransition(_)
        | ControlError::TooManyAttempts { .. } => error.to_string(),
        _ => return None,
    };
    let mut chars = message.chars();
    let first = chars.next().map(|first| first.to_uppercase().collect::<String>()).unwrap_or_default();
    Some(format!("{first}{}.", chars.as_str().trim_end_matches('.')))
}

/// A form's page again, with the problem and status 422, for problems the user can fix;
/// other errors become error pages.
fn problem(error: ControlError, render: impl FnOnce(&str) -> Markup) -> Result<Response, WebError> {
    let message = problem_message(error)?;
    Ok((StatusCode::UNPROCESSABLE_ENTITY, render(&message)).into_response())
}

/// What [`problem`] says about `error`, for pages that render it themselves; other errors
/// are error pages.
fn problem_message(error: ControlError) -> Result<String, WebError> {
    let message = match &error {
        ControlError::RevisionMismatch { .. } => Some(
            "Someone changed this while you were editing. Reload the page to see their changes, then make yours again."
                .to_string(),
        ),
        error => refused(error),
    };
    message.ok_or_else(|| error.into())
}

/// The scheme and host the browser reached this instance at (`https` when a proxy in front
/// says so), for absolute URLs given to other systems, such as webhook URLs.
fn origin(headers: &HeaderMap) -> String {
    let https = headers.get("x-forwarded-proto").is_some_and(|proto| proto.as_bytes().eq_ignore_ascii_case(b"https"));
    let host = headers.get(axum::http::header::HOST).and_then(|value| value.to_str().ok()).unwrap_or("localhost");
    format!("{}://{host}", if https { "https" } else { "http" })
}
