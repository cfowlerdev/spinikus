//! The settings' Backups tab, for instance admins: backups of Spinikus itself (its database
//! and master key, encrypted with a passphrase) on a schedule or by hand, to a storage
//! destination. The passphrase is write-only: never shown, kept when left empty.

use axum::{
    Form,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use serde::Deserialize;
use spinikus_control::{Control, ControlError, InstanceBackupScheduleInput};
use spinikus_core::{
    backup::{BackupStatus, StorageDestination},
    instance_backup::{InstanceBackup, InstanceBackupSchedule},
    secret::Secret,
};

use super::{
    backups::{human_size, status_badge},
    parse_id, problem_message, redirect,
    settings::{SettingsTab, require_admin, tabs},
};
use crate::{
    auth::Signed,
    error::WebError,
    forms::{self, Input, Notice},
    layout::{self, BUTTON_PRIMARY, BUTTON_SECONDARY, CARD, LINK, csrf_field},
};

const URL: &str = "/settings/backups";

/// How many backups the tab lists.
const SHOWN: u32 = 50;

#[derive(Deserialize)]
pub struct ScheduleForm {
    #[serde(default)]
    revision: Option<i64>,
    #[serde(default)]
    destination_id: String,
    #[serde(default)]
    cron: String,
    #[serde(default)]
    keep_last: String,
    /// `on` when checked.
    #[serde(default)]
    enabled: Option<String>,
    #[serde(default)]
    passphrase: String,
}

impl ScheduleForm {
    fn of(schedule: Option<&InstanceBackupSchedule>) -> Self {
        match schedule {
            Some(schedule) => Self {
                revision: Some(schedule.revision),
                destination_id: schedule.fields.destination_id.to_string(),
                cron: schedule.fields.cron.clone(),
                keep_last: schedule.fields.keep_last.to_string(),
                enabled: schedule.fields.enabled.then(|| "on".to_string()),
                passphrase: String::new(),
            },
            None => Self {
                revision: Some(0),
                destination_id: String::new(),
                cron: "0 3 * * *".to_string(),
                keep_last: spinikus_control::backups::DEFAULT_KEEP_LAST.to_string(),
                enabled: Some("on".to_string()),
                passphrase: String::new(),
            },
        }
    }

    fn input(&self) -> Result<InstanceBackupScheduleInput, ControlError> {
        let destination_id = self
            .destination_id
            .trim()
            .parse()
            .map_err(|_| ControlError::Invalid { field: "destination_id", problem: "choose where backups go".to_string() })?;
        let keep_last = match self.keep_last.trim() {
            "" => None,
            keep => Some(
                keep.parse()
                    .map_err(|_| ControlError::Invalid { field: "keep_last", problem: "must be a number".to_string() })?,
            ),
        };
        Ok(InstanceBackupScheduleInput {
            destination_id,
            cron: self.cron.clone(),
            keep_last,
            enabled: self.enabled.is_some(),
            passphrase: Some(Secret::new(self.passphrase.as_str())).filter(|passphrase| !passphrase.expose().is_empty()),
        })
    }

    fn fields(&self, destinations: &[StorageDestination], is_set: bool) -> Markup {
        let mut options = vec![(String::new(), "Choose a destination".to_string())];
        options.extend(destinations.iter().map(|destination| (destination.id.to_string(), destination.fields.name.clone())));
        let options: Vec<(&str, &str)> = options.iter().map(|(value, label)| (value.as_str(), label.as_str())).collect();
        let passphrase_hint = match is_set {
            true => "Leave it empty to keep the current one. A new one applies to backups made from now on.",
            false => "At least 12 characters. Backups can't be restored without it, so keep it somewhere other than this server.",
        };
        html! {
            (forms::select("instance-backup-destination", "destination_id", "Destination", &options, &self.destination_id))
            div class="grid gap-4 sm:grid-cols-2" {
                (Input::text("instance-backup-cron", "cron", "Schedule", &self.cron).required().mono().placeholder("0 3 * * *")
                    .hint("A cron expression, in UTC.").render())
                (Input::text("instance-backup-keep", "keep_last", "Keep", &self.keep_last).kind("number").mono()
                    .hint("How many scheduled backups to keep.").render())
            }
            (Input::text("instance-backup-passphrase", "passphrase", "Passphrase", "").kind("password").mono().hint(passphrase_hint).render())
            (forms::checkbox("instance-backup-enabled", "enabled", "Back up on the schedule", self.enabled.is_some(), false, None))
            @if let Some(revision) = self.revision { input type="hidden" name="revision" value=(revision); }
        }
    }
}

pub async fn set_schedule(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Form(form): Form<ScheduleForm>,
) -> Result<Response, WebError> {
    require_admin(&signed)?;
    let saved = match form.input() {
        Ok(input) => control.set_instance_backup_schedule(&signed.principal, form.revision.unwrap_or(-1), input).await,
        Err(error) => Err(error),
    };
    match saved {
        Ok(_) => Ok(redirect(&headers, &format!("{URL}?saved=schedule"))),
        Err(error) => {
            let message = problem_message(error)?;
            let schedule = control.get_instance_backup_schedule(&signed.principal).await?;
            let page = render(&control, &signed, schedule.as_ref(), &form, Notice::Problem(&message), None).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
    }
}

/// Backs up now.
pub async fn create(State(control): State<Control>, signed: Signed, headers: HeaderMap) -> Result<Response, WebError> {
    require_admin(&signed)?;
    match control.back_up_instance(&signed.principal).await {
        Ok(_) => Ok(redirect(&headers, URL)),
        Err(error) => refused(&control, &signed, error).await,
    }
}

pub async fn delete(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, WebError> {
    require_admin(&signed)?;
    match control.delete_instance_backup(&signed.principal, parse_id(&id)?).await {
        Ok(()) => Ok(redirect(&headers, URL)),
        Err(error) => refused(&control, &signed, error).await,
    }
}

/// The page with why an action was refused, at the tab's URL.
async fn refused(control: &Control, signed: &Signed, error: ControlError) -> Result<Response, WebError> {
    let message = problem_message(error)?;
    let schedule = control.get_instance_backup_schedule(&signed.principal).await?;
    let page =
        render(control, signed, schedule.as_ref(), &ScheduleForm::of(schedule.as_ref()), Notice::None, Some(&message)).await?;
    Ok((StatusCode::UNPROCESSABLE_ENTITY, [("hx-replace-url", URL)], page).into_response())
}

/// The backups, polling while one is unfinished.
pub async fn activity(State(control): State<Control>, signed: Signed) -> Result<Markup, WebError> {
    require_admin(&signed)?;
    Ok(backups(&control, &signed).await?)
}

/// Which section was just saved, from `?saved=`.
#[derive(Deserialize)]
pub struct Saved {
    saved: Option<String>,
}

/// The tab, saying the schedule was saved after `?saved=schedule`.
pub async fn page(
    State(control): State<Control>,
    signed: Signed,
    query: axum::extract::Query<Saved>,
) -> Result<Markup, WebError> {
    require_admin(&signed)?;
    let schedule = control.get_instance_backup_schedule(&signed.principal).await?;
    let notice = if query.saved.as_deref() == Some("schedule") { Notice::Saved } else { Notice::None };
    Ok(render(&control, &signed, schedule.as_ref(), &ScheduleForm::of(schedule.as_ref()), notice, None).await?)
}

async fn render(
    control: &Control,
    signed: &Signed,
    schedule: Option<&InstanceBackupSchedule>,
    form: &ScheduleForm,
    notice: Notice<'_>,
    alert: Option<&str>,
) -> Result<Markup, ControlError> {
    let destinations = control.list_storage_destinations(&signed.principal).await?;
    let code = "rounded bg-zinc-100 px-1 font-mono text-xs dark:bg-zinc-800";
    let content = html! {
        @if let Some(alert) = alert { (layout::alert(alert)) }
        (tabs(signed, SettingsTab::Backups))
        div class="space-y-8" {
            p class="max-w-2xl text-sm text-zinc-600 dark:text-zinc-300" {
                "Backups of Spinikus itself: its database (projects, services, settings, users and encrypted secrets) and its master key, "
                "in one file encrypted with a passphrase. Restore one with " span class=(code) { "spinikus admin restore <file>" }
                " on a server with Spinikus installed. Databases' data is backed up on their own Backups tab, and volumes aren't backed up yet."
            }
            @if destinations.is_empty() && schedule.is_none() {
                (layout::empty("Backups go to a storage destination. Add one first."))
                p class="text-sm" { a href="/settings/storage" class=(LINK) { "Add a storage destination" } }
            } @else {
                div class="max-w-2xl" {
                    (forms::card(signed, "Schedule", Some("Where backups go, when, and how many to keep."), &format!("{URL}/schedule"), notice,
                        form.fields(&destinations, schedule.is_some()), "Save schedule"))
                }
                @if let Some(schedule) = schedule {
                    div class="flex flex-wrap items-center gap-4 text-sm" {
                        form method="post" action=(URL) {
                            (csrf_field(signed))
                            button type="submit" class=(BUTTON_PRIMARY) { "Back up now" }
                        }
                        span class="text-zinc-500 dark:text-zinc-400" {
                            @match schedule.next_run_at {
                                Some(next) => { "Next scheduled backup " (layout::time(next)) },
                                None => "Scheduled backups are off.",
                            }
                        }
                    }
                }
                (backups(control, signed).await?)
            }
        }
    };
    Ok(layout::page(signed, "Backups · Settings", &[], content))
}

async fn backups(control: &Control, signed: &Signed) -> Result<Markup, ControlError> {
    let backups = control.list_instance_backups(&signed.principal, None, SHOWN).await?.items;
    let busy = backups.iter().any(|backup| !backup.status.is_finished());
    let url = format!("{URL}/activity");
    Ok(html! {
        section id="instance-backups" hx-get=[busy.then_some(&url)] hx-trigger=[busy.then_some("every 2s")] hx-swap="outerHTML" {
            h2 class="mb-3 font-semibold" { "Backups" }
            @if backups.is_empty() {
                (layout::empty("No backups yet."))
            } @else {
                ul class={ (CARD) " divide-y divide-zinc-200 dark:divide-zinc-800" } {
                    @for backup in &backups { (backup_row(signed, backup)) }
                }
            }
        }
    })
}

fn backup_row(signed: &Signed, backup: &InstanceBackup) -> Markup {
    html! {
        li class="flex flex-wrap items-center gap-x-4 gap-y-2 px-5 py-3 text-sm" {
            span class="w-24" { (status_badge(backup.status)) }
            span { (layout::time(backup.created_at)) }
            span class="text-zinc-500 dark:text-zinc-400" {
                @if backup.scheduled { "scheduled" } @else { "by hand" }
                @if let Some(size) = backup.size_bytes { " · " (human_size(size)) }
            }
            @if backup.status == BackupStatus::Succeeded {
                span class="min-w-0 font-mono text-xs break-all text-zinc-500 dark:text-zinc-400" { (backup.object_key) }
            }
            @if backup.status.is_finished() {
                form method="post" action={ (URL) "/" (backup.id) "/delete" } class="ml-auto"
                    hx-confirm="Delete this backup? Its file is deleted from the storage too." {
                    (csrf_field(signed))
                    button type="submit" class={ (BUTTON_SECONDARY) " py-1 text-red-600 dark:text-red-400" } { "Delete" }
                }
            }
            @if let Some(error) = &backup.error {
                p class="w-full text-xs break-words text-red-700 dark:text-red-400" { (error) }
            }
        }
    }
}
