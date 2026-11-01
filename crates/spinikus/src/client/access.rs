//! Signing in and out, contexts, linking a directory, and API tokens.

use std::io::{BufRead, IsTerminal, Write};

use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};

use super::{
    Session,
    api::{self, Api},
    args::{ContextCommand, Global, LinkArgs, LoginArgs, Role, TokenCommand},
    contexts::{ConfigDir, Context},
    error::{Exit, fail},
    link::{self, Link},
    output::{Output, Style, ago},
    views::{self, Environment, Project, Service, Token, User},
};

/// Where the password comes from without a terminal, such as in CI.
const PASSWORD_VAR: &str = "SPINIKUS_PASSWORD";

pub async fn login(global: &Global, out: Output, args: LoginArgs) -> anyhow::Result<()> {
    let config = ConfigDir::locate()?;
    let mut contexts = config.contexts()?;
    let server = match args.server.as_deref().or(global.server.as_deref()) {
        Some(server) => server.to_string(),
        None => {
            let current = global.context.as_ref().or(contexts.current.as_ref());
            match current.and_then(|name| contexts.contexts.get(name)) {
                Some(context) => context.server.clone(),
                None => prompt("Server (such as https://spinikus.example.com): ")?,
            }
        }
    };
    let url = api::parse_server(&server)?;
    let server = api::server_name(&url);
    let email = match args.email {
        Some(email) => email,
        None => prompt("Email: ")?,
    };
    let password = match std::env::var(PASSWORD_VAR) {
        Ok(password) if !password.is_empty() => password,
        _ if std::io::stdin().is_terminal() => rpassword::prompt_password("Password: ")?,
        _ => return Err(fail(Exit::Failure, format!("no terminal to ask for the password; set {PASSWORD_VAR}"))),
    };

    let anonymous = Api::new(&server, None)?;
    let body = json!({ "email": email, "password": password, "token_name": args.token_name });
    let created: Value = anonymous.post("/login", &body).await?;
    let token = created["token"].as_str().ok_or_else(|| anyhow::anyhow!("The server sent no token"))?.to_string();
    let token_id = created["api_token"]["id"].as_str().map(str::to_string);
    let api = Api::new(&server, Some(token.clone()))?;
    let user: Value = api.get("/me").await?;
    let user_view: User = views::read(&user)?;

    let name = args
        .context_name
        .or_else(|| contexts.for_server(&server).map(str::to_string))
        .unwrap_or_else(|| default_context_name(&url));
    // Signing in again replaces the old token, which is revoked so it doesn't linger.
    if let (Some(old), Ok(Some(old_token))) = (contexts.contexts.get(&name), config.token(&name))
        && let Some(old_id) = &old.token_id
        && old.server == server
        && Some(old_id) != token_id.as_ref()
    {
        let _ = Api::new(&server, Some(old_token))?.delete(&format!("/tokens/{old_id}")).await;
    }
    let context = Context { server: server.clone(), user: Some(user_view.email.clone()), token_id };
    contexts.contexts.insert(name.clone(), context);
    contexts.current = Some(name.clone());
    config.set_token(&name, Some(&token))?;
    config.save_contexts(&contexts)?;
    if out.json() {
        return out.print_json(&user);
    }
    out.done(&format!("Signed in to {server} as {} <{}> (context `{name}`).", user_view.name, user_view.email))
}

/// `host`, or `host:port` for a port that isn't the scheme's default.
fn default_context_name(url: &reqwest::Url) -> String {
    let host = url.host_str().unwrap_or("spinikus");
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    }
}

fn prompt(question: &str) -> anyhow::Result<String> {
    if !std::io::stdin().is_terminal() {
        return Err(fail(Exit::Failure, format!("no terminal to ask: {}", question.trim_end_matches([' ', ':']))));
    }
    eprint!("{question}");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(answer.trim().to_string())
}

/// Revokes the context's token and forgets it; the context stays, to sign in again.
pub async fn logout(global: &Global, out: Output) -> anyhow::Result<()> {
    let config = ConfigDir::locate()?;
    let mut contexts = config.contexts()?;
    let name = global
        .context
        .clone()
        .or_else(|| {
            global.server.as_deref().and_then(|server| {
                let server = api::server_name(&api::parse_server(server).ok()?);
                contexts.for_server(&server).map(str::to_string)
            })
        })
        .or_else(|| contexts.current.clone())
        .ok_or_else(|| fail(Exit::Failure, "not signed in anywhere"))?;
    let context = contexts.contexts.get_mut(&name).ok_or_else(|| fail(Exit::Failure, format!("there is no context `{name}`")))?;
    let Some(token) = config.token(&name)? else {
        return out.done(&format!("Already signed out of {}.", context.server));
    };
    if let Some(id) = context.token_id.take() {
        match Api::new(&context.server, Some(token))?.delete(&format!("/tokens/{id}")).await {
            Ok(()) => {}
            // Already revoked or expired: nothing left to do on the server.
            Err(error) if matches!(super::exit_status(&error), 3 | 5) => {}
            Err(error) => out.warn(&format!("couldn't revoke the token on {}: {error:#}", context.server)),
        }
    }
    let server = context.server.clone();
    config.set_token(&name, None)?;
    config.save_contexts(&contexts)?;
    out.done(&format!("Signed out of {server}."))
}

