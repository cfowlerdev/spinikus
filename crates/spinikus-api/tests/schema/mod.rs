//! Checks responses against the OpenAPI document: enough JSON Schema (types, `required`,
//! `properties`, `items`, `enum`, `oneOf`, `allOf`, `$ref`) to catch a schema that says
//! something different from what the API sends.

// Each test binary uses part of it.
#![allow(dead_code)]

use std::sync::LazyLock;

use serde_json::Value;

pub static DOCUMENT: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(&spinikus_api::openapi_json()).expect("the document is JSON"));

/// Panics unless `body`, answered with `status` to `method` on `uri`, is what the document
/// says that operation answers.
pub fn check_response(method: &str, uri: &str, status: u16, content_type: &str, body: &Value) {
    let path = uri.split('?').next().unwrap();
    let operation = DOCUMENT["paths"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(template, _)| matches(template, path))
        // A literal segment beats a parameter where both would match.
        .min_by_key(|(template, _)| template.matches('{').count())
        .and_then(|(template, item)| Some((template, item.get(method.to_ascii_lowercase())?)));
    let Some((template, operation)) = operation else {
        // No such route: the answer is a problem detail.
        assert!(matches!(status, 404 | 405), "{method} {path} is not in the OpenAPI document but answered {status}");
        let problem = serde_json::json!({ "$ref": "#/components/schemas/Problem" });
        if let Err(problem) = validate(&problem, body, "$", true) {
            panic!("{method} {path} {status} isn't a problem detail: {problem}");
        }
        return;
    };
    let responses = &operation["responses"];
    let response = responses
        .get(status.to_string())
        .or_else(|| (status >= 400).then(|| responses.get("default")).flatten())
        .unwrap_or_else(|| panic!("{method} {template} answered {status}, which the document doesn't list"));
    let media = content_type.split(';').next().unwrap().trim();
    match response.get("content") {
        None => assert!(body.is_null(), "{method} {template} {status} has no documented body, but sent {body}"),
        Some(content) => {
            let schema = &content
                .get(media)
                .unwrap_or_else(|| panic!("{method} {template} {status} answered {media}, which the document doesn't list"))["schema"];
            if media.ends_with("json")
                && let Err(problem) = validate(schema, body, "$", true)
            {
                panic!("{method} {template} {status} doesn't match the document: {problem}\n{body:#}");
            }
        }
    }
}

fn matches(template: &str, path: &str) -> bool {
    let template: Vec<_> = template.split('/').collect();
    let path: Vec<_> = path.split('/').collect();
    template.len() == path.len() && template.iter().zip(&path).all(|(part, actual)| part.starts_with('{') || part == actual)
}

fn resolve(schema: &Value) -> &Value {
    match schema.get("$ref").and_then(Value::as_str) {
        Some(reference) => {
            let name = reference.strip_prefix("#/components/schemas/").expect("references point at schemas");
            let target = &DOCUMENT["components"]["schemas"][name];
            assert!(!target.is_null(), "{reference} doesn't resolve");
            resolve(target)
        }
        None => schema,
    }
}

/// Whether `value` matches `schema`, or where it doesn't. `strict` refuses fields the
/// schema doesn't list, except in the parts of an `allOf`, which each list only theirs.
pub fn validate(schema: &Value, value: &Value, at: &str, strict: bool) -> Result<(), String> {
    let schema = resolve(schema);
    if let Some(all) = schema.get("allOf").and_then(Value::as_array) {
        for part in all {
            validate(part, value, at, false)?;
        }
    }
    if let Some(one) = schema.get("oneOf").and_then(Value::as_array) {
        let matching = one.iter().filter(|part| validate(part, value, at, strict).is_ok()).count();
        if matching != 1 {
            let problems: Vec<_> = one.iter().filter_map(|part| validate(part, value, at, strict).err()).collect();
            return Err(format!("{at} matches {matching} of `oneOf`'s {} schemas: {problems:?}", one.len()));
        }
    }
    if let Some(types) = schema.get("type") {
        let allowed: Vec<&str> = match types {
            Value::String(name) => vec![name],
            Value::Array(names) => names.iter().filter_map(Value::as_str).collect(),
            _ => panic!("odd `type` at {at}: {types}"),
        };
        if !allowed.iter().any(|name| is_type(name, value)) {
            return Err(format!("{at} is {value}, not {allowed:?}"));
        }
    }
    if let Some(choices) = schema.get("enum").and_then(Value::as_array)
        && !choices.contains(value)
    {
        return Err(format!("{at} is {value}, not one of {choices:?}"));
    }
    if let Value::Object(fields) = value {
        for name in schema.get("required").and_then(Value::as_array).into_iter().flatten() {
            let name = name.as_str().unwrap();
            if !fields.contains_key(name) {
                return Err(format!("{at} lacks the required `{name}`"));
            }
        }
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            for (name, field) in fields {
                match properties.get(name) {
                    Some(property) => validate(property, field, &format!("{at}.{name}"), true)?,
                    None if strict && schema.get("additionalProperties").is_none() && !has_composition(schema) => {
                        return Err(format!("{at} has `{name}`, which the schema doesn't list"));
                    }
                    None => {}
                }
            }
        }
    }
    if let (Value::Array(items), Some(item)) = (value, schema.get("items")) {
        for (index, element) in items.iter().enumerate() {
            validate(item, element, &format!("{at}[{index}]"), true)?;
        }
    }
    Ok(())
}

fn has_composition(schema: &Value) -> bool {
    schema.get("allOf").is_some() || schema.get("oneOf").is_some()
}

fn is_type(name: &str, value: &Value) -> bool {
    match name {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => panic!("unknown type {name}"),
    }
}
