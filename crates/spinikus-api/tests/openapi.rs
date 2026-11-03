//! The OpenAPI document: committed up to date, and complete enough for clients to generate
//! code from. `tests/api.rs` checks every answer it gets against it.

use std::collections::HashSet;

use serde_json::{Value, json};

mod schema;

use schema::{DOCUMENT, validate};

const METHODS: &[&str] = &["get", "put", "post", "delete", "patch"];

#[test]
fn the_committed_document_is_up_to_date() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/openapi.json");
    let committed = std::fs::read_to_string(path).expect("docs/openapi.json exists");
    assert!(committed == spinikus_api::openapi_json(), "docs/openapi.json is out of date; run `cargo xtask openapi`");
}

#[test]
fn every_reference_resolves() {
    fn walk(value: &Value, found: &mut Vec<String>) {
        match value {
            Value::Object(fields) => {
                if let Some(Value::String(reference)) = fields.get("$ref") {
                    found.push(reference.clone());
                }
                fields.values().for_each(|field| walk(field, found));
            }
            Value::Array(items) => items.iter().for_each(|item| walk(item, found)),
            _ => {}
        }
    }
    let mut references = Vec::new();
    walk(&DOCUMENT, &mut references);
    assert!(!references.is_empty());
    for reference in references {
        let name = reference.strip_prefix("#/components/schemas/").unwrap_or_else(|| panic!("odd reference {reference}"));
        assert!(!DOCUMENT["components"]["schemas"][name].is_null(), "{reference} doesn't resolve");
    }
}

#[test]
fn every_operation_is_described() {
    let tags: HashSet<_> = DOCUMENT["tags"].as_array().unwrap().iter().map(|tag| tag["name"].as_str().unwrap()).collect();
    let mut ids = HashSet::new();
    let mut count = 0;
    for (path, item) in DOCUMENT["paths"].as_object().unwrap() {
        assert!(path.starts_with("/api/v1/") || path.starts_with("/hooks/"), "{path}");
        let names: Vec<_> = path.split('/').filter_map(|part| part.strip_prefix('{')?.strip_suffix('}')).collect();
        for method in METHODS {
            let Some(operation) = item.get(*method) else { continue };
            count += 1;
            let id = operation["operationId"].as_str().unwrap_or_else(|| panic!("{method} {path} has no operationId"));
            assert!(ids.insert(id), "{id} is used twice");
            let summary = operation["summary"].as_str().unwrap_or_else(|| panic!("{id} has no summary"));
            assert!(summary.ends_with('.'), "{id}'s summary is cut short: {summary}");
            for tag in operation["tags"].as_array().unwrap() {
                assert!(tags.contains(tag.as_str().unwrap()), "{id}'s tag {tag} isn't declared");
            }
            let parameters = operation["parameters"].as_array().map(Vec::as_slice).unwrap_or_default();
            for name in &names {
                assert!(
                    parameters.iter().any(|parameter| parameter["in"] == "path" && parameter["name"] == *name),
                    "{id} doesn't declare the path parameter {name}"
                );
            }
            let responses = operation["responses"].as_object().unwrap();
            assert!(responses.contains_key("default"), "{id} has no problem response");
            assert!(responses.keys().any(|status| status.starts_with('2')), "{id} has no success response");
            for (status, response) in responses {
                assert!(
                    response["description"].as_str().is_some_and(|text| !text.is_empty()),
                    "{id} {status} has no description"
                );
            }
            if *method == "patch" {
                assert!(parameters.iter().any(|parameter| parameter["name"] == "If-Match"), "{id} doesn't take If-Match");
            }
        }
    }
    assert!(count > 100, "only {count} operations");
}

#[test]
fn only_the_document_setting_up_signing_in_and_webhooks_are_unauthenticated() {
    assert_eq!(DOCUMENT["security"], json!([{ "bearer": [] }]));
    let mut public = Vec::new();
    for item in DOCUMENT["paths"].as_object().unwrap().values() {
        for method in METHODS {
            if let Some(operation) = item.get(*method)
                && operation["security"] == json!([{}])
            {
                public.push(operation["operationId"].as_str().unwrap());
            }
        }
    }
    public.sort_unstable();
    assert_eq!(public, ["hooks_receive", "openapi", "session_login", "session_setup", "session_setup_status"]);
}

#[test]
fn secrets_are_write_only() {
    let secret = &DOCUMENT["components"]["schemas"]["Secret"];
    assert_eq!(secret["writeOnly"], true);
    assert_eq!(DOCUMENT["components"]["schemas"]["Login"]["properties"]["password"]["$ref"], "#/components/schemas/Secret");
}

#[test]
fn the_response_check_catches_a_mismatch() {
    let project = json!({ "$ref": "#/components/schemas/Project" });
    let good = json!({
        "id": "prj_01J9ZX4Q8V6T3N2M1K0H9G8F7E",
        "name": "Shop",
        "slug": "shop",
        "revision": 1,
        "created_at": "2026-11-03T10:00:00.000Z",
        "updated_at": "2026-11-03T10:00:00.000Z",
    });
    validate(&project, &good, "$", true).unwrap();
    let mut missing = good.clone();
    missing.as_object_mut().unwrap().remove("slug");
    assert!(validate(&project, &missing, "$", true).is_err());
    let mut extra = good.clone();
    extra["owner"] = json!("someone");
    assert!(validate(&project, &extra, "$", true).is_err());
    let mut wrong = good;
    wrong["revision"] = json!("1");
    assert!(validate(&project, &wrong, "$", true).is_err());
}
