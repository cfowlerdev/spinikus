//! Human-friendly slugs, unique per parent and usable in URLs and hostnames.

use std::{fmt, str::FromStr};

use thiserror::Error;

/// Longest allowed slug, the length of a DNS label, so slugs can appear in preview hostnames.
pub const MAX_LEN: usize = 63;

/// Lowercase letters, digits and single hyphens, starting with a letter or digit
/// and not ending with a hyphen, for example `my-app-2`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Slug(String);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("`{value}` is not a valid slug: {problem}")]
pub struct SlugError {
    value: String,
    problem: &'static str,
}

impl Slug {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A slug made from a display name, such as `my-app` from "My App!": ASCII letters and
    /// digits are kept and lowercased, and every run of anything else becomes one hyphen.
    /// `None` if the name has no ASCII letters or digits.
    pub fn from_name(name: &str) -> Option<Self> {
        let mut slug = String::new();
        for word in name.split(|c: char| !c.is_ascii_alphanumeric()).filter(|word| !word.is_empty()) {
            if !slug.is_empty() {
                slug.push('-');
            }
            slug.push_str(&word.to_ascii_lowercase());
        }
        slug.truncate(MAX_LEN);
        let slug = slug.trim_end_matches('-');
        slug.parse().ok()
    }
}

impl FromStr for Slug {
    type Err = SlugError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let problem = if value.is_empty() {
            Some("it is empty")
        } else if value.len() > MAX_LEN {
            Some("it is longer than 63 characters")
        } else if !value.bytes().all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-')) {
            Some("only lowercase letters, digits and hyphens are allowed")
        } else if value.starts_with('-') || value.ends_with('-') {
            Some("it cannot start or end with a hyphen")
        } else if value.contains("--") {
            Some("it cannot contain consecutive hyphens")
        } else {
            None
        };

        match problem {
            Some(problem) => Err(SlugError { value: value.to_string(), problem }),
            None => Ok(Self(value.to_string())),
        }
    }
}

impl fmt::Display for Slug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl serde::Serialize for Slug {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for Slug {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_slugs() {
        for value in ["a", "web", "my-app-2", "2fa", &"a".repeat(MAX_LEN)] {
            assert_eq!(value.parse::<Slug>().map(|slug| slug.to_string()), Ok(value.to_string()));
        }
    }

    #[test]
    fn rejects_invalid_slugs() {
        let too_long = "a".repeat(MAX_LEN + 1);
        for value in ["", "My-App", "my_app", "my app", "-web", "web-", "my--app", "café", &too_long] {
            assert!(value.parse::<Slug>().is_err(), "accepted `{value}`");
        }
    }

    #[test]
    fn makes_slugs_from_names() {
        let long = "word ".repeat(20);
        for (name, slug) in [("My App!", "my-app"), ("  API -- v2 ", "api-v2"), ("Café Paris", "caf-paris")] {
            assert_eq!(Slug::from_name(name).unwrap().as_str(), slug);
        }
        assert!(Slug::from_name(&long).unwrap().as_str().len() <= MAX_LEN);
        assert_eq!(Slug::from_name("日本"), None);
        assert_eq!(Slug::from_name(""), None);
    }

    #[test]
    fn deserializing_validates() {
        assert_eq!(serde_json::from_str::<Slug>("\"web\"").unwrap().as_str(), "web");
        assert!(serde_json::from_str::<Slug>("\"Web\"").is_err());
    }
}
