//! Deploying, rolling back, deployments and their logs, and a service's runtime logs.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    Session,
    args::{DeployArgs, DeploymentCommand, LogsArgs, RestartArgs, RollbackArgs},
    error::{Exit, fail},
    output::{Style, ago, status_style},
    resources::Page,
    views::{self, Deployment, DeploymentEvent, Service, ServiceDomains},
};

/// How often a broken deployment stream is reopened before giving up.
const RECONNECTS: u32 = 5;

pub async fn deploy(session: &Session, args: DeployArgs) -> anyhow::Result<()> {
    let service = session.service(args.service.as_deref()).await?;
    let path = session.service_path(&service).await?;
    let created: Value = session.api.post_empty(&format!("/services/{}/deployments", service.id)).await?;
    let deployment: Deployment = views::read(&created)?;
    session.out.info(&format!("Deploying {path} ({})", deployment.id));
    finish(session, &service, created, args.detach).await
}

pub async fn rollback(session: &Session, args: RollbackArgs) -> anyhow::Result<()> {
    let service = session.service(args.service.as_deref()).await?;
    let path = session.service_path(&service).await?;
    let body = json!({ "deployment_id": args.to });
    let created: Value = session.api.post(&format!("/services/{}/rollback", service.id), &body).await?;
    let deployment: Deployment = views::read(&created)?;
    let target = deployment.rollback_of.as_deref().unwrap_or("the previous deployment");
    session.out.info(&format!("Rolling {path} back to {target} ({})", deployment.id));
    finish(session, &service, created, args.detach).await
}

/// Restarts the service's running container. The control plane answers once the agent has
/// restarted it, so there is nothing to follow.
pub async fn restart(session: &Session, args: RestartArgs) -> anyhow::Result<()> {
    let service = session.service(args.service.as_deref()).await?;
    let path = session.service_path(&service).await?;
    session.out.info(&format!("Restarting {path}"));
    let live: Value = session.api.post_empty(&format!("/services/{}/restart", service.id)).await?;
    if session.out.json() {
        return session.out.print_json(&live);
    }
    let deployment: Deployment = views::read(&live)?;
    session.out.done(&format!("{} restarted ({}).", service.slug, deployment.id))
}

/// Prints the deployment right away with `detach`, else follows it to the end: its log on
/// stderr, then the result on stdout, failing with [`Exit::Failed`] if it didn't go live.
pub(super) async fn finish(session: &Session, service: &Service, created: Value, detach: bool) -> anyhow::Result<()> {
    let (api, out) = (&session.api, &session.out);
    let deployment: Deployment = views::read(&created)?;
    if detach {
        return if out.json() { out.print_json(&created) } else { out.line(&deployment.id) };
    }
    let last = follow(session, &deployment.id, true).await?;
    let value: Value = api.get(&format!("/deployments/{}", deployment.id)).await?;
    let finished: Deployment = views::read(&value)?;
    if out.json() {
        out.print_json(&value)?;
    }
    match finished.status.as_str() {
        "failed" | "canceled" => {
            let why = [last.reason, last.message].into_iter().flatten().collect::<Vec<_>>().join(": ");
            let why = if why.is_empty() { String::new() } else { format!(" ({why})") };
            Err(fail(Exit::Failed, format!("deployment {} {}{why}", finished.id, finished.status)))
        }
        _ if out.json() => Ok(()),
        _ => {
            let domains: Option<ServiceDomains> = match service.kind.as_str() {
                "app" => Some(views::read(&api.get::<Value>(&format!("/services/{}/domains", service.id)).await?)?),
                _ => None,
            };
            let at = domains.and_then(|domains| domains.url()).map(|url| format!(" at {url}")).unwrap_or_default();
            let live = out.paint("live", Style::Green);
            out.done(&format!("{} is {live}{at} ({}).", service.slug, finished.id))
        }
    }
}

/// The last status event of a deployment stream.
#[derive(Debug, Default, Deserialize)]
struct Status {
    status: String,
    reason: Option<String>,
    message: Option<String>,
}

