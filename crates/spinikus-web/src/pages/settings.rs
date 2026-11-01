//! The settings area: the instance's general settings, servers and storage destinations for
//! instance admins, and everyone's own API tokens.

use axum::{
    Form,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::{IntoResponse, Redirect, Response},
};
use maud::{Markup, html};
use serde::Deserialize;
use serde_json::{Value, json};
use spinikus_control::{
    Control, ControlError,
    settings::{ACME_DIRECTORY_URL, ACME_EMAIL, BASE_DOMAIN, DEFAULT_ACME_DIRECTORY_URL, MAX_RELEASES_KEPT, RELEASES_KEPT},
};
use spinikus_core::setting::Setting;

use super::{problem, redirect};
use crate::{
    auth::Signed,
    error::WebError,
    forms::{self, Input, Notice},
    layout,
};

/// The settings pages, for their tabs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingsTab {
    General,
    Servers,
    Storage,
    Users,
    AuditLog,
    Account,
    Tokens,
}

/// The settings' tabs: the instance's for instance admins, the account and API tokens for everyone.
pub(super) fn tabs(signed: &Signed, current: SettingsTab) -> Markup {
    let mut tabs = Vec::new();
    if signed.principal.is_instance_admin() {
        tabs.push(("General", "/settings/general".to_string(), current == SettingsTab::General));
        tabs.push(("Servers", "/settings/servers".to_string(), current == SettingsTab::Servers));
        tabs.push(("Storage", "/settings/storage".to_string(), current == SettingsTab::Storage));
        tabs.push(("Users", "/settings/users".to_string(), current == SettingsTab::Users));
        tabs.push(("Audit log", "/settings/audit-log".to_string(), current == SettingsTab::AuditLog));
    }
    tabs.push(("Account", "/settings/account".to_string(), current == SettingsTab::Account));
    tabs.push(("API tokens", "/settings/tokens".to_string(), current == SettingsTab::Tokens));
    html! {
        (layout::heading("Settings", None))
        (layout::tabs(&tabs))
    }
}

pub(super) fn require_admin(signed: &Signed) -> Result<(), WebError> {
    match signed.principal.is_instance_admin() {
        true => Ok(()),
        false => Err(ControlError::Forbidden.into()),
    }
}

/// The first settings page the user may see.
pub async fn index(signed: Signed) -> Response {
    match signed.principal.is_instance_admin() {
        true => Redirect::to("/settings/general").into_response(),
        false => Redirect::to("/settings/account").into_response(),
    }
}

/// The general settings, as typed. Each section saves only its own settings, each at the
/// revision the page showed.
#[derive(Deserialize, Default, Clone)]
pub struct GeneralForm {
    #[serde(default)]
    base_domain: String,
    #[serde(default)]
    base_domain_revision: i64,
    #[serde(default)]
    acme_email: String,
    #[serde(default)]
    acme_email_revision: i64,
    #[serde(default)]
    acme_directory_url: String,
    #[serde(default)]
    acme_directory_url_revision: i64,
    #[serde(default)]
    releases_kept: String,
    #[serde(default)]
    releases_kept_revision: i64,
}

/// A setting's value as a form field: empty for `null`.
fn text(setting: &Setting) -> String {
    match &setting.value {
        Value::Null => String::new(),
        Value::String(value) => value.clone(),
        value => value.to_string(),
    }
}

/// `null` for an empty field.
fn or_null(value: &str) -> Value {
    match value.trim() {
        "" => Value::Null,
        value => json!(value),
    }
}

impl GeneralForm {
    async fn load(control: &Control, signed: &Signed) -> Result<Self, ControlError> {
        let get = |key| control.get_setting(&signed.principal, key);
        let (base_domain, acme_email, acme_directory_url, releases_kept) =
            (get(BASE_DOMAIN).await?, get(ACME_EMAIL).await?, get(ACME_DIRECTORY_URL).await?, get(RELEASES_KEPT).await?);
        Ok(Self {
            base_domain: text(&base_domain),
            base_domain_revision: base_domain.revision,
            acme_email: text(&acme_email),
            acme_email_revision: acme_email.revision,
            acme_directory_url: text(&acme_directory_url),
            acme_directory_url_revision: acme_directory_url.revision,
            releases_kept: text(&releases_kept),
            releases_kept_revision: releases_kept.revision,
        })
    }

    /// The settings a section saves: (key, revision, value).
    fn section(&self, section: Section) -> Result<Vec<(&'static str, i64, Value)>, ControlError> {
        Ok(match section {
            Section::Domains => vec![(BASE_DOMAIN, self.base_domain_revision, or_null(&self.base_domain))],
            Section::Https => {
                let directory = match self.acme_directory_url.trim() {
                    "" => json!(DEFAULT_ACME_DIRECTORY_URL),
                    url => json!(url),
                };
                vec![
                    (ACME_DIRECTORY_URL, self.acme_directory_url_revision, directory),
                    (ACME_EMAIL, self.acme_email_revision, or_null(&self.acme_email)),
                ]
            }
            Section::Releases => {
                let count = self.releases_kept.trim().parse::<u64>().map_err(|_| ControlError::Invalid {
                    field: "value",
                    problem: format!("give a whole number from 0 to {MAX_RELEASES_KEPT}"),
                })?;
                vec![(RELEASES_KEPT, self.releases_kept_revision, json!(count))]
            }
        })
    }

