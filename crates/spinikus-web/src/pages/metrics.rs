//! A service's Metrics tab, and the charts the server page shows too: CPU, memory and
//! network (and disk for servers), each on its own axis, over a range picked above them.

use axum::extract::{Path, Query, State};
use maud::{Markup, html};
use serde::Deserialize;
use spinikus_control::Control;
use spinikus_core::{
    id::ServiceId,
    metrics::{MetricPoint, MetricRange, Metrics, format_bytes},
};

use super::{Tab, crumbs, parse_id, service_tabs};
use crate::{
    auth::Signed,
    charts::{Chart, Hue, Series, Unit},
    error::WebError,
    layout,
};

#[derive(Deserialize, Default)]
pub struct RangeQuery {
    #[serde(default)]
    range: String,
}

impl RangeQuery {
    /// The range asked for; anything unparsable is the default.
    pub fn range(&self) -> MetricRange {
        self.range.parse().unwrap_or_default()
    }
}

pub async fn service(
    State(control): State<Control>,
    signed: Signed,
    Path(id): Path<String>,
    Query(query): Query<RangeQuery>,
) -> Result<Markup, WebError> {
    let principal = &signed.principal;
    let service = control.get_service(principal, parse_id::<ServiceId>(&id)?).await?;
    let environment = control.get_environment(principal, service.environment_id).await?;
    let project = control.get_project(principal, environment.project_id).await?;
    let role = control.environment_role(principal, &environment).await?;
    let metrics = control.service_metrics(principal, service.id, query.range()).await?;
    let base = format!("/services/{}/metrics", service.id);
    let content = html! {
        (layout::heading(&service.name, Some(html! { span class="font-mono" { (service.slug) } })))
        (service_tabs(&service, role, Tab::Metrics))
        (ranges(&base, metrics.range))
        @if metrics.points.is_empty() {
            (layout::empty("Nothing measured in this range. Servers' agents send what their containers use every minute while they run."))
        } @else {
            (charts(&metrics, None))
        }
        p class="mt-6 text-xs text-zinc-500 dark:text-zinc-400" {
            "What all of the service's containers use together, in UTC. While a deployment replaces another, both count."
        }
    };
    let mut crumbs = crumbs(&project, Some(&environment));
    crumbs.push((service.name.as_str(), format!("/services/{}", service.id)));
    Ok(layout::page(&signed, &format!("Metrics · {}", service.name), &crumbs, content))
}

/// The ranges to pick from, as links to `base` (filters in one row above the charts).
pub fn ranges(base: &str, current: MetricRange) -> Markup {
    let label = |range: MetricRange| match range {
        MetricRange::Hour => "1 hour",
        MetricRange::SixHours => "6 hours",
        MetricRange::Day => "24 hours",
        MetricRange::Week => "7 days",
        MetricRange::Month => "30 days",
    };
    html! {
        nav aria-label="Range" class="mb-6 inline-flex rounded-md text-sm ring-1 ring-zinc-200 dark:ring-zinc-800" {
            @for range in MetricRange::ALL {
                @let selected = range == current;
                a href={ (base) "?range=" (range) } aria-current=[selected.then_some("true")]
                    class={ "px-3 py-1.5 first:rounded-l-md last:rounded-r-md "
                        @if selected { "bg-zinc-900 text-white dark:bg-zinc-100 dark:text-zinc-900" }
                        @else { "text-zinc-600 hover:bg-zinc-50 dark:text-zinc-400 dark:hover:bg-zinc-800" } } {
                    (label(range))
                }
            }
        }
    }
}

/// The charts for `metrics`; `server` with the server's memory and disk sizes adds the disk
/// chart and puts memory on the server's scale.
pub fn charts(metrics: &Metrics, server: Option<ServerScale>) -> Markup {
    let step = jiff::SignedDuration::from_secs(metrics.step_seconds);
    let series = |field: fn(&MetricPoint) -> Option<f64>| -> Vec<(jiff::Timestamp, f64)> {
        metrics.points.iter().filter_map(|point| field(point).map(|value| (point.at, value))).collect()
    };
    let chart = |title, unit, series: Vec<Series<'static>>, ceiling: Option<u64>| Chart {
        title,
        unit,
        since: metrics.since,
        until: metrics.until,
        step,
        series,
        ceiling: ceiling.map(|ceiling| ceiling as f64),
        of: ceiling.map(|ceiling| format!("of {}", format_bytes(ceiling))),
    };
    let one = |label, points| vec![Series { label, hue: Hue::Blue, points }];
    let cpu = chart("CPU (cores)", Unit::Cores, one("CPU", series(|point| point.cpu_cores)), None);
    let memory_total = server.and_then(|scale| scale.memory);
    let memory =
        chart("Memory", Unit::Bytes, one("Memory", series(|point| point.memory_bytes.map(|bytes| bytes as f64))), memory_total);
    let network = chart(
        "Network",
        Unit::Rate,
        vec![
            Series { label: "Received", hue: Hue::Blue, points: series(|point| point.network_rx_rate) },
            Series { label: "Sent", hue: Hue::Orange, points: series(|point| point.network_tx_rate) },
        ],
        None,
    );
    let disk_total = server.and_then(|scale| scale.disk);
    let disk =
        chart("Disk", Unit::Bytes, one("Used", series(|point| point.disk_used_bytes.map(|bytes| bytes as f64))), disk_total);
    html! {
        div class="grid gap-x-10 gap-y-8 lg:grid-cols-2" {
            (cpu.render())
            (memory.render())
            (network.render())
            @if server.is_some() { (disk.render()) }
        }
    }
}

/// A server's sizes, the tops of its memory and disk charts when known.
#[derive(Clone, Copy)]
pub struct ServerScale {
    pub memory: Option<u64>,
    pub disk: Option<u64>,
}

impl ServerScale {
    /// From the newest points that say.
    pub fn of(metrics: &Metrics) -> Self {
        let newest = |field: fn(&MetricPoint) -> Option<u64>| metrics.points.iter().rev().find_map(field);
        Self { memory: newest(|point| point.memory_limit_bytes), disk: newest(|point| point.disk_total_bytes) }
    }
}
