//! A small hand-written client for the JSON API under `/api/v1`: requests with the bearer
//! token, problem details turned into errors with an exit status, every page of a list, and
//! server-sent events.

use std::time::Duration;

use anyhow::Context;
use reqwest::{Method, Response, StatusCode, Url, header};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;

use super::error::{Exit, fail};

/// Items per page when reading a whole list (the server's maximum).
const PAGE_SIZE: u32 = 100;

pub struct Api {
    http: reqwest::Client,
    /// The instance's address, ending with `/`.
    server: Url,
    token: Option<String>,
}

impl Api {
    pub fn new(server: &str, token: Option<String>) -> anyhow::Result<Self> {
        // reqwest is built without a crypto provider; ring is the one the workspace uses.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("spinikus-cli/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("Couldn't set up the HTTP client")?;
        Ok(Self { http, server: parse_server(server)?, token })
    }

    /// The instance's address, as saved in contexts: no trailing `/`.
    pub fn server(&self) -> String {
        server_name(&self.server)
    }

    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    fn url(&self, path: &str, query: &[(&str, String)]) -> Url {
        let mut url = self.server.clone();
        url.set_path(&format!("{}api/v1{path}", self.server.path()));
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        url
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
        if_match: Option<i64>,
    ) -> anyhow::Result<Response> {
        let mut request = self.http.request(method, self.url(path, query));
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.header(header::CONTENT_TYPE, "application/json").body(serde_json::to_vec(body)?);
        }
        if let Some(revision) = if_match {
            request = request.header(header::IF_MATCH, format!("\"{revision}\""));
        }
        let response = request.send().await.with_context(|| format!("Couldn't reach {}", self.server()))?;
        if response.status().is_success() { Ok(response) } else { Err(problem(response).await) }
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        self.get_with(path, &[]).await
    }

    pub async fn get_with<T: DeserializeOwned>(&self, path: &str, query: &[(&str, String)]) -> anyhow::Result<T> {
        json(self.send(Method::GET, path, query, None, None).await?).await
    }

    pub async fn post<T: DeserializeOwned>(&self, path: &str, body: &Value) -> anyhow::Result<T> {
        json(self.send(Method::POST, path, &[], Some(body), None).await?).await
    }

    /// A POST without a body, for actions such as deploying.
    pub async fn post_empty<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        json(self.send(Method::POST, path, &[], None, None).await?).await
    }

    /// An update of the resource at `revision`, which the server refuses if it changed since.
    pub async fn patch<T: DeserializeOwned>(&self, path: &str, revision: i64, body: &Value) -> anyhow::Result<T> {
        json(self.send(Method::PATCH, path, &[], Some(body), Some(revision)).await?).await
    }

    pub async fn delete(&self, path: &str) -> anyhow::Result<()> {
        self.send(Method::DELETE, path, &[], None, None).await.map(drop)
    }

    pub async fn text(&self, path: &str) -> anyhow::Result<String> {
        let response = self.send(Method::GET, path, &[], None, None).await?;
        response.text().await.context("The server's answer broke off")
    }

    /// Every item of a paginated list, following `next_cursor`.
    pub async fn list<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<Vec<T>> {
        #[derive(Deserialize)]
        struct Page<T> {
            items: Vec<T>,
            next_cursor: Option<String>,
        }
        let mut items = Vec::new();
        let mut after = None;
        loop {
            let mut query = vec![("limit", PAGE_SIZE.to_string())];
            query.extend(after.take().map(|after| ("after", after)));
            let page: Page<T> = self.get_with(path, &query).await?;
            items.extend(page.items);
            match page.next_cursor {
                Some(cursor) => after = Some(cursor),
                None => return Ok(items),
            }
        }
    }

    /// Opens a stream of server-sent events, resuming after `last_event_id` if given.
    pub async fn events(&self, path: &str, query: &[(&str, String)], last_event_id: Option<&str>) -> anyhow::Result<Events> {
        let mut request = self.http.get(self.url(path, query)).header(header::ACCEPT, "text/event-stream");
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        if let Some(id) = last_event_id {
            request = request.header("last-event-id", id);
        }
        let response = request.send().await.with_context(|| format!("Couldn't reach {}", self.server()))?;
        if !response.status().is_success() {
            return Err(problem(response).await);
        }
        Ok(Events::new(response))
    }
}