    fn domains_fields(&self) -> Markup {
        html! {
            (Input::text("settings-base-domain", "base_domain", "Base domain", &self.base_domain).mono().placeholder("apps.example.com")
                .hint("Apps get a hostname under it, such as web-production-shop.apps.example.com: point a wildcard DNS record at your servers. Leave it empty to turn these hostnames off.").render())
            input type="hidden" name="base_domain_revision" value=(self.base_domain_revision);
        }
    }

    fn https_fields(&self) -> Markup {
        html! {
            (Input::text("settings-acme-email", "acme_email", "Email", &self.acme_email).kind("email").placeholder("ops@example.com")
                .hint("For the certificate authority's account and its expiry notices. Leave it empty to turn HTTPS certificates off.").render())
            (Input::text("settings-acme-directory", "acme_directory_url", "ACME directory", &self.acme_directory_url).mono().placeholder(DEFAULT_ACME_DIRECTORY_URL)
                .hint("Let's Encrypt if empty.").render())
            input type="hidden" name="acme_email_revision" value=(self.acme_email_revision);
            input type="hidden" name="acme_directory_url_revision" value=(self.acme_directory_url_revision);
        }
    }

    fn releases_fields(&self) -> Markup {
        html! {
            (Input::text("settings-releases-kept", "releases_kept", "Releases kept", &self.releases_kept).kind("number").mono()
                .hint("How many replaced deployments of each service keep their containers and images, to roll back to.").render())
            input type="hidden" name="releases_kept_revision" value=(self.releases_kept_revision);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Section {
    Domains,
    Https,
    Releases,
}

impl Section {
    fn as_str(self) -> &'static str {
        match self {
            Section::Domains => "domains",
            Section::Https => "https",
            Section::Releases => "releases",
        }
    }
}

#[derive(Deserialize)]
pub struct Saved {
    saved: Option<String>,
}

pub async fn general(
    State(control): State<Control>,
    signed: Signed,
    Query(Saved { saved }): Query<Saved>,
) -> Result<Markup, WebError> {
    require_admin(&signed)?;
    let form = GeneralForm::load(&control, &signed).await?;
    let notice = |section: Section| if saved.as_deref() == Some(section.as_str()) { Notice::Saved } else { Notice::None };
    Ok(general_page(&signed, &form, [notice(Section::Domains), notice(Section::Https), notice(Section::Releases)]))
}

pub async fn update_general(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(section): Path<String>,
    Form(form): Form<GeneralForm>,
) -> Result<Response, WebError> {
    require_admin(&signed)?;
    let section: Section = serde_json::from_value(json!(section)).map_err(|_| WebError::NoPage)?;
    let saved = async {
        for (key, revision, value) in form.section(section)? {
            control.set_setting(&signed.principal, key, revision, value).await?;
        }
        Ok::<_, ControlError>(())
    };
    match saved.await {
        Ok(()) => Ok(redirect(&headers, &format!("/settings/general?saved={}", section.as_str()))),
        Err(error) => {
            // The other sections show what is stored; this one what was typed.
            let mut shown = GeneralForm::load(&control, &signed).await?;
            match section {
                Section::Domains => shown.base_domain = form.base_domain.clone(),
                Section::Https => {
                    shown.acme_email = form.acme_email.clone();
                    shown.acme_directory_url = form.acme_directory_url.clone();
                }
                Section::Releases => shown.releases_kept = form.releases_kept.clone(),
            }
            problem(error, |message| {
                let notice = |of| if of == section { Notice::Problem(message) } else { Notice::None };
                general_page(&signed, &shown, [notice(Section::Domains), notice(Section::Https), notice(Section::Releases)])
            })
        }
    }
}

fn general_page(signed: &Signed, form: &GeneralForm, [domains, https, releases]: [Notice<'_>; 3]) -> Markup {
    let content = html! {
        (tabs(signed, SettingsTab::General))
        div class="max-w-2xl space-y-6" {
            (forms::card(signed, "Generated hostnames", None, "/settings/general/domains", domains, form.domains_fields(), "Save"))
            (forms::card(signed, "HTTPS certificates", Some("Certificates from an ACME certificate authority for every public hostname, renewed automatically."),
                "/settings/general/https", https, form.https_fields(), "Save"))
            (forms::card(signed, "Old releases", None, "/settings/general/releases", releases, form.releases_fields(), "Save"))
        }
    };
    layout::page(signed, "Settings", &[], content)
}
