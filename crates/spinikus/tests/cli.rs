//! The `spinikus` client commands, run as a process against a real control plane serving the
//! API, with a real agent (in-memory containers): signing in, linking a directory,
//! variables, deploying and rolling back, logs, domains, tokens and signing out.

use std::{net::SocketAddr, path::Path, process::Stdio, sync::Arc, time::Duration};

mod common;

use common::ControlPlane;
use serde_json::{Value, json};
use spinikus_agent::{AgentFiles, Backoff, RunOptions, runtime::MemoryRuntime};
use spinikus_control::{NewUser, ServiceChanges};
use spinikus_core::{id::ServiceId, secret::Secret, server::ServerStatus};
use spinikus_store::{InstanceRepository, ServiceRepository};
use tokio::{io::AsyncWriteExt, net::TcpListener, process::Command, sync::oneshot};

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|error| panic!("{error}: {}", self.stdout))
    }
}

/// Runs the binary in `work` with its config in `config`, optionally feeding stdin.
async fn spinikus(config: &Path, work: &Path, args: &[&str], stdin: Option<&str>) -> Run {
    let mut command = Command::new(env!("CARGO_BIN_EXE_spinikus"));
    command
        .args(args)
        .current_dir(work)
        .env("SPINIKUS_CONFIG_DIR", config)
        .env("SPINIKUS_PASSWORD", "correct horse battery")
        .env("NO_COLOR", "1")
        .env_remove("SPINIKUS_URL")
        .env_remove("SPINIKUS_TOKEN")
        .env_remove("SPINIKUS_CONTEXT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    if let Some(text) = stdin {
        input.write_all(text.as_bytes()).await.unwrap();
    }
    drop(input);
    let output = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output()).await.expect("the CLI hung").unwrap();
    let run = Run {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    };
    eprintln!("$ spinikus {}\n{}{}(exit {})", args.join(" "), run.stdout, run.stderr, run.code);
    run
}

