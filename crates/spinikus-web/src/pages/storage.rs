//! Storage destinations for backups: S3-compatible buckets, managed by instance admins in
//! the instance settings. Secret access keys are write-only: never shown, kept when left empty.

use axum::{
    Form,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use serde::Deserialize;
use spinikus_control::{Control, ControlError, NewStorageDestination, StorageDestinationChanges};
use spinikus_core::{backup::StorageDestination, secret::Secret};

use super::{parse_id, problem, problem_message, redirect};
use crate::{
    auth::Signed,
    error::WebError,
    forms::{self, Input, Notice},
    layout::{self, BUTTON_SECONDARY, CARD, LINK, csrf_field},
};

const URL: &str = "/settings/storage";

/// The instance settings' tabs.
pub(super) fn settings_tabs() -> Markup {
    layout::tabs(&[("Storage", URL.to_string(), true)])
}

fn require_admin(signed: &Signed) -> Result<(), WebError> {
    match signed.principal.is_instance_admin() {
        true => Ok(()),
        false => Err(ControlError::Forbidden.into()),
    }
}

#[derive(Deserialize, Default)]
pub struct DestinationForm {
    #[serde(default)]
    revision: Option<i64>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    endpoint: String,
    #[serde(default)]
    region: String,
    #[serde(default)]
    bucket: String,
    #[serde(default)]
    path_prefix: String,
    /// `on` when checked.
    #[serde(default)]
    path_style: Option<String>,
    #[serde(default)]
    access_key_id: String,
    #[serde(default)]
    secret_access_key: String,
}

fn given(value: &str) -> Option<String> {
    Some(value.trim().to_string()).filter(|value| !value.is_empty())
}

impl DestinationForm {
    fn of(destination: &StorageDestination) -> Self {
        let fields = &destination.fields;
        Self {
            revision: Some(destination.revision),
            name: fields.name.clone(),
            endpoint: fields.endpoint.clone().unwrap_or_default(),
            region: fields.region.clone(),
            bucket: fields.bucket.clone(),
            path_prefix: fields.path_prefix.clone(),
            path_style: fields.path_style.then(|| "on".to_string()),
            access_key_id: fields.access_key_id.clone(),
            secret_access_key: String::new(),
        }
    }

    fn new_destination(&self) -> NewStorageDestination {
        NewStorageDestination {
            name: self.name.trim().to_string(),
            endpoint: given(&self.endpoint),
            region: given(&self.region),
            bucket: self.bucket.trim().to_string(),
            path_prefix: given(&self.path_prefix),
            path_style: self.path_style.is_some(),
            access_key_id: self.access_key_id.trim().to_string(),
            secret_access_key: Secret::new(self.secret_access_key.trim()),
        }
    }

    fn changes(&self) -> StorageDestinationChanges {
        StorageDestinationChanges {
            name: Some(self.name.trim().to_string()),
            endpoint: Some(given(&self.endpoint)),
            region: given(&self.region),
            bucket: Some(self.bucket.trim().to_string()),
            path_prefix: Some(self.path_prefix.trim().to_string()),
            path_style: Some(self.path_style.is_some()),
            access_key_id: Some(self.access_key_id.trim().to_string()),
            secret_access_key: given(&self.secret_access_key).map(Secret::new),
        }
    }

    /// The fields; `editing` keeps the secret access key when it is left empty.
    fn fields(&self, editing: bool) -> Markup {
        let secret_hint = match editing {
            true => "Leave it empty to keep the current one.",
            false => "Kept encrypted, and never shown again.",
        };
        html! {
            (Input::text("storage-name", "name", "Name", &self.name).required().placeholder("Offsite backups").render())
            (Input::text("storage-endpoint", "endpoint", "Endpoint", &self.endpoint).mono().placeholder("https://<account>.r2.cloudflarestorage.com")
                .hint("The S3 API's URL. Leave it empty for Amazon S3.").render())
            div class="grid gap-4 sm:grid-cols-2" {
                (Input::text("storage-bucket", "bucket", "Bucket", &self.bucket).required().mono().render())
                (Input::text("storage-region", "region", "Region", &self.region).mono().placeholder("us-east-1").render())
            }
            (Input::text("storage-prefix", "path_prefix", "Path prefix", &self.path_prefix).mono().placeholder("spinikus/")
                .hint("Backups are stored under it. The bucket's root if empty.").render())
            (forms::checkbox("storage-path-style", "path_style", "Path-style addressing", self.path_style.is_some(), false,
                Some("Puts the bucket in the path rather than the hostname, as MinIO and some other servers need.")))
            div class="grid gap-4 sm:grid-cols-2" {
                (Input::text("storage-key-id", "access_key_id", "Access key ID", &self.access_key_id).required().mono().render())
                (Input::text("storage-secret", "secret_access_key", "Secret access key", "").kind("password").mono()
                    .hint(secret_hint).render())
            }
            @if let Some(revision) = self.revision { input type="hidden" name="revision" value=(revision); }
        }
    }
}

/// What the list page says above its content.
enum Alert<'a> {
    None,
    Done(&'a str),
    Problem(&'a str),
}

pub async fn storage(State(control): State<Control>, signed: Signed) -> Result<Markup, WebError> {
    require_admin(&signed)?;
    Ok(list_page(&control, &signed, Alert::None, &DestinationForm::default(), Notice::None).await?)
}

pub async fn create(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Form(form): Form<DestinationForm>,
) -> Result<Response, WebError> {
    require_admin(&signed)?;
    match control.create_storage_destination(&signed.principal, form.new_destination()).await {
        Ok(_) => Ok(redirect(&headers, URL)),
        Err(error) => {
            let message = problem_message(error)?;
            let page = list_page(&control, &signed, Alert::None, &form, Notice::Problem(&message)).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
    }
}

/// Writes and deletes a test object, and says how that went.
pub async fn test(State(control): State<Control>, signed: Signed, Path(id): Path<String>) -> Result<Response, WebError> {
    require_admin(&signed)?;
    let destination = control.get_storage_destination(&signed.principal, parse_id(&id)?).await?;
    let (status, page) = match control.test_storage_destination(&signed.principal, destination.id).await {
        Ok(()) => {
            let done = format!("{}: wrote and deleted a test object.", destination.fields.name);
            (StatusCode::OK, list_page(&control, &signed, Alert::Done(&done), &DestinationForm::default(), Notice::None).await?)
        }
        Err(error) => {
            let message = format!("{}: {}", destination.fields.name, problem_message(error)?);
            let page = list_page(&control, &signed, Alert::Problem(&message), &DestinationForm::default(), Notice::None).await?;
            (StatusCode::UNPROCESSABLE_ENTITY, page)
        }
    };
    // The page is the list's, whatever URL the button posted to.
    Ok((status, [("hx-replace-url", URL)], page).into_response())
}

pub async fn delete(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, WebError> {
    require_admin(&signed)?;
    match control.delete_storage_destination(&signed.principal, parse_id(&id)?).await {
        Ok(()) => Ok(redirect(&headers, URL)),
        Err(ControlError::HasChildren { .. }) => {
            let message = "A backup schedule backs up to it. Delete or change the schedule first.";
            let page = list_page(&control, &signed, Alert::Problem(message), &DestinationForm::default(), Notice::None).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
        Err(error) => {
            let message = problem_message(error)?;
            let page = list_page(&control, &signed, Alert::Problem(&message), &DestinationForm::default(), Notice::None).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
    }
}

pub async fn edit(State(control): State<Control>, signed: Signed, Path(id): Path<String>) -> Result<Markup, WebError> {
    require_admin(&signed)?;
    let destination = control.get_storage_destination(&signed.principal, parse_id(&id)?).await?;
    Ok(edit_page(&signed, &destination, &DestinationForm::of(&destination), Notice::None))
}

pub async fn update(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<DestinationForm>,
) -> Result<Response, WebError> {
    require_admin(&signed)?;
    let destination = control.get_storage_destination(&signed.principal, parse_id(&id)?).await?;
    match control.update_storage_destination(&signed.principal, destination.id, form.revision.unwrap_or(-1), form.changes()).await
    {
        Ok(_) => Ok(redirect(&headers, URL)),
        Err(error) => problem(error, |message| edit_page(&signed, &destination, &form, Notice::Problem(message))),
    }
}

async fn list_page(
    control: &Control,
    signed: &Signed,
    alert: Alert<'_>,
    form: &DestinationForm,
    notice: Notice<'_>,
) -> Result<Markup, ControlError> {
    let destinations = control.list_storage_destinations(&signed.principal).await?;
    let content = html! {
        @match alert {
            Alert::None => {},
            Alert::Done(message) => (layout::notice(message)),
            Alert::Problem(message) => (layout::alert(message)),
        }
        (layout::heading("Settings", None))
        (settings_tabs())
        div class="space-y-8" {
            section {
                h2 class="mb-1 font-semibold" { "Storage destinations" }
                p class="mb-3 text-sm text-zinc-500 dark:text-zinc-400" {
                    "S3-compatible buckets that database backups go to: Amazon S3, Cloudflare R2, Backblaze B2, MinIO and others."
                }
                @if destinations.is_empty() {
                    (layout::empty("No storage destinations yet."))
                } @else {
                    ul class={ (CARD) " divide-y divide-zinc-200 dark:divide-zinc-800" } {
                        @for destination in &destinations { (destination_row(signed, destination)) }
                    }
                }
            }
            div class="max-w-2xl" {
                (forms::card(signed, "Add a storage destination", None, URL, notice, form.fields(false), "Add destination"))
            }
        }
    };
    Ok(layout::page(signed, "Storage · Settings", &[], content))
}

fn destination_row(signed: &Signed, destination: &StorageDestination) -> Markup {
    let fields = &destination.fields;
    let base = format!("{URL}/{}", destination.id);
    html! {
        li class="flex flex-wrap items-center gap-x-4 gap-y-2 px-5 py-3 text-sm" {
            div class="min-w-0" {
                p class="font-medium" { (fields.name) }
                p class="font-mono text-xs break-all text-zinc-500 dark:text-zinc-400" {
                    (fields.endpoint.as_deref().unwrap_or("Amazon S3")) " · " (fields.bucket) "/" (fields.path_prefix) " · " (fields.region)
                }
            }
            div class="ml-auto flex gap-2" {
                form method="post" action={ (base) "/test" } {
                    (csrf_field(signed))
                    button type="submit" class={ (BUTTON_SECONDARY) " py-1" } { "Test" }
                }
                a href=(base) class={ (BUTTON_SECONDARY) " py-1" } { "Edit" }
                form method="post" action={ (base) "/delete" } hx-confirm="Delete this destination? Backups already in it stay in the bucket." {
                    (csrf_field(signed))
                    button type="submit" class={ (BUTTON_SECONDARY) " py-1 text-red-600 dark:text-red-400" } { "Delete" }
                }
            }
        }
    }
}

fn edit_page(signed: &Signed, destination: &StorageDestination, form: &DestinationForm, notice: Notice<'_>) -> Markup {
    let action = format!("{URL}/{}", destination.id);
    let content = html! {
        (layout::heading("Settings", None))
        (settings_tabs())
        div class="max-w-2xl" {
            (forms::card(signed, &format!("Edit {}", destination.fields.name), None, &action, notice, form.fields(true), "Save"))
            div class="mt-4 text-sm" { a href=(URL) class=(LINK) { "Back to storage destinations" } }
        }
    };
    layout::page(signed, &format!("{} · Settings", destination.fields.name), &[], content)
}
