//! A service's runtime logs: its container's output, read from the agent of its server on
//! demand. The page follows them live over SSE (rendered lines, like a deployment's log),
//! filtered on the server by text and stream, and offers them as a download. Viewers may
//! read them.

use std::{convert::Infallible, time::Duration};

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, header},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use jiff::Timestamp;
use maud::{Markup, html};
use serde::Deserialize;
use spinikus_control::{
    Control,
    service_logs::{ContainerLogUpdate, LogQuery},
};
use spinikus_core::{
    id::{DeploymentId, ServiceId},
    service::Service,
};
use tokio_stream::{Stream, StreamExt};

use super::{Tab, crumbs, parse_id, refused, service_tabs};
use crate::{
    auth::Signed,
    error::WebError,
    layout::{self, BUTTON_SECONDARY, INPUT, LABEL, LINK},
};

/// The choices of how many lines already written come first; the first is the default.
const TAILS: [u32; 3] = [100, 1_000, 10_000];

/// How long a download may take to read the log.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// What to show, from the query string. Everything is optional, and anything that doesn't
/// parse falls back to its default rather than failing the page.
#[derive(Deserialize, Default, Clone)]
pub struct LogParams {
    #[serde(default)]
    q: String,
    /// `stdout`, `stderr`, or anything else for both.
    #[serde(default)]
    stream: String,
    #[serde(default)]
    tail: String,
    #[serde(default)]
    deployment_id: String,
}

impl LogParams {
    fn tail(&self) -> u32 {
        self.tail.parse().ok().filter(|tail| TAILS.contains(tail)).unwrap_or(TAILS[0])
    }

    fn deployment(&self) -> Result<Option<DeploymentId>, WebError> {
        match self.deployment_id.trim() {
            "" => Ok(None),
            id => parse_id(id).map(Some),
        }
    }

    fn filter(&self) -> Filter {
        let needle = self.q.trim().to_ascii_lowercase();
        Filter {
            needle: Some(needle).filter(|needle| !needle.is_empty()),
            stdout: self.stream != "stderr",
            stderr: self.stream != "stdout",
        }
    }

    /// The query string for these parameters, with `?`, or empty.
    fn query(&self) -> String {
        let fields = [
            ("q", self.q.trim().to_string()),
            ("stream", self.stream.clone()),
            ("tail", self.tail.clone()),
            ("deployment_id", self.deployment_id.trim().to_string()),
        ];
        let pairs: Vec<String> = fields
            .iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(name, value)| format!("{name}={}", encode(value)))
            .collect();
        match pairs.is_empty() {
            true => String::new(),
            false => format!("?{}", pairs.join("&")),
        }
    }
}

/// Percent-encodes a query string value.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (byte as char).to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// Which lines to keep: those of the chosen streams that contain the text, ignoring ASCII case.
struct Filter {
    needle: Option<String>,
    stdout: bool,
    stderr: bool,
}

impl Filter {
    fn keeps(&self, stderr: bool, text: &str) -> bool {
        let stream = if stderr { self.stderr } else { self.stdout };
        stream && self.needle.as_ref().is_none_or(|needle| text.to_ascii_lowercase().contains(needle.as_str()))
    }
}

async fn service(control: &Control, signed: &Signed, id: &str) -> Result<Service, WebError> {
    let id: ServiceId = parse_id(id)?;
    Ok(control.get_service(&signed.principal, id).await?)
}

