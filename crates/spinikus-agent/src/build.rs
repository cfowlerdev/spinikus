//! Building a deployment from Git: a shallow checkout with the `git` command, then the
//! settings from the service config and the checkout's `spinikus.json`, then an image
//! built by the container runtime, from the Dockerfile or, without one, from the plan
//! Railpack writes (`railpack.rs`).
//!
//! Each build has its own directory under the work directory, removed when it ends. Git
//! runs without the user's or system's Git config, never prompts, and only speaks
//! `https`, `http` and `ssh`, also for submodules. An SSH repository is cloned with the
//! service's deploy key, and a host's key is trusted the first time it is seen
//! (`known_hosts` in the agent's directory). Paths from the settings must stay inside the
//! checkout, symlinks included.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
};

use spinikus_core::{
    app_file::{APP_FILE, AppFile, MAX_APP_FILE_SIZE},
    build::{BuildSettings, BuildStrategy},
    id::{DeploymentId, ServiceId},
    service::ServiceConfig,
};
use spinikus_proto::agent::v1::{self as pb, FailureReason};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::Command,
};

use crate::{
    commands::{Progress, failure, parse_id},
    railpack,
    runtime::{BuildSpec, ContainerRuntime},
};

/// Where builds happen, and how far Git may reach.
#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// Checkouts go in a directory per deployment under this one.
    pub work_dir: PathBuf,
    /// SSH host keys, trusted on first use.
    pub known_hosts: PathBuf,
    /// Also allow local repositories (`file`), for tests.
    pub local_repositories: bool,
    /// The `railpack` command, for builds without a Dockerfile.
    pub railpack: PathBuf,
}

impl BuildOptions {
    /// Builds under the agent's own directory, with `railpack` from `$SPINIKUS_RAILPACK` or
    /// the `PATH`.
    pub fn in_dir(agent_dir: &Path) -> Self {
        Self {
            work_dir: agent_dir.join("builds"),
            known_hosts: agent_dir.join("known_hosts"),
            local_repositories: false,
            railpack: railpack::command(),
        }
    }
}

fn build_failed(message: impl Into<String>) -> pb::CommandFailed {
    failure(FailureReason::BuildFailed, message)
}

