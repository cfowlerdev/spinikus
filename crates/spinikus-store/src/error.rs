#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{entity} `{id}` was not found")]
    NotFound { entity: &'static str, id: String },
    #[error("a {entity} with {field} `{value}` already exists")]
    Conflict { entity: &'static str, field: &'static str, value: String },
    /// An update was based on an older revision than the one stored.
    #[error("{entity} `{id}` is at revision {actual}, not {expected}")]
    RevisionMismatch { entity: &'static str, id: String, expected: i64, actual: i64 },
    /// A stored value could not be read back into its domain type.
    #[error("invalid {column} in {entity} row: {problem}")]
    InvalidRow { entity: &'static str, column: &'static str, problem: String },
    #[error("failed to create database directory {path}")]
    CreateDir { path: String, source: std::io::Error },
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError)
}

impl StoreError {
    /// Turns a unique constraint violation into the given conflict, and passes other errors through.
    pub(crate) fn on_unique_violation(error: sqlx::Error, conflict: impl FnOnce() -> StoreError) -> StoreError {
        match error.as_database_error() {
            Some(db_error) if db_error.is_unique_violation() => conflict(),
            _ => StoreError::Database(error)
        }
    }
}

/// Parses a column read from the database into its domain type.
pub(crate) fn parse_column<T>(entity: &'static str, column: &'static str, value: &str) -> Result<T, StoreError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display
{
    value.parse().map_err(|error: T::Err| StoreError::InvalidRow { entity, column, problem: error.to_string() })
}
