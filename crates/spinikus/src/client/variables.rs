//! `spinikus env`: a service's, environment's or project's variables.

use std::io::{IsTerminal, Read};

use serde_json::{Map, Value, json};

use super::{
    Session,
    args::{Availability, EnvCommand, VariableScope},
    dotenv,
    error::{Exit, fail},
    output::Style,
    views::{self, Variable},
};

/// Where a command's variables live: the API path and how to say it.
struct Scope {
    path: String,
    label: String,
}

impl Session {
    async fn variable_scope(&self, scope: &VariableScope) -> anyhow::Result<Scope> {
        let project = |id: &str| format!("/projects/{id}/variables");
        let environment = |id: &str| format!("/environments/{id}/variables");
        match (&scope.service, &scope.environment, &scope.project) {
            (Some(name), _, _) => self.service_scope(Some(name)).await,
            (_, Some(name), _) => {
                let found = self.environment(Some(name)).await?;
                Ok(Scope { path: environment(&found.id), label: self.environment_path(&found).await? })
            }
            (_, _, Some(name)) => {
                let found = self.project(Some(name)).await?;
                Ok(Scope { path: project(&found.id), label: found.slug })
            }
            (None, None, None) => match self.link()? {
                Some(link) if link.service_id.is_some() => self.service_scope(None).await,
                Some(_) => {
                    let found = self.environment(None).await?;
                    Ok(Scope { path: environment(&found.id), label: self.environment_path(&found).await? })
                }
                None => Err(fail(Exit::Failure, "name a scope with -s, -e or -p, or run `spinikus link` here first")),
            },
        }
    }

    async fn service_scope(&self, name: Option<&str>) -> anyhow::Result<Scope> {
        let found = self.service(name).await?;
        Ok(Scope { path: format!("/services/{}/variables", found.id), label: self.service_path(&found).await? })
    }
}

