//! Static assets, embedded in the binary: the stylesheet built by `cargo xtask css` and the
//! vendored HTMX (2.0.11, `htmx.org` on npm) and its SSE extension (2.2.4, `htmx-ext-sse`),
//! each checked against the registry's integrity hash.
//! Pages link them with their content's hash as `?v=`, so a response for the current version
//! can be cached for good.

use std::{collections::HashMap, sync::LazyLock};

use axum::{
    extract::{Path, Query},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::WebError;

struct Asset {
    content_type: &'static str,
    bytes: &'static [u8],
    /// The start of the content's SHA-256, in hex.
    version: String,
}

static ASSETS: LazyLock<HashMap<&'static str, Asset>> = LazyLock::new(|| {
    let assets: [(&str, &str, &[u8]); 3] = [
        ("app.css", "text/css; charset=utf-8", include_bytes!("../assets/app.css")),
        ("htmx.min.js", "text/javascript; charset=utf-8", include_bytes!("../assets/htmx.min.js")),
        ("htmx-ext-sse.min.js", "text/javascript; charset=utf-8", include_bytes!("../assets/htmx-ext-sse.min.js")),
    ];
    assets
        .into_iter()
        .map(|(name, content_type, bytes)| {
            let digest = Sha256::digest(bytes);
            let version = digest[..8].iter().map(|byte| format!("{byte:02x}")).collect();
            (name, Asset { content_type, bytes, version })
        })
        .collect()
});

/// The URL of an embedded asset, with its version.
pub fn url(name: &str) -> String {
    let asset = ASSETS.get(name).unwrap_or_else(|| panic!("no asset `{name}`"));
    format!("/assets/{name}?v={}", asset.version)
}

#[derive(Deserialize)]
pub struct Version {
    v: Option<String>,
}

pub async fn serve(Path(name): Path<String>, Query(version): Query<Version>, headers: HeaderMap) -> Response {
    let Some(asset) = ASSETS.get(name.as_str()) else {
        return WebError::NoPage.into_response();
    };
    let etag = format!("\"{}\"", asset.version);
    // An old version's URL gets the current content, but must not be cached as it.
    let cache = match version.v == Some(asset.version.clone()) {
        true => "public, max-age=31536000, immutable",
        false => "no-cache",
    };
    let headers_out = [
        (header::CONTENT_TYPE, HeaderValue::from_static(asset.content_type)),
        (header::CACHE_CONTROL, HeaderValue::from_static(cache)),
        (header::ETAG, HeaderValue::from_str(&etag).expect("hex is a valid header value")),
    ];
    let unchanged = headers.get(header::IF_NONE_MATCH).is_some_and(|value| value.as_bytes() == etag.as_bytes());
    match unchanged {
        true => (StatusCode::NOT_MODIFIED, headers_out).into_response(),
        false => (headers_out, asset.bytes).into_response(),
    }
}
