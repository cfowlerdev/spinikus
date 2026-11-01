//! The link file, `.spinikus/link.json`: which project, environment and (optionally) service
//! a directory means, so commands run in it need no names. Found by walking up from the
//! current directory, like `.git`. Its directory has its own `.gitignore`, since the IDs
//! belong to one instance rather than to the code.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::Context;
use serde::{Deserialize, Serialize};

pub const LINK_DIR: &str = ".spinikus";
const LINK_FILE: &str = "link.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    /// The instance the IDs belong to.
    pub server: String,
    pub project_id: String,
    pub environment_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_id: Option<String>,
}

/// A link file and where it was found.
#[derive(Debug, Clone)]
pub struct Linked {
    /// The directory holding `.spinikus`.
    pub dir: PathBuf,
    pub link: Link,
}

/// The nearest link file at or above `start`.
pub fn find(start: &Path) -> anyhow::Result<Option<Linked>> {
    for dir in start.ancestors() {
        let path = dir.join(LINK_DIR).join(LINK_FILE);
        match fs::read(&path) {
            Ok(bytes) => {
                let link = serde_json::from_slice(&bytes).with_context(|| format!("Couldn't read {}", path.display()))?;
                return Ok(Some(Linked { dir: dir.to_path_buf(), link }));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("Couldn't read {}", path.display())),
        }
    }
    Ok(None)
}

pub fn save(dir: &Path, link: &Link) -> anyhow::Result<()> {
    let link_dir = dir.join(LINK_DIR);
    fs::create_dir_all(&link_dir).with_context(|| format!("Couldn't create {}", link_dir.display()))?;
    fs::write(link_dir.join(".gitignore"), "# Written by `spinikus link`: this directory's link to a Spinikus instance.\n*\n")?;
    let mut json = serde_json::to_string_pretty(link)?;
    json.push('\n');
    let path = link_dir.join(LINK_FILE);
    fs::write(&path, json).with_context(|| format!("Couldn't write {}", path.display()))
}

/// Removes the link file (and the `.gitignore` written with it, and the directory if that
/// leaves it empty). Returns whether there was one.
pub fn remove(dir: &Path) -> anyhow::Result<bool> {
    let link_dir = dir.join(LINK_DIR);
    let path = link_dir.join(LINK_FILE);
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).with_context(|| format!("Couldn't remove {}", path.display())),
    }
    let _ = fs::remove_file(link_dir.join(".gitignore"));
    let _ = fs::remove_dir(&link_dir);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_are_found_from_subdirectories() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("src/app");
        fs::create_dir_all(&nested).unwrap();
        let link = Link {
            server: "https://a.example".into(),
            project_id: "prj_1".into(),
            environment_id: "env_1".into(),
            service_id: None,
        };
        assert!(find(&nested).unwrap().is_none());

        save(dir.path(), &link).unwrap();
        let found = find(&nested).unwrap().unwrap();

        assert_eq!((found.dir.as_path(), &found.link), (dir.path(), &link));
        assert!(fs::read_to_string(dir.path().join(".spinikus/.gitignore")).unwrap().ends_with("*\n"));
        assert!(remove(dir.path()).unwrap());
        assert!(!dir.path().join(LINK_DIR).exists());
        assert!(!remove(dir.path()).unwrap());
    }
}
