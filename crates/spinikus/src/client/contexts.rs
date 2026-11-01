//! Named contexts: the instances this machine has signed in to, in
//! `~/.config/spinikus/config.toml`, with their tokens in `credentials.toml` (mode 0600).

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

/// Overrides where the files live, such as for tests.
pub const CONFIG_DIR_VAR: &str = "SPINIKUS_CONFIG_DIR";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contexts {
    /// The context commands use unless told otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    #[serde(default)]
    pub contexts: BTreeMap<String, Context>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Context {
    /// The instance's address, such as `https://spinikus.example.com`.
    pub server: String,
    /// Who signed in, for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// The token's ID, to revoke it on `logout`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_id: Option<String>,
}

impl Contexts {
    /// The name of a context for `server`, preferring the current one.
    pub fn for_server(&self, server: &str) -> Option<&str> {
        let current = self.current.as_deref().filter(|name| self.contexts.get(*name).is_some_and(|c| c.server == server));
        current.or_else(|| self.contexts.iter().find(|(_, context)| context.server == server).map(|(name, _)| name.as_str()))
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Credentials {
    /// Tokens by context name.
    #[serde(default)]
    tokens: BTreeMap<String, String>,
}

/// The directory holding the CLI's files.
pub struct ConfigDir {
    path: PathBuf,
}

impl ConfigDir {
    /// `$SPINIKUS_CONFIG_DIR`, else `$XDG_CONFIG_HOME/spinikus`, else `~/.config/spinikus`.
    pub fn locate() -> anyhow::Result<Self> {
        let var = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty()).map(PathBuf::from);
        let path = match (var(CONFIG_DIR_VAR), var("XDG_CONFIG_HOME"), var("HOME")) {
            (Some(dir), _, _) => dir,
            (None, Some(config), _) => config.join("spinikus"),
            (None, None, Some(home)) => home.join(".config").join("spinikus"),
            (None, None, None) => anyhow::bail!("Couldn't find a home directory; set {CONFIG_DIR_VAR}"),
        };
        Ok(Self { path })
    }

    #[cfg(test)]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn contexts_path(&self) -> PathBuf {
        self.path.join("config.toml")
    }

    fn credentials_path(&self) -> PathBuf {
        self.path.join("credentials.toml")
    }

    pub fn contexts(&self) -> anyhow::Result<Contexts> {
        read_toml(&self.contexts_path())
    }

    pub fn save_contexts(&self, contexts: &Contexts) -> anyhow::Result<()> {
        self.write(&self.contexts_path(), &toml::to_string_pretty(contexts)?)
    }

    pub fn token(&self, context: &str) -> anyhow::Result<Option<String>> {
        let credentials: Credentials = read_toml(&self.credentials_path())?;
        Ok(credentials.tokens.get(context).cloned())
    }

    pub fn set_token(&self, context: &str, token: Option<&str>) -> anyhow::Result<()> {
        let mut credentials: Credentials = read_toml(&self.credentials_path())?;
        match token {
            Some(token) => credentials.tokens.insert(context.to_string(), token.to_string()),
            None => credentials.tokens.remove(context),
        };
        self.write(&self.credentials_path(), &toml::to_string_pretty(&credentials)?)
    }

    /// Replaces `path` atomically, readable only by its owner.
    fn write(&self, path: &Path, contents: &str) -> anyhow::Result<()> {
        create_private_dir(&self.path)?;
        let temporary = path.with_extension("toml.tmp");
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&temporary).with_context(|| format!("Couldn't write {}", temporary.display()))?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path).with_context(|| format!("Couldn't write {}", path.display()))
    }
}

fn create_private_dir(path: &Path) -> anyhow::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(path).with_context(|| format!("Couldn't create {}", path.display()))
}

fn read_toml<T: Default + serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    match fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("Couldn't read {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error).with_context(|| format!("Couldn't read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contexts_and_tokens_are_saved_apart() {
        let dir = tempfile::tempdir().unwrap();
        let config = ConfigDir::new(dir.path().join("spinikus"));
        assert_eq!(config.contexts().unwrap(), Contexts::default());
        let context = Context { server: "https://a.example".into(), user: Some("ada@example.com".into()), token_id: None };
        let contexts = Contexts { current: Some("a".into()), contexts: [("a".to_string(), context)].into() };

        config.save_contexts(&contexts).unwrap();
        config.set_token("a", Some("spk_secret")).unwrap();

        assert_eq!(config.contexts().unwrap(), contexts);
        assert_eq!(config.token("a").unwrap().as_deref(), Some("spk_secret"));
        assert_eq!(contexts.for_server("https://a.example"), Some("a"));
        assert_eq!(contexts.for_server("https://b.example"), None);
        let saved = fs::read_to_string(dir.path().join("spinikus/config.toml")).unwrap();
        assert!(!saved.contains("spk_"), "{saved}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join("spinikus/credentials.toml")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        config.set_token("a", None).unwrap();
        assert_eq!(config.token("a").unwrap(), None);
    }
}
