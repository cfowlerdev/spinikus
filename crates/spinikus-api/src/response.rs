//! Response helpers: revision ETags, `201 Created` with a location, and cursor pages.

use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use spinikus_control::Page;

/// The page size when a list request gives no `limit`.
pub const DEFAULT_PAGE_SIZE: u32 = 50;

/// `?after=<id>&limit=<n>` on list requests.
#[derive(Deserialize)]
pub struct PageParams<I> {
    pub after: Option<I>,
    pub limit: Option<u32>,
}

impl<I> PageParams<I> {
    pub fn limit(&self) -> u32 {
        self.limit.unwrap_or(DEFAULT_PAGE_SIZE)
    }
}

/// A page of a list: pass `next_cursor` as `after` to get the next one. It is `null` on the last page.
#[derive(Serialize)]
pub struct PageBody<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

impl<T> PageBody<T> {
    pub fn new(page: Page<T>, id: impl Fn(&T) -> String) -> Json<Self> {
        let next_cursor = if page.has_more { page.items.last().map(id) } else { None };
        Json(Self { items: page.items, next_cursor })
    }
}

fn etag(revision: i64) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{revision}\"")).expect("a number is a valid header value")
}

/// `200 OK` with the resource's revision as its `ETag`, for `If-Match` on the next update.
pub fn tagged<T: Serialize>(revision: i64, body: T) -> Response {
    ([(header::ETAG, etag(revision))], Json(body)).into_response()
}

/// `201 Created` with the new resource's location and `ETag`.
pub fn created<T: Serialize>(location: String, revision: i64, body: T) -> Response {
    let location = HeaderValue::from_str(&location).expect("resource paths are valid header values");
    (StatusCode::CREATED, [(header::LOCATION, location), (header::ETAG, etag(revision))], Json(body)).into_response()
}
