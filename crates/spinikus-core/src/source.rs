//! Where a service's code or image comes from.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::id::{ServiceId, SourceId};

/// An SSH key a service clones its Git repository with, for private repositories. Add the
/// public key to the repository as a read-only deploy key. The private key is stored
/// sealed and only ever sent to the agent that builds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DeployKey {
    pub service_id: ServiceId,
    /// In OpenSSH format: `ssh-ed25519 AAAA… spinikus`.
    pub public_key: String,
    /// `SHA256:…`, as `ssh-keygen -l` and Git hosts show it.
    pub fingerprint: String,
    pub created_at: Timestamp,
}

/// Longest repo URL, branch, root directory or image reference we accept.
const MAX_LEN: usize = 1024;

/// A service's source. Each service has at most one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Source {
    pub id: SourceId,
    pub service_id: ServiceId,
    #[serde(flatten)]
    pub spec: SourceSpec,
    pub revision: i64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceSpec {
    /// Build from a Git repository.
    Git(GitSource),
    /// Deploy a prebuilt image.
    Image(ImageSource),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct GitSource {
    /// `https://`, `http://` or `ssh://` URL, or scp-style `git@host:owner/repo.git`.
    pub repo_url: String,
    pub branch: String,
    /// Directory inside the repo to build from, for monorepos. `None` means the repo root.
    pub root_directory: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ImageSource {
    /// Image reference, such as `ghcr.io/acme/web:1.4` or `postgres:17`.
    pub image: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid {field} `{value}`: {problem}")]
pub struct SourceError {
    pub field: &'static str,
    pub value: String,
    pub problem: &'static str,
}

impl GitSource {
    /// Whether the repository is cloned over SSH, which needs a deploy key.
    pub fn uses_ssh(&self) -> bool {
        self.repo_url.starts_with("ssh://") || !self.repo_url.contains("://")
    }
}

impl SourceSpec {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Git(_) => "git",
            Self::Image(_) => "image",
        }
    }

    /// Checks the fields for obvious mistakes. Whether the repo or image actually
    /// exists is only known when the agent clones or pulls it.
    pub fn validate(&self) -> Result<(), SourceError> {
        match self {
            Self::Git(git) => {
                check("repo_url", &git.repo_url, repo_url_problem)?;
                check("branch", &git.branch, branch_problem)?;
                match &git.root_directory {
                    Some(dir) => check("root_directory", dir, relative_path_problem),
                    None => Ok(()),
                }
            }
            Self::Image(image) => check("image", &image.image, image_problem),
        }
    }
}

fn check(field: &'static str, value: &str, problem: fn(&str) -> Option<&'static str>) -> Result<(), SourceError> {
    let problem = if value.is_empty() {
        Some("it is empty")
    } else if value.len() > MAX_LEN {
        Some("it is too long")
    } else if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        Some("it contains whitespace or control characters")
    } else {
        problem(value)
    };

    match problem {
        Some(problem) => Err(SourceError { field, value: value.to_string(), problem }),
        None => Ok(()),
    }
}

fn repo_url_problem(url: &str) -> Option<&'static str> {
    // Git refuses such hosts itself; refusing them here too keeps them away from `git` and `ssh`.
    if url.starts_with('-') || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Some("it cannot start with `-` or contain spaces or control characters");
    }
    if let Some(rest) = ["https://", "http://", "ssh://"].iter().find_map(|scheme| url.strip_prefix(scheme)) {
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        return (host.is_empty() || path.is_empty()).then_some("it needs a host and a repository path");
    }
    // scp-style `user@host:path`.
    let scp_style = url
        .split_once(':')
        .and_then(|(user_host, path)| Some((user_host.split_once('@')?, path)))
        .is_some_and(|((user, host), path)| !user.is_empty() && !host.is_empty() && !path.is_empty());
    (!scp_style).then_some("use an https://, http:// or ssh:// URL, or user@host:path")
}

