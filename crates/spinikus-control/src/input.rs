//! Validation of names and slugs shared by the projects, environments and services.

use spinikus_core::slug::Slug;

use crate::ControlError;

pub const MAX_NAME_LEN: usize = 100;

/// A display name, trimmed: not empty, at most [`MAX_NAME_LEN`] characters, no control characters.
pub(crate) fn name(value: &str) -> Result<String, ControlError> {
    let value = value.trim();
    let problem = if value.is_empty() {
        "it is empty"
    } else if value.chars().count() > MAX_NAME_LEN {
        "it is longer than 100 characters"
    } else if value.chars().any(char::is_control) {
        "it contains control characters"
    } else {
        return Ok(value.to_string());
    };
    Err(ControlError::Invalid { field: "name", problem: problem.to_string() })
}

/// The given slug, or one made from `name` when none is given.
pub(crate) fn slug(value: Option<&str>, name: &str) -> Result<Slug, ControlError> {
    match value {
        Some(value) => value
            .parse()
            .map_err(|error: spinikus_core::slug::SlugError| ControlError::Invalid { field: "slug", problem: error.to_string() }),
        None => Slug::from_name(name).ok_or_else(|| ControlError::Invalid {
            field: "slug",
            problem: "no slug can be made from the name, so give one".to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_names() {
        assert_eq!(name("  Web app ").unwrap(), "Web app");
        for bad in ["", "   ", "line\nbreak", &"a".repeat(MAX_NAME_LEN + 1)] {
            assert!(matches!(name(bad), Err(ControlError::Invalid { field: "name", .. })), "accepted {bad:?}");
        }
    }

    #[test]
    fn slugs_default_to_the_name() {
        assert_eq!(slug(None, "Web App").unwrap().as_str(), "web-app");
        assert_eq!(slug(Some("api"), "Web App").unwrap().as_str(), "api");
        assert!(matches!(slug(Some("API"), "x"), Err(ControlError::Invalid { field: "slug", .. })));
        assert!(matches!(slug(None, "日本"), Err(ControlError::Invalid { field: "slug", .. })));
    }
}
