//! Projects, environments, services, domains and volumes.

use serde_json::{Map, Value, json};
use spinikus_core::domain::Hostname;

use super::{
    Session,
    args::{DomainCommand, EnvironmentCommand, ProjectCommand, ServiceCommand, ServiceCreateArgs, VolumeCommand},
    confirm,
    error::{Exit, fail},
    output::{Style, ago, status_style},
    views::{self, Deployment, Environment, Project, Service, ServiceDomains, Volume},
};

pub async fn project(session: &Session, command: ProjectCommand) -> anyhow::Result<()> {
    let (api, out) = (&session.api, &session.out);
    match command {
        ProjectCommand::List => {
            let projects: Vec<Value> = api.list("/projects").await?;
            if out.json() {
                return out.print_json(&projects);
            }
            let rows = projects
                .iter()
                .map(|value| views::read::<Project>(value).map(|project| vec![project.slug, project.name, project.id]))
                .collect::<Result<Vec<_>, _>>()?;
            out.table(&["SLUG", "NAME", "ID"], &rows)
        }
        ProjectCommand::Create { name, slug } => {
            let created: Value = api.post("/projects", &json!({ "name": name, "slug": slug })).await?;
            if out.json() {
                return out.print_json(&created);
            }
            let project: Project = views::read(&created)?;
            out.done(&format!("Created project {} ({}, {}).", project.name, project.slug, project.id))
        }
        ProjectCommand::Show { project } => {
            let (value, project) = session.project_value(project.as_deref()).await?;
            if out.json() {
                return out.print_json(&value);
            }
            let environments: Vec<Value> = api.list(&format!("/projects/{}/environments", project.id)).await?;
            out.done(&format!("{} {}", out.paint(&project.name, Style::Bold), out.paint(&project.id, Style::Dim)))?;
            out.done("")?;
            environment_table(session, &environments)
        }
        ProjectCommand::Delete { project, yes } => {
            let project = session.project(Some(&project)).await?;
            confirm(yes, &format!("Delete project {} ({})?", project.name, project.slug))?;
            api.delete(&format!("/projects/{}", project.id)).await?;
            out.done(&format!("Deleted project {}.", project.slug))
        }
    }
}