pub async fn logs(
    State(control): State<Control>,
    signed: Signed,
    Path(id): Path<String>,
    Query(params): Query<LogParams>,
) -> Result<Markup, WebError> {
    let principal = &signed.principal;
    let service = service(&control, &signed, &id).await?;
    let deployment = params.deployment()?;
    let environment = control.get_environment(principal, service.environment_id).await?;
    let project = control.get_project(principal, environment.project_id).await?;
    let role = control.environment_role(principal, &environment).await?;
    let base = format!("/services/{}/logs", service.id);
    let query = params.query();
    let stream = format!("{base}/stream{query}");
    let tail = params.tail().to_string();
    let content = html! {
        (layout::heading(&service.name, Some(html! { span class="font-mono" { (service.slug) } })))
        (service_tabs(&service, role, Tab::Logs))
        form method="get" action=(base) class="mb-4 flex flex-wrap items-end gap-3" {
            @if let Some(deployment) = deployment { input type="hidden" name="deployment_id" value=(deployment); }
            div class="min-w-48 grow" {
                label for="logs-q" class=(LABEL) { "Search" }
                input id="logs-q" type="search" name="q" value=(params.q) placeholder="Lines containing…" class={ (INPUT) " mt-1.5" };
            }
            div {
                label for="logs-stream" class=(LABEL) { "Stream" }
                select id="logs-stream" name="stream" class={ (INPUT) " mt-1.5" } {
                    @for (value, label) in [("", "stdout and stderr"), ("stdout", "stdout"), ("stderr", "stderr")] {
                        option value=(value) selected[params.stream == value] { (label) }
                    }
                }
            }
            div {
                label for="logs-tail" class=(LABEL) { "Start with" }
                select id="logs-tail" name="tail" class={ (INPUT) " mt-1.5" } {
                    @for count in TAILS {
                        option value=(count) selected[tail == count.to_string()] { "the last " (count) " lines" }
                    }
                }
            }
            button type="submit" class=(BUTTON_SECONDARY) { "Show" }
            a href={ (base) "/download" (query) } hx-boost="false" download class=(BUTTON_SECONDARY) { "Download" }
        }
        p class="mb-3 text-sm text-zinc-500 dark:text-zinc-400" {
            @match deployment {
                Some(deployment) => {
                    "The container of deployment "
                    a href={ "/deployments/" (deployment) } class={ (LINK) " font-mono" } { (deployment) }
                    ". " a href=(base) class=(LINK) { "Show what runs now" } "."
                }
                None => { "The container that runs now, followed live. The search applies to the lines read, so it may show fewer." }
            }
        }
        div hx-ext="sse" sse-connect=(stream) sse-close="end" {
            div class="flex max-h-[36rem] min-h-48 flex-col-reverse overflow-y-auto rounded-lg bg-zinc-950 p-4 font-mono text-xs leading-relaxed text-zinc-200 ring-1 ring-zinc-800" {
                div id="log" sse-swap="log" hx-swap="beforeend" {}
            }
        }
    };
    let mut crumbs = crumbs(&project, Some(&environment));
    crumbs.push((service.name.as_str(), format!("/services/{}", service.id)));
    Ok(layout::page(&signed, &format!("Logs · {}", service.name), &crumbs, content))
}

/// The logs as server-sent events for the page: a rendered `log` event per line that passes
/// the filter, a last line saying why they ended, and `end`. A browser that reconnects gets
/// the lines from the time of the last one it got (`Last-Event-ID`) instead of the tail again.
pub async fn stream(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<LogParams>,
) -> Result<Response, WebError> {
    let service = service(&control, &signed, &id).await?;
    let resume = headers.get("last-event-id").and_then(|value| value.to_str().ok()).and_then(|value| value.parse().ok());
    let query = LogQuery {
        deployment_id: params.deployment()?,
        since: resume,
        tail: if resume.is_some() { None } else { Some(params.tail()) },
        follow: true,
    };
    let filter = params.filter();
    let needle = filter.needle.clone();
    let updates = match control.service_logs(&signed.principal, service.id, query).await {
        Ok(updates) => updates,
        // Said in the log box, rather than failing the connection, which the browser would retry.
        Err(error) => match refused(&error) {
            Some(message) => return Ok(sse(tokio_stream::iter(ended(Some(&message))))),
            None => return Err(error.into()),
        },
    };
    // A task turns updates into events (a line, or the end's two), and stops when the
    // browser goes away, which drops the updates and so closes the session on the agent.
    let (sender, events) = tokio::sync::mpsc::channel(256);
    tokio::spawn(async move {
        let mut updates = updates;
        while let Some(update) = updates.next().await {
            let events = match update {
                ContainerLogUpdate::Line { at, stderr, text } if filter.keeps(stderr, &text) => {
                    let event = Event::default().event("log").data(line(at, stderr, &text, needle.as_deref()).into_string());
                    vec![match at {
                        Some(at) => event.id(at.to_string()),
                        None => event,
                    }]
                }
                ContainerLogUpdate::Line { .. } => vec![],
                ContainerLogUpdate::End { error } => ended(error.as_deref()),
            };
            for event in events {
                if sender.send(event).await.is_err() {
                    return;
                }
            }
        }
    });
    let events = tokio_stream::wrappers::ReceiverStream::new(events);
    Ok(sse(events))
}

fn sse(events: impl Stream<Item = Event> + Send + 'static) -> Response {
    Sse::new(events.map(Ok::<_, Infallible>)).keep_alive(KeepAlive::default()).into_response()
}

/// A last line saying why the log ended, then `end`.
fn ended(error: Option<&str>) -> Vec<Event> {
    let notice = html! {
        p class="mt-2 font-sans text-zinc-400" {
            @match error {
                Some(error) => (error),
                None => "The log ended: the container stopped.",
            }
        }
    };
    vec![Event::default().event("log").data(notice.into_string()), Event::default().event("end").data("")]
}

