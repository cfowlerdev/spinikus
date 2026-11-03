//! Dev chores, run with `cargo xtask <task>`.
//!
//! - `css`: builds the web UI's stylesheet (`crates/spinikus-web/assets/app.css`, committed so
//!   that Cargo alone builds Spinikus) from `crates/spinikus-web/styles/app.css` with the
//!   Tailwind standalone CLI. `--watch` rebuilds on changes, `--check` fails if the committed
//!   file is out of date. The CLI is downloaded once into `target/xtask/`, pinned by version
//!   and checked against its SHA-256.
//! - `openapi`: writes the API's OpenAPI document to `docs/openapi.json` (from the handlers'
//!   annotations, through `spinikus-api`'s `openapi` example). `--check` fails if the
//!   committed file is out of date, as `spinikus-api`'s tests do.
//! - `guide`: builds the user guide (`docs/guide`, mdBook) into `target/guide`, after
//!   checking that its links between pages point at pages that exist; `--serve` serves it
//!   on http://localhost:3000 and rebuilds on changes. mdBook is downloaded once into
//!   `target/xtask/`, pinned and checked like Tailwind.
//! - `release`: static Linux binaries in `dist/`, as GitHub releases publish them (see
//!   `release.rs`).

mod release;

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

const MDBOOK_VERSION: &str = "v0.5.4";

/// The release asset and its SHA-256 for each platform mdBook is published for.
const MDBOOK_BINARIES: &[(&str, &str, &str, &str)] = &[
    ("macos", "aarch64", "aarch64-apple-darwin", "03e8a6d8b13a2971e0b3280affd03b388373c1485e26f73407c3a76b0b1838df"),
    ("macos", "x86_64", "x86_64-apple-darwin", "a47d7bf0d5d670cff9ee6cce95537cbeb62dc10704d9e7131ffbd13e2b59a5de"),
    ("linux", "aarch64", "aarch64-unknown-linux-musl", "753e5c5c363ee8a56972344dcf91466f005a51db84a7aeffe427ae3ef83d6d44"),
    ("linux", "x86_64", "x86_64-unknown-linux-musl", "5222beabd3e37dc5be0d18ff99b79058469354db5c220153a1b92db5ba12be89"),
];

const GUIDE: &str = "docs/guide";
const GUIDE_OUTPUT: &str = "target/guide";

const STYLES: &str = "crates/spinikus-web/styles/app.css";
const STYLESHEET: &str = "crates/spinikus-web/assets/app.css";
const OPENAPI: &str = "docs/openapi.json";

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["css"] => css(Mode::Build),
        ["css", "--watch"] => css(Mode::Watch),
        ["css", "--check"] => css(Mode::Check),
        ["openapi"] => openapi(false),
        ["openapi", "--check"] => openapi(true),
        ["guide"] => guide(false),
        ["guide", "--serve"] => guide(true),
        ["release", options @ ..] => release::release(release::Options::parse(options)?),
        [] | ["help" | "--help" | "-h"] => {
            println!("cargo xtask css [--watch | --check]   build the web UI's stylesheet with Tailwind");
            println!("cargo xtask guide [--serve]           build the user guide into target/guide");
            println!("cargo xtask openapi [--check]         write the API's OpenAPI document to {OPENAPI}");
            println!(
                "cargo xtask release [--target <target>]... [--tag v<version>] [--docker]\n    \
                 build release archives of static Linux binaries into dist/"
            );
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

fn openapi(check: bool) -> anyhow::Result<()> {
    let root = workspace_root();
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .current_dir(&root)
        .args(["run", "--quiet", "--package", "spinikus-api", "--example", "openapi"])
        .output()
        .context("Failed to run cargo")?;
    if !output.status.success() {
        bail!("building the document failed ({}):\n{}", output.status, String::from_utf8_lossy(&output.stderr));
    }
    let path = root.join(OPENAPI);
    if check {
        let committed = fs::read(&path).with_context(|| format!("Failed to read {OPENAPI}"))?;
        if committed != output.stdout {
            bail!("{OPENAPI} is out of date; run `cargo xtask openapi`");
        }
        println!("{OPENAPI} is up to date");
    } else {
        fs::write(&path, &output.stdout).with_context(|| format!("Failed to write {OPENAPI}"))?;
        println!("Wrote {OPENAPI}");
    }
    Ok(())
}

/// The Tailwind CLI for this platform, downloaded and checked if it isn't there yet.
fn tailwind(root: &Path) -> anyhow::Result<PathBuf> {
    let (_, _, asset, sha256) = TAILWIND_BINARIES
        .iter()
        .find(|(os, arch, _, _)| *os == env::consts::OS && *arch == env::consts::ARCH)
        .with_context(|| format!("Tailwind publishes no CLI for {} on {}", env::consts::OS, env::consts::ARCH))?;
    let path = root.join("target/xtask").join(format!("{TAILWIND_VERSION}-{asset}"));
    if path.exists() {
        return Ok(path);
    }
    let url = format!("https://github.com/tailwindlabs/tailwindcss/releases/download/{TAILWIND_VERSION}/{asset}");
    install_executable(&path, &download(&url, sha256)?)?;
    Ok(path)
}

/// mdBook for this platform, downloaded and checked if it isn't there yet.
fn mdbook(root: &Path) -> anyhow::Result<PathBuf> {
    let (_, _, target, sha256) = MDBOOK_BINARIES
        .iter()
        .find(|(os, arch, _, _)| *os == env::consts::OS && *arch == env::consts::ARCH)
        .with_context(|| format!("mdBook publishes no binary for {} on {}", env::consts::OS, env::consts::ARCH))?;
    let path = root.join("target/xtask").join(format!("mdbook-{MDBOOK_VERSION}-{target}"));
    if path.exists() {
        return Ok(path);
    }
    let asset = format!("mdbook-{MDBOOK_VERSION}-{target}.tar.gz");
    let url = format!("https://github.com/rust-lang/mdBook/releases/download/{MDBOOK_VERSION}/{asset}");
    let archive = download(&url, sha256)?;
    let mut entries = tar::Archive::new(flate2::read::GzDecoder::new(archive.as_slice()));
    for entry in entries.entries()? {
        let mut entry = entry?;
        if entry.path()?.file_name().is_some_and(|name| name == "mdbook") {
            let mut binary = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut binary)?;
            install_executable(&path, &binary)?;
            return Ok(path);
        }
    }
    bail!("{asset} has no mdbook binary")
}