fn environment_table(session: &Session, environments: &[Value]) -> anyhow::Result<()> {
    let rows = environments
        .iter()
        .map(|value| {
            views::read::<Environment>(value).map(|environment| {
                vec![environment.slug.clone(), environment.name.clone(), environment.described_kind(), environment.id]
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    session.out.table(&["SLUG", "NAME", "KIND", "ID"], &rows)
}

pub async fn environment(session: &Session, command: EnvironmentCommand) -> anyhow::Result<()> {
    let (api, out) = (&session.api, &session.out);
    match command {
        EnvironmentCommand::List { project } => {
            let project = session.project(project.as_deref()).await?;
            let environments: Vec<Value> = api.list(&format!("/projects/{}/environments", project.id)).await?;
            if out.json() {
                return out.print_json(&environments);
            }
            environment_table(session, &environments)
        }
        EnvironmentCommand::Create { name, project, slug, kind, previews } => {
            let project = session.project(project.as_deref()).await?;
            let body = json!({ "name": name, "slug": slug, "kind": kind, "previews": previews });
            let created: Value = api.post(&format!("/projects/{}/environments", project.id), &body).await?;
            if out.json() {
                return out.print_json(&created);
            }
            let environment: Environment = views::read(&created)?;
            out.done(&format!("Created environment {}/{} ({}).", project.slug, environment.slug, environment.id))
        }
        EnvironmentCommand::Show { environment } => {
            let (value, environment) = session.environment_value(environment.as_deref()).await?;
            if out.json() {
                return out.print_json(&value);
            }
            let path = session.environment_path(&environment).await?;
            let services: Vec<Value> = api.list(&format!("/environments/{}/services", environment.id)).await?;
            let title = format!("{} ({}, {})", out.paint(&path, Style::Bold), environment.described_kind(), environment.id);
            out.done(&title)?;
            out.done("")?;
            service_table(session, &services)
        }
        EnvironmentCommand::Previews { state, environment } => {
            let environment = session.environment(environment.as_deref()).await?;
            let path = session.environment_path(&environment).await?;
            let body = json!({ "previews": state == "on" });
            let updated: Value = api.patch(&format!("/environments/{}", environment.id), environment.revision, &body).await?;
            if out.json() {
                return out.print_json(&updated);
            }
            out.done(&format!("Previews of {path} are {state}."))
        }
        EnvironmentCommand::Delete { environment, yes } => {
            let environment = session.environment(Some(&environment)).await?;
            let path = session.environment_path(&environment).await?;
            confirm(yes, &format!("Delete environment {path}?"))?;
            api.delete(&format!("/environments/{}", environment.id)).await?;
            out.done(&format!("Deleted environment {path}."))
        }
    }
}

fn service_table(session: &Session, services: &[Value]) -> anyhow::Result<()> {
    let rows = services
        .iter()
        .map(|value| views::read::<Service>(value).map(|service| vec![service.slug, service.name, service.kind, service.id]))
        .collect::<Result<Vec<_>, _>>()?;
    session.out.table(&["SLUG", "NAME", "KIND", "ID"], &rows)
}

pub async fn service(session: &Session, command: ServiceCommand) -> anyhow::Result<()> {
    let (api, out) = (&session.api, &session.out);
    match command {
        ServiceCommand::List { environment } => {
            let environment = session.environment(environment.as_deref()).await?;
            let services: Vec<Value> = api.list(&format!("/environments/{}/services", environment.id)).await?;
            if out.json() {
                return out.print_json(&services);
            }
            service_table(session, &services)
        }
        ServiceCommand::Create(args) => create_service(session, args).await,
        ServiceCommand::Show { service } => show_service(session, service.as_deref()).await,
        ServiceCommand::Delete { service, yes } => {
            let service = session.service(Some(&service)).await?;
            let path = session.service_path(&service).await?;
            confirm(yes, &format!("Delete {path}? Its deployments are stopped and removed."))?;
            api.delete(&format!("/services/{}", service.id)).await?;
            out.done(&format!("Deleted {path}."))
        }
    }
}

/// Creates the service, then its source; if the source is refused, the service goes again,
/// as on the dashboard, so a typo doesn't leave a service with nothing to deploy.
async fn create_service(session: &Session, args: ServiceCreateArgs) -> anyhow::Result<()> {
    let (api, out) = (&session.api, &session.out);
    let environment = session.environment(args.environment.as_deref()).await?;
    let mut config = Map::new();
    if let Some(port) = args.port {
        config.insert("runtime".to_string(), json!({ "port": port }));
    }
    let body = json!({ "name": args.name, "slug": args.slug, "kind": "app", "config": config, "server_id": args.server_id });
    let created: Value = api.post(&format!("/environments/{}/services", environment.id), &body).await?;
    let service: Service = views::read(&created)?;
    let source = match (&args.image, &args.git) {
        (Some(image), _) => json!({ "type": "image", "image": image }),
        (None, Some(repo_url)) => {
            json!({ "type": "git", "repo_url": repo_url, "branch": args.branch, "root_directory": args.root_dir })
        }
        (None, None) => unreachable!("clap requires one"),
    };
    if let Err(error) = api.post::<Value>(&format!("/services/{}/source", service.id), &source).await {
        let _ = api.delete(&format!("/services/{}", service.id)).await;
        return Err(error);
    }
    if out.json() {
        return out.print_json(&created);
    }
    let path = format!("{}/{}", session.environment_path(&environment).await?, service.slug);
    out.done(&format!("Created {path} ({}).", service.id))?;
    if args.git.as_deref().is_some_and(is_ssh) {
        let key: Value = api.get(&format!("/services/{}/source/deploy-key", service.id)).await?;
        out.done("Add this deploy key (read-only) to the repository so Spinikus can clone it:")?;
        out.done(&format!("  {}", key["public_key"].as_str().unwrap_or_default()))?;
    }
    out.done(&format!("Deploy it with `spinikus deploy {path}`."))
}

/// Whether a Git URL is cloned over SSH (`ssh://…` or `git@host:path`).
fn is_ssh(url: &str) -> bool {
    url.starts_with("ssh://") || (!url.contains("://") && url.contains('@') && url.contains(':'))
}

async fn show_service(session: &Session, name: Option<&str>) -> anyhow::Result<()> {
    let (api, out) = (&session.api, &session.out);
    let (value, service) = session.service_value(name).await?;
    if out.json() {
        return out.print_json(&value);
    }
    let path = session.service_path(&service).await?;
    let mut lines = vec![format!("{} ({}, {})", out.paint(&path, Style::Bold), service.kind, service.id)];
    let source: Option<Value> = match api.get(&format!("/services/{}/source", service.id)).await {
        Ok(source) => Some(source),
        Err(error) if super::exit_status(&error) == Exit::NotFound as u8 => None,
        Err(error) => return Err(error),
    };
    let source = match source {
        Some(source) if source["type"] == "git" => {
            let root = source["root_directory"].as_str().map(|root| format!(" in {root}")).unwrap_or_default();
            format!(
                "{} ({}{root})",
                source["repo_url"].as_str().unwrap_or_default(),
                source["branch"].as_str().unwrap_or_default()
            )
        }
        Some(source) => source["image"].as_str().unwrap_or_default().to_string(),
        None => out.paint("none yet", Style::Dim),
    };
    lines.push(format!("Source:   {source}"));
    lines.push(format!("Server:   {}", service.server_id.as_deref().unwrap_or("the control plane's")));
    let deployments: Vec<Value> =
        api.get_with::<Page>(&format!("/services/{}/deployments", service.id), &[("limit", "10".into())]).await?.items;
    let deployments: Vec<Deployment> = deployments.iter().map(views::read).collect::<Result<_, _>>()?;
    let running = match deployments.iter().find(|deployment| deployment.status == "live") {
        Some(live) => format!("{} {} ({}, {})", out.paint("live", Style::Green), live.id, live.what(), ago(&live.created_at)),
        None => out.paint("nothing live", Style::Dim),
    };
    lines.push(format!("Running:  {running}"));
    if let Some(latest) = deployments.first().filter(|latest| latest.status != "live") {
        let status = out.paint(&latest.status, status_style(&latest.status));
        lines.push(format!("Latest:   {status} {} ({})", latest.id, ago(&latest.created_at)));
    }
    if service.kind == "app" {
        let domains: ServiceDomains = views::read(&api.get::<Value>(&format!("/services/{}/domains", service.id)).await?)?;
        if let Some(url) = domains.url() {
            lines.push(format!("URL:      {url}"));
        }
    }
    for line in lines {
        out.done(&line)?;
    }
    Ok(())
}

#[derive(serde::Deserialize)]
pub struct Page {
    pub items: Vec<Value>,
}

pub async fn domain(session: &Session, command: DomainCommand) -> anyhow::Result<()> {
    let (api, out) = (&session.api, &session.out);
    match command {
        DomainCommand::List { service } => {
            let service = session.service(service.as_deref()).await?;
            let value: Value = api.get(&format!("/services/{}/domains", service.id)).await?;
            if out.json() {
                return out.print_json(&value);
            }
            let domains: ServiceDomains = views::read(&value)?;
            let https = |hostname: &str| match domains.certificates.iter().find(|c| c.hostname == hostname) {
                Some(certificate) if certificate.certificate_id.is_some() => out.paint("yes", Style::Green),
                Some(certificate) if certificate.last_error.is_some() => out.paint("failing", Style::Red),
                // Names such as `*.localhost` can't get a certificate from a public CA.
                _ if !hostname.parse::<Hostname>().is_ok_and(|hostname| hostname.is_public()) => {
                    out.paint("HTTP only", Style::Dim)
                }
                _ => out.paint("not yet", Style::Dim),
            };
            let mut rows = Vec::new();
            if let Some(generated) = &domains.generated_hostname {
                rows.push(vec![generated.clone(), out.paint("generated", Style::Dim), https(generated), String::new()]);
            }
            for domain in &domains.domains {
                let serves = match &domain.redirect_to {
                    Some(target) => {
                        let other = domains.domains.iter().find(|other| &other.id == target);
                        format!("redirects to {}", other.map_or_else(|| target.clone(), |other| other.hostname.clone()))
                    }
                    None => "the app".to_string(),
                };
                rows.push(vec![domain.hostname.clone(), serves, https(&domain.hostname), domain.id.clone()]);
            }
            out.table(&["HOSTNAME", "SERVES", "HTTPS", "ID"], &rows)
        }
        DomainCommand::Add { hostname, redirect_to, service } => {
            let service = session.service(service.as_deref()).await?;
            let redirect_to = match redirect_to {
                Some(target) => Some(domain_id(session, &service, &target).await?),
                None => None,
            };
            let body = json!({ "hostname": hostname, "redirect_to": redirect_to });
            let created: Value = api.post(&format!("/services/{}/domains", service.id), &body).await?;
            if out.json() {
                return out.print_json(&created);
            }
            out.done(&format!("Added {hostname}. Point its DNS at the server; HTTPS follows once it resolves."))
        }
        DomainCommand::Remove { hostname, service } => {
            let service = session.service(service.as_deref()).await?;
            let id = domain_id(session, &service, &hostname).await?;
            api.delete(&format!("/domains/{id}")).await?;
            out.done(&format!("Removed {hostname}."))
        }
    }
}

pub async fn volume(session: &Session, command: VolumeCommand) -> anyhow::Result<()> {
    let (api, out) = (&session.api, &session.out);
    match command {
        VolumeCommand::List { service } => {
            let service = session.service(service.as_deref()).await?;
            let value: Value = api.get(&format!("/services/{}/volumes", service.id)).await?;
            if out.json() {
                return out.print_json(&value);
            }
            let volumes: Vec<Volume> = views::read(&value)?;
            let rows: Vec<_> = volumes.into_iter().map(|volume| vec![volume.name, volume.mount_path, volume.id]).collect();
            out.table(&["NAME", "MOUNT PATH", "ID"], &rows)
        }
        VolumeCommand::Add { name, mount_path, service } => {
            let service = session.service(service.as_deref()).await?;
            let body = json!({ "name": name, "mount_path": mount_path });
            let created: Value = api.post(&format!("/services/{}/volumes", service.id), &body).await?;
            if out.json() {
                return out.print_json(&created);
            }
            let volume: Volume = views::read(&created)?;
            out.done(&format!("Added {} at {}; deploy {} to mount it.", volume.name, volume.mount_path, service.slug))
        }
        VolumeCommand::Update { volume, name, mount_path, service } => {
            let service = session.service(service.as_deref()).await?;
            let current = find_volume(session, &service, &volume).await?;
            let body = json!({ "name": name, "mount_path": mount_path });
            let updated: Value = api.patch(&format!("/volumes/{}", current.id), current.revision, &body).await?;
            if out.json() {
                return out.print_json(&updated);
            }
            let volume: Volume = views::read(&updated)?;
            out.done(&format!("Updated {} at {}; deploy {} to apply it.", volume.name, volume.mount_path, service.slug))
        }
        VolumeCommand::Remove { volume, service } => {
            let service = session.service(service.as_deref()).await?;
            let current = find_volume(session, &service, &volume).await?;
            api.delete(&format!("/volumes/{}", current.id)).await?;
            out.done(&format!(
                "Removed {}; its data stays on the server. Deploy {} to stop mounting it.",
                current.name, service.slug
            ))
        }
    }
}

/// The service's volume with this name (or ID).
async fn find_volume(session: &Session, service: &Service, volume: &str) -> anyhow::Result<Volume> {
    let volumes: Vec<Volume> = views::read(&session.api.get::<Value>(&format!("/services/{}/volumes", service.id)).await?)?;
    volumes
        .into_iter()
        .find(|candidate| candidate.name == volume || candidate.id == volume)
        .ok_or_else(|| fail(Exit::NotFound, format!("{} has no volume {volume}", service.slug)))
}

/// The ID of the service's domain with this hostname (or ID).
async fn domain_id(session: &Session, service: &Service, hostname: &str) -> anyhow::Result<String> {
    let domains: ServiceDomains = views::read(&session.api.get::<Value>(&format!("/services/{}/domains", service.id)).await?)?;
    domains
        .domains
        .iter()
        .find(|domain| domain.hostname.eq_ignore_ascii_case(hostname) || domain.id == hostname)
        .map(|domain| domain.id.clone())
        .ok_or_else(|| fail(Exit::NotFound, format!("{} has no domain {hostname}", service.slug)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_urls_are_recognized() {
        assert!(is_ssh("git@github.com:acme/web.git"));
        assert!(is_ssh("ssh://git@example.com/acme/web.git"));
        assert!(!is_ssh("https://github.com/acme/web.git"));
        assert!(!is_ssh("https://user@github.com/acme/web.git"));
    }
}
