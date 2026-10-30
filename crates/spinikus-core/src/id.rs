//! Prefixed ULID identifiers, such as `svc_01JB8X5Q3V7Y2M4N6P8R0T2W4Y`.
//!
//! Each entity has its own ID type, so a `ServiceId` can never be passed where a
//! `ProjectId` is expected. IDs sort by creation time and are safe to expose in
//! the API and in URLs.

use std::sync::Mutex;

use thiserror::Error;

/// Shared by every ID type, so IDs created in the same millisecond still sort in creation order.
static GENERATOR: Mutex<ulid::Generator> = Mutex::new(ulid::Generator::new());

fn generate() -> ulid::Ulid {
    let mut generator = GENERATOR.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // Overflow needs 2^80 IDs in one millisecond; fall back to a random ULID if it ever happens.
    generator.generate().unwrap_or_else(|overflow| overflow.commit_overflow_random())
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IdError {
    #[error("expected an ID starting with `{expected}_`, got `{value}`")]
    WrongPrefix { expected: &'static str, value: String },
    #[error("`{value}` is not a valid ID")]
    InvalidUlid { value: String },
}

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(ulid::Ulid);

        impl $name {
            pub const PREFIX: &'static str = $prefix;

            /// Generates a new ID for the current time, sorting after every ID this process created before.
            pub fn new() -> Self {
                Self(generate())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}_{}", Self::PREFIX, self.0)
            }
        }

        impl std::str::FromStr for $name {
            type Err = IdError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                let ulid = value
                    .strip_prefix(Self::PREFIX)
                    .and_then(|rest| rest.strip_prefix('_'))
                    .ok_or_else(|| IdError::WrongPrefix { expected: Self::PREFIX, value: value.to_string() })?;
                ulid::Ulid::from_string(ulid)
                    .map(Self)
                    .map_err(|_| IdError::InvalidUlid { value: value.to_string() })
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
                value.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

define_id!(ProjectId, "prj");
define_id!(EnvironmentId, "env");
define_id!(ServiceId, "svc");
define_id!(SourceId, "src");
define_id!(DeploymentId, "dep");
define_id!(DeploymentEventId, "evt");
define_id!(ServerId, "srv");
define_id!(InstanceId, "ins");
define_id!(JobId, "job");
define_id!(DomainId, "dom");
define_id!(CertificateId, "crt");
define_id!(AcmeAccountId, "acc");
define_id!(VariableId, "var");
define_id!(VolumeId, "vol");
define_id!(UserId, "usr");
define_id!(MembershipId, "mem");
define_id!(ApiTokenId, "tok");
define_id!(SessionId, "ses");
define_id!(AuditEntryId, "aud");
define_id!(
    /// A data key for envelope encryption of secrets.
    DataKeyId,
    "key"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_with_prefix() {
        let id = ServiceId::new();
        let text = id.to_string();

        assert!(text.starts_with("svc_"));
        assert_eq!(text.len(), "svc_".len() + 26);
    }

    #[test]
    fn round_trips_through_string() {
        let id = ProjectId::new();

        assert_eq!(id.to_string().parse::<ProjectId>(), Ok(id));
    }

    #[test]
    fn parses_lowercase_ulid() {
        let id = ServerId::new();
        let lower = id.to_string().to_lowercase();

        assert_eq!(lower.parse::<ServerId>(), Ok(id));
    }

    #[test]
    fn rejects_another_entitys_id() {
        let text = ServiceId::new().to_string();

        assert!(matches!(text.parse::<ProjectId>(), Err(IdError::WrongPrefix { expected: "prj", .. })));
    }

    #[test]
    fn rejects_missing_separator_and_bad_ulid() {
        let ulid = ServiceId::new().to_string()["svc_".len()..].to_string();

        assert!(matches!(format!("svc{ulid}").parse::<ServiceId>(), Err(IdError::WrongPrefix { .. })));
        assert!(matches!("svc_not-a-ulid".parse::<ServiceId>(), Err(IdError::InvalidUlid { .. })));
    }

    #[test]
    fn sorts_by_creation_time_within_a_millisecond() {
        let ids: Vec<DeploymentId> = (0..1000).map(|_| DeploymentId::new()).collect();

        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(ids.windows(2).all(|pair| pair[0].to_string() < pair[1].to_string()));
    }

    #[test]
    fn serializes_as_string() {
        let id = EnvironmentId::new();
        let json = format!("\"{id}\"");

        assert_eq!(serde_json::to_string(&id).unwrap(), json);
        assert_eq!(serde_json::from_str::<EnvironmentId>(&json).unwrap(), id);
        assert!(serde_json::from_str::<ServiceId>(&json).is_err());
    }
}