/// Downloads `url`, refusing anything but the pinned `sha256`.
fn download(url: &str, sha256: &str) -> anyhow::Result<Vec<u8>> {
    eprintln!("Downloading {url}");
    let _ = rustls::crypto::ring::default_provider().install_default();
    let response = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?
        .get(url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .with_context(|| format!("Failed to download {url}"))?;
    let bytes = response.bytes()?.to_vec();
    let actual = hex(&Sha256::digest(&bytes));
    if actual != sha256 {
        bail!("{url} has SHA-256 {actual}, not the pinned {sha256}");
    }
    Ok(bytes)
}

/// Writes an executable atomically, so an interrupted download never leaves half of one.
fn install_executable(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let dir = path.parent().expect("tools go in target/xtask");
    fs::create_dir_all(dir)?;
    let partial = path.with_extension("partial");
    fs::write(&partial, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&partial, fs::Permissions::from_mode(0o755))?;
    }
    fs::rename(&partial, path)?;
    Ok(())
}

fn guide(serve: bool) -> anyhow::Result<()> {
    let root = workspace_root();
    let src = root.join(GUIDE).join("src");
    check_guide_links(&src, None)?;
    let mdbook = mdbook(&root)?;
    let mut command = Command::new(&mdbook);
    command.current_dir(root.join(GUIDE)).arg(if serve { "serve" } else { "build" });
    let status = command.status().with_context(|| format!("Failed to run {}", mdbook.display()))?;
    if !status.success() {
        bail!("mdBook failed ({status})");
    }
    if !serve {
        check_guide_links(&src, Some(&root.join(GUIDE_OUTPUT)))?;
        println!("Built the guide into {GUIDE_OUTPUT}; open {GUIDE_OUTPUT}/index.html");
    }
    Ok(())
}

/// Fails on a link from one page to another (`[…](other.md)`, `[…](../dir/page.md#part)`)
/// whose page doesn't exist, or, given the built book, whose `#part` is no heading there:
/// mdBook would render either as a dead link.
fn check_guide_links(src: &Path, built: Option<&Path>) -> anyhow::Result<()> {
    let mut broken = Vec::new();
    let mut pages = vec![src.to_path_buf()];
    while let Some(path) = pages.pop() {
        if path.is_dir() {
            pages.extend(fs::read_dir(&path)?.map(|entry| entry.map(|entry| entry.path())).collect::<Result<Vec<_>, _>>()?);
            continue;
        }
        if path.extension().is_none_or(|extension| extension != "md") {
            continue;
        }
        let text = fs::read_to_string(&path)?;
        let page = path.strip_prefix(src).unwrap_or(&path);
        for (_, rest) in text.match_indices("](").map(|(at, _)| text.split_at(at + 2)) {
            let target = rest.split(')').next().unwrap_or_default();
            let (file, anchor) = target.split_once('#').unwrap_or((target, ""));
            if file.contains("://") || file.starts_with("mailto:") || (file.is_empty() && anchor.is_empty()) {
                continue;
            }
            // A link to a part of the same page names no file.
            let linked = if file.is_empty() { page.to_path_buf() } else { page.parent().unwrap_or(page).join(file) };
            if !src.join(&linked).exists() {
                broken.push(format!("{}: {target}", page.display()));
                continue;
            }
            if let Some(built) = built
                && !anchor.is_empty()
            {
                let html = fs::read_to_string(built.join(linked.with_extension("html")))?;
                if !html.contains(&format!("id=\"{anchor}\"")) {
                    broken.push(format!("{}: {target} (no such heading)", page.display()));
                }
            }
        }
    }
    if !broken.is_empty() {
        broken.sort();
        bail!("broken links in the guide:\n  {}", broken.join("\n  "));
    }
    Ok(())
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("xtask is in the workspace root").to_path_buf()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
