//! Persistent volumes, mounted into a service's containers and kept across deployments.

use std::{fmt, str::FromStr};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    id::{ServiceId, VolumeId},
    slug::Slug
};

/// Longest allowed mount path.
pub const MAX_MOUNT_PATH_LEN: usize = 4096;

/// A named volume of a service. The data lives on the server running the service, in a
/// runtime volume named after the volume's ID, so renaming it never touches the data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Volume {
    pub id: VolumeId,
    pub service_id: ServiceId,
    /// Unique among the service's volumes.
    pub name: Slug,
    /// Where it is mounted in the container. Unique among the service's volumes.
    pub mount_path: MountPath,
    pub revision: i64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp
}

/// The fields a user sets when creating a volume, or replaces when updating one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeFields {
    pub name: Slug,
    pub mount_path: MountPath
}

/// An absolute path inside a container, such as `/var/lib/postgresql/data`. Parsing drops
/// a trailing slash. `.` and `..` components, empty components and the root itself are refused.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MountPath(String);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("`{value}` is not a valid mount path: {problem}")]
pub struct MountPathError {
    value: String,
    problem: &'static str
}

impl MountPath {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for MountPath {
    type Err = MountPathError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let path = if value.len() > 1 { value.strip_suffix('/').unwrap_or(value) } else { value };

        let problem = if !path.starts_with('/') {
            Some("it must be absolute")
        } else if path == "/" {
            Some("the container's root cannot be a volume")
        } else if path.len() > MAX_MOUNT_PATH_LEN {
            Some("it is longer than 4096 characters")
        } else if path.chars().any(|c| c == '\0' || c.is_control()) {
            Some("control characters are not allowed")
        } else if path[1..].split('/').any(|component| matches!(component, "" | "." | "..")) {
            Some("it cannot contain empty, `.` or `..` components")
        } else {
            None
        };

        match problem {
            Some(problem) => Err(MountPathError { value: value.to_string(), problem }),
            None => Ok(Self(path.to_string()))
        }
    }
}

impl fmt::Display for MountPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for MountPath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for MountPath {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_and_normalizes_paths() {
        for (input, expected) in [("/data", "/data"), ("/var/lib/postgresql/data/", "/var/lib/postgresql/data"), ("/app/.cache", "/app/.cache")] {
            assert_eq!(input.parse::<MountPath>().map(|path| path.to_string()), Ok(expected.to_string()));
        }
    }

    #[test]
    fn rejects_invalid_paths() {
        let too_long = format!("/{}", "a".repeat(MAX_MOUNT_PATH_LEN));
        for input in ["", "/", "//", "data", "./data", "/data//x", "/data/../etc", "/data/.", "/da\0ta", "/a\nb", &too_long] {
            assert!(input.parse::<MountPath>().is_err(), "{input:?} should be rejected");
        }
    }
}
