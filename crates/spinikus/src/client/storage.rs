//! `spinikus db`: creating databases, backing them up and restoring backups, and the backups
//! and restores `spinikus volume` shares with it.

use std::time::Duration;

use serde_json::{Value, json};

use super::{
    Session,
    args::{DbCommand, Engine},
    confirm, deploy,
    error::{Exit, fail},
    output::{Style, ago, size, status_style},
    resources::Page,
    views::{self, Backup, Destination, Restore, Service, Volume},
};

/// How often a backup or restore is checked while waiting for it.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

pub async fn db(session: &Session, command: DbCommand) -> anyhow::Result<()> {
    let (api, out) = (&session.api, &session.out);
    match command {
        DbCommand::Create { name, engine, version, from_backup, environment, slug, server_id, detach, no_deploy } => {
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
                "deploy": !no_deploy,
            });
            let created: Value = api.post(&format!("/environments/{}/databases", environment.id), &body).await?;
            let service: Service = views::read(&created["service"])?;
            let path = format!("{}/{}", session.environment_path(&environment).await?, service.slug);
            if no_deploy {
                if out.json() {
                    return out.print_json(&created);
                }
                return out
                    .done(&format!("Created {path} ({}), not deployed. Deploy it with `spinikus deploy {path}`.", service.id));
            }
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
            back_up(session, &service, None, &service.slug, destination.as_deref(), detach).await
        }
        DbCommand::Backups { database } => {
            let service = database_service(session, database.as_deref()).await?;
            list_backups(session, &service, None, None).await
        }
        DbCommand::Restore { backup, yes, detach } => restore(session, &backup, yes, detach).await,
    }
}

/// Backs a service up now, a database or one of an app's volumes, and waits unless `detach`.
/// `what` names it in messages.
pub(super) async fn back_up(
    session: &Session,
    service: &Service,
    volume_id: Option<&str>,
    what: &str,
    destination: Option<&str>,
    detach: bool,
) -> anyhow::Result<()> {
    let (api, out) = (&session.api, &session.out);
    let destination = destination_id(session, destination).await?;
    let body = json!({ "destination_id": destination, "volume_id": volume_id });
    let created: Value = api.post(&format!("/services/{}/backups", service.id), &body).await?;
    let backup: Backup = views::read(&created)?;
    if detach {
        return if out.json() { out.print_json(&created) } else { out.line(&backup.id) };
    }
    out.info(&format!("Backing up {what} ({})", backup.id));
    let value = wait(session, &format!("/backups/{}", backup.id)).await?;
    let finished: Backup = views::read(&value)?;
    if out.json() {
        out.print_json(&value)?;
    }
    match finished.status.as_str() {
        "succeeded" if out.json() => Ok(()),
        "succeeded" => {
            let bytes = finished.size_bytes.map(|bytes| format!(", {}", size(bytes))).unwrap_or_default();
            out.done(&format!("Backed up {what} ({}{bytes}).", finished.id))
        }
        _ => Err(fail(Exit::Failed, format!("backup {} failed: {}", finished.id, finished.error.unwrap_or_default()))),
    }
}

/// Lists a service's backups, newest first. With an app's `volumes` the table names each
/// backup's volume, and `only` keeps one volume's.
pub(super) async fn list_backups(
    session: &Session,
    service: &Service,
    volumes: Option<&[Volume]>,
    only: Option<&str>,
) -> anyhow::Result<()> {
    let (api, out) = (&session.api, &session.out);
    let page: Page = api.get_with(&format!("/services/{}/backups", service.id), &[("limit", "50".into())]).await?;
    let mut items = Vec::new();
    for value in page.items {
        let backup: Backup = views::read(&value)?;
        if only.is_none_or(|only| backup.volume_id.as_deref() == Some(only)) {
            items.push((value, backup));
        }
    }
    if out.json() {
        return out.print_json(&items.into_iter().map(|(value, _)| value).collect::<Vec<_>>());
    }
    let mut rows = Vec::new();
    for (_, backup) in items {
        let status = out.paint(&backup.status, status_style(&backup.status));
        let made_by = if backup.schedule_id.is_some() { "schedule" } else { "by hand" };
        let bytes = backup.size_bytes.map(size).unwrap_or_default();
        let mut row = vec![backup.id, status, made_by.to_string(), bytes, ago(&backup.created_at)];
        if let Some(volumes) = volumes {
            let volume = backup
                .volume_id
                .as_deref()
                .map(|id| volumes.iter().find(|volume| volume.id == id).map_or("(deleted)", |volume| volume.name.as_str()));
            row.insert(1, volume.unwrap_or_default().to_string());
        }
        rows.push(row);
    }
    match volumes {
        Some(_) => out.table(&["ID", "VOLUME", "STATUS", "MADE", "SIZE", "CREATED"], &rows),
        None => out.table(&["ID", "STATUS", "MADE", "SIZE", "CREATED"], &rows),
    }
}

/// Restores a backup into its database or volume, after a safety backup, and waits unless
/// `detach`.
pub(super) async fn restore(session: &Session, backup: &str, yes: bool, detach: bool) -> anyhow::Result<()> {
    let (api, out) = (&session.api, &session.out);
    let value: Value = api.get(&format!("/backups/{backup}")).await?;
    let backup: Backup = views::read(&value)?;
    let service = session.service(Some(&backup.service_id)).await?;
    let mut path = session.service_path(&service).await?;
    let made = ago(&backup.created_at);
    let question = match &backup.volume_id {
        None => format!("Replace the data in {path} with backup {} ({made})? A safety backup is made first.", backup.id),
        Some(volume_id) => {
            let volumes: Vec<Volume> = views::read(&api.get::<Value>(&format!("/services/{}/volumes", service.id)).await?)?;
            let name = volumes.iter().find(|volume| &volume.id == volume_id).map_or(volume_id.as_str(), |volume| &volume.name);
            path = format!("{path}'s volume {name}");
            format!(
                "Replace the files in {path} with backup {} ({made})? A safety backup is made first, and the app is stopped meanwhile.",
                backup.id
            )
        }
    };
    confirm(yes, &question)?;
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
