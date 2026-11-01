//! Errors with an exit status scripts can rely on.

use std::fmt;

/// The exit statuses of the client commands. They are stable: scripts may test for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Anything not below, such as the server being unreachable.
    Failure = 1,
    // 2 is a usage error, from clap.
    /// No token, or the server didn't accept it.
    Unauthenticated = 3,
    /// Signed in, but without the role this needs.
    Forbidden = 4,
    /// What was named doesn't exist, or isn't visible to you.
    NotFound = 5,
    /// The server refused the request: invalid input, a conflict, an edit made since it was read.
    Refused = 6,
    /// A deployment, backup or restore that was waited for failed or was canceled.
    Failed = 7,
}

/// An error with its exit status. Anything else exits with [`Exit::Failure`].
#[derive(Debug)]
pub struct CliError {
    pub exit: Exit,
    pub message: String,
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

pub fn fail(exit: Exit, message: impl Into<String>) -> anyhow::Error {
    CliError { exit, message: message.into() }.into()
}

/// The exit status for an error from a client command.
pub fn exit_status(error: &anyhow::Error) -> u8 {
    error.downcast_ref::<CliError>().map_or(Exit::Failure, |error| error.exit) as u8
}

#[cfg(test)]
mod tests {
    use anyhow::Context;

    use super::*;

    #[test]
    fn the_status_survives_added_context() {
        let error = Err::<(), _>(fail(Exit::NotFound, "no such service")).context("Couldn't deploy").unwrap_err();
        assert_eq!(exit_status(&error), 5);
        assert_eq!(exit_status(&anyhow::anyhow!("something else")), 1);
    }
}
