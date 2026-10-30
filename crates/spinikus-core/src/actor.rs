//! Who caused a change, for deployment events and the audit log.

use std::{fmt, str::FromStr};

use thiserror::Error;

use crate::id::{ServerId, UserId};

/// Stored as text: `system`, a user ID (`usr_…`) or a server ID (`srv_…`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Actor {
    /// Spinikus itself, such as the reconciler or a timeout.
    System,
    /// A user, through the UI, the API or the CLI.
    User(UserId),
    /// The agent on a server, reporting a command's progress or result.
    Server(ServerId),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("`{0}` is not `system`, a user ID or a server ID")]
pub struct ActorError(String);

impl fmt::Display for Actor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::System => f.write_str("system"),
            Self::User(id) => id.fmt(f),
            Self::Server(id) => id.fmt(f),
        }
    }
}

impl FromStr for Actor {
    type Err = ActorError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == "system" {
            return Ok(Self::System);
        }
        value.parse().map(Self::User).or_else(|_| value.parse().map(Self::Server)).map_err(|_| ActorError(value.to_string()))
    }
}

impl serde::Serialize for Actor {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_text() {
        for actor in [Actor::System, Actor::User(UserId::new()), Actor::Server(ServerId::new())] {
            assert_eq!(actor.to_string().parse(), Ok(actor));
        }
    }

    #[test]
    fn rejects_other_ids() {
        assert!(crate::id::ServiceId::new().to_string().parse::<Actor>().is_err());
        assert!("System".parse::<Actor>().is_err());
    }
}
