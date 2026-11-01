//! An app service's domains: its generated hostname and the domains users add, each with
//! where its HTTPS certificate stands. Developers add domains, point them at another domain
//! of the service (a redirect) or back at the app, and remove them.

use axum::{
    Form,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use serde::Deserialize;
use spinikus_control::{Control, ControlError, DomainChanges, NewDomain, ServiceDomains};
use spinikus_core::{
    certificate::HostnameCertificate,
    domain::{Domain, Hostname},
    environment::Environment,
    id::{DomainId, ServiceId},
    membership::Role,
    project::Project,
    service::{Service, ServiceKind},
};

use super::{Tab, crumbs, parse_id, problem_message, redirect, service_tabs};
use crate::{
    auth::Signed,
    error::WebError,
    forms::{self, Input, Notice},
    layout::{self, BADGE, BUTTON_SECONDARY, CARD, LINK, csrf_field},
};

struct Context {
    service: Service,
    environment: Environment,
    project: Project,
    role: Role,
}

impl Context {
    async fn load(control: &Control, signed: &Signed, id: ServiceId) -> Result<Self, ControlError> {
        let principal = &signed.principal;
        let service = control.get_service(principal, id).await?;
        let environment = control.get_environment(principal, service.environment_id).await?;
        let role = control.environment_role(principal, &environment).await?;
        let project = control.get_project(principal, environment.project_id).await?;
        Ok(Self { service, environment, project, role })
    }

    fn url(&self) -> String {
        format!("/services/{}/domains", self.service.id)
    }

    fn can_edit(&self) -> bool {
        self.role >= Role::Developer
    }
}

/// What the add form and each domain's redirect form post. `redirect_to` is a domain ID, or
/// empty to serve the app.
#[derive(Deserialize, Default)]
pub struct DomainForm {
    #[serde(default)]
    revision: Option<i64>,
    #[serde(default)]
    hostname: String,
    #[serde(default)]
    redirect_to: String,
}

impl DomainForm {
    fn redirect_to(&self) -> Result<Option<DomainId>, ControlError> {
        match self.redirect_to.trim() {
            "" => Ok(None),
            id => id.parse().map(Some).map_err(|_| ControlError::Invalid {
                field: "redirect_to",
                problem: format!("there is no domain `{id}` to redirect to"),
            }),
        }
    }
}

pub async fn domains(State(control): State<Control>, signed: Signed, Path(id): Path<String>) -> Result<Markup, WebError> {
    let context = Context::load(&control, &signed, parse_id(&id)?).await?;
    Ok(render(&control, &signed, &context, &DomainForm::default(), Notice::None, None).await?)
}

pub async fn create(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<DomainForm>,
) -> Result<Response, WebError> {
    let context = Context::load(&control, &signed, parse_id(&id)?).await?;
    let created = match form.redirect_to() {
        Ok(redirect_to) => {
            let new = NewDomain { hostname: form.hostname.trim().to_ascii_lowercase(), redirect_to };
            control.create_domain(&signed.principal, context.service.id, new).await
        }
        Err(error) => Err(error),
    };
    match created {
        Ok(_) => Ok(redirect(&headers, &context.url())),
        Err(error) => {
            let message = problem_message(error)?;
            let page = render(&control, &signed, &context, &form, Notice::Problem(&message), None).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
    }
}

/// Changes where a domain goes: the app, or another domain of the service.
pub async fn update(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<DomainForm>,
) -> Result<Response, WebError> {
    let domain = control.get_domain(&signed.principal, parse_id(&id)?).await?;
    let context = Context::load(&control, &signed, domain.service_id).await?;
    let updated = match form.redirect_to() {
        Ok(redirect_to) => {
            let changes = DomainChanges { redirect_to: Some(redirect_to), ..Default::default() };
            control.update_domain(&signed.principal, domain.id, form.revision.unwrap_or(-1), changes).await
        }
        Err(error) => Err(error),
    };
    match updated {
        Ok(_) => Ok(redirect(&headers, &context.url())),
        Err(error) => {
            let message = problem_message(error)?;
            let page = render(&control, &signed, &context, &DomainForm::default(), Notice::None, Some(&message)).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
    }
}

pub async fn delete(
    State(control): State<Control>,
    signed: Signed,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, WebError> {
    let domain = control.get_domain(&signed.principal, parse_id(&id)?).await?;
    let context = Context::load(&control, &signed, domain.service_id).await?;
    match control.delete_domain(&signed.principal, domain.id).await {
        Ok(()) => Ok(redirect(&headers, &context.url())),
        Err(error) => {
            let message = problem_message(error)?;
            let page = render(&control, &signed, &context, &DomainForm::default(), Notice::None, Some(&message)).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
    }
}

async fn render(
    control: &Control,
    signed: &Signed,
    context: &Context,
    form: &DomainForm,
    notice: Notice<'_>,
    alert: Option<&str>,
) -> Result<Markup, ControlError> {
    let service = &context.service;
    let content = match service.kind {
        ServiceKind::App => {
            let domains = control.list_domains(&signed.principal, service.id).await?;
            app_domains(signed, context, &domains, form, notice)
        }
        _ => layout::empty("Only apps have domains. Other services in the environment reach this one by its slug."),
    };
    let content = html! {
        @if let Some(alert) = alert { (layout::alert(alert)) }
        (layout::heading(&service.name, Some(html! { span class="font-mono" { (service.slug) } })))
        (service_tabs(service, context.role, Tab::Domains))
        (content)
    };
    let crumbs = crumbs(&context.project, Some(&context.environment));
    Ok(layout::page(signed, &format!("Domains · {}", service.name), &crumbs, content))
}

fn app_domains(signed: &Signed, context: &Context, domains: &ServiceDomains, form: &DomainForm, notice: Notice<'_>) -> Markup {
    let certificate = |hostname: &Hostname| domains.certificates.iter().find(|record| &record.hostname == hostname);
    html! {
        div class="space-y-8" {
            section {
                h2 class="mb-1 font-semibold" { "Where the app is reachable" }
                p class="mb-3 text-sm text-zinc-500 dark:text-zinc-400" { "Traffic goes to the live deployment. Changes apply within seconds, without a deployment." }
                ul class={ (CARD) " divide-y divide-zinc-200 dark:divide-zinc-800" } {
                    @if let Some(hostname) = &domains.generated_hostname {
                        li class="flex flex-wrap items-center gap-x-4 gap-y-2 px-5 py-3 text-sm" {
                            (host_link(hostname, certificate(hostname)))
                            span class={ (BADGE) " bg-zinc-50 text-zinc-600 ring-zinc-500/20 dark:bg-zinc-800 dark:text-zinc-300 dark:ring-zinc-700" } { "generated" }
                            span class="ml-auto" { (certificate_status(hostname, certificate(hostname))) }
                            (certificate_failure(certificate(hostname)))
                        }
                    }
                    @for domain in &domains.domains {
                        li class="flex flex-wrap items-center gap-x-4 gap-y-2 px-5 py-3 text-sm" {
                            (host_link(&domain.hostname, certificate(&domain.hostname)))
                            @if let Some(target) = domain.redirect_to.and_then(|id| domains.domains.iter().find(|other| other.id == id)) {
                                span class="text-zinc-500 dark:text-zinc-400" { "redirects to " span class="font-mono" { (target.hostname) } }
                            }
                            span class="ml-auto" { (certificate_status(&domain.hostname, certificate(&domain.hostname))) }
                            (certificate_failure(certificate(&domain.hostname)))
                            @if context.can_edit() {
                                div class="flex flex-wrap items-center gap-2" {
                                    (redirect_form(signed, domain, &domains.domains))
                                    form method="post" action={ "/domains/" (domain.id) "/delete" }
                                        hx-confirm={ "Remove " (domain.hostname) "? It stops serving the app right away." } {
                                        (csrf_field(signed))
                                        button type="submit" class={ (BUTTON_SECONDARY) " py-1 text-red-600 dark:text-red-400" } { "Remove" }
                                    }
                                }
                            }
                        }
                    }
                    @if domains.generated_hostname.is_none() && domains.domains.is_empty() {
                        li class="px-5 py-6 text-center text-sm text-zinc-500 dark:text-zinc-400" { "No domains yet." }
                    }
                }
            }
            @if context.can_edit() {
                div class="max-w-2xl" {
                    (forms::card(signed, "Add a domain", Some("Point the domain's DNS (an A or AAAA record, or a CNAME) at the server the service runs on. A certificate is ordered once DNS answers, if an ACME email is set."),
                        &context.url(), notice, new_domain_fields(form, &domains.domains), "Add domain"))
                }
            }
        }
    }
}

fn new_domain_fields(form: &DomainForm, domains: &[Domain]) -> Markup {
    html! {
        (Input::text("domain-hostname", "hostname", "Hostname", &form.hostname).required().mono().placeholder("www.example.com").render())
        @if domains.iter().any(|domain| domain.redirect_to.is_none()) {
            (forms::select("domain-redirect", "redirect_to", "Serves", &targets(domains, None).iter().map(|(id, label)| (id.as_str(), label.as_str())).collect::<Vec<_>>(), &form.redirect_to))
        }
    }
}

/// What a domain can serve: the app, or a redirect to another domain of the service that
/// serves the app itself.
fn targets(domains: &[Domain], except: Option<DomainId>) -> Vec<(String, String)> {
    let mut targets = vec![(String::new(), "The app".to_string())];
    for domain in domains.iter().filter(|domain| domain.redirect_to.is_none() && Some(domain.id) != except) {
        targets.push((domain.id.to_string(), format!("A redirect to {}", domain.hostname)));
    }
    targets
}

fn redirect_form(signed: &Signed, domain: &Domain, domains: &[Domain]) -> Markup {
    let targets = targets(domains, Some(domain.id));
    let selected = domain.redirect_to.map(|id| id.to_string()).unwrap_or_default();
    let id = format!("serves-{}", domain.id);
    html! {
        @if targets.len() > 1 || domain.redirect_to.is_some() {
            form method="post" action={ "/domains/" (domain.id) } class="flex items-center gap-2" {
                (csrf_field(signed))
                input type="hidden" name="revision" value=(domain.revision);
                label for=(id) class="sr-only" { "What " (domain.hostname) " serves" }
                select id=(id) name="redirect_to" class={ (layout::INPUT) " w-auto py-1" } {
                    @for (value, label) in &targets { option value=(value) selected[*value == selected] { (label) } }
                }
                button type="submit" class={ (BUTTON_SECONDARY) " py-1" } { "Save" }
            }
        }
    }
}

/// The hostname, linked over HTTPS once it has a certificate.
fn host_link(hostname: &Hostname, certificate: Option<&HostnameCertificate>) -> Markup {
    let scheme = match certificate.is_some_and(|record| record.certificate_id.is_some()) {
        true => "https",
        false => "http",
    };
    html! {
        a href={ (scheme) "://" (hostname) } class={ (LINK) " font-mono" } rel="noreferrer" target="_blank" { (hostname) }
    }
}

/// Why getting a certificate failed, while it does.
fn certificate_failure(certificate: Option<&HostnameCertificate>) -> Markup {
    html! {
        @if let Some(record) = certificate.filter(|record| record.certificate_id.is_none() && record.failures > 0) {
            @if let Some(error) = &record.last_error {
                p class="w-full text-xs break-words text-red-700 dark:text-red-400" { (error) }
            }
        }
    }
}

fn certificate_status(hostname: &Hostname, certificate: Option<&HostnameCertificate>) -> Markup {
    const GREEN: &str = " bg-emerald-50 text-emerald-700 ring-emerald-600/20 dark:bg-emerald-500/10 dark:text-emerald-400 dark:ring-emerald-500/20";
    const RED: &str = " bg-red-50 text-red-700 ring-red-600/10 dark:bg-red-500/10 dark:text-red-400 dark:ring-red-500/20";
    const GRAY: &str = " bg-zinc-50 text-zinc-600 ring-zinc-500/20 dark:bg-zinc-800 dark:text-zinc-300 dark:ring-zinc-700";
    html! {
        @match certificate {
            Some(record) if record.certificate_id.is_some() => span class={ (BADGE) (GREEN) } { "HTTPS" },
            Some(record) if record.failures > 0 => {
                span class={ (BADGE) (RED) } {
                    "Certificate failing"
                    @if let Some(retry) = record.retry_at { " · retry " (retry.strftime("%H:%M UTC")) }
                }
            }
            _ if !hostname.is_public() => span class={ (BADGE) (GRAY) } { "HTTP only" },
            _ => span class={ (BADGE) (GRAY) } { "No certificate yet" },
        }
    }
}
