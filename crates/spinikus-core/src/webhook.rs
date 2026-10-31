//! Inbound Git webhooks: one per service, which deploys the pushed commit when a push to the
//! service's branch arrives. The repository's host sends it to
//! `/hooks/git/<provider>/<webhook ID>`, signed with the webhook's secret.
//!
//! GitHub, GitLab and Gitea (Forgejo too, which sends Gitea's headers and payload) are
//! understood. Their push payloads share a shape: `ref`, `before`, `after`, and `commits`
//! listing the files each commit added, modified and removed. That list is how a monorepo
//! service is deployed only when something under its root directory changed; when it may be
//! incomplete, the push deploys anyway.

use std::{fmt, str::FromStr};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::id::{ServiceId, WebhookId};

/// The host of a repository, which decides how its webhooks are signed and shaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GitProvider {
    GitHub,
    GitLab,
    /// Gitea and Forgejo.
    Gitea,
}

impl GitProvider {
    pub const ALL: [GitProvider; 3] = [Self::GitHub, Self::GitLab, Self::Gitea];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::GitHub => "github",
            Self::GitLab => "gitlab",
            Self::Gitea => "gitea",
        }
    }

    /// The header naming the kind of event.
    pub fn event_header(self) -> &'static str {
        match self {
            Self::GitHub => "x-github-event",
            Self::GitLab => "x-gitlab-event",
            Self::Gitea => "x-gitea-event",
        }
    }

    /// The header proving the delivery comes from the webhook: an HMAC-SHA256 of the body
    /// (GitHub: `sha256=<hex>`, Gitea: `<hex>`), or the secret itself (GitLab).
    pub fn signature_header(self) -> &'static str {
        match self {
            Self::GitHub => "x-hub-signature-256",
            Self::GitLab => "x-gitlab-token",
            Self::Gitea => "x-gitea-signature",
        }
    }

    /// What the event header says for a push to a branch or tag.
    pub fn is_push(self, event: &str) -> bool {
        match self {
            Self::GitHub | Self::Gitea => event == "push",
            Self::GitLab => event == "Push Hook",
        }
    }

    /// GitHub's `ping`, sent when a webhook is added.
    pub fn is_ping(self, event: &str) -> bool {
        self == Self::GitHub && event == "ping"
    }
}

impl fmt::Display for GitProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown Git provider `{0}`: use github, gitlab, gitea or forgejo")]
pub struct GitProviderError(String);

impl FromStr for GitProvider {
    type Err = GitProviderError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "github" => Ok(Self::GitHub),
            "gitlab" => Ok(Self::GitLab),
            "gitea" | "forgejo" => Ok(Self::Gitea),
            _ => Err(GitProviderError(value.to_string())),
        }
    }
}

/// Where each provider sends a webhook's deliveries, relative to the instance's URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WebhookPaths {
    pub github: String,
    pub gitlab: String,
    /// For Gitea and Forgejo.
    pub gitea: String,
}

impl WebhookPaths {
    pub fn new(id: WebhookId) -> Self {
        let path = |provider: GitProvider| format!("/hooks/git/{provider}/{id}");
        Self { github: path(GitProvider::GitHub), gitlab: path(GitProvider::GitLab), gitea: path(GitProvider::Gitea) }
    }
}

/// A service's webhook. Its secret is shown once, when it is made, and never again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Webhook {
    pub id: WebhookId,
    pub service_id: ServiceId,
    pub paths: WebhookPaths,
    pub created_at: Timestamp,
    /// When the last delivery arrived, signed or not.
    pub last_delivery_at: Option<Timestamp>,
    /// What the last delivery did, such as `deployed dep_…` or why it deployed nothing.
    pub last_delivery: Option<String>,
}

/// What a push changed, as far as Spinikus needs to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Push {
    /// The branch pushed to, or `None` for a tag.
    pub branch: Option<String>,
    /// The commit the branch now points at, or `None` if the push deleted it.
    pub commit: Option<String>,
    /// Every path the pushed commits added, modified or removed, or `None` if the payload
    /// may not list them all: a forced push, a new branch, or more commits than it lists.
    pub changed_paths: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("not a push payload: {0}")]
pub struct PushError(String);

/// The fields of a push payload Spinikus reads; GitHub, GitLab and Gitea share them.
#[derive(Deserialize)]
struct Payload {
    #[serde(rename = "ref")]
    reference: String,
    #[serde(default)]
    before: String,
    after: String,
    /// GitHub.
    #[serde(default)]
    deleted: bool,
    /// GitHub.
    #[serde(default)]
    forced: bool,
    #[serde(default)]
    commits: Vec<PayloadCommit>,
    /// GitLab.
    total_commits_count: Option<usize>,
    /// Gitea.
    total_commits: Option<usize>,
}

#[derive(Deserialize)]
struct PayloadCommit {
    #[serde(default)]
    added: Vec<String>,
    #[serde(default)]
    modified: Vec<String>,
    #[serde(default)]
    removed: Vec<String>,
}

/// GitHub lists at most this many commits.
const GITHUB_COMMIT_LIMIT: usize = 2048;

