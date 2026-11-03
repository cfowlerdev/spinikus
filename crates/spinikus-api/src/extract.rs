//! Extractors that reject with problem details instead of axum's plain-text errors.

use std::net::SocketAddr;

use axum::{
    extract::{ConnectInfo, FromRequest, FromRequestParts, Request, rejection::JsonRejection},
    http::{HeaderName, HeaderValue, StatusCode, header, request::Parts},
};
use serde::de::DeserializeOwned;
use spinikus_control::{ClientInfo, Control, ControlError, Principal};

use crate::error::ApiError;

/// Set by the proxy in front of Spinikus; believed only from this machine ([`ClientInfo::from_request`]).
const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");

/// A JSON request body.
pub struct Body<T>(pub T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for Body<T> {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(request, state).await {
            Ok(axum::Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(body_rejection(rejection)),
        }
    }
}

/// A JSON request body that may be left out, for actions whose input is all optional.
pub struct OptionalBody<T>(pub Option<T>);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for OptionalBody<T> {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let empty = request.headers().get(header::CONTENT_LENGTH).is_some_and(|length| length == "0")
            || (!request.headers().contains_key(header::CONTENT_LENGTH)
                && !request.headers().contains_key(header::TRANSFER_ENCODING)
                && !request.headers().contains_key(header::CONTENT_TYPE));
        if empty {
            return Ok(Self(None));
        }
        Body::from_request(request, state).await.map(|Body(value)| Self(Some(value)))
    }
}

fn body_rejection(rejection: JsonRejection) -> ApiError {
    let status = match rejection {
        JsonRejection::MissingJsonContentType(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        JsonRejection::JsonDataError(_) => StatusCode::UNPROCESSABLE_ENTITY,
        _ => StatusCode::BAD_REQUEST,
    };
    ApiError::BadRequest { status, code: "invalid_body", detail: rejection.body_text() }
}

/// A path parameter, such as a resource ID. One that does not parse is a 404: no resource has that ID.
pub struct Path<T>(pub T);

impl<T: DeserializeOwned + Send, S: Send + Sync> FromRequestParts<S> for Path<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match axum::extract::Path::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Path(value)) => Ok(Self(value)),
            Err(_) => Err(ApiError::NoRoute),
        }
    }
}

/// A request to upgrade to a WebSocket. Anything else is refused with `400 websocket_required`
/// (or `426`, as axum says) before the handler does anything.
pub struct WebSocket(pub axum::extract::ws::WebSocketUpgrade);

impl<S: Send + Sync> FromRequestParts<S> for WebSocket {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match axum::extract::ws::WebSocketUpgrade::from_request_parts(parts, state).await {
            Ok(upgrade) => Ok(Self(upgrade)),
            Err(rejection) => Err(ApiError::BadRequest {
                status: rejection.status(),
                code: "websocket_required",
                detail: rejection.body_text(),
            }),
        }
    }
}

/// Query parameters.
pub struct Query<T>(pub T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequestParts<S> for Query<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(value)) => Ok(Self(value)),
            Err(rejection) => Err(ApiError::BadRequest {
                status: StatusCode::BAD_REQUEST,
                code: "invalid_query",
                detail: rejection.body_text(),
            }),
        }
    }
}

/// The caller, from an `Authorization: Bearer spk_…` API token.
pub struct Authenticated(pub Principal);

impl FromRequestParts<Control> for Authenticated {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, control: &Control) -> Result<Self, Self::Rejection> {
        let bearer = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, token)| token.trim())
            .ok_or(ControlError::Unauthenticated)?;
        Ok(Self(control.authenticate_api_token(bearer).await?))
    }
}

/// The revision an update expects, from `If-Match: "3"`. Required: updates without it are
/// refused, so clients cannot overwrite changes they have not seen.
pub struct IfMatch(pub i64);

impl<S: Send + Sync> FromRequestParts<S> for IfMatch {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let value = parts.headers.get(header::IF_MATCH).ok_or(ApiError::PreconditionRequired)?;
        value
            .to_str()
            .ok()
            .and_then(|value| value.trim().strip_prefix('"')?.strip_suffix('"')?.parse().ok())
            .map(Self)
            .ok_or_else(|| ApiError::BadRequest {
                status: StatusCode::BAD_REQUEST,
                code: "invalid_header",
                detail: "`If-Match` must be a revision ETag, such as \"3\"".to_string(),
            })
    }
}

impl utoipa::IntoParams for IfMatch {
    fn into_params(_: impl Fn() -> Option<utoipa::openapi::path::ParameterIn>) -> Vec<utoipa::openapi::path::Parameter> {
        use utoipa::openapi::{
            Required,
            path::{ParameterBuilder, ParameterIn},
        };
        let parameter = ParameterBuilder::new()
            .name("If-Match")
            .parameter_in(ParameterIn::Header)
            .required(Required::True)
            .description(Some("The revision this update expects, as the resource's `ETag` gave it, such as `\"3\"`."))
            .schema(Some(<String as utoipa::PartialSchema>::schema()))
            .build();
        vec![parameter]
    }
}

/// The client's user agent and address, for sessions and the audit log.
pub struct Client(pub ClientInfo);

impl<S: Send + Sync> FromRequestParts<S> for Client {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let header = |name: HeaderName| parts.headers.get(name).and_then(|value: &HeaderValue| value.to_str().ok());
        let peer = parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|ConnectInfo(address)| address.ip());
        Ok(Self(ClientInfo::from_request(header(header::USER_AGENT), peer, header(X_FORWARDED_FOR))))
    }
}