pub async fn whoami(session: &Session) -> anyhow::Result<()> {
    let user: Value = session.api.get("/me").await?;
    if session.out.json() {
        return session.out.print_json(&user);
    }
    let view: User = views::read(&user)?;
    let admin = if view.admin { " (instance admin)" } else { "" };
    let context = session.context.as_deref().map(|name| format!(", context `{name}`")).unwrap_or_default();
    session.out.done(&format!("{} <{}>{admin} on {}{context}", view.name, view.email, session.api.server()))
}

pub fn context(out: Output, command: ContextCommand) -> anyhow::Result<()> {
    let config = ConfigDir::locate()?;
    let mut contexts = config.contexts()?;
    match command {
        ContextCommand::List => {
            if out.json() {
                return out.print_json(&contexts);
            }
            let rows: Vec<Vec<String>> = contexts
                .contexts
                .iter()
                .map(|(name, context)| {
                    let current = if contexts.current.as_ref() == Some(name) { "*" } else { "" };
                    let signed_in = config.token(name).ok().flatten().is_some();
                    let user = match (&context.user, signed_in) {
                        (Some(user), true) => user.clone(),
                        (_, false) => out.paint("signed out", Style::Dim),
                        (None, true) => String::new(),
                    };
                    vec![current.to_string(), name.clone(), context.server.clone(), user]
                })
                .collect();
            out.table(&["", "NAME", "SERVER", "USER"], &rows)
        }
        ContextCommand::Use { name } => {
            if !contexts.contexts.contains_key(&name) {
                return Err(fail(Exit::Failure, format!("there is no context `{name}`")));
            }
            contexts.current = Some(name.clone());
            config.save_contexts(&contexts)?;
            out.done(&format!("Now using `{name}`."))
        }
        ContextCommand::Remove { name } => {
            if contexts.contexts.remove(&name).is_none() {
                return Err(fail(Exit::Failure, format!("there is no context `{name}`")));
            }
            if contexts.current.as_ref() == Some(&name) {
                contexts.current = None;
            }
            config.set_token(&name, None)?;
            config.save_contexts(&contexts)?;
            out.done(&format!("Removed `{name}`. Its token still works until revoked (`spinikus token revoke`)."))
        }
    }
}

pub async fn link(session: &Session, args: LinkArgs) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let out = &session.out;
    if args.remove {
        let dir = session.linked_dir().unwrap_or(cwd);
        return match link::remove(&dir)? {
            true => out.done(&format!("Unlinked {}.", dir.display())),
            false => out.done("This directory isn't linked."),
        };
    }
    let (environment, service) = match args.target.as_deref() {
        Some(id) if id.starts_with("svc_") => {
            let service = session.service(Some(id)).await?;
            (session.environment(Some(&service.environment_id)).await?, Some(service))
        }
        Some(id) if id.starts_with("env_") => (session.environment(Some(id)).await?, None),
        Some(path) => match path.split('/').count() {
            2 => (session.environment(Some(path)).await?, None),
            3 => {
                let service = session.service(Some(path)).await?;
                (session.environment(Some(&service.environment_id)).await?, Some(service))
            }
            _ => return Err(fail(Exit::Failure, "name `project/environment` or `project/environment/service`")),
        },
        None => choose(session).await?,
    };
    let link = Link {
        server: session.api.server(),
        project_id: environment.project_id.clone(),
        environment_id: environment.id.clone(),
        service_id: service.as_ref().map(|service| service.id.clone()),
    };
    link::save(&cwd, &link)?;
    let mut path = session.environment_path(&environment).await?;
    if let Some(service) = &service {
        path = format!("{path}/{}", service.slug);
    }
    out.done(&format!("Linked {} to {path} on {}.", cwd.display(), session.api.server()))
}

