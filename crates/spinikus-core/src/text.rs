//! Text forms for enums stored in `TEXT` columns and used in the API.

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown {what} `{value}`")]
pub struct UnknownValue {
    what: &'static str,
    value: String
}

impl UnknownValue {
    pub fn new(what: &'static str, value: &str) -> Self {
        Self { what, value: value.to_string() }
    }
}

/// Implements `Display` and `FromStr` for an enum with an `as_str` method and an `ALL` constant.
macro_rules! impl_text {
    ($type:ty, $what:literal) => {
        impl std::fmt::Display for $type {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $type {
            type Err = $crate::text::UnknownValue;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::ALL
                    .into_iter()
                    .find(|item| item.as_str() == value)
                    .ok_or_else(|| $crate::text::UnknownValue::new($what, value))
            }
        }
    };
}

pub(crate) use impl_text;

/// Longest allowed dotted name.
pub const MAX_DOTTED_NAME_LEN: usize = 64;

/// What is wrong with a dotted name such as `deployment.prepare`, if anything: lowercase
/// letters, digits and underscores, in non-empty dot-separated parts. Used for job kinds
/// and audit actions.
pub(crate) fn dotted_name_problem(value: &str) -> Option<&'static str> {
    if value.is_empty() {
        Some("it is empty")
    } else if value.len() > MAX_DOTTED_NAME_LEN {
        Some("it is longer than 64 characters")
    } else if !value.bytes().all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.')) {
        Some("only lowercase letters, digits, underscores and dots are allowed")
    } else if value.split('.').any(str::is_empty) {
        Some("it cannot start or end with a dot, or contain consecutive dots")
    } else {
        None
    }
}