/// Checks out the repository and builds its image, returning what was prepared.
pub(crate) async fn build_from_git(
    runtime: &dyn ContainerRuntime,
    options: &BuildOptions,
    prepare: &pb::PrepareDeployment,
    git: &pb::GitSource,
    progress: &Progress,
) -> Result<pb::PreparedImage, pb::CommandFailed> {
    let deployment: DeploymentId = parse_id("deployment ID", &prepare.deployment_id)?;
    let service: ServiceId = parse_id("service ID", &prepare.service_id)?;
    let work = options.work_dir.join(deployment.to_string());
    let internal = |error: std::io::Error| failure(FailureReason::Internal, format!("preparing {}: {error}", work.display()));
    // Left over from a build the agent did not finish.
    if work.exists() {
        tokio::fs::remove_dir_all(&work).await.map_err(internal)?;
    }
    tokio::fs::create_dir_all(&work).await.map_err(internal)?;
    let result = build(runtime, options, prepare, git, &work, service, deployment, progress).await;
    if let Err(error) = tokio::fs::remove_dir_all(&work).await {
        tracing::warn!(path = %work.display(), %error, "failed to remove a build directory");
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn build(
    runtime: &dyn ContainerRuntime,
    options: &BuildOptions,
    prepare: &pb::PrepareDeployment,
    git: &pb::GitSource,
    work: &Path,
    service: ServiceId,
    deployment: DeploymentId,
    progress: &Progress,
) -> Result<pb::PreparedImage, pb::CommandFailed> {
    progress.phase("cloning");
    let repo = work.join("repo");
    let commit = checkout(options, git, work, &repo, progress).await?;
    progress.line(format!("Checked out {commit}"));

    let repo = repo.canonicalize().map_err(|error| build_failed(format!("reading the checkout: {error}")))?;
    let root_directory = Some(git.root_directory.as_str()).filter(|dir| !dir.is_empty()).unwrap_or(".");
    let root = inside(&repo, &repo, root_directory, "the root directory")?;
    let app_file = read_app_file(&repo, &root).await?;
    let config: ServiceConfig = match prepare.config_json.as_str() {
        "" => ServiceConfig::new(),
        json => serde_json::from_str(json)
            .map_err(|error| failure(FailureReason::Internal, format!("unreadable service config: {error}")))?,
    };
    let config = match &app_file {
        Some(text) => {
            let file = AppFile::parse(text).map_err(|error| build_failed(error.to_string()))?;
            progress.line(format!("Read {APP_FILE}"));
            file.apply(&config).map_err(|error| build_failed(error.to_string()))?.0
        }
        None => config,
    };
    let settings = BuildSettings::from_config(&config).map_err(|error| build_failed(format!("build settings: {error}")))?;
    let tag = spinikus_core::build::image_tag(service, deployment);
    let dockerfile_exists = tokio::fs::symlink_metadata(root.join(settings.dockerfile())).await.is_ok();
    let spec = match settings.strategy(dockerfile_exists) {
        BuildStrategy::Dockerfile => dockerfile_build(&repo, &root, settings, prepare, tag, progress)?,
        BuildStrategy::Railpack => {
            progress.phase("planning");
            let variables = &prepare.build_variables;
            let planned = railpack::plan(&options.railpack, &root, work, &settings, variables, service, progress).await?;
            BuildSpec { context: root.clone(), dockerfile: planned.plan_file, target: None, args: planned.args, tag }
        }
        strategy => return Err(build_failed(format!("the `{}` build strategy is not supported yet", strategy.as_str()))),
    };

    progress.phase("building");
    let image = runtime.build(&spec, &|line| progress.line(line)).await.map_err(|error| build_failed(error.to_string()))?;
    progress.line(format!("Built {image}"));
    Ok(pb::PreparedImage { image, commit_sha: commit, app_file: app_file.unwrap_or_default() })
}

/// What to build from the Dockerfile, with the build variables as build arguments.
fn dockerfile_build(
    repo: &Path,
    root: &Path,
    settings: BuildSettings,
    prepare: &pb::PrepareDeployment,
    tag: String,
    progress: &Progress,
) -> Result<BuildSpec, pb::CommandFailed> {
    let context = inside(repo, root, settings.context(), "the build context")?;
    let dockerfile = inside(repo, root, settings.dockerfile(), "the Dockerfile")?;
    let dockerfile = dockerfile
        .strip_prefix(&context)
        .map_err(|_| build_failed(format!("the Dockerfile `{}` is not inside the build context", settings.dockerfile())))?
        .to_string_lossy()
        .into_owned();

    // Variables win over the settings' defaults.
    let mut args: Vec<(String, String)> = settings.args.into_iter().collect();
    for variable in &prepare.build_variables {
        args.retain(|(name, _)| *name != variable.name);
        args.push((variable.name.clone(), variable.value.clone()));
    }
    progress.redact(prepare.build_variables.iter().filter(|variable| variable.secret).map(|variable| variable.value.clone()));
    Ok(BuildSpec { context, dockerfile, target: settings.target, args, tag })
}

/// `relative` under `base`, which must exist and stay inside `repo` once symlinks resolve.
fn inside(repo: &Path, base: &Path, relative: &str, what: &str) -> Result<PathBuf, pb::CommandFailed> {
    let path = base.join(relative).canonicalize().map_err(|_| build_failed(format!("{what} `{relative}` does not exist")))?;
    if !path.starts_with(repo) {
        return Err(build_failed(format!("{what} `{relative}` is outside the repository")));
    }
    Ok(path)
}

/// The text of `spinikus.json` in the root directory, if there is one.
async fn read_app_file(repo: &Path, root: &Path) -> Result<Option<String>, pb::CommandFailed> {
    if tokio::fs::symlink_metadata(root.join(APP_FILE)).await.is_err() {
        return Ok(None);
    }
    let path = inside(repo, root, APP_FILE, APP_FILE)?;
    let unreadable = |error: std::io::Error| build_failed(format!("cannot read {APP_FILE}: {error}"));
    let file = tokio::fs::File::open(&path).await.map_err(unreadable)?;
    let mut text = String::new();
    file.take(MAX_APP_FILE_SIZE as u64 + 1).read_to_string(&mut text).await.map_err(unreadable)?;
    Ok(Some(text))
}

/// Fetches the commit (or the branch's latest) into `repo`, returning the commit's SHA.
async fn checkout(
    options: &BuildOptions,
    git: &pb::GitSource,
    work: &Path,
    repo: &Path,
    progress: &Progress,
) -> Result<String, pb::CommandFailed> {
    let mut env = vec![
        ("GIT_TERMINAL_PROMPT", "0".to_string()),
        ("GIT_CONFIG_NOSYSTEM", "1".to_string()),
        ("GIT_CONFIG_GLOBAL", "/dev/null".to_string()),
        ("GIT_ALLOW_PROTOCOL", if options.local_repositories { "https:http:ssh:file" } else { "https:http:ssh" }.to_string()),
    ];
    if !git.ssh_private_key.is_empty() {
        let key = work.join("deploy_key");
        write_private(&key, &git.ssh_private_key).await.map_err(|error| failure(FailureReason::Internal, error.to_string()))?;
        if let Some(parent) = options.known_hosts.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let ssh = format!(
            "ssh -i {} -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile={}",
            quote(&key),
            quote(&options.known_hosts)
        );
        env.push(("GIT_SSH_COMMAND", ssh));
    }
    let reference = match git.commit_sha.as_str() {
        "" => format!("refs/heads/{}", git.branch),
        sha => sha.to_string(),
    };
    let repo_arg = repo.to_string_lossy();
    run_git("init", &["init", "--quiet", &repo_arg], &env, progress).await?;
    run_git("remote add", &["-C", &repo_arg, "remote", "add", "origin", &git.repo_url], &env, progress).await?;
    let fetch = ["-C", &repo_arg, "fetch", "--depth=1", "--no-tags", "origin", &reference];
    run_git("fetch", &fetch, &env, progress).await?;
    run_git("checkout", &["-C", &repo_arg, "checkout", "--quiet", "--detach", "FETCH_HEAD"], &env, progress).await?;
    if repo.join(".gitmodules").exists() {
        let update = ["-C", &repo_arg, "submodule", "update", "--init", "--recursive", "--depth=1"];
        run_git("submodule update", &update, &env, progress).await?;
    }
    let commit = run_git("rev-parse", &["-C", &repo_arg, "rev-parse", "HEAD"], &env, progress).await?;
    Ok(commit.trim().to_string())
}

/// Runs `git` (`what` names the step in errors), showing its stderr as progress, and
/// returns its stdout.
async fn run_git(what: &str, args: &[&str], env: &[(&str, String)], progress: &Progress) -> Result<String, pb::CommandFailed> {
    let mut child = Command::new("git")
        .args(args)
        .envs(env.iter().map(|(name, value)| (*name, value.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => build_failed("git is not installed on this server; install it to build from Git"),
            _ => failure(FailureReason::Internal, format!("running git: {error}")),
        })?;
    let mut stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let reading = async {
        let mut text = String::new();
        stdout.read_to_string(&mut text).await.map(|_| text)
    };
    let showing = async {
        let mut last = Vec::new();
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            progress.line(line.clone());
            last.push(line);
            if last.len() > 3 {
                last.remove(0);
            }
        }
        last
    };
    let (output, last) = tokio::join!(reading, showing);
    let status = child.wait().await.map_err(|error| failure(FailureReason::Internal, format!("running git: {error}")))?;
    if !status.success() {
        return Err(build_failed(format!("git {what} failed: {}", last.join(" ").trim())));
    }
    output.map_err(|error| failure(FailureReason::Internal, format!("reading git's output: {error}")))
}

/// Writes a file only its owner can read.
async fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).await?;
    let mut contents = contents.to_string();
    // OpenSSH rejects a key file without a final newline.
    if !contents.ends_with('\n') {
        contents.push('\n');
    }
    tokio::io::AsyncWriteExt::write_all(&mut file, contents.as_bytes()).await
}