/// Asks for the project, environment and service on a terminal.
async fn choose(session: &Session) -> anyhow::Result<(Environment, Option<Service>)> {
    if !std::io::stdin().is_terminal() {
        return Err(fail(Exit::Failure, "name what to link, such as `spinikus link shop/production/web`"));
    }
    let projects: Vec<Value> = session.api.list("/projects").await?;
    let projects: Vec<Project> = projects.iter().map(views::read).collect::<Result<_, _>>()?;
    let project = pick("Project", &projects, |project| format!("{} ({})", project.name, project.slug), false)?
        .ok_or_else(|| fail(Exit::Failure, "there are no projects you can see"))?;
    let environments: Vec<Value> = session.api.list(&format!("/projects/{}/environments", project.id)).await?;
    let environments: Vec<Environment> = environments.iter().map(views::read).collect::<Result<_, _>>()?;
    let environment =
        pick("Environment", &environments, |environment| format!("{} ({})", environment.name, environment.slug), false)?
            .ok_or_else(|| fail(Exit::Failure, format!("{} has no environments", project.name)))?;
    let services: Vec<Value> = session.api.list(&format!("/environments/{}/services", environment.id)).await?;
    let services: Vec<Service> = services.iter().map(views::read).collect::<Result<_, _>>()?;
    let service = pick("Service", &services, |service| format!("{} ({})", service.name, service.slug), true)?;
    Ok((environment.clone(), service.cloned()))
}

/// A numbered choice; with `optional`, 0 (or nothing) is none.
fn pick<'a, T>(what: &str, items: &'a [T], label: impl Fn(&T) -> String, optional: bool) -> anyhow::Result<Option<&'a T>> {
    if items.is_empty() {
        return Ok(None);
    }
    if items.len() == 1 && !optional {
        eprintln!("{what}: {}", label(&items[0]));
        return Ok(Some(&items[0]));
    }
    eprintln!("{what}:");
    if optional {
        eprintln!("  0) none: link the environment only");
    }
    for (index, item) in items.iter().enumerate() {
        eprintln!("  {}) {}", index + 1, label(item));
    }
    loop {
        let answer = prompt("Number: ")?;
        match answer.parse::<usize>() {
            Ok(0) if optional => return Ok(None),
            Err(_) if answer.is_empty() && optional => return Ok(None),
            Ok(number) if (1..=items.len()).contains(&number) => return Ok(Some(&items[number - 1])),
            _ => eprintln!("Choose a number from the list."),
        }
    }
}

pub async fn token(session: &Session, command: TokenCommand) -> anyhow::Result<()> {
    let (api, out) = (&session.api, &session.out);
    match command {
        TokenCommand::List => {
            #[derive(serde::Deserialize)]
            struct List {
                items: Vec<Value>,
            }
            let list: List = api.get("/tokens").await?;
            if out.json() {
                return out.print_json(&list.items);
            }
            let mut rows = Vec::new();
            for value in &list.items {
                let token: Token = views::read(value)?;
                let scope = match &token.scope {
                    Some(scope) => format!("{} {}", scope.scope, scope.scope_id),
                    None => "everywhere".to_string(),
                };
                let expires = token.expires_at.as_deref().map(expires_in).unwrap_or_else(|| "never".to_string());
                let used = token.last_used_at.as_deref().map(ago).unwrap_or_else(|| "never".to_string());
                rows.push(vec![token.id, token.name, token.role, scope, expires, used]);
            }
            out.table(&["ID", "NAME", "ROLE", "SCOPE", "EXPIRES", "LAST USED"], &rows)
        }
        TokenCommand::Create { name, role, project, environment, expires } => {
            let scope = match (project, environment) {
                (Some(project), _) => json!({ "scope": "project", "scope_id": session.project(Some(&project)).await?.id }),
                (_, Some(environment)) => {
                    json!({ "scope": "environment", "scope_id": session.environment(Some(&environment)).await?.id })
                }
                (None, None) => Value::Null,
            };
            let role = match role {
                Role::Viewer => "viewer",
                Role::Developer => "developer",
                Role::Admin => "admin",
            };
            let expires_at = match expires.as_str() {
                "never" => Value::Null,
                days => {
                    let days: i64 = days.parse().ok().filter(|days| *days > 0).ok_or_else(|| {
                        fail(Exit::Failure, format!("--expires takes a number of days or `never`, not `{days}`"))
                    })?;
                    json!((Timestamp::now() + SignedDuration::from_hours(days * 24)).to_string())
                }
            };
            let body = json!({ "name": name, "role": role, "scope": scope, "expires_at": expires_at });
            let created: Value = api.post("/tokens", &body).await?;
            if out.json() {
                return out.print_json(&created);
            }
            let secret = created["token"].as_str().unwrap_or_default();
            out.info("The token is shown only this once:");
            out.line(secret)
        }
        TokenCommand::Revoke { id } => {
            api.delete(&format!("/tokens/{id}")).await?;
            out.done(&format!("Revoked {id}."))
        }
    }
}

/// When a token expires: "in 89d", or "expired".
fn expires_in(timestamp: &str) -> String {
    let Ok(at) = timestamp.parse::<Timestamp>() else { return timestamp.to_string() };
    let seconds = at.as_second() - Timestamp::now().as_second();
    match seconds {
        ..=0 => "expired".to_string(),
        1..86_400 => format!("in {}h", (seconds + 3_599) / 3_600),
        _ => format!("in {}d", seconds / 86_400),
    }
}
