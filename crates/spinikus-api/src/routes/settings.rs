use axum::{extract::State, response::Response};
use serde::Deserialize;
use spinikus_control::Control;

use crate::{
    error::ApiError,
    extract::{Authenticated, Body, IfMatch, Path},
    response::tagged,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Put {
    value: serde_json::Value,
}

/// A setting, with its default value and revision 0 if it was never set.
pub async fn show(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(key): Path<String>,
) -> Result<Response, ApiError> {
    let setting = control.get_setting(&principal, &key).await?;
    Ok(tagged(setting.revision, setting))
}

/// Sets the value. `If-Match: "0"` sets a setting that was never set.
pub async fn put(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(key): Path<String>,
    IfMatch(revision): IfMatch,
    Body(body): Body<Put>,
) -> Result<Response, ApiError> {
    let setting = control.set_setting(&principal, &key, revision, body.value).await?;
    Ok(tagged(setting.revision, setting))
}