/// Follows a deployment until it is live or has finished: log lines (to stderr when they
/// accompany a result, else stdout) and status changes. Reconnects where it left off.
async fn follow(session: &Session, id: &str, to_stderr: bool) -> anyhow::Result<Status> {
    let (api, out) = (&session.api, &session.out);
    let path = format!("/deployments/{id}/stream");
    let mut last_id: Option<String> = None;
    let mut status = Status::default();
    let mut failures = 0;
    loop {
        let opened = api.events(&path, &[], last_id.as_deref()).await;
        let mut events = match opened {
            Ok(events) => events,
            Err(error) if failures < RECONNECTS && super::exit_status(&error) == Exit::Failure as u8 => {
                failures += 1;
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            Err(error) => return Err(error),
        };
        loop {
            let event = match events.next().await {
                Ok(Some(event)) => event,
                Ok(None) | Err(_) => break,
            };
            failures = 0;
            match event.name.as_str() {
                "log" if !to_stderr => out.line(&event.data)?,
                "log" => out.info(&event.data),
                "status" => {
                    let next: Status = serde_json::from_str(&event.data).unwrap_or_default();
                    if next.status != status.status && to_stderr {
                        out.info(&out.paint_err(&format!("--> {}", next.status), status_style(&next.status)));
                    }
                    status = next;
                }
                "end" => return Ok(status),
                _ => {}
            }
        }
        last_id = events.last_id.clone().or(last_id);
        failures += 1;
        if failures > RECONNECTS {
            return Err(fail(Exit::Failure, format!("lost the stream of deployment {id}; `spinikus deployment show {id}`")));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

pub async fn deployment(session: &Session, command: DeploymentCommand) -> anyhow::Result<()> {
    let (api, out) = (&session.api, &session.out);
    match command {
        DeploymentCommand::List { service } => {
            let service = session.service(service.as_deref()).await?;
            let page: Page = api.get_with(&format!("/services/{}/deployments", service.id), &[("limit", "20".into())]).await?;
            if out.json() {
                return out.print_json(&page.items);
            }
            let mut rows = Vec::new();
            for value in &page.items {
                let deployment: Deployment = views::read(value)?;
                let status = match &deployment.reason {
                    Some(reason) => format!("{} ({reason})", deployment.status),
                    None => deployment.status.clone(),
                };
                let status = out.paint(&status, status_style(&deployment.status));
                rows.push(vec![
                    deployment.id.clone(),
                    status,
                    deployment.trigger.clone(),
                    deployment.what(),
                    ago(&deployment.created_at),
                ]);
            }
            out.table(&["ID", "STATUS", "TRIGGER", "RUNS", "CREATED"], &rows)
        }
        DeploymentCommand::Show { deployment } => {
            let value: Value = api.get(&format!("/deployments/{deployment}")).await?;
            if out.json() {
                return out.print_json(&value);
            }
            let deployment: Deployment = views::read(&value)?;
            let events: Vec<Value> = api.get(&format!("/deployments/{}/events", deployment.id)).await?;
            let status = out.paint(&deployment.status, status_style(&deployment.status));
            out.done(&format!("{} {status}", out.paint(&deployment.id, Style::Bold)))?;
            out.done(&format!("Trigger:  {}", deployment.trigger))?;
            if !deployment.what().is_empty() {
                out.done(&format!("Runs:     {}", deployment.what()))?;
            }
            out.done("")?;
            let mut rows = Vec::new();
            for event in &events {
                let event: DeploymentEvent = views::read(event)?;
                let note = [event.reason, event.message].into_iter().flatten().collect::<Vec<_>>().join(": ");
                rows.push(vec![ago(&event.created_at), out.paint(&event.to, status_style(&event.to)), note]);
            }
            out.table(&["WHEN", "STATUS", "NOTE"], &rows)
        }
        DeploymentCommand::Logs { deployment, follow: true } => follow(session, &deployment, false).await.map(drop),
        DeploymentCommand::Logs { deployment, follow: false } => {
            let text = api.text(&format!("/deployments/{deployment}/logs")).await?;
            match text.trim_end_matches('\n') {
                "" => Ok(()),
                text => out.line(text),
            }
        }
        DeploymentCommand::Cancel { deployment } => {
            let value: Value = api.post_empty(&format!("/deployments/{deployment}/cancel")).await?;
            if out.json() {
                return out.print_json(&value);
            }
            out.done(&format!("Canceled {deployment}."))
        }
    }
}

#[derive(Deserialize)]
struct LogLine {
    at: String,
    stream: String,
    text: String,
}

#[derive(Deserialize)]
struct LogEnd {
    error: Option<String>,
}

pub async fn logs(session: &Session, args: LogsArgs) -> anyhow::Result<()> {
    let (api, out) = (&session.api, &session.out);
    let service = session.service(args.service.as_deref()).await?;
    let mut query = vec![("tail", args.tail.to_string()), ("follow", args.follow.to_string())];
    query.extend(args.since.map(|since| ("since", since)));
    query.extend(args.deployment.map(|deployment| ("deployment_id", deployment)));
    let mut events = api.events(&format!("/services/{}/logs", service.id), &query, None).await?;
    while let Some(event) = events.next().await? {
        match event.name.as_str() {
            "log" => {
                let line: LogLine = serde_json::from_str(&event.data)?;
                if out.json() {
                    out.line(&event.data)?;
                    continue;
                }
                let text = if line.stream == "stderr" { out.paint(&line.text, Style::Red) } else { line.text };
                match args.timestamps {
                    true => out.line(&format!("{} {text}", out.paint(&line.at, Style::Dim)))?,
                    false => out.line(&text)?,
                }
            }
            "end" => {
                let end: LogEnd = serde_json::from_str(&event.data)?;
                return match end.error {
                    Some(error) => Err(fail(Exit::Failure, error)),
                    None => Ok(()),
                };
            }
            _ => {}
        }
    }
    Ok(())
}
