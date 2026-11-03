//! Web sessions: the session cookie, turning it into a principal, and CSRF protection.
//!
//! The cookie holds the session's secret (`HttpOnly`, `SameSite=Lax`, `Secure` behind HTTPS).
//! A session's CSRF token is a hash of that secret, so it needs no storage, changes with
//! each session, and can't be made without the cookie, which other sites can't read.
//! Requests that change something must come from this site (`Sec-Fetch-Site` and `Origin`,
//! when the browser sends them) and, with a session cookie, carry the token.

use std::net::SocketAddr;

use axum::{
    body::{Body, to_bytes},
    extract::{ConnectInfo, FromRequestParts, Request},
    http::{HeaderMap, HeaderName, HeaderValue, Method, header, request::Parts},
    middleware::Next,
    response::{IntoResponse, Response},
};
use sha2::{Digest, Sha256};
use spinikus_control::{ClientInfo, Control, ControlError, Principal, auth::SESSION_LIFETIME};

use crate::error::WebError;

/// Set by the proxy in front of Spinikus; believed only from this machine ([`ClientInfo::from_request`]).
const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");

pub const SESSION_COOKIE: &str = "spinikus_session";
/// The form field that carries the CSRF token.
pub const CSRF_FIELD: &str = "_csrf";
/// The header that carries the CSRF token, set on every HTMX request.
pub const CSRF_HEADER: &str = "x-csrf-token";
/// Forms posted without a session, protected by the origin checks only.
const SIGNED_OUT_FORMS: [&str; 2] = ["/login", "/setup"];
/// The largest form body read for its CSRF token.
const MAX_FORM_SIZE: usize = 1024 * 1024;

/// A signed-in request: the principal, and the session's CSRF token for the page's forms.
pub struct Signed {
    pub principal: Principal,
    pub csrf: String,
}

impl FromRequestParts<Control> for Signed {
    type Rejection = WebError;

    async fn from_request_parts(parts: &mut Parts, control: &Control) -> Result<Self, Self::Rejection> {
        let sign_in = || {
            // Only a page the browser can open again is worth coming back to.
            let next = (parts.method == Method::GET).then(|| parts.uri.path_and_query().map(|path| path.to_string())).flatten();
            WebError::SignIn { next, htmx: is_htmx(&parts.headers) }
        };
        let Some(cookie) = cookie(&parts.headers, SESSION_COOKIE) else {
            return Err(sign_in());
        };
        match control.authenticate_session(cookie).await {
            Ok(principal) => Ok(Self { principal, csrf: csrf_token(cookie) }),
            Err(ControlError::Unauthenticated) => Err(sign_in()),
            Err(error) => Err(error.into()),
        }
    }
}

/// Whether HTMX made the request (it then follows `HX-Redirect` rather than a redirect).
pub fn is_htmx(headers: &HeaderMap) -> bool {
    headers.get("hx-request").is_some_and(|value| value == "true")
}

/// A cookie's value from the request's `Cookie` headers.
pub fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, value)| *key == name && !value.is_empty())
        .map(|(_, value)| value)
}

/// The CSRF token of the session with this cookie.
pub fn csrf_token(session_cookie: &str) -> String {
    let digest = Sha256::new().chain_update(b"spinikus-csrf\0").chain_update(session_cookie.as_bytes()).finalize();
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The `Set-Cookie` value that starts a session.
pub fn session_cookie(secret: &str, headers: &HeaderMap) -> HeaderValue {
    let max_age = SESSION_LIFETIME.as_secs();
    let value = format!("{SESSION_COOKIE}={secret}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{}", secure(headers));
    HeaderValue::from_str(&value).expect("a session secret is a valid header value")
}

/// The `Set-Cookie` value that removes the session cookie.
pub fn cleared_cookie(headers: &HeaderMap) -> HeaderValue {
    let value = format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{}", secure(headers));
    HeaderValue::from_str(&value).expect("a valid header value")
}

/// `; Secure` when the browser reached Spinikus over HTTPS, through a proxy in front of it.
fn secure(headers: &HeaderMap) -> &'static str {
    let https = headers.get("x-forwarded-proto").is_some_and(|proto| proto.as_bytes().eq_ignore_ascii_case(b"https"));
    if https { "; Secure" } else { "" }
}

/// About the browser, for the session list and the audit log.
pub struct Client(pub ClientInfo);

impl<S: Send + Sync> FromRequestParts<S> for Client {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let header = |name: HeaderName| parts.headers.get(name).and_then(|value: &HeaderValue| value.to_str().ok());
        let peer = parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|ConnectInfo(address)| address.ip());
        Ok(Self(ClientInfo::from_request(header(header::USER_AGENT), peer, header(X_FORWARDED_FOR))))
    }
}