impl Push {
    pub fn parse(body: &[u8]) -> Result<Self, PushError> {
        let payload: Payload = serde_json::from_slice(body).map_err(|error| PushError(error.to_string()))?;
        let zero = |sha: &str| !sha.is_empty() && sha.bytes().all(|byte| byte == b'0');
        let commit = match payload.deleted || zero(&payload.after) {
            true => None,
            false if is_commit(&payload.after) => Some(payload.after.to_ascii_lowercase()),
            false => return Err(PushError(format!("`after` is not a commit: `{}`", payload.after))),
        };
        let total = payload.total_commits_count.or(payload.total_commits);
        let complete = !payload.forced
            && !zero(&payload.before)
            && !payload.commits.is_empty()
            && payload.commits.len() < GITHUB_COMMIT_LIMIT
            && total.is_none_or(|total| total <= payload.commits.len());
        let changed_paths = complete.then(|| {
            let mut paths: Vec<String> = payload
                .commits
                .into_iter()
                .flat_map(|commit| commit.added.into_iter().chain(commit.modified).chain(commit.removed))
                .collect();
            paths.sort();
            paths.dedup();
            paths
        });
        let branch = payload.reference.strip_prefix("refs/heads/").map(str::to_string);
        Ok(Self { branch, commit, changed_paths })
    }

    /// Whether the push may have changed something under `root` (a relative directory, the
    /// whole repository if `None`). Unknown changes count as changes.
    pub fn touches(&self, root: Option<&str>) -> bool {
        let (Some(paths), Some(root)) = (&self.changed_paths, root.map(|root| root.trim_matches('/'))) else {
            return true;
        };
        root.is_empty()
            || paths.iter().any(|path| path.strip_prefix(root).is_some_and(|rest| rest.is_empty() || rest.starts_with('/')))
    }
}

/// A full SHA-1 or SHA-256 commit ID.
fn is_commit(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64) && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const BEFORE: &str = "1111111111111111111111111111111111111111";
    const AFTER: &str = "2222222222222222222222222222222222222222";
    const ZERO: &str = "0000000000000000000000000000000000000000";

    fn parse(payload: serde_json::Value) -> Push {
        Push::parse(payload.to_string().as_bytes()).unwrap()
    }

    #[test]
    fn reads_a_github_push() {
        let push = parse(json!({
            "ref": "refs/heads/main", "before": BEFORE, "after": AFTER, "deleted": false, "forced": false,
            "commits": [
                { "added": ["apps/web/new.ts"], "modified": ["README.md"], "removed": [] },
                { "added": [], "modified": ["apps/web/new.ts"], "removed": ["old.txt"] },
            ],
            "repository": { "full_name": "acme/shop" },
        }));

        assert_eq!(push.branch.as_deref(), Some("main"));
        assert_eq!(push.commit.as_deref(), Some(AFTER));
        assert_eq!(push.changed_paths.unwrap(), ["README.md", "apps/web/new.ts", "old.txt"]);
    }

    #[test]
    fn reads_gitlab_and_gitea_pushes_and_their_limits() {
        let commits = json!([{ "added": [], "modified": ["api/main.go"], "removed": [] }]);
        let gitlab = parse(json!({
            "object_kind": "push", "ref": "refs/heads/dev", "before": BEFORE, "after": AFTER,
            "total_commits_count": 1, "commits": commits,
        }));
        let gitea_truncated =
            parse(json!({ "ref": "refs/heads/dev", "before": BEFORE, "after": AFTER, "total_commits": 30, "commits": commits }));

        assert_eq!((gitlab.branch.as_deref(), gitlab.changed_paths.unwrap()), (Some("dev"), vec!["api/main.go".to_string()]));
        assert_eq!(gitea_truncated.changed_paths, None);
    }

    #[test]
    fn knows_when_the_changes_are_unknown_and_when_a_branch_goes() {
        let one = json!([{ "modified": ["a"] }]);
        let forced = parse(json!({ "ref": "refs/heads/main", "before": BEFORE, "after": AFTER, "forced": true, "commits": one }));
        let new_branch = parse(json!({ "ref": "refs/heads/main", "before": ZERO, "after": AFTER, "commits": one }));
        let no_commits = parse(json!({ "ref": "refs/heads/main", "before": BEFORE, "after": AFTER, "commits": [] }));
        let deleted = parse(json!({ "ref": "refs/heads/old", "before": BEFORE, "after": ZERO, "deleted": true }));
        let tag = parse(json!({ "ref": "refs/tags/v1", "before": ZERO, "after": AFTER }));

        for push in [&forced, &new_branch, &no_commits] {
            assert_eq!(push.changed_paths, None, "{push:?}");
        }
        assert_eq!((deleted.branch.as_deref(), deleted.commit), (Some("old"), None));
        assert_eq!((tag.branch, tag.commit.as_deref()), (None, Some(AFTER)));
        assert!(Push::parse(br#"{ "ref": "refs/heads/main", "after": "HEAD" }"#).is_err());
        assert!(Push::parse(b"{ \"zen\": \"Keep it logically awesome.\" }").is_err());
    }

    #[test]
    fn touches_only_the_root_directory_it_changed() {
        let push = |paths: &[&str]| Push {
            branch: Some("main".to_string()),
            commit: Some(AFTER.to_string()),
            changed_paths: Some(paths.iter().map(|path| path.to_string()).collect()),
        };
        let web = push(&["apps/web/src/index.ts", "README.md"]);

        assert!(web.touches(Some("apps/web")) && web.touches(Some("/apps/web/")) && web.touches(None));
        assert!(!web.touches(Some("apps/api")) && !web.touches(Some("apps/we")));
        assert!(push(&["apps/web"]).touches(Some("apps/web")));
        assert!(Push { changed_paths: None, ..web }.touches(Some("apps/api")));
    }

    #[test]
    fn names_providers_and_their_paths() {
        let id = WebhookId::new();

        assert_eq!("forgejo".parse(), Ok(GitProvider::Gitea));
        assert!("bitbucket".parse::<GitProvider>().is_err());
        assert_eq!(WebhookPaths::new(id).gitlab, format!("/hooks/git/gitlab/{id}"));
        for provider in GitProvider::ALL {
            assert_eq!(provider.as_str().parse(), Ok(provider));
        }
    }
}
