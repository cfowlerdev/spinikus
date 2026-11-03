//! The page shell and the small pieces pages share. Class lists live here as constants so
//! pages stay consistent; Tailwind finds them in the source like any other class.

use jiff::Timestamp;
use maud::{DOCTYPE, Markup, html};
use serde_json::json;
use spinikus_core::{deployment::DeploymentStatus, environment::EnvironmentKind, service::ServiceKind};

use crate::{
    assets,
    auth::{CSRF_FIELD, Signed},
};

pub const BUTTON_PRIMARY: &str = "inline-flex items-center justify-center gap-2 rounded-md bg-indigo-600 px-3 py-2 \
     text-sm font-medium text-white shadow-xs hover:bg-indigo-500 focus-visible:outline-2 \
     focus-visible:outline-offset-2 focus-visible:outline-indigo-600 disabled:opacity-50";
pub const BUTTON_SECONDARY: &str = "inline-flex items-center justify-center gap-2 rounded-md bg-white px-3 py-2 \
     text-sm font-medium text-zinc-900 shadow-xs ring-1 ring-zinc-300 ring-inset hover:bg-zinc-50 \
     dark:bg-zinc-800 dark:text-zinc-100 dark:ring-zinc-700 dark:hover:bg-zinc-700";
pub const INPUT: &str = "block w-full rounded-md border border-zinc-300 bg-white px-3 py-2 text-sm shadow-xs \
     placeholder:text-zinc-400 focus:border-indigo-500 focus:outline-2 focus:outline-indigo-500/30 \
     dark:border-zinc-700 dark:bg-zinc-900";
pub const LABEL: &str = "block text-sm font-medium text-zinc-700 dark:text-zinc-300";
pub const CARD: &str = "rounded-lg border border-zinc-200 bg-white dark:border-zinc-800 dark:bg-zinc-900";
pub const LINK: &str = "font-medium text-indigo-600 hover:text-indigo-500 dark:text-indigo-400 dark:hover:text-indigo-300";
const MUTED: &str = "text-sm text-zinc-500 dark:text-zinc-400";

/// HTMX's settings: no inline styles (the CSP refuses them), error responses are swapped in
/// like any page (they are pages), and no page is kept in the browser's history cache,
/// since pages may show secrets.
fn htmx_config() -> String {
    json!({
        "includeIndicatorStyles": false,
        "allowEval": false,
        "historyCacheSize": 0,
        "responseHandling": [
            { "code": "204", "swap": false },
            { "code": "[23]..", "swap": true },
            { "code": "[45]..", "swap": true, "error": true },
        ],
    })
    .to_string()
}

/// What a page needs beyond the usual: its own scripts and stylesheets, and whether HTMX
/// boosts its links and forms.
#[derive(Clone, Copy)]
pub struct Head<'a> {
    pub boost: bool,
    pub scripts: &'a [&'a str],
    pub styles: &'a [&'a str],
}

impl Head<'_> {
    const USUAL: Head<'static> = Head { boost: true, scripts: &[], styles: &[] };
}

fn document(title: &str, body: Markup, head: Head) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" class="h-full" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="htmx-config" content=(htmx_config());
                title { (title) " · Spinikus" }
                link rel="stylesheet" href=(assets::url("app.css"));
                script src=(assets::url("htmx.min.js")) defer {}
                script src=(assets::url("htmx-ext-sse.min.js")) defer {}
                script src=(assets::url("palette.js")) defer {}
                @for style in head.styles { link rel="stylesheet" href=(assets::url(style)); }
                @for script in head.scripts { script src=(assets::url(script)) defer {} }
            }
            body class="h-full bg-zinc-50 text-zinc-900 antialiased dark:bg-zinc-950 dark:text-zinc-100" hx-boost=(head.boost) {
                (body)
            }
        }
    }
}

/// A page for someone signed in: the header, then `content`. HTMX requests carry the CSRF
/// token from the wrapper, which is swapped with the page (the body's own attributes are not).
pub fn page(signed: &Signed, title: &str, crumbs: &[(&str, String)], content: Markup) -> Markup {
    page_with(signed, title, crumbs, content, Head::USUAL)
}

