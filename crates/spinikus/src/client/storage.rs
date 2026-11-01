//! `spinikus db`: creating databases, backing them up and restoring backups.

use std::time::Duration;

use serde_json::{Value, json};

use super::{
    Session,
    args::{DbCommand, Engine},
    confirm, deploy,
    error::{Exit, fail},
    output::{Style, ago, size, status_style},
    resources::Page,
    views::{self, Backup, Destination, Restore, Service},
};

/// How often a backup or restore is checked while waiting for it.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

pub async fn db(session: &Session, command: DbCommand) -> anyhow::Result<()> {
    let (api, out) = (&session.api, &session.out);
    match command {
        DbCommand::Create { name, engine, version, from_backup, environment, slug, server_id, detach } => {
            let environment = session.environment(environment.as_deref()).await?;
            let engine = engine.map(|engine| match engine {
                Engine::Postgres => "postgres",
                Engine::Redis => "redis",
            });
            let body = json!({
                "name": name,
                "slug": slug,
                "engine": engine,
                "version": version,
                "server_id": server_id,
                "from_backup": from_backup,
            });
            let created: Value = api.post(&format!("/environments/{}/databases", environment.id), &body).await?;
            let service: Service = views::read(&created["service"])?;
            let path = format!("{}/{}", session.environment_path(&environment).await?, service.slug);
            out.info(&format!("Created {path} ({}); deploying it", service.id));
            deploy::finish(session, &service, created["deployment"].clone(), detach).await?;
            if let Some(restore) = created.get("restore").filter(|restore| !detach && !restore.is_null()) {
                let restore: Restore = views::read(restore)?;
                out.info(&format!("Restoring the backup into it ({})", restore.id));
                let finished: Restore = views::read(&wait(session, &format!("/restores/{}", restore.id)).await?)?;
                if finished.status != "succeeded" {
                    let problem = finished.error.unwrap_or_default();
                    return Err(fail(Exit::Failed, format!("restore {} failed: {problem}", finished.id)));
                }
            }
            if !out.json() {
                let reference = match created["service"]["config"]["database"]["engine"].as_str() {
                    Some("redis") => "REDIS_URL",
                    _ => "DATABASE_URL",
                };
                out.done(&format!(
                    "Apps in this environment reach it as `{}`; use ${{{{ {}.{reference} }}}} in their variables.",
                    service.slug, service.slug
                ))?;
            }
            Ok(())
        }
        DbCommand::Backup { database, destination, detach } => {
            let service = database_service(session, database.as_deref()).await?;
            let destination = destination_id(session, destination.as_deref()).await?;
            let body = json!({ "destination_id": destination });
            let created: Value = api.post(&format!("/services/{}/backups", service.id), &body).await?;
            let backup: Backup = views::read(&created)?;
            if detach {
                return if out.json() { out.print_json(&created) } else { out.line(&backup.id) };
            }
            out.info(&format!("Backing up {} ({})", service.slug, backup.id));
            let value = wait(session, &format!("/backups/{}", backup.id)).await?;
            let finished: Backup = views::read(&value)?;
            if out.json() {
                out.print_json(&value)?;
            }
            match finished.status.as_str() {
                "succeeded" if out.json() => Ok(()),
                "succeeded" => {
                    let bytes = finished.size_bytes.map(|bytes| format!(", {}", size(bytes))).unwrap_or_default();
                    out.done(&format!("Backed up {} ({}{bytes}).", service.slug, finished.id))
                }
                _ => Err(fail(Exit::Failed, format!("backup {} failed: {}", finished.id, finished.error.unwrap_or_default()))),
            }
        }
        DbCommand::Backups { database } => {
            let service = database_service(session, database.as_deref()).await?;
            let page: Page = api.get_with(&format!("/services/{}/backups", service.id), &[("limit", "50".into())]).await?;
            if out.json() {
                return out.print_json(&page.items);
            }
            let mut rows = Vec::new();
            for value in &page.items {
                let backup: Backup = views::read(value)?;
                let status = out.paint(&backup.status, status_style(&backup.status));
                let made_by = if backup.schedule_id.is_some() { "schedule" } else { "by hand" };
                let bytes = backup.size_bytes.map(size).unwrap_or_default();
                rows.push(vec![backup.id, status, made_by.to_string(), bytes, ago(&backup.created_at)]);
            }
            out.table(&["ID", "STATUS", "MADE", "SIZE", "CREATED"], &rows)
        }
        DbCommand::Restore { backup, yes, detach } => {
            let value: Value = api.get(&format!("/backups/{backup}")).await?;
            let backup: Backup = views::read(&value)?;
            let service_id = value["service_id"].as_str().unwrap_or_default();
            let service = session.service(Some(service_id)).await?;
            let path = session.service_path(&service).await?;
            let made = ago(&backup.created_at);
            confirm(
                yes,
                &format!("Replace the data in {path} with backup {} ({made})? A safety backup is made first.", backup.id),
            )?;
            let created: Value = api.post_empty(&format!("/backups/{}/restore", backup.id)).await?;
            let restore: Restore = views::read(&created)?;
            if detach {
                return if out.json() { out.print_json(&created) } else { out.line(&restore.id) };
            }
            out.info(&format!("Restoring {path} ({}): a safety backup first, then the restore", restore.id));
            let value = wait(session, &format!("/restores/{}", restore.id)).await?;
            let finished: Restore = views::read(&value)?;
            if out.json() {
                out.print_json(&value)?;
            }
            match finished.status.as_str() {
                "succeeded" if out.json() => Ok(()),
                "succeeded" => out.done(&format!("Restored {path} from {}.", backup.id)),
                _ => Err(fail(Exit::Failed, format!("restore {} failed: {}", finished.id, finished.error.unwrap_or_default()))),
            }
        }
    }
}

/// The database service named, refusing apps early with a clear message.
async fn database_service(session: &Session, name: Option<&str>) -> anyhow::Result<Service> {
    let service = session.service(name).await?;
    if service.kind != "database" {
        return Err(fail(Exit::Failure, format!("{} is not a database", service.slug)));
    }
    Ok(service)
}

/// The destination by ID or name, or the only one if none is named.
async fn destination_id(session: &Session, name: Option<&str>) -> anyhow::Result<String> {
    let values: Vec<Value> = session.api.get("/storage-destinations").await?;
    let destinations: Vec<Destination> = values.iter().map(views::read).collect::<Result<_, _>>()?;
    let names = || destinations.iter().map(|destination| destination.name.clone()).collect::<Vec<_>>().join(", ");
    match name {
        Some(name) => destinations
            .iter()
            .find(|destination| destination.id == name || destination.name == name)
            .map(|destination| destination.id.clone())
            .ok_or_else(|| fail(Exit::NotFound, format!("no storage destination `{name}`; there are: {}", names()))),
        None => match destinations.as_slice() {
            [only] => Ok(only.id.clone()),
            [] => Err(fail(
                Exit::Failure,
                "there are no storage destinations; an instance admin adds them under Settings → Storage",
            )),
            _ => Err(fail(Exit::Failure, format!("choose a destination with --destination: {}", names()))),
        },
    }
}

/// Polls a backup or restore until it has finished, and returns it.
async fn wait(session: &Session, path: &str) -> anyhow::Result<Value> {
    let mut shown = String::new();
    loop {
        let value: Value = session.api.get(path).await?;
        let status = value["status"].as_str().unwrap_or_default().to_string();
        if matches!(status.as_str(), "succeeded" | "failed") {
            return Ok(value);
        }
        if status != shown {
            session.out.info(&session.out.paint_err(&format!("--> {status}"), Style::Yellow));
            shown = status;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}