/// A line: the time of day, dimmed, then the text, red for stderr, with the search's matches marked.
fn line(at: Option<Timestamp>, stderr: bool, text: &str, needle: Option<&str>) -> Markup {
    let time = at.map(|at| at.strftime("%H:%M:%S").to_string()).unwrap_or_default();
    let full = at.map(|at| at.to_string());
    html! {
        div class={ "flex gap-3 whitespace-pre-wrap break-all" @if stderr { " text-red-300" } } {
            span class="shrink-0 select-none text-zinc-500" title=[full] { (time) }
            span { (marked(text, needle)) }
        }
    }
}

/// The text with each match of `needle` (lowercase) in a `mark`, ignoring ASCII case.
/// ASCII lowercasing keeps byte offsets, and a match of valid UTF-8 starts and ends on
/// character boundaries, so the slices are safe.
fn marked(text: &str, needle: Option<&str>) -> Markup {
    let Some(needle) = needle.filter(|needle| !needle.is_empty()) else {
        return html! { (text) };
    };
    let lower = text.to_ascii_lowercase();
    let mut parts = Vec::new();
    let mut start = 0;
    while let Some(found) = lower[start..].find(needle) {
        let at = start + found;
        parts.push((&text[start..at], false));
        parts.push((&text[at..at + needle.len()], true));
        start = at + needle.len();
    }
    parts.push((&text[start..], false));
    html! {
        @for (part, matched) in parts {
            @if matched { mark class="rounded-sm bg-amber-300 text-zinc-950" { (part) } } @else { (part) }
        }
    }
}

/// The logs as a text file: the lines already written that pass the filter, one per line
/// as `<time> <stream> <text>`.
pub async fn download(
    State(control): State<Control>,
    signed: Signed,
    Path(id): Path<String>,
    Query(params): Query<LogParams>,
) -> Result<Response, WebError> {
    let service = service(&control, &signed, &id).await?;
    let query = LogQuery { deployment_id: params.deployment()?, since: None, tail: Some(params.tail()), follow: false };
    let filter = params.filter();
    let mut body = String::new();
    match control.service_logs(&signed.principal, service.id, query).await {
        Ok(mut updates) => {
            let read = async {
                while let Some(update) = updates.next().await {
                    match update {
                        ContainerLogUpdate::Line { at, stderr, text } if filter.keeps(stderr, &text) => {
                            let at = at.map(|at| at.to_string()).unwrap_or_else(|| "-".to_string());
                            let stream = if stderr { "stderr" } else { "stdout" };
                            body.push_str(&format!("{at} {stream} {text}\n"));
                        }
                        ContainerLogUpdate::Line { .. } => {}
                        ContainerLogUpdate::End { error: Some(error) } => body.push_str(&format!("# {error}\n")),
                        ContainerLogUpdate::End { error: None } => break,
                    }
                }
            };
            if tokio::time::timeout(DOWNLOAD_TIMEOUT, read).await.is_err() {
                body.push_str("# Stopped reading: the log took too long.\n");
            }
        }
        Err(error) => match refused(&error) {
            Some(message) => body.push_str(&format!("# {message}\n")),
            None => return Err(error.into()),
        },
    }
    let name = format!("{}-{}.log", service.slug, Timestamp::now().strftime("%Y%m%d-%H%M%S"));
    // A slug and a time are letters, digits and dashes.
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")).expect("a slug is a valid header value");
    Ok((
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8")),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        body,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_matches_ignoring_case_and_keeps_other_characters() {
        let marked = marked("Café ERROR: error é", Some("error")).into_string();
        assert_eq!(
            marked,
            r#"Café <mark class="rounded-sm bg-amber-300 text-zinc-950">ERROR</mark>: <mark class="rounded-sm bg-amber-300 text-zinc-950">error</mark> é"#
        );
        assert_eq!(marked_plain("<b>"), "&lt;b&gt;");
    }

    fn marked_plain(text: &str) -> String {
        marked(text, None).into_string()
    }

    #[test]
    fn filters_by_stream_and_text() {
        let params = LogParams { q: " Timeout ".into(), stream: "stderr".into(), ..Default::default() };
        let filter = params.filter();
        assert!(filter.keeps(true, "request TIMEOUT after 5s"));
        assert!(!filter.keeps(false, "request timeout after 5s"));
        assert!(!filter.keeps(true, "all good"));
        assert_eq!(params.query(), "?q=Timeout&stream=stderr");
        assert_eq!(LogParams { tail: "7".into(), ..Default::default() }.tail(), 100, "only the offered tails");
    }
}
