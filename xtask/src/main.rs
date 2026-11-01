//! Dev chores, run with `cargo xtask <task>`.
//!
//! - `css`: builds the web UI's stylesheet (`crates/spinikus-web/assets/app.css`, committed so
//!   that Cargo alone builds Spinikus) from `crates/spinikus-web/styles/app.css` with the
//!   Tailwind standalone CLI. `--watch` rebuilds on changes, `--check` fails if the committed
//!   file is out of date. The CLI is downloaded once into `target/xtask/`, pinned by version
//!   and checked against its SHA-256.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, bail};
use sha2::{Digest, Sha256};

const TAILWIND_VERSION: &str = "v4.3.3";

/// The release asset and its SHA-256 for each platform the CLI is published for.
const TAILWIND_BINARIES: &[(&str, &str, &str, &str)] = &[
    ("macos", "aarch64", "tailwindcss-macos-arm64", "cdf646702987a743464dff4d9c60fd4480d1c1e73dd819a9a67f1078815dce9d"),
    ("macos", "x86_64", "tailwindcss-macos-x64", "7922e0953f2110c05976e3bf58f14e643d90427575e766b7d433f5f80cbee7e1"),
    ("linux", "aarch64", "tailwindcss-linux-arm64", "55fd0b241214eff3de1e8ee4f22796662f2d2e7a49bcfca7477cfd0bac398195"),
    ("linux", "x86_64", "tailwindcss-linux-x64", "dc61b3ac6b8c9ca874c0cc4c57b2409791a64c5540404ca5f5367360babc313a"),
    ("windows", "x86_64", "tailwindcss-windows-x64.exe", "e0e260ce048014e9268f6237ff18f8ccf02cef521cbd0ae04e82c2cdf7aa3955"),
];

const STYLES: &str = "crates/spinikus-web/styles/app.css";
const STYLESHEET: &str = "crates/spinikus-web/assets/app.css";

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["css"] => css(Mode::Build),
        ["css", "--watch"] => css(Mode::Watch),
        ["css", "--check"] => css(Mode::Check),
        [] | ["help" | "--help" | "-h"] => {
            println!("cargo xtask css [--watch | --check]   build the web UI's stylesheet with Tailwind");
            Ok(())
        }
        _ => bail!("unknown task {args:?}; see `cargo xtask help`"),
    }
}

enum Mode {
    Build,
    Watch,
    Check,
}

fn css(mode: Mode) -> anyhow::Result<()> {
    let root = workspace_root();
    let tailwind = tailwind(&root)?;
    let output = match mode {
        Mode::Check => root.join("target/xtask/app.css"),
        Mode::Build | Mode::Watch => root.join(STYLESHEET),
    };
    let mut command = Command::new(&tailwind);
    command.current_dir(&root).args(["--input", STYLES, "--output"]).arg(&output).arg("--minify");
    if let Mode::Watch = mode {
        command.arg("--watch");
    }
    let status = command.status().with_context(|| format!("Failed to run {}", tailwind.display()))?;
    if !status.success() {
        bail!("Tailwind failed ({status})");
    }
    if let Mode::Check = mode {
        let built = fs::read(&output)?;
        let committed = fs::read(root.join(STYLESHEET)).with_context(|| format!("Failed to read {STYLESHEET}"))?;
        if built != committed {
            bail!("{STYLESHEET} is out of date; run `cargo xtask css`");
        }
        println!("{STYLESHEET} is up to date");
    }
    Ok(())
}

/// The Tailwind CLI for this platform, downloaded and checked if it isn't there yet.
fn tailwind(root: &Path) -> anyhow::Result<PathBuf> {
    let (_, _, asset, sha256) = TAILWIND_BINARIES
        .iter()
        .find(|(os, arch, _, _)| *os == env::consts::OS && *arch == env::consts::ARCH)
        .with_context(|| format!("Tailwind publishes no CLI for {} on {}", env::consts::OS, env::consts::ARCH))?;
    let dir = root.join("target/xtask");
    let path = dir.join(format!("{TAILWIND_VERSION}-{asset}"));
    if path.exists() {
        return Ok(path);
    }
    let url = format!("https://github.com/tailwindlabs/tailwindcss/releases/download/{TAILWIND_VERSION}/{asset}");
    eprintln!("Downloading {url}");
    let _ = rustls::crypto::ring::default_provider().install_default();
    let response = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?
        .get(&url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .with_context(|| format!("Failed to download {url}"))?;
    let bytes = response.bytes()?;
    let actual = hex(&Sha256::digest(&bytes));
    if actual != *sha256 {
        bail!("{url} has SHA-256 {actual}, not the pinned {sha256}");
    }
    fs::create_dir_all(&dir)?;
    let partial = dir.join(format!("{TAILWIND_VERSION}-{asset}.partial"));
    fs::write(&partial, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&partial, fs::Permissions::from_mode(0o755))?;
    }
    fs::rename(&partial, &path)?;
    Ok(path)
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("xtask is in the workspace root").to_path_buf()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