/// [`page`] with more in its head.
pub fn page_with(signed: &Signed, title: &str, crumbs: &[(&str, String)], content: Markup, head: Head) -> Markup {
    let headers = json!({ "X-CSRF-Token": signed.csrf }).to_string();
    let user = &signed.principal.user;
    document(
        title,
        html! {
            div class="min-h-full" hx-headers=(headers) {
                header class="border-b border-zinc-200 bg-white dark:border-zinc-800 dark:bg-zinc-900" {
                    div class="mx-auto flex h-14 max-w-6xl items-center gap-4 px-4 sm:gap-6" {
                        a href="/" class="flex items-center gap-2 font-semibold tracking-tight" {
                            span class="size-2.5 rounded-full bg-indigo-500" {}
                            "Spinikus"
                        }
                        nav class="flex items-center gap-3 text-sm sm:gap-4" {
                            a href="/" class="text-zinc-600 hover:text-zinc-900 dark:text-zinc-400 dark:hover:text-zinc-100" { "Projects" }
                            a href="/settings" class="text-zinc-600 hover:text-zinc-900 dark:text-zinc-400 dark:hover:text-zinc-100" { "Settings" }
                        }
                        div class="ml-auto flex items-center gap-3 text-sm whitespace-nowrap sm:gap-4" {
                            button type="button" data-palette-open title="Go to… (Cmd/Ctrl+K)"
                                class="flex items-center gap-2 rounded-md px-2 py-1 text-zinc-500 ring-1 ring-zinc-200 hover:text-zinc-900 dark:text-zinc-400 dark:ring-zinc-800 dark:hover:text-zinc-100" {
                                "Search"
                                kbd class="hidden font-sans text-xs text-zinc-400 sm:inline" { "⌘K" }
                            }
                            span class="hidden text-zinc-600 sm:inline dark:text-zinc-400" title=(user.email.as_str()) { (user.name) }
                            form method="post" action="/logout" {
                                (csrf_field(signed))
                                button type="submit" class="text-zinc-600 hover:text-zinc-900 dark:text-zinc-400 dark:hover:text-zinc-100" {
                                    "Sign out"
                                }
                            }
                        }
                    }
                }
                main class="mx-auto max-w-6xl px-4 py-8" {
                    @if !crumbs.is_empty() {
                        nav aria-label="Breadcrumb" class="mb-2 flex flex-wrap items-center gap-1.5 text-sm text-zinc-500 dark:text-zinc-400" {
                            @for (index, (label, href)) in crumbs.iter().enumerate() {
                                @if index > 0 { span aria-hidden="true" { "/" } }
                                a href=(href) class="hover:text-zinc-900 dark:hover:text-zinc-100" { (label) }
                            }
                        }
                    }
                    (content)
                }
                (palette())
            }
        },
        head,
    )
}

/// The command palette's dialog, filled and driven by `palette.js`. The dialog itself has
/// no padding, so a click on it rather than its content is a click on the backdrop.
fn palette() -> Markup {
    html! {
        dialog id="palette" aria-label="Go to"
            class="mx-auto mt-[12vh] w-[min(36rem,calc(100%-2rem))] rounded-xl bg-white p-0 text-zinc-900 shadow-2xl ring-1 ring-zinc-200 backdrop:bg-zinc-950/40 dark:bg-zinc-900 dark:text-zinc-100 dark:ring-zinc-800" {
            div class="p-2" {
                input id="palette-input" type="search" autocomplete="off" spellcheck="false" aria-controls="palette-results"
                    placeholder="Go to a project, environment, service or page…"
                    class="block w-full rounded-md border-0 bg-transparent px-3 py-2.5 text-sm outline-none placeholder:text-zinc-400";
                ul id="palette-results" role="listbox" class="max-h-[50vh] overflow-y-auto border-t border-zinc-200 pt-2 dark:border-zinc-800" {}
                p id="palette-empty" hidden class="px-3 py-6 text-center text-sm text-zinc-500 dark:text-zinc-400" { "Nothing matches." }
            }
        }
    }
}

/// A page without the header, centered: signing in, setup and errors.
pub fn bare(title: &str, content: Markup) -> Markup {
    document(
        title,
        html! {
            div class="flex min-h-full flex-col items-center justify-center px-4 py-12" {
                (content)
            }
        },
        Head::USUAL,
    )
}

/// The hidden CSRF field every form posted with a session needs.
pub fn csrf_field(signed: &Signed) -> Markup {
    html! { input type="hidden" name=(CSRF_FIELD) value=(signed.csrf); }
}

pub fn heading(title: &str, subtitle: Option<Markup>) -> Markup {
    header(title, subtitle, html! {})
}

/// A page's title, with buttons or links for what can be done there on the right.
pub fn header(title: &str, subtitle: Option<Markup>, actions: Markup) -> Markup {
    html! {
        div class="mb-6 flex flex-wrap items-start gap-4" {
            div {
                h1 class="text-2xl font-semibold tracking-tight" { (title) }
                @if let Some(subtitle) = subtitle { p class={ "mt-1 " (MUTED) } { (subtitle) } }
            }
            div class="ml-auto flex flex-wrap items-center gap-2" { (actions) }
        }
    }
}

/// Links to the pages of one thing (a service's deployments, variables, …), the current one
/// marked. Each is `(label, href, current)`.
pub fn tabs(tabs: &[(&str, String, bool)]) -> Markup {
    html! {
        nav aria-label="Sections" class="mb-6 flex gap-6 overflow-x-auto border-b border-zinc-200 text-sm dark:border-zinc-800" {
            @for (label, href, current) in tabs {
                // The terminal's page loads in full, for its scripts and its own CSP.
                @let boost = if href.ends_with("/terminal") { "false" } else { "true" };
                @if *current {
                    a href=(href) hx-boost=(boost) aria-current="page" class="-mb-px shrink-0 border-b-2 border-indigo-500 pb-3 font-medium whitespace-nowrap text-zinc-900 dark:text-zinc-100" { (label) }
                } @else {
                    a href=(href) hx-boost=(boost) class="-mb-px shrink-0 border-b-2 border-transparent pb-3 whitespace-nowrap text-zinc-500 hover:border-zinc-300 hover:text-zinc-900 dark:text-zinc-400 dark:hover:text-zinc-100" { (label) }
                }
            }
        }
    }
}

