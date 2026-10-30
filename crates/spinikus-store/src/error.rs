#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{entity} `{id}` was not found")]
    NotFound { entity: &'static str, id: String },
    #[error("a {entity} with {field} `{value}` already exists")]
    Conflict { entity: &'static str, field: &'static str, value: String },
    /// A delete was refused because the entity still has children that are not deleted.
    #[error("{entity} `{id}` still has {children}")]
    HasChildren { entity: &'static str, id: String, children: &'static str },
    /// An update was based on an older revision than the one stored.
    #[error("{entity} `{id}` is at revision {actual}, not {expected}")]
    RevisionMismatch { entity: &'static str, id: String, expected: i64, actual: i64 },
    /// The change is never allowed, whatever the revision.
    #[error("{entity} `{id}`: {problem}")]
    Rejected { entity: &'static str, id: String, problem: &'static str },
    /// The deployment state machine does not allow the requested change.
    #[error(transparent)]
    InvalidTransition(#[from] spinikus_core::deployment::TransitionError),
    /// A stored value could not be read back into its domain type.
    #[error("invalid {column} in {entity} row: {problem}")]
    InvalidRow { entity: &'static str, column: &'static str, problem: String },
    #[error(transparent)]
    Secrets(#[from] crate::secrets::SecretsError),
    #[error("failed to create database directory {path}")]
    CreateDir { path: String, source: std::io::Error },
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

impl StoreError {
    /// Turns a unique constraint violation into a conflict, and passes other errors through.
    ///
    /// `conflict` gets SQLite's message, which names the columns of the violated index,
    /// for example `UNIQUE constraint failed: projects.slug`.
    pub(crate) fn on_unique_violation(error: sqlx::Error, conflict: impl FnOnce(&str) -> StoreError) -> StoreError {
        match error.as_database_error() {
            Some(db_error) if db_error.is_unique_violation() => conflict(db_error.message()),
            _ => StoreError::Database(error),
        }
    }
}

/// Parses a column read from the database into its domain type.
pub(crate) fn parse_column<T>(entity: &'static str, column: &'static str, value: &str) -> Result<T, StoreError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value.parse().map_err(|error: T::Err| StoreError::InvalidRow { entity, column, problem: error.to_string() })
}