pub async fn env(session: &Session, command: EnvCommand) -> anyhow::Result<()> {
    let (api, out) = (&session.api, &session.out);
    match command {
        EnvCommand::List { scope } => {
            let scope = session.variable_scope(&scope).await?;
            let variables: Vec<Value> = api.get(&scope.path).await?;
            if out.json() {
                return out.print_json(&variables);
            }
            let mut rows = Vec::new();
            for value in &variables {
                let variable: Variable = views::read(value)?;
                let shown = match (&variable.value, variable.secret) {
                    (_, true) => out.paint("(secret)", Style::Dim),
                    (Some(value), false) => one_line(value),
                    (None, false) => String::new(),
                };
                rows.push(vec![variable.name, shown, variable.availability]);
            }
            out.table(&["NAME", "VALUE", "AVAILABLE AT"], &rows)
        }
        EnvCommand::Set { variables, secret, availability, scope } => {
            let assignments = assignments(&variables)?;
            let scope = session.variable_scope(&scope).await?;
            set(session, &scope, assignments, secret, availability).await
        }
        EnvCommand::Unset { names, scope } => {
            let scope = session.variable_scope(&scope).await?;
            let existing = existing(session, &scope).await?;
            // Check them all first, so a typo removes nothing.
            let ids = names
                .iter()
                .map(|name| {
                    existing
                        .iter()
                        .find(|variable| &variable.name == name)
                        .map(|variable| variable.id.clone())
                        .ok_or_else(|| fail(Exit::NotFound, format!("{} has no variable {name}", scope.label)))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            for (name, id) in names.iter().zip(ids) {
                api.delete(&format!("/variables/{id}")).await?;
                out.done(&format!("Removed {name} from {}.", scope.label))?;
            }
            Ok(())
        }
        EnvCommand::Import { file, secret, availability, scope } => {
            let text = match file.as_str() {
                "-" => {
                    let mut text = String::new();
                    std::io::stdin().read_to_string(&mut text)?;
                    text
                }
                path => std::fs::read_to_string(path)
                    .map_err(|error| fail(Exit::Failure, format!("couldn't read {path}: {error}")))?,
            };
            let assignments = dotenv::parse(&text).map_err(|error| fail(Exit::Failure, format!("{file}: {error}")))?;
            if assignments.is_empty() {
                return Err(fail(Exit::Failure, format!("{file} has no variables")));
            }
            let scope = session.variable_scope(&scope).await?;
            set(session, &scope, assignments, secret, availability).await
        }
        EnvCommand::Export { reveal, scope } => {
            let scope = session.variable_scope(&scope).await?;
            let variables: Vec<Value> = api.get(&scope.path).await?;
            let mut text = String::new();
            for value in &variables {
                let variable: Variable = views::read(value)?;
                let value = match (variable.secret, reveal, variable.value) {
                    (true, true, _) => {
                        let revealed: Value = api.post_empty(&format!("/variables/{}/reveal", variable.id)).await?;
                        revealed["value"].as_str().unwrap_or_default().to_string()
                    }
                    (true, false, _) => {
                        text.push_str(&format!("# {} is secret: pass --reveal to export it\n", variable.name));
                        continue;
                    }
                    (false, _, value) => value.unwrap_or_default(),
                };
                text.push_str(&dotenv::line(&variable.name, &value));
                text.push('\n');
            }
            out.line(text.trim_end_matches('\n'))
        }
    }
}

/// `NAME=value` arguments, and bare `NAME`s whose value is typed (hidden) or piped in.
fn assignments(arguments: &[String]) -> anyhow::Result<Vec<(String, String)>> {
    let bare = arguments.iter().filter(|argument| !argument.contains('=')).count();
    let terminal = std::io::stdin().is_terminal();
    if bare > 1 && !terminal {
        return Err(fail(Exit::Failure, "only one value can be piped in; give the others as NAME=value"));
    }
    arguments
        .iter()
        .map(|argument| match argument.split_once('=') {
            Some((name, value)) => Ok((name.to_string(), value.to_string())),
            None if terminal => Ok((argument.clone(), rpassword::prompt_password(format!("{argument}: "))?)),
            None => {
                let mut value = String::new();
                std::io::stdin().read_to_string(&mut value)?;
                let value = value.strip_suffix('\n').map(|value| value.strip_suffix('\r').unwrap_or(value)).unwrap_or(&value);
                Ok((argument.clone(), value.to_string()))
            }
        })
        .collect()
}

async fn existing(session: &Session, scope: &Scope) -> anyhow::Result<Vec<Variable>> {
    let values: Vec<Value> = session.api.get(&scope.path).await?;
    values.iter().map(views::read).collect()
}

/// Creates each variable, or updates the one with its name.
async fn set(
    session: &Session,
    scope: &Scope,
    assignments: Vec<(String, String)>,
    secret: bool,
    availability: Option<Availability>,
) -> anyhow::Result<()> {
    let (api, out) = (&session.api, &session.out);
    let existing = existing(session, scope).await?;
    let mut saved = Vec::new();
    for (name, value) in assignments {
        let mut body = Map::new();
        body.insert("value".to_string(), json!(value));
        if let Some(availability) = availability {
            body.insert("availability".to_string(), json!(availability.as_str()));
        }
        let (verb, variable): (&str, Value) = match existing.iter().find(|variable| variable.name == name) {
            Some(variable) => {
                if secret && !variable.secret {
                    body.insert("secret".to_string(), json!(true));
                }
                ("Updated", api.patch(&format!("/variables/{}", variable.id), variable.revision, &Value::Object(body)).await?)
            }
            None => {
                body.insert("name".to_string(), json!(name));
                body.insert("secret".to_string(), json!(secret));
                ("Set", api.post(&scope.path, &Value::Object(body)).await?)
            }
        };
        if !out.json() {
            let kind = if variable["secret"] == true { " (secret)" } else { "" };
            out.done(&format!("{verb} {name}{kind} on {}.", scope.label))?;
        }
        saved.push(variable);
    }
    if out.json() {
        out.print_json(&saved)?;
    }
    out.info("Changes apply at the next deployment.");
    Ok(())
}

/// A value on one line for a table, shortened.
fn one_line(value: &str) -> String {
    const MAX: usize = 60;
    let line = value.replace('\n', "\\n");
    match line.char_indices().nth(MAX) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line,
    }
}
