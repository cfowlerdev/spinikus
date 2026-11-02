//! `cargo xtask release`: static Linux binaries of `spinikus`, packaged as the install script
//! downloads them.
//!
//! For each target it builds `-p spinikus` with the `dist` profile and writes
//! `dist/spinikus-<target>.tar.gz` (the binary alone, at the archive's root), then
//! `dist/SHA256SUMS` over every archive in `dist/`. Targets are musl, so the binary runs on
//! any Linux distribution. `--tag v<version>` fails unless it matches the workspace version,
//! as the release workflow passes the pushed tag.
//!
//! Without `--docker` it runs Cargo here, for a Linux host (CI): the target's standard
//! library and a musl C compiler (`musl-tools`) must be installed. With `--docker` it builds
//! inside `rust:<toolchain>-alpine` for the target's platform (emulated if it isn't the
//! Docker host's), keeping downloaded crates in the volume `spinikus-xtask-cargo` and the
//! build in `target/docker/`.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, bail};
use sha2::{Digest, Sha256};

use crate::{hex, workspace_root};

/// The targets releases are built for, with their Docker platforms.
const TARGETS: &[(&str, &str)] = &[("x86_64-unknown-linux-musl", "linux/amd64"), ("aarch64-unknown-linux-musl", "linux/arm64")];

/// Where Docker builds keep downloaded crates between runs.
const CARGO_VOLUME: &str = "spinikus-xtask-cargo";

pub struct Options {
    pub targets: Vec<String>,
    pub tag: Option<String>,
    pub docker: bool,
}

impl Options {
    pub fn parse(args: &[&str]) -> anyhow::Result<Self> {
        let mut options = Self { targets: Vec::new(), tag: None, docker: false };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match *arg {
                "--target" => options.targets.push(args.next().context("--target needs a target")?.to_string()),
                "--tag" => options.tag = Some(args.next().context("--tag needs a tag")?.to_string()),
                "--docker" => options.docker = true,
                other => bail!("unknown option {other:?} for release"),
            }
        }
        Ok(options)
    }
}

pub fn release(options: Options) -> anyhow::Result<()> {
    let root = workspace_root();
    let version = env!("CARGO_PKG_VERSION");
    if let Some(tag) = &options.tag
        && tag.strip_prefix('v') != Some(version)
    {
        bail!("the tag {tag} is not v{version}, the workspace version in Cargo.toml");
    }
    let targets = match options.targets.is_empty() {
        true => vec![default_target(options.docker)?],
        false => options.targets.clone(),
    };
    let dist = root.join("dist");
    fs::create_dir_all(&dist)?;
    for target in &targets {
        let platform = TARGETS
            .iter()
            .find(|(known, _)| known == target)
            .map(|(_, platform)| *platform)
            .with_context(|| format!("{target} is not a release target; they are {:?}", names()))?;
        let binary = match options.docker {
            true => build_in_docker(&root, target, platform)?,
            false => build(&root, target)?,
        };
        let archive = dist.join(format!("spinikus-{target}.tar.gz"));
        package(&binary, &archive)?;
        eprintln!("Wrote {}", archive.display());
    }
    let sums = checksums(&dist)?;
    eprintln!("Wrote {} (version {version})", sums.display());
    Ok(())
}

fn names() -> Vec<&'static str> {
    TARGETS.iter().map(|(target, _)| *target).collect()
}

/// The target for this machine, or for the Docker host's architecture.
fn default_target(docker: bool) -> anyhow::Result<String> {
    let arch = match docker {
        true => {
            let output = Command::new("docker")
                .args(["version", "--format", "{{.Server.Arch}}"])
                .output()
                .context("Failed to run docker")?;
            if !output.status.success() {
                bail!("docker version failed: {}", String::from_utf8_lossy(&output.stderr).trim());
            }
            match String::from_utf8_lossy(&output.stdout).trim() {
                "amd64" => "x86_64",
                "arm64" | "aarch64" => "aarch64",
                other => bail!("Docker runs on {other}, which has no release target; pass --target"),
            }
        }
        false if std::env::consts::OS != "linux" => {
            bail!("release binaries are built for Linux; use --docker, or run on Linux")
        }
        false => std::env::consts::ARCH,
    };
    Ok(format!("{arch}-unknown-linux-musl"))
}

fn build(root: &Path, target: &str) -> anyhow::Result<PathBuf> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    run(Command::new(cargo).current_dir(root).args([
        "build",
        "--locked",
        "--profile",
        "dist",
        "-p",
        "spinikus",
        "--target",
        target,
    ]))?;
    Ok(root.join("target").join(target).join("dist/spinikus"))
}

fn build_in_docker(root: &Path, target: &str, platform: &str) -> anyhow::Result<PathBuf> {
    let image = format!("rust:{}-alpine", toolchain(root)?);
    let script =
        format!("apk add --no-cache musl-dev >/dev/null && cargo build --locked --profile dist -p spinikus --target {target}");
    run(Command::new("docker").args([
        "run",
        "--rm",
        "--platform",
        platform,
        "--volume",
        &format!("{}:/src", root.display()),
        "--volume",
        &format!("{CARGO_VOLUME}:/usr/local/cargo/registry"),
        "--env",
        "CARGO_TARGET_DIR=/src/target/docker",
        "--workdir",
        "/src",
        &image,
        "sh",
        "-c",
        &script,
    ]))?;
    Ok(root.join("target/docker").join(target).join("dist/spinikus"))
}

/// The channel in `rust-toolchain.toml`.
fn toolchain(root: &Path) -> anyhow::Result<String> {
    let file = fs::read_to_string(root.join("rust-toolchain.toml")).context("Failed to read rust-toolchain.toml")?;
    file.lines()
        .find_map(|line| line.trim().strip_prefix("channel"))
        .and_then(|rest| rest.split('"').nth(1))
        .map(str::to_string)
        .context("rust-toolchain.toml names no channel")
}

fn run(command: &mut Command) -> anyhow::Result<()> {
    eprintln!("Running {command:?}");
    let status = command.status().with_context(|| format!("Failed to run {:?}", command.get_program()))?;
    if !status.success() {
        bail!("{:?} failed: {status}", command.get_program());
    }
    Ok(())
}

/// A gzipped tar with the binary as `spinikus`, mode 0755, owned by root, with a fixed time,
/// so the same binary gives the same archive.
fn package(binary: &Path, archive: &Path) -> anyhow::Result<()> {
    let bytes = fs::read(binary).with_context(|| format!("Failed to read {}", binary.display()))?;
    let file = fs::File::create(archive)?;
    let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(file, flate2::Compression::best()));
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o755);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_entry_type(tar::EntryType::Regular);
    tar.append_data(&mut header, "spinikus", bytes.as_slice())?;
    tar.into_inner()?.finish()?.flush()?;
    Ok(())
}

/// `SHA256SUMS` over the archives in `dist`, in the format `sha256sum -c` reads.
fn checksums(dist: &Path) -> anyhow::Result<PathBuf> {
    let mut archives: Vec<_> = fs::read_dir(dist)?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("spinikus-") && name.ends_with(".tar.gz"))
        .collect();
    archives.sort();
    let mut sums = String::new();
    for name in archives {
        let digest = Sha256::digest(fs::read(dist.join(&name))?);
        sums.push_str(&format!("{}  {name}\n", hex(&digest)));
    }
    let path = dist.join("SHA256SUMS");
    fs::write(&path, sums)?;
    Ok(path)
}
