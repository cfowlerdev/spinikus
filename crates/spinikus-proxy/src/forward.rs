//! Turning a client's request into one for the app, and the app's response into one for
//! the client: hop-by-hop headers stay on their hop, and the app learns who asked through
//! the `X-Forwarded-*` headers.

use std::net::SocketAddr;

use hyper::{
    HeaderMap, Request, Version,
    header::{self, HeaderName, HeaderValue},
};

static X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
static X_FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");
static X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");
static KEEP_ALIVE: HeaderName = HeaderName::from_static("keep-alive");
static PROXY_CONNECTION: HeaderName = HeaderName::from_static("proxy-connection");

/// Whether the client asks to switch protocols, as WebSockets do.
pub(crate) fn is_upgrade(headers: &HeaderMap) -> bool {
    headers.contains_key(header::UPGRADE)
        && headers
            .get_all(header::CONNECTION)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
}

/// Removes the headers that apply to one connection only, including those `Connection`
/// names. An upgrade keeps `Upgrade` and asks the next hop for it again.
pub(crate) fn strip_hop_by_hop(headers: &mut HeaderMap, keep_upgrade: bool) {
    let named: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|token| HeaderName::from_bytes(token.trim().as_bytes()).ok())
        .collect();
    let upgrade = headers.get(header::UPGRADE).cloned();
    for name in named {
        headers.remove(name);
    }
    for name in [
        &header::CONNECTION,
        &KEEP_ALIVE,
        &PROXY_CONNECTION,
        &header::PROXY_AUTHENTICATE,
        &header::PROXY_AUTHORIZATION,
        &header::TE,
        &header::TRAILER,
        &header::TRANSFER_ENCODING,
        &header::UPGRADE,
    ] {
        headers.remove(name);
    }
    if keep_upgrade && let Some(upgrade) = upgrade {
        headers.insert(header::UPGRADE, upgrade);
        headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    }
}

/// Points the request at the app and adds the forwarding headers. `host` is what the
/// client asked for, kept as `Host` so the app sees its own name.
pub(crate) fn to_upstream<B>(
    request: &mut Request<B>,
    host: &HeaderValue,
    upstream: SocketAddr,
    peer: SocketAddr,
    upgrade: bool,
) {
    let path = request.uri().path_and_query().map_or("/", |path| path.as_str());
    *request.uri_mut() = format!("http://{upstream}{path}").parse().expect("an address and a path make a URI");
    // The app gets HTTP/1.1 whatever the client spoke.
    *request.version_mut() = Version::HTTP_11;

    let headers = request.headers_mut();
    strip_hop_by_hop(headers, upgrade);
    headers.insert(header::HOST, host.clone());
    let client = peer.ip().to_string();
    let forwarded_for = match headers.get(&X_FORWARDED_FOR).and_then(|value| value.to_str().ok()) {
        Some(earlier) => format!("{earlier}, {client}"),
        None => client,
    };
    if let Ok(value) = HeaderValue::from_str(&forwarded_for) {
        headers.insert(X_FORWARDED_FOR.clone(), value);
    }
    headers.insert(X_FORWARDED_HOST.clone(), host.clone());
    headers.insert(X_FORWARDED_PROTO.clone(), HeaderValue::from_static("http"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_the_request_for_the_app() {
        let mut request = Request::get("http://app.example.com/a/b?c=d")
            .version(Version::HTTP_2)
            .header(header::CONNECTION, "keep-alive, x-secret")
            .header("x-secret", "drop me")
            .header(header::TE, "trailers")
            .header("x-forwarded-for", "10.0.0.1")
            .header("x-forwarded-proto", "https")
            .header("x-other", "kept")
            .body(())
            .unwrap();
        let host = HeaderValue::from_static("app.example.com");

        to_upstream(&mut request, &host, "127.0.0.1:49000".parse().unwrap(), "192.0.2.7:5000".parse().unwrap(), false);

        assert_eq!(request.uri(), "http://127.0.0.1:49000/a/b?c=d");
        assert_eq!(request.version(), Version::HTTP_11);
        let headers = request.headers();
        assert_eq!(headers[header::HOST], "app.example.com");
        assert_eq!(headers["x-forwarded-for"], "10.0.0.1, 192.0.2.7");
        assert_eq!(headers["x-forwarded-host"], "app.example.com");
        assert_eq!(headers["x-forwarded-proto"], "http");
        assert_eq!(headers["x-other"], "kept");
        for gone in ["connection", "x-secret", "te"] {
            assert!(!headers.contains_key(gone), "{gone}");
        }
    }

    #[test]
    fn upgrades_keep_their_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive, Upgrade"));
        headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));

        assert!(is_upgrade(&headers));
        strip_hop_by_hop(&mut headers, true);

        assert_eq!(headers[header::UPGRADE], "websocket");
        assert_eq!(headers[header::CONNECTION], "upgrade");
        strip_hop_by_hop(&mut headers, false);
        assert!(headers.is_empty());
        assert!(!is_upgrade(&headers));
    }
}
