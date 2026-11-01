//! A database's backups: backing up now, schedules, the backups made, and restoring one.
//! Viewers see them; developers back up now; admins manage schedules, restore and delete.
//! While a backup or restore is under way, the list refreshes itself every 2 seconds.

use std::collections::HashMap;

use axum::{
    Form,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use serde::Deserialize;
use spinikus_control::{BackupScheduleChanges, Control, ControlError, NewBackupSchedule, backups::DEFAULT_KEEP_LAST};
use spinikus_core::{
    backup::{Backup, BackupSchedule, BackupStatus, Restore, StorageDestination},
    database::Engine,
    environment::Environment,
    id::{BackupId, ServiceId, StorageDestinationId},
    membership::Role,
    project::Project,
    service::Service,
};

use super::{Tab, crumbs, parse_id, problem_message, redirect, service_tabs};
use crate::{
    auth::Signed,
    error::WebError,
    forms::{self, Input, Notice},
    layout::{self, BADGE, BUTTON_PRIMARY, BUTTON_SECONDARY, CARD, INPUT, LINK, csrf_field},
};

/// How many backups a page lists.
const BACKUPS_PER_PAGE: u32 = 25;
/// How many restores the page lists.
const RESTORES_SHOWN: u32 = 5;
const POLL_EVERY: &str = "every 2s";

struct Context {
    service: Service,
    environment: Environment,
    project: Project,
    role: Role,
    engine: Engine,
}

impl Context {
    async fn load(control: &Control, signed: &Signed, id: ServiceId) -> Result<Self, WebError> {
        let principal = &signed.principal;
        let service = control.get_service(principal, id).await?;
        let engine = Engine::of(&service.config).ok_or(WebError::NoPage)?;
        let environment = control.get_environment(principal, service.environment_id).await?;
        let role = control.environment_role(principal, &environment).await?;
        let project = control.get_project(principal, environment.project_id).await?;
        Ok(Self { service, environment, project, role, engine })
    }

    fn url(&self) -> String {
        format!("/services/{}/backups", self.service.id)
    }
}

#[derive(Deserialize)]
pub struct After {
    after: Option<String>,
    /// Set after a restore was queued, to say so.
    restoring: Option<String>,
}

const RESTORING: &str = "Restore queued: the current data is backed up first, then replaced with the backup's.";

/// What the page says above its content.
enum Alert<'a> {
    None,
    Done(&'a str),
    Problem(&'a str),
}

pub async fn backups(
    State(control): State<Control>,
    signed: Signed,
    Path(id): Path<String>,
    Query(After { after, restoring }): Query<After>,
) -> Result<Markup, WebError> {
    let context = Context::load(&control, &signed, parse_id(&id)?).await?;
    let after = after.as_deref().map(parse_id).transpose()?;
    let alert = match restoring {
        Some(_) => Alert::Done(RESTORING),
        None => Alert::None,
    };
    Ok(render(&control, &signed, &context, after, alert, &ScheduleForm::new(), Notice::None).await?)
}

/// The backups and restores alone, which reload themselves while one is under way.
pub async fn activity(State(control): State<Control>, signed: Signed, Path(id): Path<String>) -> Result<Markup, WebError> {
    let context = Context::load(&control, &signed, parse_id(&id)?).await?;
    let destinations = destination_names(&control, &signed).await?;
    Ok(activity_section(&control, &signed, &context, None, &destinations).await?)
}

#[derive(Deserialize, Default)]
pub struct DestinationForm {
    #[serde(default)]
    destination_id: String,
}

pub async fn back_up(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<DestinationForm>,
) -> Result<Response, WebError> {
    let context = Context::load(&control, &signed, parse_id(&id)?).await?;
    let queued = match destination(&form.destination_id) {
        Ok(destination) => control.create_backup(&signed.principal, context.service.id, destination).await,
        Err(error) => Err(error),
    };
    match queued {
        Ok(_) => Ok(redirect(&headers, &context.url())),
        Err(error) => refused(&control, &signed, &context, error).await,
    }
}

fn destination(id: &str) -> Result<StorageDestinationId, ControlError> {
    id.parse().map_err(|_| ControlError::Invalid { field: "destination_id", problem: "choose a storage destination".to_string() })
}

/// The page again with why an action was refused, or an error page.
async fn refused(control: &Control, signed: &Signed, context: &Context, error: ControlError) -> Result<Response, WebError> {
    let message = problem_message(error)?;
    let page = render(control, signed, context, None, Alert::Problem(&message), &ScheduleForm::new(), Notice::None).await?;
    Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
}

// Schedules.

#[derive(Deserialize, Default)]
pub struct ScheduleForm {
    #[serde(default)]
    destination_id: String,
    #[serde(default)]
    cron: String,
    #[serde(default)]
    keep_last: String,
}

impl ScheduleForm {
    fn new() -> Self {
        Self { cron: "0 3 * * *".to_string(), keep_last: DEFAULT_KEEP_LAST.to_string(), ..Default::default() }
    }

    fn keep_last(&self) -> Result<u32, ControlError> {
        self.keep_last.trim().parse().map_err(|_| ControlError::Invalid {
            field: "keep_last",
            problem: "how many backups to keep must be a whole number".to_string(),
        })
    }
}

pub async fn create_schedule(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<ScheduleForm>,
) -> Result<Response, WebError> {
    let context = Context::load(&control, &signed, parse_id(&id)?).await?;
    let new = destination(&form.destination_id).and_then(|destination_id| {
        Ok(NewBackupSchedule { destination_id, cron: form.cron.clone(), keep_last: Some(form.keep_last()?), enabled: Some(true) })
    });
    let created = match new {
        Ok(new) => control.create_backup_schedule(&signed.principal, context.service.id, new).await,
        Err(error) => Err(error),
    };
    match created {
        Ok(_) => Ok(redirect(&headers, &context.url())),
        Err(error) => {
            let message = problem_message(error)?;
            let page = render(&control, &signed, &context, None, Alert::None, &form, Notice::Problem(&message)).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
    }
}

#[derive(Deserialize, Default)]
pub struct ToggleForm {
    #[serde(default)]
    revision: Option<i64>,
    /// `true` or `false`.
    #[serde(default)]
    enabled: String,
}

/// Turns a schedule on or off.
pub async fn update_schedule(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<ToggleForm>,
) -> Result<Response, WebError> {
    let schedule = control.get_backup_schedule(&signed.principal, parse_id(&id)?).await?;
    let context = Context::load(&control, &signed, schedule.service_id).await?;
    let changes = BackupScheduleChanges { enabled: Some(form.enabled == "true"), ..Default::default() };
    match control.update_backup_schedule(&signed.principal, schedule.id, form.revision.unwrap_or(-1), changes).await {
        Ok(_) => Ok(redirect(&headers, &context.url())),
        Err(error) => refused(&control, &signed, &context, error).await,
    }
}

pub async fn delete_schedule(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, WebError> {
    let schedule = control.get_backup_schedule(&signed.principal, parse_id(&id)?).await?;
    let context = Context::load(&control, &signed, schedule.service_id).await?;
    match control.delete_backup_schedule(&signed.principal, schedule.id).await {
        Ok(()) => Ok(redirect(&headers, &context.url())),
        Err(error) => refused(&control, &signed, &context, error).await,
    }
}

// Backups.

pub async fn restore(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, WebError> {
    let backup = control.get_backup(&signed.principal, parse_id(&id)?).await?;
    let context = Context::load(&control, &signed, backup.service_id).await?;
    match control.restore_backup(&signed.principal, backup.id).await {
        Ok(_) => Ok(redirect(&headers, &format!("{}?restoring=1", context.url()))),
        Err(error) => refused(&control, &signed, &context, error).await,
    }
}

pub async fn delete_backup(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, WebError> {
    let backup = control.get_backup(&signed.principal, parse_id(&id)?).await?;
    let context = Context::load(&control, &signed, backup.service_id).await?;
    match control.delete_backup(&signed.principal, backup.id).await {
        Ok(()) => Ok(redirect(&headers, &context.url())),
        Err(error) => refused(&control, &signed, &context, error).await,
    }
}

// Rendering.

/// Destination names by ID, deleted ones left out.
async fn destination_names(control: &Control, signed: &Signed) -> Result<HashMap<StorageDestinationId, String>, ControlError> {
    let destinations = control.list_storage_destinations(&signed.principal).await?;
    Ok(destinations.into_iter().map(|destination| (destination.id, destination.fields.name)).collect())
}

async fn render(
    control: &Control,
    signed: &Signed,
    context: &Context,
    after: Option<BackupId>,
    alert: Alert<'_>,
    schedule_form: &ScheduleForm,
    schedule_notice: Notice<'_>,
) -> Result<Markup, ControlError> {
    let principal = &signed.principal;
    let destinations = control.list_storage_destinations(principal).await?;
    let names: HashMap<StorageDestinationId, String> =
        destinations.iter().map(|destination| (destination.id, destination.fields.name.clone())).collect();
    let schedules = control.list_backup_schedules(principal, context.service.id).await?;
    let service = &context.service;
    let content = html! {
        @match alert {
            Alert::None => {},
            Alert::Done(message) => (layout::notice(message)),
            Alert::Problem(message) => (layout::alert(message)),
        }
        (layout::heading(&service.name, Some(html! { span class="font-mono" { (service.slug) } " · " (super::databases::engine_name(context.engine)) })))
        (service_tabs(service, context.role, Tab::Backups))
        div class="space-y-8" {
            @if destinations.is_empty() {
                div class="rounded-lg border border-dashed border-zinc-300 px-6 py-8 text-center text-sm text-zinc-500 dark:border-zinc-700 dark:text-zinc-400" {
                    "Backups go to S3-compatible storage, and none is set up yet. "
                    @if principal.is_instance_admin() {
                        a href="/settings/storage" class=(LINK) { "Add a storage destination" }
                    } @else {
                        "An instance admin can add a storage destination."
                    }
                }
            } @else if context.role >= Role::Developer {
                (back_up_now(signed, context, &destinations))
            }
            (activity_section(control, signed, context, after, &names).await?)
            (schedules_section(signed, context, &schedules, &destinations, &names, schedule_form, schedule_notice))
        }
    };
    let title = format!("Backups · {}", service.name);
    Ok(layout::page(signed, &title, &crumbs(&context.project, Some(&context.environment)), content))
}

fn back_up_now(signed: &Signed, context: &Context, destinations: &[StorageDestination]) -> Markup {
    html! {
        form method="post" action=(context.url()) class={ (CARD) " flex flex-wrap items-end gap-3 p-5" } {
            (csrf_field(signed))
            div class="min-w-48 flex-1 space-y-1.5" {
                label for="backup-destination" class=(layout::LABEL) { "Destination" }
                select id="backup-destination" name="destination_id" class=(INPUT) {
                    @for destination in destinations { option value=(destination.id) { (destination.fields.name) } }
                }
            }
            button type="submit" class=(BUTTON_PRIMARY) { "Back up now" }
        }
    }
}

/// Restores, then backups; refreshes itself while one is queued or running (on the first
/// page only).
async fn activity_section(
    control: &Control,
    signed: &Signed,
    context: &Context,
    after: Option<BackupId>,
    names: &HashMap<StorageDestinationId, String>,
) -> Result<Markup, ControlError> {
    let principal = &signed.principal;
    let backups = control.list_backups(principal, context.service.id, after, BACKUPS_PER_PAGE).await?;
    let restores = match after {
        None => control.list_restores(principal, context.service.id, None, RESTORES_SHOWN).await?.items,
        Some(_) => Vec::new(),
    };
    let busy = after.is_none()
        && (backups.items.iter().any(|backup| !backup.status.is_finished())
            || restores.iter().any(|restore| !restore.status.is_finished()));
    let url = format!("/services/{}/backups/activity", context.service.id);
    let admin = context.role >= Role::Admin;
    // Backups taken before a restore, to say so; only for the restores shown.
    let safety: Vec<BackupId> = restores.iter().filter_map(|restore| restore.safety_backup_id).collect();
    Ok(html! {
        div id="activity" class="space-y-8" hx-get=[busy.then_some(&url)] hx-trigger=[busy.then_some(POLL_EVERY)] hx-swap="outerHTML" {
            @if !restores.is_empty() {
                section {
                    h2 class="mb-3 font-semibold" { "Restores" }
                    ul class={ (CARD) " divide-y divide-zinc-200 dark:divide-zinc-800" } {
                        @for restore in &restores { (restore_row(restore, &backups.items)) }
                    }
                }
            }
            section {
                h2 class="mb-1 font-semibold" { "Backups" }
                p class="mb-3 text-sm text-zinc-500 dark:text-zinc-400" {
                    "Restoring one replaces the database's data with it, after backing up what is there now."
                }
                @if backups.items.is_empty() {
                    (layout::empty("No backups yet."))
                } @else {
                    ul class={ (CARD) " divide-y divide-zinc-200 dark:divide-zinc-800" } {
                        @for backup in &backups.items { (backup_row(signed, &context.service, backup, names, admin, safety.contains(&backup.id))) }
                    }
                    @if let (true, Some(last)) = (backups.has_more, backups.items.last()) {
                        div class="mt-3 text-sm" { a href={ (context.url()) "?after=" (last.id) } class=(LINK) { "Older backups" } }
                    }
                }
            }
        }
    })
}

fn backup_row(
    signed: &Signed,
    database: &Service,
    backup: &Backup,
    names: &HashMap<StorageDestinationId, String>,
    admin: bool,
    safety: bool,
) -> Markup {
    let destination = names.get(&backup.destination_id).map_or("a deleted destination", String::as_str);
    html! {
        li class="flex flex-wrap items-center gap-x-4 gap-y-2 px-5 py-3 text-sm" {
            span class="w-24" { (status_badge(backup.status)) }
            span { (layout::time(backup.created_at)) }
            span class="text-zinc-500 dark:text-zinc-400" {
                @if safety { "safety backup, before a restore" } @else if backup.schedule_id.is_some() { "scheduled" } @else { "by hand" }
                " · " (destination)
                @if let Some(size) = backup.size_bytes { " · " (human_size(size)) }
            }
            @if admin && backup.status.is_finished() {
                div class="ml-auto flex flex-wrap gap-2" {
                    @if backup.status == BackupStatus::Succeeded {
                        form method="post" action={ "/backups/" (backup.id) "/restore" }
                            hx-confirm="Restore this backup? The database's current data is backed up first, then replaced. Apps using it may get errors until the restore finishes." {
                            (csrf_field(signed))
                            button type="submit" class={ (BUTTON_SECONDARY) " py-1" } { "Restore" }
                        }
                        a href={ "/environments/" (database.environment_id) "/databases/new?from_backup=" (backup.id) }
                            class={ (BUTTON_SECONDARY) " py-1" } { "Restore into a new database" }
                    }
                    form method="post" action={ "/backups/" (backup.id) "/delete" } hx-confirm="Delete this backup? Its file is deleted from the storage too." {
                        (csrf_field(signed))
                        button type="submit" class={ (BUTTON_SECONDARY) " py-1 text-red-600 dark:text-red-400" } { "Delete" }
                    }
                }
            }
            @if let Some(error) = &backup.error {
                p class="w-full text-xs break-words text-red-700 dark:text-red-400" { (error) }
            }
        }
    }
}

/// A restore, naming its backup by when it was made if it is in `backups`.
fn restore_row(restore: &Restore, backups: &[Backup]) -> Markup {
    let backup = backups.iter().find(|backup| backup.id == restore.backup_id);
    html! {
        li class="flex flex-wrap items-center gap-x-4 gap-y-2 px-5 py-3 text-sm" {
            span class="w-24" { (status_badge(restore.status)) }
            span { (layout::time(restore.created_at)) }
            span class="text-zinc-500 dark:text-zinc-400" {
                @match backup {
                    Some(backup) => { "of the backup from " (layout::time(backup.created_at)) },
                    None => { "of the backup " span class="font-mono" { (restore.backup_id) } },
                }
                @if restore.status == BackupStatus::Queued {
                    @if restore.safety_backup_id.is_some() { " · waiting for the safety backup" } @else { " · waiting for the database to start" }
                }
            }
            @if let Some(error) = &restore.error {
                p class="w-full text-xs break-words text-red-700 dark:text-red-400" { (error) }
            }
        }
    }
}

fn schedules_section(
    signed: &Signed,
    context: &Context,
    schedules: &[BackupSchedule],
    destinations: &[StorageDestination],
    names: &HashMap<StorageDestinationId, String>,
    form: &ScheduleForm,
    notice: Notice<'_>,
) -> Markup {
    let admin = context.role >= Role::Admin;
    let options: Vec<(String, String)> =
        destinations.iter().map(|destination| (destination.id.to_string(), destination.fields.name.clone())).collect();
    let options: Vec<(&str, &str)> = options.iter().map(|(id, name)| (id.as_str(), name.as_str())).collect();
    let fields = html! {
        (forms::select("schedule-destination", "destination_id", "Back up to", &options, &form.destination_id))
        div class="grid gap-4 sm:grid-cols-2" {
            (Input::text("schedule-cron", "cron", "When", &form.cron).required().mono().placeholder("0 3 * * *")
                .hint("A cron expression in UTC: minute, hour, day, month, weekday. 0 3 * * * is every day at 03:00.").render())
            (Input::text("schedule-keep", "keep_last", "Backups kept", &form.keep_last).kind("number").mono()
                .hint("Older backups of this schedule are deleted.").render())
        }
    };
    html! {
        section {
            h2 class="mb-3 font-semibold" { "Schedules" }
            @if schedules.is_empty() {
                (layout::empty("No schedules: backups happen only when asked for."))
            } @else {
                ul class={ (CARD) " divide-y divide-zinc-200 dark:divide-zinc-800" } {
                    @for schedule in schedules {
                        li class="flex flex-wrap items-center gap-x-4 gap-y-2 px-5 py-3 text-sm" {
                            code class="font-mono" { (schedule.fields.cron) }
                            span class="text-zinc-500 dark:text-zinc-400" {
                                "to " (names.get(&schedule.fields.destination_id).map_or("a deleted destination", String::as_str))
                                " · keeps " (schedule.fields.keep_last)
                            }
                            @match schedule.next_run_at {
                                Some(next) => span class="text-zinc-500 dark:text-zinc-400" { "next " (layout::time(next)) },
                                None => span class={ (BADGE) " bg-zinc-50 text-zinc-600 ring-zinc-500/20 dark:bg-zinc-800 dark:text-zinc-300 dark:ring-zinc-700" } { "off" },
                            }
                            @if admin {
                                div class="ml-auto flex gap-2" {
                                    form method="post" action={ "/backup-schedules/" (schedule.id) } {
                                        (csrf_field(signed))
                                        input type="hidden" name="revision" value=(schedule.revision);
                                        input type="hidden" name="enabled" value=(if schedule.fields.enabled { "false" } else { "true" });
                                        button type="submit" class={ (BUTTON_SECONDARY) " py-1" } {
                                            @if schedule.fields.enabled { "Turn off" } @else { "Turn on" }
                                        }
                                    }
                                    form method="post" action={ "/backup-schedules/" (schedule.id) "/delete" }
                                        hx-confirm="Delete this schedule? Its backups are kept." {
                                        (csrf_field(signed))
                                        button type="submit" class={ (BUTTON_SECONDARY) " py-1 text-red-600 dark:text-red-400" } { "Delete" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            @if admin && !destinations.is_empty() {
                div class="mt-4 max-w-2xl" {
                    (forms::card(signed, "Add a schedule", None, &format!("/services/{}/backup-schedules", context.service.id), notice, fields, "Add schedule"))
                }
            }
        }
    }
}

pub fn status_badge(status: BackupStatus) -> Markup {
    let colors = match status {
        BackupStatus::Queued => {
            "bg-zinc-50 text-zinc-600 ring-zinc-500/20 dark:bg-zinc-800 dark:text-zinc-300 dark:ring-zinc-700"
        }
        BackupStatus::Running => {
            "bg-indigo-50 text-indigo-700 ring-indigo-600/20 dark:bg-indigo-500/10 dark:text-indigo-300 dark:ring-indigo-500/20"
        }
        BackupStatus::Succeeded => {
            "bg-emerald-50 text-emerald-700 ring-emerald-600/20 dark:bg-emerald-500/10 dark:text-emerald-400 dark:ring-emerald-500/20"
        }
        BackupStatus::Failed => {
            "bg-red-50 text-red-700 ring-red-600/10 dark:bg-red-500/10 dark:text-red-400 dark:ring-red-500/20"
        }
    };
    html! {
        span class={ (BADGE) " gap-1.5 " (colors) } {
            @if !status.is_finished() { span class="size-1.5 animate-pulse rounded-full bg-current" {} }
            (status.as_str())
        }
    }
}

/// A size in bytes, in the largest unit that keeps it at least 1.
pub(super) fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => format!("{bytes} B"),
        _ => format!("{size:.1} {}", UNITS[unit]),
    }
}

#[cfg(test)]
mod tests {
    use super::human_size;

    #[test]
    fn sizes_read_like_people_write_them() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024 * 1024), "5.0 GB");
    }
}