pub fn empty(message: &str) -> Markup {
    html! {
        div class="rounded-lg border border-dashed border-zinc-300 px-6 py-10 text-center text-sm text-zinc-500 dark:border-zinc-700 dark:text-zinc-400" {
            (message)
        }
    }
}

pub fn time(at: Timestamp) -> Markup {
    html! { time datetime=(at) { (at.strftime("%Y-%m-%d %H:%M UTC")) } }
}

pub const BADGE: &str = "inline-flex items-center rounded-md px-2 py-0.5 text-xs font-medium ring-1 ring-inset";

pub fn environment_badge(kind: EnvironmentKind) -> Markup {
    let colors = match kind {
        EnvironmentKind::Production => {
            "bg-emerald-50 text-emerald-700 ring-emerald-600/20 dark:bg-emerald-500/10 dark:text-emerald-400 dark:ring-emerald-500/20"
        }
        EnvironmentKind::Staging => {
            "bg-amber-50 text-amber-700 ring-amber-600/20 dark:bg-amber-500/10 dark:text-amber-400 dark:ring-amber-500/20"
        }
        EnvironmentKind::Preview { .. } => {
            "bg-sky-50 text-sky-700 ring-sky-600/20 dark:bg-sky-500/10 dark:text-sky-400 dark:ring-sky-500/20"
        }
    };
    html! {
        span class={ (BADGE) " " (colors) } {
            (kind.as_str())
            @if let Some(number) = kind.pr_number() { " #" (number) }
        }
    }
}

pub fn service_badge(kind: ServiceKind) -> Markup {
    let colors = match kind {
        ServiceKind::App => {
            "bg-indigo-50 text-indigo-700 ring-indigo-600/20 dark:bg-indigo-500/10 dark:text-indigo-400 dark:ring-indigo-500/20"
        }
        ServiceKind::Database => {
            "bg-violet-50 text-violet-700 ring-violet-600/20 dark:bg-violet-500/10 dark:text-violet-400 dark:ring-violet-500/20"
        }
        ServiceKind::Template => {
            "bg-zinc-50 text-zinc-700 ring-zinc-600/20 dark:bg-zinc-500/10 dark:text-zinc-300 dark:ring-zinc-500/20"
        }
    };
    html! { span class={ (BADGE) " " (colors) } { (kind.as_str()) } }
}

/// A deployment's status; in-progress ones get a pulsing dot.
pub fn status_badge(status: DeploymentStatus) -> Markup {
    use DeploymentStatus::*;
    let colors = match status {
        Live => {
            "bg-emerald-50 text-emerald-700 ring-emerald-600/20 dark:bg-emerald-500/10 dark:text-emerald-400 dark:ring-emerald-500/20"
        }
        Failed => "bg-red-50 text-red-700 ring-red-600/20 dark:bg-red-500/10 dark:text-red-400 dark:ring-red-500/20",
        Queued | Preparing | Releasing | Starting | Verifying => {
            "bg-sky-50 text-sky-700 ring-sky-600/20 dark:bg-sky-500/10 dark:text-sky-400 dark:ring-sky-500/20"
        }
        Draining | Superseded | Canceled => {
            "bg-zinc-50 text-zinc-600 ring-zinc-500/20 dark:bg-zinc-500/10 dark:text-zinc-400 dark:ring-zinc-500/20"
        }
    };
    html! {
        span class={ (BADGE) " gap-1.5 " (colors) } {
            @if status.is_in_progress() { span class="size-1.5 animate-pulse rounded-full bg-current" {} }
            (status.as_str())
        }
    }
}

/// A message above a page's content, such as why an action was refused.
pub fn alert(message: &str) -> Markup {
    html! {
        div role="alert" class="mb-6 rounded-md bg-red-50 px-4 py-3 text-sm text-red-700 ring-1 ring-red-600/10 ring-inset dark:bg-red-500/10 dark:text-red-400 dark:ring-red-500/20" {
            (message)
        }
    }
}

/// A message above a page's content saying something worked, such as a test.
pub fn notice(message: &str) -> Markup {
    html! {
        div role="status" class="mb-6 rounded-md bg-emerald-50 px-4 py-3 text-sm text-emerald-700 ring-1 ring-emerald-600/10 ring-inset dark:bg-emerald-500/10 dark:text-emerald-400 dark:ring-emerald-500/20" {
            (message)
        }
    }
}

/// A link to the next page of a list, if there is one.
pub fn next_page(has_more: bool, last_id: Option<String>) -> Markup {
    html! {
        @if let (true, Some(last)) = (has_more, last_id) {
            div class="mt-4" { a href={ "?after=" (last) } class=(LINK) { "More" } }
        }
    }
}
