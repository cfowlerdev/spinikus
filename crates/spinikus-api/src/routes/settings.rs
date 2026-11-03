use axum::{extract::State, response::Response};
use serde::Deserialize;
use spinikus_control::Control;

use crate::{
    error::ApiError,
    extract::{Authenticated, Body, IfMatch, Path},
    response::tagged,
};

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = SettingPut)]
pub struct Put {
    value: serde_json::Value,
}

/// A setting, with its default value and revision 0 if it was never set.
#[utoipa::path(
    get,
    path = "/settings/{key}",
    operation_id = "settings_show",
    tag = "settings",
    params(("key" = String, Path)),
    responses((status = 200, body = spinikus_core::setting::Setting, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
pub async fn show(
    State(control): State<Control>,
    Authenticated(principal): Authenticated,
    Path(key): Path<String>,
) -> Result<Response, ApiError> {
    let setting = control.get_setting(&principal, &key).await?;
    Ok(tagged(setting.revision, setting))
}

/// Sets the value. `If-Match: "0"` sets a setting that was never set.
#[utoipa::path(
    put,
    path = "/settings/{key}",
    operation_id = "settings_put",
    tag = "settings",
    params(("key" = String, Path), crate::extract::IfMatch),
    request_body = Put,
    responses((status = 200, body = spinikus_core::setting::Setting, headers(("ETag" = String, description = "The revision, for `If-Match`.")))),
)]
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