#[tokio::test]
async fn the_cli_signs_in_links_deploys_and_manages_variables() {
    let control_plane = ControlPlane::start().await;
    let control = control_plane.control.clone();
    let dir = tempfile::tempdir().unwrap();
    let (config, work) = (dir.path().join("config"), dir.path().join("work"));
    std::fs::create_dir_all(work.join("src")).unwrap();

    // An agent with in-memory containers, whose app port answers.
    let files = AgentFiles::new(dir.path().join("agent"));
    let (server, token) = control_plane.join_token().await;
    let credentials =
        spinikus_agent::join(&control_plane.url, &token, &control_plane.fingerprint, spinikus_agent::server_facts())
            .await
            .unwrap();
    files.save(&credentials).unwrap();
    let runtime = Arc::new(MemoryRuntime::new());
    runtime.add_image("ghcr.io/acme/web:1", "ghcr.io/acme/web@sha256:01");
    let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
    runtime.set_endpoint(app.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(app, axum::Router::new().route("/", axum::routing::get(|| async { "hi" }))).await });
    let (_stop_agent, stopped) = oneshot::channel::<()>();
    let options = RunOptions {
        facts: spinikus_agent::server_facts(),
        backoff: Backoff::new(Duration::from_millis(50), Duration::from_millis(200)),
        runtime: runtime.clone(),
        proxy: None,
        local_repositories: true,
        backups: Default::default(),
    };
    tokio::spawn(async move { spinikus_agent::run(&files, options, async move { drop(stopped.await) }).await });
    control_plane.wait_for_status(server, ServerStatus::Connected).await;

    // The API, and someone to sign in.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = spinikus_api::router(control.clone());
    tokio::spawn(axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).into_future());
    let new = NewUser {
        email: "ada@example.com".into(),
        name: "Ada".into(),
        admin: true,
        password: Some(Secret::new("correct horse battery")),
    };
    control.create_user(&control_plane.admin, new).await.unwrap();
    let cli = |args: &'static [&'static str]| spinikus(&config, &work, args, None);
    let server_id = server.to_string();

    // Signing in.
    assert_eq!(cli(&["whoami"]).await.code, 3, "not signed in yet");
    let login = spinikus(&config, &work, &["login", &url, "--email", "ada@example.com"], None).await;
    assert_eq!(login.code, 0, "{}", login.stderr);
    assert!(login.stdout.contains("as Ada <ada@example.com>"), "{}", login.stdout);
    assert!(cli(&["whoami"]).await.stdout.contains("Ada <ada@example.com> (instance admin) on http://127.0.0.1"));
    let credentials = std::fs::read_to_string(config.join("credentials.toml")).unwrap();
    let login_token = credentials.split('"').find(|part| part.starts_with("spk_")).expect("a saved token").to_string();

    // Creating things, and linking the directory to the service.
    assert_eq!(cli(&["project", "create", "Shop"]).await.code, 0);
    assert_eq!(cli(&["environment", "create", "Production", "--project", "shop"]).await.code, 0);
    let previews = cli(&["environment", "previews", "on", "shop/production"]).await;
    assert_eq!((previews.code, previews.stdout.as_str()), (0, "Previews of shop/production are on.\n"), "{}", previews.stderr);
    assert!(cli(&["environment", "list", "shop"]).await.stdout.contains("production (previews)"));
    let created = spinikus(
        &config,
        &work,
        &[
            "service",
            "create",
            "Web",
            "-e",
            "shop/production",
            "--image",
            "ghcr.io/acme/web:1",
            "--port",
            "3000",
            "--server-id",
            &server_id,
            "-o",
            "json",
        ],
        None,
    )
    .await;
    let service: ServiceId = created.json()["id"].as_str().unwrap().parse().unwrap();
    // No path to check, so the container only has to keep running; don't wait for that.
    let current = control.store().get_service(service).await.unwrap();
    let config_value = json!({ "runtime": { "port": 3000, "healthcheck": { "start_period": "0s" } } });
    let changes = ServiceChanges { name: None, slug: None, config: config_value.as_object().cloned(), server_id: None };
    control.update_service(&control_plane.admin, service, current.revision, changes).await.unwrap();
    let unlinked = cli(&["service", "list"]).await;
    assert_eq!(unlinked.code, 1, "nothing linked, nothing named");
    assert!(unlinked.stderr.contains("name an environment, or run `spinikus link`"), "{}", unlinked.stderr);
    let linked = cli(&["link", "shop/production/web"]).await;
    assert!(linked.stdout.contains("to shop/production/web on http://127.0.0.1"), "{}", linked.stdout);
    let link: Value = serde_json::from_str(&std::fs::read_to_string(work.join(".spinikus/link.json")).unwrap()).unwrap();
    assert_eq!(link["service_id"], service.to_string());
    let listed = spinikus(&config, &work.join("src"), &["service", "list"], None).await;
    assert!(listed.stdout.lines().nth(1).is_some_and(|line| line.starts_with("web ")), "{}", listed.stdout);

    // Variables.
    assert_eq!(cli(&["env", "set", "GREETING=hello", "MODE=web"]).await.code, 0);
    let secret = spinikus(&config, &work, &["env", "set", "API_KEY", "--secret"], Some("s3cret\n")).await;
    assert!(secret.stdout.contains("Set API_KEY (secret) on shop/production/web."), "{}", secret.stdout);
    let variables = cli(&["env", "list", "-o", "json"]).await.json();
    let names: Vec<_> = variables.as_array().unwrap().iter().map(|variable| variable["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["API_KEY", "GREETING", "MODE"]);
    assert_eq!(variables[0]["value"], Value::Null, "secrets are write-only");
    assert_eq!(
        cli(&["env", "export"]).await.stdout,
        "# API_KEY is secret: pass --reveal to export it\nGREETING=hello\nMODE=web\n"
    );
    assert!(cli(&["env", "export", "--reveal"]).await.stdout.starts_with("API_KEY=s3cret\n"));
    std::fs::write(work.join(".env"), "# from the old host\nGREETING='hello there'\nexport EXTRA=1\n").unwrap();
    let imported = cli(&["env", "import", ".env"]).await;
    assert!(imported.stdout.contains("Updated GREETING") && imported.stdout.contains("Set EXTRA"), "{}", imported.stdout);
    assert_eq!(cli(&["env", "unset", "EXTRA"]).await.code, 0);
    assert_eq!(cli(&["env", "unset", "MISSING"]).await.code, 5);
    assert_eq!(cli(&["env", "set", "SHARED=1", "-e"]).await.code, 0, "the linked environment");
    assert!(cli(&["env", "list", "-e", "shop/production"]).await.stdout.contains("SHARED"));

    // Deploying, logs and rolling back.
    let deployed = cli(&["deploy"]).await;
    assert_eq!(deployed.code, 0, "{}", deployed.stderr);
    assert!(
        deployed.stderr.contains("Deploying shop/production/web") && deployed.stderr.contains("--> live"),
        "{}",
        deployed.stderr
    );
    assert!(deployed.stdout.contains("web is live at http://web-production-shop.localhost"), "{}", deployed.stdout);
    let container = control.store().list_server_instances(server).await.unwrap()[0].observed.container_id.clone();
    runtime.write_log(&container, false, "listening on :3000");
    runtime.write_log(&container, true, "warning: no cache");
    let logs = cli(&["logs", "-n", "10"]).await;
    assert_eq!(logs.stdout, "listening on :3000\nwarning: no cache\n");
    let second = cli(&["deploy", "-q", "-o", "json"]).await;
    assert_eq!((second.code, second.stderr.as_str()), (0, ""), "quiet");
    assert_eq!(second.json()["status"], "live");
    let rolled = cli(&["rollback"]).await;
    assert!(
        rolled.stderr.contains("Rolling shop/production/web back to dep_") && rolled.stdout.contains("web is live"),
        "{}",
        rolled.stderr
    );
    let history = cli(&["deployment", "list"]).await.stdout;
    assert_eq!(history.lines().count(), 4, "a header and three deployments: {history}");
    assert!(history.lines().nth(1).unwrap().contains("live") && history.contains("rollback"), "{history}");
    let missing = cli(&["deploy", "missing"]).await;
    assert_eq!((missing.code, missing.stderr.as_str()), (5, "error: no service `missing` in shop/production; there are: web\n"));

    // A deployment that fails: the image doesn't exist.
    let args = ["service", "create", "Broken", "--image", "ghcr.io/acme/missing:1", "--server-id", &server_id];
    assert_eq!(spinikus(&config, &work, &args, None).await.code, 0);
    let failed = cli(&["deploy", "production/broken"]).await;
    assert_eq!(failed.code, 7, "{}", failed.stderr);
    assert!(
        failed.stderr.contains("--> failed")
            && failed.stderr.contains("failed (build_failed: the container runtime failed: cannot pull"),
        "{}",
        failed.stderr
    );
    assert_eq!(failed.stdout, "");

    // Restarting what runs, through the agent; one with nothing live is refused.
    let restarted = cli(&["restart"]).await;
    assert_eq!(restarted.code, 0, "{}", restarted.stderr);
    assert!(restarted.stderr.contains("Restarting shop/production/web"), "{}", restarted.stderr);
    assert!(restarted.stdout.starts_with("web restarted (dep_"), "{}", restarted.stdout);
    let not_live = cli(&["restart", "production/broken"]).await;
    assert_eq!(not_live.code, 6, "{}", not_live.stderr);
    assert!(not_live.stderr.contains("nothing live to restart"), "{}", not_live.stderr);

    // Stopping and starting it in place, through the agent; a stopped one isn't restarted.
    let stopped = cli(&["stop"]).await;
    assert_eq!(stopped.code, 0, "{}", stopped.stderr);
    assert!(stopped.stderr.contains("Stopping shop/production/web"), "{}", stopped.stderr);
    assert_eq!(stopped.stdout, "web stopped. Start it with `spinikus start shop/production/web`.\n");
    let shown = cli(&["service", "show"]).await;
    assert!(shown.stdout.contains("State:    stopped ("), "{}", shown.stdout);
    assert!(shown.stdout.contains("start it with `spinikus start shop/production/web`)"), "{}", shown.stdout);
    assert!(!shown.stdout.contains("crashing"), "{}", shown.stdout);
    let restart_stopped = cli(&["restart"]).await;
    assert_eq!(restart_stopped.code, 6, "{}", restart_stopped.stderr);
    assert!(restart_stopped.stderr.contains("it is stopped; start it instead"), "{}", restart_stopped.stderr);
    let started = cli(&["-o", "json", "start"]).await;
    assert_eq!(started.code, 0, "{}", started.stderr);
    let service: serde_json::Value = serde_json::from_str(&started.stdout).unwrap();
    assert_eq!(service["stopped_at"], serde_json::Value::Null);

    // A command in the running container, its input piped (the in-memory terminal echoes it,
    // and `exit <code>` ends it), exiting with the command's status.
    let shell = spinikus(&config, &work, &["shell", "--", "sh", "-l"], Some("echo hi\nexit 5\n")).await;
    assert_eq!(shell.code, 5, "{}", shell.stderr);
    assert_eq!(shell.stdout, "echo hi\nexit 5\n");
    assert_eq!(shell.stderr, "", "the command's status isn't an error message");
    let opened = runtime.terminals().pop().expect("a terminal was opened");
    assert_eq!(opened.command, ["sh", "-l"]);
    let ended = spinikus(&config, &work, &["shell"], Some("true\n")).await;
    assert_eq!((ended.code, ended.stdout.as_str()), (0, "true\n"), "the end of input ends it: {}", ended.stderr);
    let not_running = cli(&["shell", "production/broken"]).await;
    assert_eq!(not_running.code, 6, "{}", not_running.stderr);

    // Domains.
    assert_eq!(cli(&["domain", "add", "www.example.com"]).await.code, 0);
    let domains = cli(&["domain", "list"]).await.stdout;
    assert!(
        domains.lines().any(|line| line.starts_with("web-production-shop.localhost") && line.contains("HTTP only")),
        "{domains}"
    );
    assert!(domains.lines().any(|line| line.starts_with("www.example.com ") && line.contains("the app")), "{domains}");
    assert_eq!(cli(&["domain", "remove", "www.example.com"]).await.code, 0);

    // Volumes.
    let added = cli(&["volume", "add", "uploads", "/app/uploads"]).await;
    assert_eq!(added.code, 0, "{}", added.stderr);
    assert!(added.stdout.contains("Added uploads at /app/uploads; deploy web to mount it."), "{}", added.stdout);
    assert_eq!(cli(&["volume", "add", "bad name", "/data"]).await.code, 6, "names are slugs");
    assert_eq!(cli(&["volume", "add", "data", "relative"]).await.code, 6, "mount paths are absolute");
    assert_eq!(cli(&["volume", "update", "uploads"]).await.code, 2, "nothing to change");
    let moved = cli(&["volume", "update", "uploads", "--mount-path", "/srv/uploads"]).await;
    assert!(moved.stdout.contains("Updated uploads at /srv/uploads"), "{}", moved.stdout);
    let volumes = cli(&["volume", "list"]).await.stdout;
    assert!(
        volumes.starts_with("NAME") && volumes.lines().nth(1).unwrap().starts_with("uploads  /srv/uploads  vol_"),
        "{volumes}"
    );
    assert_eq!(cli(&["volume", "list", "-o", "json"]).await.json()[0]["mount_path"], "/srv/uploads");
    assert_eq!(cli(&["volume", "remove", "missing"]).await.code, 5);
    let removed = cli(&["volume", "remove", "uploads"]).await;
    assert!(removed.stdout.contains("its data stays on the server"), "{}", removed.stdout);
    assert_eq!(cli(&["volume", "list"]).await.stdout, "NAME  MOUNT PATH  ID\n");

    // Databases (not waiting for this one to go live), and backups without a destination.
    let created =
        spinikus(&config, &work, &["db", "create", "Cache", "--engine", "redis", "--server-id", &server_id, "--detach"], None)
            .await;
    assert!(created.stdout.contains("use ${{ cache.REDIS_URL }} in their variables"), "{}", created.stdout);
    assert!(cli(&["service", "show", "production/cache"]).await.stdout.contains("redis:8"));
    let moved = cli(&["volume", "update", "data", "--mount-path", "/elsewhere", "--service", "production/cache"]).await;
    assert_eq!(moved.code, 6, "a database's data stays where its image keeps it: {}", moved.stderr);
    assert!(moved.stderr.contains("moving it would start the database empty"), "{}", moved.stderr);
    let removed = cli(&["volume", "remove", "data", "--service", "production/cache"]).await;
    assert_eq!(removed.code, 6, "{}", removed.stderr);
    assert!(removed.stderr.contains("deleting it would start the database empty"), "{}", removed.stderr);
    assert_eq!(cli(&["db", "backups", "production/cache"]).await.stdout, "ID  STATUS  MADE  SIZE  CREATED\n");
    let no_destination = cli(&["db", "backup", "production/cache"]).await;
    assert!(no_destination.stderr.contains("there are no storage destinations"), "{}", no_destination.stderr);
    assert_eq!(cli(&["db", "backup"]).await.stderr, "error: web is not a database\n");
    assert_eq!(cli(&["db", "create", "Copy"]).await.code, 2, "an engine, or a backup to start from");
    let unknown = cli(&["db", "create", "Copy", "--from-backup", "bkp_01J0000000000000000000000Z"]).await;
    assert_eq!(unknown.code, 5, "{}", unknown.stderr);
    let later = cli(&["db", "create", "Later", "--engine", "postgres", "--no-deploy"]).await;
    assert_eq!(later.code, 0, "{}", later.stderr);
    assert!(later.stdout.contains("not deployed. Deploy it with `spinikus deploy shop/production/later`"), "{}", later.stdout);
    assert_eq!(cli(&["deployment", "list", "production/later"]).await.stdout.lines().count(), 1, "only the header");
    let both = cli(&["db", "create", "Copy", "--no-deploy", "--from-backup", "bkp_01J0000000000000000000000Z"]).await;
    assert_eq!(both.code, 2, "a copy is deployed to restore it");

    // Tokens, and using one for CI.
    let created = cli(&["token", "create", "ci", "--role", "viewer", "--project", "shop", "-o", "json"]).await.json();
    let ci_token = created["token"].as_str().unwrap().to_string();
    let as_ci = |args: &[&str]| {
        let mut all = vec!["--server", &url, "--token", &ci_token];
        all.extend_from_slice(args);
        let all: Vec<String> = all.iter().map(|arg| arg.to_string()).collect();
        let (config, work) = (dir.path().join("ci"), dir.path().to_path_buf());
        async move { spinikus(&config, &work, &all.iter().map(String::as_str).collect::<Vec<_>>(), None).await }
    };
    assert!(as_ci(&["project", "list"]).await.stdout.contains("shop"));
    assert_eq!(as_ci(&["deploy", "shop/production/web"]).await.code, 4, "a viewer token");
    assert!(cli(&["token", "list"]).await.stdout.contains(" ci "));
    let id = created["api_token"]["id"].as_str().unwrap();
    assert_eq!(spinikus(&config, &work, &["token", "revoke", id], None).await.code, 0);
    assert_eq!(as_ci(&["project", "list"]).await.code, 3, "revoked");

    // Refusals, and signing out.
    assert_eq!(cli(&["project", "delete", "shop", "--yes"]).await.code, 6, "it still has an environment");
    assert_eq!(cli(&["project", "delete", "shop"]).await.code, 1, "no terminal to confirm on");
    assert!(cli(&["logout"]).await.stdout.contains("Signed out of http://127.0.0.1"));
    assert_eq!(cli(&["whoami"]).await.code, 3);
    assert!(cli(&["context", "list"]).await.stdout.contains("signed out"));
    let old = spinikus(&config, &work, &["--server", &url, "--token", &login_token, "whoami"], None).await;
    assert_eq!(old.code, 3, "logging out revoked the token");
}
