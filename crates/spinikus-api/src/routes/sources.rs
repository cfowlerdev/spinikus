use axum::{extract::State, response::Response};
use serde::Deserialize;
use spinikus_control::Control;
use spinikus_core::{
    id::ServiceId,
    source::{GitSource, ImageSource, SourceSpec},
};

use crate::{
    error::ApiError,
    extract::{Authenticated, Body, IfMatch, Path},
    response::{created, tagged},
};

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceBody {
    Git { repo_url: String, branch: String, root_directory: Option<String> },
    Image { image: String },
}

impl From<SourceBody> for SourceSpec {
    fn from(body: SourceBody) -> Self {
        match body {
            SourceBody::Git { repo_url, branch, root_directory } => Self::Git(GitSource { repo_url, branch, root_directory }),
            SourceBody::Image { image } => Self::Image(ImageSource { image }),
        }
    }
}

pub async fn show(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
) -> Result<Response, ApiError> {
    let source = control.get_source(&principal, service_id).await?;
    Ok(tagged(source.revision, source))
}

pub async fn create(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
    Body(body): Body<SourceBody>,
) -> Result<Response, ApiError> {
    let source = control.create_source(&principal, service_id, body.into()).await?;
    Ok(created(format!("/api/v1/services/{service_id}/source"), source.revision, source))
}

/// Replaces the whole source, such as to switch from an image to a Git repository.
pub async fn replace(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(service_id): Path<ServiceId>,
    IfMatch(revision): IfMatch,
    Body(body): Body<SourceBody>,
) -> Result<Response, ApiError> {
    let source = control.replace_source(&principal, service_id, revision, body.into()).await?;
    Ok(tagged(source.revision, source))
}