/// Reads an instance's address: a URL, or a bare host (HTTPS, or HTTP for loopback names).
/// A pasted API URL (`…/api/v1`) is taken back to the instance.
pub fn parse_server(server: &str) -> anyhow::Result<Url> {
    let server = server.trim();
    let with_scheme = if server.contains("://") {
        server.to_string()
    } else {
        let host = server.split([':', '/']).next().unwrap_or_default();
        let scheme = if matches!(host, "localhost" | "127.0.0.1") || host.ends_with(".localhost") { "http" } else { "https" };
        format!("{scheme}://{server}")
    };
    let mut url = Url::parse(&with_scheme).map_err(|_| fail(Exit::Failure, format!("`{server}` is not a server address")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(fail(Exit::Failure, format!("`{server}` is not an http(s) address")));
    }
    let path = url.path().trim_end_matches('/').trim_end_matches("/api/v1").trim_end_matches('/').to_string();
    url.set_path(&format!("{path}/"));
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

/// The address as shown and saved: no trailing `/`.
pub fn server_name(url: &Url) -> String {
    url.as_str().trim_end_matches('/').to_string()
}

async fn json<T: DeserializeOwned>(response: Response) -> anyhow::Result<T> {
    let body = response.bytes().await.context("The server's answer broke off")?;
    serde_json::from_slice(&body).context("The server's answer wasn't what was expected")
}

/// Turns a problem details answer into an error with the matching exit status.
async fn problem(response: Response) -> anyhow::Error {
    #[derive(Deserialize)]
    struct Problem {
        detail: Option<String>,
        title: Option<String>,
    }
    let status = response.status();
    let body = response.bytes().await.unwrap_or_default();
    let problem = serde_json::from_slice::<Problem>(&body).ok();
    let exit = match status {
        StatusCode::UNAUTHORIZED => Exit::Unauthenticated,
        StatusCode::FORBIDDEN => Exit::Forbidden,
        StatusCode::NOT_FOUND => Exit::NotFound,
        StatusCode::BAD_REQUEST
        | StatusCode::CONFLICT
        | StatusCode::PRECONDITION_FAILED
        | StatusCode::PRECONDITION_REQUIRED
        | StatusCode::UNPROCESSABLE_ENTITY => Exit::Refused,
        _ => Exit::Failure,
    };
    let mut message = match problem.and_then(|problem| problem.detail.or(problem.title)) {
        Some(detail) => detail,
        None => format!("the server answered {status}"),
    };
    if exit == Exit::Unauthenticated {
        message.push_str("; sign in with `spinikus login`");
    }
    fail(exit, message)
}

/// One server-sent event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub name: String,
    pub data: String,
}

/// A stream of server-sent events, read as it arrives.
pub struct Events {
    response: Option<Response>,
    buffer: Vec<u8>,
    name: String,
    data: Vec<String>,
    pending_id: Option<String>,
    /// The ID of the last event dispatched, to resume from.
    pub last_id: Option<String>,
}

impl Events {
    fn new(response: Response) -> Self {
        Self { response: Some(response), ..Self::empty() }
    }

    fn empty() -> Self {
        Self { response: None, buffer: Vec::new(), name: String::new(), data: Vec::new(), pending_id: None, last_id: None }
    }

    /// The next event, or `None` once the server ended the stream.
    pub async fn next(&mut self) -> anyhow::Result<Option<Event>> {
        loop {
            if let Some(event) = self.parse() {
                return Ok(Some(event));
            }
            let Some(response) = self.response.as_mut() else { return Ok(None) };
            match response.chunk().await.context("The stream broke off")? {
                Some(chunk) => self.buffer.extend_from_slice(&chunk),
                None => {
                    self.response = None;
                    return Ok(None);
                }
            }
        }
    }

    /// The next complete event in the buffer, if any.
    fn parse(&mut self) -> Option<Event> {
        while let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line[..end]);
            let line = line.strip_suffix('\r').unwrap_or(&line);
            if line.is_empty() {
                let name = std::mem::take(&mut self.name);
                if let Some(id) = self.pending_id.take() {
                    self.last_id = Some(id);
                }
                if self.data.is_empty() {
                    continue;
                }
                let data = std::mem::take(&mut self.data).join("\n");
                return Some(Event { name: if name.is_empty() { "message".to_string() } else { name }, data });
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
                None => (line, ""),
            };
            match field {
                "event" => self.name = value.to_string(),
                "data" => self.data.push(value.to_string()),
                "id" => self.pending_id = Some(value.to_string()),
                _ => {}
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_are_read_from_urls_or_hosts() {
        let parsed = |server: &str| parse_server(server).map(|url| url.to_string()).map_err(|error| error.to_string());
        assert_eq!(parsed("https://spinikus.example.com").unwrap(), "https://spinikus.example.com/");
        assert_eq!(parsed("spinikus.example.com").unwrap(), "https://spinikus.example.com/");
        assert_eq!(parsed("localhost:8080").unwrap(), "http://localhost:8080/");
        assert_eq!(parsed("127.0.0.1:8080/").unwrap(), "http://127.0.0.1:8080/");
        assert_eq!(parsed("https://example.com/paas/api/v1/").unwrap(), "https://example.com/paas/");
        assert!(parsed("ftp://example.com").is_err());
        let api = Api::new("https://example.com/paas", None).unwrap();
        assert_eq!(
            api.url("/projects", &[("after", "prj_1".into())]).as_str(),
            "https://example.com/paas/api/v1/projects?after=prj_1"
        );
        assert_eq!(api.server(), "https://example.com/paas");
    }

    #[test]
    fn events_are_parsed_across_chunks() {
        let mut events = Events::empty();
        let mut feed = |bytes: &str| {
            events.buffer.extend_from_slice(bytes.as_bytes());
            let mut parsed = Vec::new();
            while let Some(event) = events.parse() {
                parsed.push((event, events.last_id.clone()));
            }
            parsed
        };
        assert_eq!(feed(": keep-alive\n\nevent: log\nid: 12\ndata: hel"), []);
        let log = Event { name: "log".into(), data: "hello".into() };
        assert_eq!(
            feed("lo\r\n\r\nevent: status\ndata: {\"a\":1}\ndata:two\n\n"),
            [(log, Some("12".into())), (Event { name: "status".into(), data: "{\"a\":1}\ntwo".into() }, Some("12".into())),]
        );
        assert_eq!(feed("data: plain\n\n"), [(Event { name: "message".into(), data: "plain".into() }, Some("12".into()))]);
    }
}
