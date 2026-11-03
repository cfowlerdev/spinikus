//! The OpenAPI document, built from the handlers' `#[utoipa::path]` annotations and the
//! types' schemas. What every operation shares (the bearer token, problem details for
//! errors) is added here rather than repeated in each annotation.

use std::sync::LazyLock;

use axum::{http::header, response::IntoResponse};

use utoipa::{
    PartialSchema,
    openapi::{
        ContentBuilder, InfoBuilder, OpenApi, OpenApiBuilder, Ref, RefOr, ResponseBuilder, ServerBuilder, Tag,
        path::Operation,
        security::{HttpAuthScheme, HttpBuilder, SecurityRequirement, SecurityScheme},
    },
};

use crate::{error::Problem, routes};

/// Operations that take no bearer token: this document, setting up, signing in, and inbound webhooks
/// (which their signature authenticates).
const PUBLIC: &[&str] = &["openapi", "session_setup_status", "session_setup", "session_login", "hooks_receive"];

const DESCRIPTION: &str = "The API of a Spinikus instance, as the dashboard and the `spinikus` CLI use it.

Callers authenticate with an API token, `Authorization: Bearer spk_…` (from `POST /api/v1/login` or \
the dashboard's API tokens page). Errors are RFC 9457 problem details whose `code` is stable. \
Single resources carry their revision as an `ETag`, and updates need it back in `If-Match`. \
Lists are cursor pages: pass a page's `next_cursor` as `after` for the next one.";

/// The tags, in the order the document lists them, with what each covers.
const TAGS: &[(&str, &str)] = &[
    ("openapi", "This document."),
    ("session", "Setting up, signing in and the current user."),
    ("users", "Accounts, for instance admins; everyone changes their own password."),
    ("tokens", "The signed-in user's API tokens."),
    ("members", "The members of projects and environments."),
    ("projects", "Projects."),
    ("environments", "A project's environments, previews of pull requests among them."),
    ("services", "Apps and databases in an environment."),
    ("databases", "One-click databases."),
    ("sources", "Where a service's code or image comes from, its deploy key and its webhook."),
    ("deployments", "Deploying, following, rolling back and restarting."),
    ("logs", "A service's runtime logs."),
    ("variables", "Variables of projects, environments and services."),
    ("volumes", "A service's persistent volumes."),
    ("domains", "A service's custom domains."),
    ("backups", "Storage destinations, backup schedules, database backups and restores."),
    ("instance-backups", "Backups of Spinikus itself."),
    ("settings", "Instance settings."),
    ("servers", "Servers and their agents' enrollment."),
    ("audit-log", "Who changed what."),
    ("hooks", "Inbound Git webhooks, authenticated by their signature."),
];

/// The document, built once.
pub fn document() -> &'static OpenApi {
    static DOCUMENT: LazyLock<OpenApi> = LazyLock::new(build);
    &DOCUMENT
}

fn build() -> OpenApi {
    let (_, v1) = routes::v1().split_for_parts();
    let (_, hooks) = routes::hooks().split_for_parts();
    let info = InfoBuilder::new().title("Spinikus API").version(env!("CARGO_PKG_VERSION")).description(Some(DESCRIPTION)).build();
    let tags = TAGS.iter().map(|(name, description)| {
        let mut tag = Tag::new(*name);
        tag.description = Some(description.to_string());
        tag
    });
    let mut document = OpenApiBuilder::new()
        .info(info)
        .servers(Some([ServerBuilder::new().url("/").description(Some("This instance")).build()]))
        .tags(Some(tags))
        .security(Some([SecurityRequirement::new("bearer", Vec::<String>::new())]))
        .build()
        .nest("/api/v1", v1)
        .nest("/hooks", hooks);

    let components = document.components.get_or_insert_with(Default::default);
    components.schemas.insert("Problem".to_string(), Problem::schema());
    components.add_security_scheme(
        "bearer",
        SecurityScheme::Http(
            HttpBuilder::new().scheme(HttpAuthScheme::Bearer).description(Some("An API token, `spk_…`.")).build(),
        ),
    );

    let problem = ResponseBuilder::new()
        .description("A problem detail; `code` says which.")
        .content("application/problem+json", ContentBuilder::new().schema(Some(Ref::from_schema_name("Problem"))).build())
        .build();
    for item in document.paths.paths.values_mut() {
        for operation in operations(item) {
            reflow(operation);
            describe_responses(operation);
            operation.responses.responses.entry("default".to_string()).or_insert_with(|| problem.clone().into());
            if operation.operation_id.as_deref().is_some_and(|id| PUBLIC.contains(&id)) {
                // An empty requirement: no authentication.
                operation.security = Some(vec![SecurityRequirement::default()]);
            }
        }
    }
    document
}

/// OpenAPI requires a description on every response; the annotations give one only when
/// the status alone doesn't say enough.
fn describe_responses(operation: &mut Operation) {
    for (status, response) in &mut operation.responses.responses {
        if let RefOr::T(response) = response
            && response.description.is_empty()
        {
            response.description = match status.as_str() {
                "201" => "Created.",
                "204" => "Done.",
                _ => "OK.",
            }
            .to_string();
        }
    }
}

/// utoipa takes a doc comment's first line as the summary and the rest as the description,
/// which splits wrapped sentences: the summary becomes the first sentence, and the
/// description the whole comment when there is more.
fn reflow(operation: &mut Operation) {
    let text = [operation.summary.take(), operation.description.take()]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return;
    }
    let summary = match text.find(". ") {
        Some(end) => &text[..=end],
        None => &text,
    };
    operation.summary = Some(summary.to_string());
    if summary.len() < text.len() {
        operation.description = Some(text);
    }
}

fn operations(item: &mut utoipa::openapi::PathItem) -> impl Iterator<Item = &mut Operation> {
    [&mut item.get, &mut item.put, &mut item.post, &mut item.delete, &mut item.patch].into_iter().flatten()
}

/// The document as JSON, as `docs/openapi.json` holds it.
pub fn json() -> String {
    let mut json = document().to_pretty_json().expect("the OpenAPI document serializes");
    json.push('\n');
    json
}

/// The OpenAPI document of this API.
#[utoipa::path(
    get,
    path = "/openapi.json",
    operation_id = "openapi",
    tag = "openapi",
    responses((status = 200, body = Object, description = "An OpenAPI 3.1 document.")),
)]
pub async fn serve() -> impl IntoResponse {
    static JSON: LazyLock<String> = LazyLock::new(json);
    ([(header::CONTENT_TYPE, "application/json")], JSON.as_str())
}
