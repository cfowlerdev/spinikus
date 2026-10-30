use spinikus_store::StoreError;

/// Errors from the service layer. Each has a stable `code` for API problem details, so
/// clients can match on it instead of the message.
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("{entity} `{id}` was not found")]
    NotFound { entity: &'static str, id: String },
    #[error("a {entity} with {field} `{value}` already exists")]
    Conflict { entity: &'static str, field: &'static str, value: String },
    #[error("{entity} `{id}` still has {children}")]
    HasChildren { entity: &'static str, id: String, children: &'static str },
    #[error("{entity} `{id}` is at revision {actual}, not {expected}")]
    RevisionMismatch { entity: &'static str, id: String, expected: i64, actual: i64 },
    /// The change is never allowed in the entity's current state.
    #[error("{entity} `{id}`: {problem}")]
    Rejected { entity: &'static str, id: String, problem: String },
    /// The deployment state machine does not allow the change.
    #[error(transparent)]
    InvalidTransition(spinikus_core::deployment::TransitionError),
    /// Input that failed validation.
    #[error("invalid {field}: {problem}")]
    Invalid { field: &'static str, problem: String },
    /// Deliberately vague, so sign-in does not reveal which emails have accounts.
    #[error("invalid email or password")]
    InvalidCredentials,
    #[error("authentication required")]
    Unauthenticated,
    #[error("you do not have permission to do this")]
    Forbidden,
    #[error("Spinikus is already set up")]
    AlreadySetUp,
    /// Something went wrong that the caller cannot fix, such as a database error.
    #[error("internal error")]
    Internal(#[source] StoreError),
}

impl ControlError {
    /// The stable, machine-readable code for this kind of error.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "not_found",
            Self::Conflict { .. } => "conflict",
            Self::HasChildren { .. } => "has_children",
            Self::RevisionMismatch { .. } => "revision_mismatch",
            Self::Rejected { .. } => "rejected",
            Self::InvalidTransition(_) => "invalid_transition",
            Self::Invalid { .. } => "invalid",
            Self::InvalidCredentials => "invalid_credentials",
            Self::Unauthenticated => "unauthenticated",
            Self::Forbidden => "forbidden",
            Self::AlreadySetUp => "already_set_up",
            Self::Internal(_) => "internal",
        }
    }
}

impl From<StoreError> for ControlError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::NotFound { entity, id } => Self::NotFound { entity, id },
            StoreError::Conflict { entity, field, value } => Self::Conflict { entity, field, value },
            StoreError::HasChildren { entity, id, children } => Self::HasChildren { entity, id, children },
            StoreError::RevisionMismatch { entity, id, expected, actual } => {
                Self::RevisionMismatch { entity, id, expected, actual }
            }
            StoreError::Rejected { entity, id, problem } => Self::Rejected { entity, id, problem: problem.to_string() },
            StoreError::InvalidTransition(transition) => Self::InvalidTransition(transition),
            other => {
                tracing::error!(error = %other, "store error");
                Self::Internal(other)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_store_errors_and_hides_internal_ones() {
        let not_found = ControlError::from(StoreError::NotFound { entity: "user", id: "usr_1".to_string() });
        let internal = ControlError::from(StoreError::InvalidRow { entity: "user", column: "email", problem: "bad".to_string() });

        assert_eq!(not_found.code(), "not_found");
        assert_eq!(internal.code(), "internal");
        assert_eq!(internal.to_string(), "internal error");
    }
}