/// A path quoted for the shell that runs `GIT_SSH_COMMAND`.
fn quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use std::process::Command as StdCommand;

    use tokio::sync::mpsc;

    use super::*;
    use crate::runtime::MemoryRuntime;

    /// Runs git in `dir` for setting up a test repository.
    fn git(dir: &Path, args: &[&str]) -> String {
        let output = StdCommand::new("git")
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main"])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    /// A repository with `files`, committed on `main`; returns its path and the commit.
    fn repository(dir: &Path, files: &[(&str, &str)]) -> (PathBuf, String) {
        let repo = dir.join("origin");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--quiet"]);
        commit(&repo, files)
    }

    fn commit(repo: &Path, files: &[(&str, &str)]) -> (PathBuf, String) {
        for (path, contents) in files {
            let path = repo.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        git(repo, &["add", "--all"]);
        git(repo, &["commit", "--quiet", "--message", "change"]);
        (repo.to_path_buf(), git(repo, &["rev-parse", "HEAD"]))
    }

    struct Setup {
        dir: tempfile::TempDir,
        options: BuildOptions,
        runtime: MemoryRuntime,
    }

    fn setup() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        // Never a `railpack` installed on this machine.
        let railpack = dir.path().join("no-railpack");
        let options = BuildOptions { local_repositories: true, railpack, ..BuildOptions::in_dir(&dir.path().join("agent")) };
        Setup { dir, options, runtime: MemoryRuntime::new() }
    }

    fn prepare(repo: &Path, root_directory: &str, config: serde_json::Value) -> (pb::PrepareDeployment, pb::GitSource) {
        let git = pb::GitSource {
            repo_url: repo.to_string_lossy().into_owned(),
            branch: "main".to_string(),
            root_directory: root_directory.to_string(),
            ..Default::default()
        };
        let prepare = pb::PrepareDeployment {
            deployment_id: DeploymentId::new().to_string(),
            service_id: ServiceId::new().to_string(),
            source: Some(pb::prepare_deployment::Source::Git(git.clone())),
            config_json: config.to_string(),
            build_variables: vec![
                pb::Variable { name: "NODE_ENV".to_string(), value: "staging".to_string(), secret: false },
                pb::Variable { name: "TOKEN".to_string(), value: "s3cret-token".to_string(), secret: true },
            ],
        };
        (prepare, git)
    }

    async fn run(
        setup: &Setup,
        prepare: &pb::PrepareDeployment,
        git: &pb::GitSource,
    ) -> (Result<pb::PreparedImage, pb::CommandFailed>, Vec<pb::CommandProgress>) {
        let (sender, mut receiver) = mpsc::channel(1024);
        let progress = Progress::new("cmd".to_string(), sender);
        let result = build_from_git(&setup.runtime, &setup.options, prepare, git, &progress).await;
        let mut reported = Vec::new();
        while let Ok(progress) = receiver.try_recv() {
            reported.push(progress);
        }
        (result, reported)
    }

    #[tokio::test]
    async fn builds_the_branch_with_its_app_file() {
        let setup = setup();
        let app_file = r#"{
            // Ships with the code.
            "build": { "dockerfile": "docker/Dockerfile", "args": { "NODE_ENV": "production", "LEVEL": "info" } },
            "runtime": { "port": 3000 }
        }"#;
        let (repo, _) = repository(
            setup.dir.path(),
            &[("README.md", "hi"), ("apps/web/docker/Dockerfile", "FROM scratch\n"), ("apps/web/spinikus.json", app_file)],
        );
        let (repo, head) = commit(&repo, &[("apps/web/docker/Dockerfile", "FROM alpine\n")]);
        let (prepare, git) = prepare(&repo, "apps/web", serde_json::json!({ "build": { "target": "app" } }));

        let (result, reported) = run(&setup, &prepare, &git).await;

        let prepared = result.unwrap();
        assert_eq!(prepared.commit_sha, head);
        assert_eq!(prepared.app_file, app_file);
        let builds = setup.runtime.builds();
        let (spec, dockerfile) = &builds[0];
        assert_eq!(dockerfile, "FROM alpine\n");
        assert!(spec.context.ends_with("apps/web"), "{:?}", spec.context);
        assert_eq!((spec.dockerfile.as_str(), spec.target.as_deref()), ("docker/Dockerfile", Some("app")));
        let args: Vec<_> = spec.args.iter().map(|(name, value)| format!("{name}={value}")).collect();
        assert_eq!(args, ["LEVEL=info", "NODE_ENV=staging", "TOKEN=s3cret-token"]);
        assert!(prepared.image.starts_with("sha256:"), "{}", prepared.image);
        assert!(spec.tag.starts_with("spinikus/svc_") && spec.tag.contains(":dep_"), "{}", spec.tag);
        assert_eq!(spec.tag, spec.tag.to_lowercase());
        let phases: Vec<_> = reported.iter().map(|progress| progress.phase.as_str()).collect();
        assert!(phases.contains(&"cloning") && phases.contains(&"building"), "{phases:?}");
        assert!(!setup.options.work_dir.join(&prepare.deployment_id).exists(), "the checkout is removed");
    }

    #[tokio::test]
    async fn builds_a_given_commit() {
        let setup = setup();
        let (repo, first) = repository(setup.dir.path(), &[("Dockerfile", "FROM one\n")]);
        commit(&repo, &[("Dockerfile", "FROM two\n")]);
        let (prepare, mut git) = prepare(&repo, "", serde_json::json!({}));
        git.commit_sha = first.clone();

        let (result, _) = run(&setup, &prepare, &git).await;

        assert_eq!(result.unwrap().commit_sha, first);
        assert_eq!(setup.runtime.builds()[0].1, "FROM one\n");
    }

    #[tokio::test]
    async fn explains_what_stops_a_build() {
        let setup = setup();
        let (repo, _) = repository(
            setup.dir.path(),
            &[("web/README.md", "no Dockerfile"), ("bad/spinikus.json", r#"{ "cron": [] }"#), ("bad/Dockerfile", "FROM a\n")],
        );
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc", repo.join("escape")).unwrap();
        #[cfg(unix)]
        commit(&repo, &[]);
        let failure = |root: &str, config: serde_json::Value| {
            let (prepare, git) = prepare(&repo, root, config);
            let setup = &setup;
            async move { run(setup, &prepare, &git).await.0.unwrap_err() }
        };

        let no_dockerfile = failure("web", serde_json::json!({})).await;
        let named_dockerfile = failure("web", serde_json::json!({ "build": { "dockerfile": "Dockerfile.prod" } })).await;
        let bad_file = failure("bad", serde_json::json!({})).await;
        let missing_root = failure("nope", serde_json::json!({})).await;
        let (prepare_branch, mut git) = prepare(&repo, "", serde_json::json!({}));
        git.branch = "nope".to_string();
        let missing_branch = run(&setup, &prepare_branch, &git).await.0.unwrap_err();

        assert_eq!(no_dockerfile.reason, FailureReason::BuildFailed as i32);
        assert!(no_dockerfile.message.contains("needs Railpack, which isn't installed"), "{}", no_dockerfile.message);
        assert!(
            named_dockerfile.message.contains("the Dockerfile `Dockerfile.prod` does not exist"),
            "{}",
            named_dockerfile.message
        );
        assert!(bad_file.message.contains("spinikus.json: unknown field `cron`"), "{}", bad_file.message);
        assert!(missing_root.message.contains("the root directory `nope` does not exist"), "{}", missing_root.message);
        assert!(missing_branch.message.starts_with("git fetch failed"), "{}", missing_branch.message);
        #[cfg(unix)]
        {
            let escaped = failure("escape", serde_json::json!({})).await;
            assert!(escaped.message.contains("outside the repository"), "{}", escaped.message);
        }
    }

    /// A stand-in for `railpack`: reports version 0.43.0, records its arguments in `args`
    /// next to itself, fails for a directory with a `FAIL` file, and otherwise writes a plan
    /// whose secrets are the `--env` names, as Railpack does.
    #[cfg(unix)]
    fn fake_railpack(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let script = r#"#!/bin/sh
if [ "$1" = --version ]; then echo "railpack version 0.43.0"; exit 0; fi
printf '%s\n' "$@" > "$(dirname "$0")/args"
if [ -f "$2/FAIL" ]; then echo "Detecting"; echo "error: no provider matched" >&2; exit 1; fi
shift 2
secrets=""
while [ $# -gt 0 ]; do
  case "$1" in
    --plan-out) plan="$2" ;;
    --env) secrets="$secrets${secrets:+,}\"${2%%=*}\"" ;;
  esac
  shift 2
done
echo "Detected Node"
printf '{"steps":[{"name":"build","secrets":["*"]}],"secrets":[%s],"deploy":{"startCommand":"node server.js"}}' "$secrets" > "$plan"
"#;
        let path = dir.join("bin/railpack");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn builds_without_a_dockerfile_from_railpacks_plan() {
        let mut setup = setup();
        setup.options.railpack = fake_railpack(setup.dir.path());
        let (repo, _) =
            repository(setup.dir.path(), &[("app/package.json", "{}"), ("broken/FAIL", ""), ("both/Dockerfile", "FROM a\n")]);
        let (mut prepare, git) = prepare(&repo, "app", serde_json::json!({ "build": { "command": "npm run build:prod" } }));
        prepare.build_variables.retain(|variable| !variable.secret);

        let (result, reported) = run(&setup, &prepare, &git).await;

        result.unwrap();
        let builds = setup.runtime.builds();
        let (spec, plan) = &builds[0];
        assert!(spec.context.ends_with("app"), "{:?}", spec.context);
        assert_eq!((spec.dockerfile.as_str(), spec.target.as_deref()), (railpack::PLAN_FILE, None));
        let syntax = ("BUILDKIT_SYNTAX".to_string(), "ghcr.io/railwayapp/railpack-frontend:v0.43.0".to_string());
        assert_eq!(spec.args, [syntax, ("cache-key".to_string(), prepare.service_id.clone())]);
        let plan: serde_json::Value = serde_json::from_str(plan).unwrap();
        assert_eq!(plan["steps"][0]["variables"], serde_json::json!({ "NODE_ENV": "staging" }));
        assert_eq!(plan.get("secrets"), None);
        assert_eq!(plan["exclude"], serde_json::json!([railpack::PLAN_FILE]));
        let args = std::fs::read_to_string(setup.dir.path().join("bin/args")).unwrap();
        assert!(args.contains("--env\nNODE_ENV=staging\n--build-cmd\nnpm run build:prod\n"), "{args}");
        let phases: Vec<_> = reported.iter().map(|progress| progress.phase.as_str()).collect();
        assert!(phases.contains(&"planning") && phases.contains(&"building"), "{phases:?}");
        let lines: Vec<_> =
            reported.iter().flat_map(|progress| progress.log_lines.iter().map(|line| line.text.as_str())).collect();
        assert!(lines.contains(&"No Dockerfile: building with Railpack 0.43.0") && lines.contains(&"Detected Node"), "{lines:?}");

        // Secret build variables have no way into a Railpack build yet.
        let (with_secret, _) = self::prepare(&repo, "app", serde_json::json!({}));
        let refused = run(&setup, &with_secret, &git).await.0.unwrap_err();
        assert!(refused.message.contains("secret build variables yet (TOKEN)"), "{}", refused.message);
        let (broken, git_broken) = self::prepare(&repo, "broken", serde_json::json!({}));
        let mut broken = broken;
        broken.build_variables.clear();
        let failed = run(&setup, &broken, &git_broken).await.0.unwrap_err();
        assert_eq!(failed.message, "railpack prepare failed: Detecting error: no provider matched");
        // `railpack` is asked for explicitly even with a Dockerfile, and a Dockerfile setting is refused.
        let (both, git_both) = self::prepare(&repo, "both", serde_json::json!({ "build": { "strategy": "railpack" } }));
        let mut both = both;
        both.build_variables.clear();
        run(&setup, &both, &git_both).await.0.unwrap();
        assert_eq!(setup.runtime.builds()[1].0.dockerfile, railpack::PLAN_FILE);
    }

    #[tokio::test]
    async fn only_remote_protocols_unless_allowed() {
        let mut setup = setup();
        setup.options.local_repositories = false;
        let (repo, _) = repository(setup.dir.path(), &[("Dockerfile", "FROM a\n")]);
        let (prepare, git) = prepare(&repo, "", serde_json::json!({}));

        let failed = run(&setup, &prepare, &git).await.0.unwrap_err();

        assert!(failed.message.starts_with("git fetch failed"), "{}", failed.message);
        assert!(setup.runtime.builds().is_empty());
    }

    #[test]
    fn quotes_paths_for_the_shell() {
        assert_eq!(quote(Path::new("/srv/it's here/key")), r"'/srv/it'\''s here/key'");
    }
}
