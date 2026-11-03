//! OpenAPI schemas for the types that serialize themselves as strings (IDs, hostnames,
//! slugs and the like), behind the `openapi` feature. Types that derive their serde
//! representation derive `utoipa::ToSchema` next to it.

use utoipa::openapi::{
    RefOr, Type,
    schema::{ObjectBuilder, Schema},
};

/// A string schema with a description and an example.
pub(crate) fn string(description: &str, pattern: Option<&str>, example: &str) -> RefOr<Schema> {
    ObjectBuilder::new().schema_type(Type::String).description(Some(description)).pattern(pattern).examples([example]).into()
}

/// Implements the schema traits for a type serialized as a string.
macro_rules! string_schema {
    ($name:ty, $description:expr, $pattern:expr, $example:expr) => {
        impl utoipa::PartialSchema for $name {
            fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
                $crate::openapi::string($description, $pattern, $example)
            }
        }

        impl utoipa::ToSchema for $name {
            fn name() -> std::borrow::Cow<'static, str> {
                // The type's own name, without its module path.
                let path = stringify!($name);
                std::borrow::Cow::Borrowed(path.rsplit(':').next().unwrap_or(path).trim())
            }
        }
    };
}
pub(crate) use string_schema;

string_schema!(crate::domain::Hostname, "A DNS name, lower case; a wildcard starts with `*.`.", None, "shop.example.com");
string_schema!(crate::route::BaseDomain, "The domain generated hostnames go under.", None, "apps.example.com");
string_schema!(
    crate::slug::Slug,
    "Lower-case letters, digits and single hyphens, starting with a letter.",
    Some("^[a-z][a-z0-9]*(-[a-z0-9]+)*$"),
    "web"
);
string_schema!(crate::user::Email, "An email address.", None, "ada@example.com");
string_schema!(crate::volume::MountPath, "An absolute path in the container.", None, "/app/uploads");
string_schema!(crate::variable::VariableName, "An environment variable name.", Some("^[A-Za-z_][A-Za-z0-9_]*$"), "DATABASE_URL");
string_schema!(crate::audit::AuditAction, "What was done, as `<entity>.<verb>`.", None, "service.restart");
string_schema!(
    crate::actor::Actor,
    "Who did it: `system`, or the ID of a user, server or webhook.",
    None,
    "usr_01J9ZX4Q8V6T3N2M1K0H9G8F7E"
);

impl utoipa::PartialSchema for crate::secret::Secret {
    fn schema() -> RefOr<Schema> {
        ObjectBuilder::new()
            .schema_type(Type::String)
            .description(Some("A secret: sent, never returned."))
            .write_only(true)
            .into()
    }
}

impl utoipa::ToSchema for crate::secret::Secret {
    fn name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("Secret")
    }
}
