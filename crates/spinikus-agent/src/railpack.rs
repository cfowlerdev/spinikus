//! Building without a Dockerfile: `railpack prepare` detects the language and framework
//! and writes a build plan, and BuildKit builds the plan with Railpack's frontend image,
//! through the same runtime build as a Dockerfile. The plan takes the Dockerfile's place,
//! the `BUILDKIT_SYNTAX` build argument names the frontend (the version matching the
//! `railpack` that wrote the plan), and `cache-key` keeps each service's build caches apart.
//!
//! Railpack gives build steps the build variables as BuildKit secrets (the plan lists their
//! names), so every build variable is one, and `secrets-hash` (a digest of their values)
//! makes a change to them invalidate the cache.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use spinikus_core::{build::BuildSettings, id::ServiceId};
use spinikus_proto::agent::v1::{self as pb, FailureReason};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::Command,
};

use crate::commands::{Progress, failure};

/// The plan, written into the build context under a name of ours, and excluded from the
/// files the plan copies into the image.
pub const PLAN_FILE: &str = ".spinikus-railpack-plan.json";

/// Railpack's BuildKit frontend; each Railpack release has one with its version as the tag.
pub const FRONTEND: &str = "ghcr.io/railwayapp/railpack-frontend";

/// Where `railpack` is, if not on the `PATH`.
pub const RAILPACK_VAR: &str = "SPINIKUS_RAILPACK";

/// How many of the last output lines explain a failed `railpack prepare`.
const LAST_LINES: usize = 5;

fn build_failed(message: impl Into<String>) -> pb::CommandFailed {
    failure(FailureReason::BuildFailed, message)
}

/// The `railpack` command: `$SPINIKUS_RAILPACK`, else `railpack` from the `PATH`.
pub fn command() -> PathBuf {
    std::env::var_os(RAILPACK_VAR).filter(|value| !value.is_empty()).map_or_else(|| PathBuf::from("railpack"), PathBuf::from)
}

/// What the runtime builds instead of a Dockerfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    /// The plan, relative to the context (the root directory).
    pub plan_file: String,
    /// The frontend and cache key, as build arguments.
    pub args: Vec<(String, String)>,
}

/// Writes the build plan for `root` into it, using `work` for Railpack's own files.
pub(crate) async fn plan(
    railpack: &Path,
    root: &Path,
    work: &Path,
    settings: &BuildSettings,
    variables: &[pb::Variable],
    service: ServiceId,
    progress: &Progress,
) -> Result<Planned, pb::CommandFailed> {
    let version = version(railpack).await?;
    if !settings.args.is_empty() {
        return Err(build_failed("build.args are for Dockerfile builds; set build variables instead"));
    }
    progress.redact(variables.iter().filter(|variable| variable.secret).map(|variable| variable.value.clone()));
    progress.line(format!("No Dockerfile: building with Railpack {version}"));

    let plan_out = work.join("railpack-plan.json");
    let mut args: Vec<String> = vec![
        "prepare".into(),
        root.to_string_lossy().into_owned(),
        "--plan-out".into(),
        plan_out.to_string_lossy().into_owned(),
        "--info-out".into(),
        work.join("railpack-info.json").to_string_lossy().into_owned(),
    ];
    for variable in variables {
        args.extend(["--env".to_string(), format!("{}={}", variable.name, variable.value)]);
    }
    if let Some(command) = &settings.command {
        args.extend(["--build-cmd".to_string(), command.clone()]);
    }
    run(railpack, &args, progress).await?;

    let text = tokio::fs::read_to_string(&plan_out)
        .await
        .map_err(|error| build_failed(format!("Railpack wrote no build plan: {error}")))?;
    let plan: Value = serde_json::from_str(&text).map_err(|error| build_failed(format!("Railpack's build plan: {error}")))?;
    let plan = rewrite(plan, variables)?;
    let path = root.join(PLAN_FILE);
    let json = serde_json::to_vec_pretty(&plan).expect("a JSON value serializes");
    tokio::fs::write(&path, json)
        .await
        .map_err(|error| failure(FailureReason::Internal, format!("writing the build plan: {error}")))?;
    Ok(Planned {
        plan_file: PLAN_FILE.to_string(),
        args: vec![
            ("BUILDKIT_SYNTAX".to_string(), format!("{FRONTEND}:v{version}")),
            ("cache-key".to_string(), service.to_string()),
            ("secrets-hash".to_string(), secrets_hash(variables)),
        ],
    })
}