/// A subset of `git check-ref-format` rules.
fn branch_problem(branch: &str) -> Option<&'static str> {
    if branch.starts_with('-') || branch.starts_with('/') || branch.ends_with('/') || branch.ends_with('.') {
        Some("it cannot start with `-` or `/`, or end with `/` or `.`")
    } else if branch.ends_with(".lock") {
        Some("it cannot end with `.lock`")
    } else if ["..", "//", "@{"].iter().any(|bad| branch.contains(bad)) {
        Some("it cannot contain `..`, `//` or `@{`")
    } else if branch.contains(['~', '^', ':', '?', '*', '[', '\\']) {
        Some("it cannot contain `~ ^ : ? * [ \\`")
    } else {
        None
    }
}

/// Why `path` is not a relative path inside the repository, if it is not.
pub(crate) fn relative_path_problem(dir: &str) -> Option<&'static str> {
    if dir.starts_with('/') || dir.contains('\\') {
        Some("it must be a relative path using `/`")
    } else if dir.split('/').any(|part| part == "..") {
        Some("it cannot leave the repository with `..`")
    } else {
        None
    }
}

fn image_problem(image: &str) -> Option<&'static str> {
    if image.starts_with(['-', ':', '/', '@']) || image.ends_with([':', '/', '@']) {
        Some("it is not an image reference")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(repo_url: &str, branch: &str, root_directory: Option<&str>) -> SourceSpec {
        SourceSpec::Git(GitSource {
            repo_url: repo_url.to_string(),
            branch: branch.to_string(),
            root_directory: root_directory.map(str::to_string),
        })
    }

    fn image(image: &str) -> SourceSpec {
        SourceSpec::Image(ImageSource { image: image.to_string() })
    }

    fn invalid_field(spec: SourceSpec) -> &'static str {
        spec.validate().expect_err("expected an invalid source").field
    }

    #[test]
    fn accepts_common_repo_urls() {
        for url in [
            "https://github.com/acme/web.git",
            "http://gitea.lan/acme/web",
            "ssh://git@gitlab.com:2222/acme/web.git",
            "git@github.com:acme/web.git",
        ] {
            assert_eq!(git(url, "main", None).validate(), Ok(()), "{url}");
        }
    }

    #[test]
    fn rejects_bad_repo_urls() {
        for url in
            ["", "github.com/acme/web", "https://github.com", "ftp://host/repo", "@host:repo", "git@host:", "https://host/a b"]
                .into_iter()
                .chain(["-oProxyCommand=touch /tmp/x@host:repo", "git@host:repo\n", "https://host/repo\tx"])
        {
            assert_eq!(invalid_field(git(url, "main", None)), "repo_url", "{url}");
        }
    }

    #[test]
    fn checks_branch_names() {
        for branch in ["main", "release/1.4", "feature/x-y_z", "v1.0"] {
            assert_eq!(git("https://h/r", branch, None).validate(), Ok(()), "{branch}");
        }
        for branch in ["", "-x", "/x", "x/", "x.", "x.lock", "a..b", "a//b", "a@{1}", "a~1", "a:b", "a b"] {
            assert_eq!(invalid_field(git("https://h/r", branch, None)), "branch", "{branch}");
        }
    }

    #[test]
    fn checks_root_directories() {
        for dir in ["apps/web", "web", "./web"] {
            assert_eq!(git("https://h/r", "main", Some(dir)).validate(), Ok(()), "{dir}");
        }
        for dir in ["", "/apps", "../other", "apps/../../x", "apps\\web"] {
            assert_eq!(invalid_field(git("https://h/r", "main", Some(dir))), "root_directory", "{dir}");
        }
    }

    #[test]
    fn checks_image_references() {
        for reference in ["postgres:17", "ghcr.io/acme/web:1.4", "registry.lan:5000/web@sha256:abc"] {
            assert_eq!(image(reference).validate(), Ok(()), "{reference}");
        }
        for reference in ["", ":17", "web:", "-web", "a b"] {
            assert_eq!(invalid_field(image(reference)), "image", "{reference}");
        }
    }

    #[test]
    fn serializes_with_a_type_tag() {
        let spec = git("https://h/r", "main", None);
        let json = serde_json::to_value(&spec).unwrap();

        assert_eq!(
            json,
            serde_json::json!({ "type": "git", "repo_url": "https://h/r", "branch": "main", "root_directory": null })
        );
        assert_eq!(serde_json::from_value::<SourceSpec>(json).unwrap(), spec);
    }
}