/// Refuses requests that change something unless they come from this site and, with a
/// session, carry its CSRF token.
pub async fn protect(request: Request, next: Next) -> Response {
    if matches!(*request.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        return next.run(request).await;
    }
    if !same_origin(request.headers()) {
        return WebError::CrossSite.into_response();
    }
    let session = cookie(request.headers(), SESSION_COOKIE).map(csrf_token);
    let request = match session {
        Some(expected) if !SIGNED_OUT_FORMS.contains(&request.uri().path()) => match has_token(request, &expected).await {
            Some(request) => request,
            None => return WebError::CrossSite.into_response(),
        },
        _ => request,
    };
    next.run(request).await
}

/// Whether the browser says the request comes from this site; browsers that say nothing
/// pass, and are left to the CSRF token and `SameSite`.
fn same_origin(headers: &HeaderMap) -> bool {
    let site = headers.get("sec-fetch-site").map(HeaderValue::as_bytes);
    if site.is_some_and(|site| site != b"same-origin" && site != b"none") {
        return false;
    }
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let host = headers.get(header::HOST).map(HeaderValue::as_bytes);
    let origin_host = origin.to_str().ok().and_then(|origin| origin.split_once("://")).map(|(_, host)| host.as_bytes());
    origin_host.is_some() && origin_host == host
}

/// Whether a WebSocket's handshake comes from a page of this site. Browsers always send
/// `Origin` with one, and cookies go with it whatever site opens it, so it must be there and
/// match: this, not the CSRF token, stops other sites opening sockets as the user.
pub fn websocket_from_this_site(headers: &HeaderMap) -> bool {
    headers.contains_key(header::ORIGIN) && same_origin(headers)
}

/// The request back if it carries `expected` in the CSRF header or form field.
async fn has_token(request: Request, expected: &str) -> Option<Request> {
    if let Some(token) = request.headers().get(CSRF_HEADER) {
        return constant_time_eq(token.as_bytes(), expected.as_bytes()).then_some(request);
    }
    let form = request
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|value| value.as_bytes().starts_with(b"application/x-www-form-urlencoded"));
    if !form {
        return None;
    }
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, MAX_FORM_SIZE).await.ok()?;
    let token = bytes
        .split(|byte| *byte == b'&')
        .filter_map(|pair| pair.strip_prefix(CSRF_FIELD.as_bytes())?.strip_prefix(b"="))
        .next()?;
    constant_time_eq(token, expected.as_bytes()).then(|| Request::from_parts(parts, Body::from(bytes)))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |difference, (a, b)| difference | (a ^ b)) == 0
}

/// `next` if it is a path on this site, else the overview, so a link can't send someone
/// elsewhere after signing in.
pub fn local_path(next: Option<&str>) -> &str {
    match next {
        Some(next) if next.starts_with('/') && !next.starts_with("//") && !next.starts_with("/\\") => {
            if next.chars().any(char::is_control) { "/" } else { next }
        }
        _ => "/",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_local_paths() {
        assert_eq!(local_path(Some("/projects/prj_1?x=1")), "/projects/prj_1?x=1");
        for elsewhere in ["//evil.example", "/\\evil.example", "https://evil.example", "evil", "/a\nb"] {
            assert_eq!(local_path(Some(elsewhere)), "/", "{elsewhere}");
        }
        assert_eq!(local_path(None), "/");
    }

    #[test]
    fn reads_cookies_and_derives_tokens_from_them() {
        let mut headers = HeaderMap::new();
        headers.append(header::COOKIE, HeaderValue::from_static("theme=dark; spinikus_session=abc"));

        assert_eq!(cookie(&headers, SESSION_COOKIE), Some("abc"));
        assert_eq!(cookie(&headers, "missing"), None);
        assert_eq!(csrf_token("abc"), csrf_token("abc"));
        assert_ne!(csrf_token("abc"), csrf_token("abd"));
        assert_eq!(csrf_token("abc").len(), 64);
    }

    #[test]
    fn checks_where_requests_come_from() {
        let headers = |pairs: &[(&'static str, &'static str)]| {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, HeaderValue::from_static("spinikus.example:8080"));
            for (name, value) in pairs {
                headers.insert(*name, HeaderValue::from_static(value));
            }
            headers
        };

        assert!(same_origin(&headers(&[])));
        assert!(same_origin(&headers(&[("sec-fetch-site", "same-origin"), ("origin", "https://spinikus.example:8080")])));
        assert!(!same_origin(&headers(&[("sec-fetch-site", "cross-site")])));
        assert!(!same_origin(&headers(&[("sec-fetch-site", "same-site")])));
        assert!(!same_origin(&headers(&[("origin", "https://evil.example")])));
        assert!(!same_origin(&headers(&[("origin", "null")])));
    }

    #[test]
    fn websockets_need_an_origin_of_this_site() {
        let headers = |origin: Option<&'static str>| {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, HeaderValue::from_static("spinikus.example:8080"));
            if let Some(origin) = origin {
                headers.insert(header::ORIGIN, HeaderValue::from_static(origin));
            }
            headers
        };
        assert!(websocket_from_this_site(&headers(Some("https://spinikus.example:8080"))));
        assert!(!websocket_from_this_site(&headers(None)));
        assert!(!websocket_from_this_site(&headers(Some("https://evil.example"))));
    }
}