/// Checks that the plan needs no secrets but the build variables, and leaves the plan file
/// out of the image.
fn rewrite(mut plan: Value, variables: &[pb::Variable]) -> Result<Value, pb::CommandFailed> {
    let object = plan.as_object_mut().ok_or_else(|| build_failed("Railpack's build plan is not an object"))?;
    // Railpack only declares secrets for what it was given, and it was given only these.
    let secrets = object.get("secrets").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str);
    let unknown: Vec<&str> = secrets.filter(|name| !variables.iter().any(|variable| variable.name == *name)).collect();
    if !unknown.is_empty() {
        return Err(build_failed(format!("Railpack's build plan needs secrets Spinikus can't give it: {}", unknown.join(", "))));
    }
    let exclude = object.entry("exclude").or_insert_with(|| json!([]));
    if let Some(exclude) = exclude.as_array_mut() {
        exclude.push(json!(PLAN_FILE));
    }
    Ok(plan)
}

/// A digest of the build variables' names and values, in name order.
fn secrets_hash(variables: &[pb::Variable]) -> String {
    let mut sorted: Vec<&pb::Variable> = variables.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let mut digest = Sha256::new();
    for variable in sorted {
        digest.update(variable.name.as_bytes());
        digest.update([0]);
        digest.update(variable.value.as_bytes());
        digest.update([0]);
    }
    digest.finalize().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The version `railpack --version` reports, such as `0.43.0`.
async fn version(railpack: &Path) -> Result<String, pb::CommandFailed> {
    let output = Command::new(railpack)
        .arg("--version")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| not_runnable(railpack, error))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let version = text.split_whitespace().last().map(|version| version.trim_start_matches('v')).unwrap_or_default();
    if !output.status.success() || !version.chars().next().is_some_and(|char| char.is_ascii_digit()) {
        return Err(build_failed(format!("`{} --version` answered `{}`", railpack.display(), text.trim())));
    }
    Ok(version.to_string())
}

fn not_runnable(railpack: &Path, error: std::io::Error) -> pb::CommandFailed {
    match error.kind() {
        std::io::ErrorKind::NotFound => build_failed(format!(
            "there is no Dockerfile, and building without one needs Railpack, which isn't installed on this server \
             (install it from https://railpack.com, or set {RAILPACK_VAR} to where `{}` is)",
            railpack.display()
        )),
        _ => failure(FailureReason::Internal, format!("running {}: {error}", railpack.display())),
    }
}

/// Runs `railpack`, showing its output as progress.
async fn run(railpack: &Path, args: &[String], progress: &Progress) -> Result<(), pb::CommandFailed> {
    let mut child = Command::new(railpack)
        .args(args)
        .env("NO_COLOR", "1")
        .env_remove("FORCE_COLOR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| not_runnable(railpack, error))?;
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let (mut out, err) = tokio::join!(show(stdout, progress), show(stderr, progress));
    let status = child.wait().await.map_err(|error| failure(FailureReason::Internal, format!("running railpack: {error}")))?;
    if status.success() {
        return Ok(());
    }
    out.extend(err);
    let start = out.len().saturating_sub(LAST_LINES);
    Err(build_failed(format!("railpack prepare failed: {}", out[start..].join(" ").trim())))
}

/// Sends each line to progress, and returns the last few that weren't blank.
async fn show(output: impl AsyncRead + Unpin, progress: &Progress) -> Vec<String> {
    let mut last = Vec::new();
    let mut lines = BufReader::new(output).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        progress.line(line.clone());
        if !line.trim().is_empty() {
            last.push(line.trim().to_string());
            if last.len() > LAST_LINES {
                last.remove(0);
            }
        }
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variable(name: &str, value: &str) -> pb::Variable {
        pb::Variable { name: name.to_string(), value: value.to_string(), secret: false }
    }

    #[test]
    fn the_plan_keeps_its_secrets_and_leaves_itself_out() {
        let plan = json!({
            "steps": [{ "name": "install", "secrets": ["*"], "variables": { "CI": "true" } }],
            "secrets": ["NODE_ENV", "RAILPACK_NODE_VERSION"],
            "deploy": { "startCommand": "node server.js" },
        });
        let variables = [variable("NODE_ENV", "production"), variable("RAILPACK_NODE_VERSION", "22")];

        let rewritten = rewrite(plan.clone(), &variables).unwrap();

        assert_eq!(rewritten["secrets"], plan["secrets"]);
        assert_eq!(rewritten["steps"], plan["steps"]);
        assert_eq!(rewritten["exclude"], json!([PLAN_FILE]));
        let unknown = rewrite(json!({ "secrets": ["GITHUB_TOKEN"] }), &variables).unwrap_err();
        assert!(unknown.message.ends_with("can't give it: GITHUB_TOKEN"), "{}", unknown.message);
    }

    #[test]
    fn the_secrets_hash_changes_with_the_values_only() {
        let hash = secrets_hash(&[variable("A", "1"), variable("B", "2")]);

        assert_eq!(hash, secrets_hash(&[variable("B", "2"), variable("A", "1")]));
        assert_ne!(hash, secrets_hash(&[variable("A", "1"), variable("B", "3")]));
        assert_ne!(secrets_hash(&[variable("A", "1=")]), secrets_hash(&[variable("A=1", "")]));
        assert_eq!(hash.len(), 64);
    }
}
