//! References from a variable's value to another service's variable, such as
//! `postgres://app:${{ postgres.PASSWORD }}@postgres:5432/app`. They name a service of the
//! same environment by its slug, and one of the variables that service gets, and are
//! replaced when a deployment is created. `$${{` writes a literal `${{`.
//!
//! The referenced value is used as it is: references inside it are not followed, so there
//! are no chains or cycles to resolve.

use std::fmt;

use thiserror::Error;

use crate::slug::Slug;

/// A `${{ service.NAME }}` in a value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Reference {
    /// The slug of a service in the same environment.
    pub service: Slug,
    /// The variable's name, as the service gets it.
    pub name: String,
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "${{{{ {}.{} }}}}", self.service, self.name)
    }
}

/// A value split into text and references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    parts: Vec<Part>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Text(String),
    Reference(Reference),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{problem} at `{at}`; write references as ${{{{ service.NAME }}}}, or $${{{{ for a literal ${{{{")]
pub struct ReferenceError {
    at: String,
    problem: &'static str,
}

impl Template {
    pub fn parse(value: &str) -> Result<Self, ReferenceError> {
        let mut parts = Vec::new();
        let mut text = String::new();
        let mut rest = value;
        while let Some(start) = rest.find("${{") {
            if rest[..start].ends_with('$') {
                text.push_str(&rest[..start - 1]);
                text.push_str("${{");
                rest = &rest[start + 3..];
                continue;
            }
            text.push_str(&rest[..start]);
            let error = |problem| ReferenceError { at: excerpt(&rest[start..]), problem };
            let Some(end) = rest[start..].find("}}") else {
                return Err(error("a reference is not closed"));
            };
            let inner = rest[start + 3..start + end].trim();
            let (service, name) = inner.split_once('.').ok_or_else(|| error("a reference must name a service and a variable"))?;
            let service = service.parse().map_err(|_| error("a reference must start with a service's slug"))?;
            if !is_name(name) {
                return Err(error("a reference must end with a variable name"));
            }
            if !text.is_empty() {
                parts.push(Part::Text(std::mem::take(&mut text)));
            }
            parts.push(Part::Reference(Reference { service, name: name.to_string() }));
            rest = &rest[start + end + 2..];
        }
        text.push_str(rest);
        if !text.is_empty() {
            parts.push(Part::Text(text));
        }
        Ok(Self { parts })
    }

    pub fn references(&self) -> impl Iterator<Item = &Reference> {
        self.parts.iter().filter_map(|part| match part {
            Part::Reference(reference) => Some(reference),
            Part::Text(_) => None,
        })
    }

    /// The value with each reference replaced by what `lookup` gives for it.
    pub fn render<E>(&self, mut lookup: impl FnMut(&Reference) -> Result<String, E>) -> Result<String, E> {
        let mut value = String::new();
        for part in &self.parts {
            match part {
                Part::Text(text) => value.push_str(text),
                Part::Reference(reference) => value.push_str(&lookup(reference)?),
            }
        }
        Ok(value)
    }
}

/// Letters, digits and underscores, not starting with a digit.
fn is_name(name: &str) -> bool {
    name.bytes().next().is_some_and(|first| !first.is_ascii_digit())
        && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// The start of `value`, short enough for an error message.
fn excerpt(value: &str) -> String {
    match value.char_indices().nth(40) {
        Some((at, _)) => format!("{}…", &value[..at]),
        None => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(value: &str) -> String {
        Template::parse(value)
            .unwrap()
            .render(|reference| Ok::<_, ()>(format!("<{}/{}>", reference.service, reference.name)))
            .unwrap()
    }

    #[test]
    fn finds_and_replaces_references() {
        let template = Template::parse("postgres://app:${{ postgres.PASSWORD }}@${{postgres.HOST}}:5432").unwrap();
        let names: Vec<_> = template.references().map(|reference| reference.to_string()).collect();

        assert_eq!(names, ["${{ postgres.PASSWORD }}", "${{ postgres.HOST }}"]);
        assert_eq!(render("postgres://app:${{ postgres.PASSWORD }}@x"), "postgres://app:<postgres/PASSWORD>@x");
        assert_eq!(render("${{ db-2.URL }}${{ cache.URL }}"), "<db-2/URL><cache/URL>");
    }

    #[test]
    fn plain_values_and_escapes_stay_as_they_are() {
        for value in ["", "plain", "a $ b", "${ HOME }", "{{ x }}", "}}"] {
            assert_eq!(render(value), value);
        }
        assert_eq!(render("$${{ not.A_REFERENCE }}"), "${{ not.A_REFERENCE }}");
        assert_eq!(Template::parse("$${{ x").unwrap().references().count(), 0);
    }

    #[test]
    fn rejects_malformed_references() {
        for value in [
            "${{",
            "${{ postgres.PASSWORD",
            "${{ PASSWORD }}",
            "${{ Postgres.PASSWORD }}",
            "${{ postgres.1X }}",
            "${{ postgres. }}",
            "${{ .X }}",
            "${{ a.b.c }}",
        ] {
            assert!(Template::parse(value).is_err(), "{value:?} should be rejected");
        }
    }
}
