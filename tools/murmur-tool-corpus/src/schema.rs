//! A deliberately small JSON Schema subset, validated fail-closed.
//!
//! Two entry points, used at two different times:
//!
//! * [`check_schema`] runs when the operator's config is loaded. It rejects any keyword
//!   outside [`SUPPORTED_KEYWORDS`], naming it. Silently ignoring a constraint the
//!   operator wrote would make "schema validation on append" a lie, and in an append-only
//!   log the bad record that gets through is permanent.
//! * [`validate_body`] runs on every append, against a schema `check_schema` already
//!   accepted.

use serde_json::Value;

/// Every keyword this validator understands. Anything else in an operator schema is a
/// configuration error.
pub const SUPPORTED_KEYWORDS: [&str; 12] = [
    "type",
    "properties",
    "required",
    "items",
    "enum",
    "minLength",
    "maxLength",
    "minimum",
    "maximum",
    "minItems",
    "maxItems",
    "additionalProperties",
];

/// Every value the `type` keyword may take.
pub const SUPPORTED_TYPES: [&str; 7] = [
    "object", "array", "string", "number", "integer", "boolean", "null",
];

/// Config-time check: every keyword in `schema`, at every depth, is supported.
///
/// The error string names the offending keyword so the operator can find it. It does not
/// name the type — the caller adds that, since only it knows which type the schema
/// belongs to.
pub fn check_schema(schema: &Value) -> Result<(), String> {
    check_at(schema, "schema")
}

fn check_at(schema: &Value, path: &str) -> Result<(), String> {
    let obj = match schema.as_object() {
        Some(o) => o,
        None => {
            return Err(format!(
                "{path} must be a JSON object; boolean and other schema forms are not supported"
            ))
        }
    };

    for (keyword, value) in obj {
        if !SUPPORTED_KEYWORDS.contains(&keyword.as_str()) {
            return Err(format!(
                "{path}: unsupported schema keyword \"{keyword}\"; supported keywords are {}",
                SUPPORTED_KEYWORDS.join(", ")
            ));
        }
        match keyword.as_str() {
            "type" => {
                let name = value.as_str().ok_or_else(|| {
                    format!("{path}.type must be a string naming one of {}", SUPPORTED_TYPES.join(", "))
                })?;
                if !SUPPORTED_TYPES.contains(&name) {
                    return Err(format!(
                        "{path}.type: unsupported type \"{name}\"; supported types are {}",
                        SUPPORTED_TYPES.join(", ")
                    ));
                }
            }
            "properties" => {
                let props = value
                    .as_object()
                    .ok_or_else(|| format!("{path}.properties must be a JSON object"))?;
                for (name, sub) in props {
                    check_at(sub, &format!("{path}.properties.{name}"))?;
                }
            }
            "items" => check_at(value, &format!("{path}.items"))?,
            "required" => {
                let names = value
                    .as_array()
                    .ok_or_else(|| format!("{path}.required must be an array of strings"))?;
                if names.iter().any(|n| !n.is_string()) {
                    return Err(format!("{path}.required must be an array of strings"));
                }
            }
            "enum" => {
                let choices = value
                    .as_array()
                    .ok_or_else(|| format!("{path}.enum must be a non-empty array"))?;
                if choices.is_empty() {
                    return Err(format!("{path}.enum must be a non-empty array"));
                }
            }
            "additionalProperties" => {
                if !value.is_boolean() {
                    return Err(format!(
                        "{path}: unsupported schema keyword \"additionalProperties\" in object \
                         form; only the boolean form is supported"
                    ));
                }
            }
            "minLength" | "maxLength" | "minItems" | "maxItems" => {
                if value.as_u64().is_none() {
                    return Err(format!("{path}.{keyword} must be a non-negative integer"));
                }
            }
            "minimum" | "maximum" => {
                if value.as_f64().is_none() {
                    return Err(format!("{path}.{keyword} must be a number"));
                }
            }
            _ => unreachable!("keyword was checked against SUPPORTED_KEYWORDS above"),
        }
    }
    Ok(())
}

/// Append-time check: does `body` satisfy `schema`?
///
/// The error string names the failing field path and the constraint it failed.
pub fn validate_body(schema: &Value, body: &Value) -> Result<(), String> {
    validate_at(schema, body, "body")
}

fn validate_at(schema: &Value, value: &Value, path: &str) -> Result<(), String> {
    let obj = match schema.as_object() {
        Some(o) => o,
        None => return Err(format!("{path}: schema is not a JSON object")),
    };

    if let Some(expected) = obj.get("type").and_then(Value::as_str) {
        if !matches_type(expected, value) {
            return Err(format!(
                "{path}: expected type \"{expected}\", got {}",
                type_name(value)
            ));
        }
    }

    if let Some(choices) = obj.get("enum").and_then(Value::as_array) {
        if !choices.contains(value) {
            return Err(format!("{path}: value is not one of the {} enum choices", choices.len()));
        }
    }

    match value {
        Value::String(s) => {
            let len = s.chars().count() as u64;
            if let Some(min) = obj.get("minLength").and_then(Value::as_u64) {
                if len < min {
                    return Err(format!("{path}: minLength is {min}, got a string of length {len}"));
                }
            }
            if let Some(max) = obj.get("maxLength").and_then(Value::as_u64) {
                if len > max {
                    return Err(format!("{path}: maxLength is {max}, got a string of length {len}"));
                }
            }
        }
        Value::Number(n) => {
            let x = n.as_f64().unwrap_or(f64::NAN);
            if let Some(min) = obj.get("minimum").and_then(Value::as_f64) {
                if x < min {
                    return Err(format!("{path}: minimum is {min}, got {x}"));
                }
            }
            if let Some(max) = obj.get("maximum").and_then(Value::as_f64) {
                if x > max {
                    return Err(format!("{path}: maximum is {max}, got {x}"));
                }
            }
        }
        Value::Array(items) => {
            let len = items.len() as u64;
            if let Some(min) = obj.get("minItems").and_then(Value::as_u64) {
                if len < min {
                    return Err(format!("{path}: minItems is {min}, got {len}"));
                }
            }
            if let Some(max) = obj.get("maxItems").and_then(Value::as_u64) {
                if len > max {
                    return Err(format!("{path}: maxItems is {max}, got {len}"));
                }
            }
            if let Some(item_schema) = obj.get("items") {
                for (i, item) in items.iter().enumerate() {
                    validate_at(item_schema, item, &format!("{path}[{i}]"))?;
                }
            }
        }
        Value::Object(map) => {
            if let Some(required) = obj.get("required").and_then(Value::as_array) {
                for name in required.iter().filter_map(Value::as_str) {
                    if !map.contains_key(name) {
                        return Err(format!("{path}: missing required property \"{name}\""));
                    }
                }
            }
            let properties = obj.get("properties").and_then(Value::as_object);
            if obj.get("additionalProperties") == Some(&Value::Bool(false)) {
                for name in map.keys() {
                    let declared = properties.map(|p| p.contains_key(name)).unwrap_or(false);
                    if !declared {
                        return Err(format!(
                            "{path}: additionalProperties is false, but property \"{name}\" is not declared"
                        ));
                    }
                }
            }
            if let Some(properties) = properties {
                for (name, sub_schema) in properties {
                    if let Some(sub_value) = map.get(name) {
                        validate_at(sub_schema, sub_value, &format!("{path}.{name}"))?;
                    }
                }
            }
        }
        _ => {}
    }

    Ok(())
}

fn matches_type(expected: &str, value: &Value) -> bool {
    match expected {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        // A JSON number with a fractional part is not an integer, however it is spelled.
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        _ => false,
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok(schema: &Value, body: &Value) {
        check_schema(schema).expect("schema uses only supported keywords");
        validate_body(schema, body).expect("body should validate");
    }

    fn reject(schema: &Value, body: &Value, needle: &str) {
        check_schema(schema).expect("schema uses only supported keywords");
        let err = validate_body(schema, body).expect_err("body should be rejected");
        assert!(err.contains(needle), "expected {needle:?} in {err:?}");
    }

    #[test]
    fn type_keyword_accepts_and_rejects() {
        for (name, good, bad) in [
            ("object", json!({}), json!([])),
            ("array", json!([]), json!({})),
            ("string", json!("x"), json!(1)),
            ("number", json!(1.5), json!("x")),
            ("integer", json!(3), json!(1.5)),
            ("boolean", json!(true), json!("true")),
            ("null", json!(null), json!(0)),
        ] {
            let schema = json!({ "type": name });
            ok(&schema, &good);
            reject(&schema, &bad, "expected type");
        }
    }

    #[test]
    fn integer_rejects_a_fractional_number_that_number_accepts() {
        ok(&json!({ "type": "number" }), &json!(1.5));
        reject(&json!({ "type": "integer" }), &json!(1.5), "expected type \"integer\"");
        ok(&json!({ "type": "integer" }), &json!(-4));
    }

    #[test]
    fn properties_and_required_accept_and_reject() {
        let schema = json!({
            "type": "object",
            "required": ["text"],
            "properties": { "text": { "type": "string" } }
        });
        ok(&schema, &json!({ "text": "hi" }));
        reject(&schema, &json!({}), "missing required property \"text\"");
        reject(&schema, &json!({ "text": 3 }), "body.text");
    }

    #[test]
    fn additional_properties_false_accepts_and_rejects() {
        let schema = json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "additionalProperties": false
        });
        ok(&schema, &json!({ "text": "hi" }));
        reject(&schema, &json!({ "text": "hi", "extra": 1 }), "\"extra\"");
        // The true form imposes nothing.
        let open = json!({ "type": "object", "additionalProperties": true });
        ok(&open, &json!({ "anything": 1 }));
    }

    #[test]
    fn enum_accepts_and_rejects() {
        let schema = json!({ "enum": ["a", "b", 3] });
        ok(&schema, &json!("a"));
        ok(&schema, &json!(3));
        reject(&schema, &json!("c"), "enum choices");
    }

    #[test]
    fn length_and_range_and_item_bounds_accept_and_reject() {
        let s = json!({ "type": "string", "minLength": 2, "maxLength": 4 });
        ok(&s, &json!("abc"));
        reject(&s, &json!("a"), "minLength is 2");
        reject(&s, &json!("abcde"), "maxLength is 4");

        let n = json!({ "type": "number", "minimum": 1, "maximum": 10 });
        ok(&n, &json!(5));
        reject(&n, &json!(0), "minimum is 1");
        reject(&n, &json!(11), "maximum is 10");

        let a = json!({ "type": "array", "minItems": 1, "maxItems": 2 });
        ok(&a, &json!([1]));
        reject(&a, &json!([]), "minItems is 1");
        reject(&a, &json!([1, 2, 3]), "maxItems is 2");
    }

    #[test]
    fn nested_object_and_array_validation_names_the_failing_path() {
        let schema = json!({
            "type": "object",
            "required": ["tags", "meta"],
            "properties": {
                "tags": { "type": "array", "items": { "type": "string" } },
                "meta": {
                    "type": "object",
                    "required": ["n"],
                    "properties": { "n": { "type": "integer", "minimum": 0 } }
                }
            }
        });
        ok(&schema, &json!({ "tags": ["a", "b"], "meta": { "n": 2 } }));
        reject(&schema, &json!({ "tags": ["a", 2], "meta": { "n": 2 } }), "body.tags[1]");
        reject(&schema, &json!({ "tags": [], "meta": { "n": -1 } }), "body.meta.n");
        reject(&schema, &json!({ "tags": [], "meta": {} }), "body.meta: missing required property \"n\"");
    }

    #[test]
    fn unsupported_keywords_are_named() {
        for (schema, needle) in [
            (json!({ "pattern": "^a" }), "pattern"),
            (json!({ "oneOf": [] }), "oneOf"),
            (json!({ "anyOf": [] }), "anyOf"),
            (json!({ "$ref": "#/x" }), "$ref"),
            (json!({ "format": "email" }), "format"),
        ] {
            let err = check_schema(&schema).expect_err("unsupported keyword must be rejected");
            assert!(err.contains(needle), "expected {needle:?} in {err:?}");
        }
    }

    #[test]
    fn object_form_additional_properties_is_rejected_by_name() {
        let err = check_schema(&json!({ "additionalProperties": { "type": "string" } }))
            .expect_err("object-form additionalProperties must be rejected");
        assert!(err.contains("additionalProperties"), "{err}");
        assert!(err.contains("boolean form"), "{err}");
    }

    #[test]
    fn unsupported_keyword_nested_inside_properties_is_rejected() {
        let err = check_schema(&json!({
            "type": "object",
            "properties": { "text": { "type": "string", "pattern": "^a" } }
        }))
        .expect_err("nested unsupported keyword must be rejected");
        assert!(err.contains("pattern"), "{err}");
        assert!(err.contains("properties.text"), "{err}");
    }

    #[test]
    fn unsupported_keyword_nested_inside_items_is_rejected() {
        let err = check_schema(&json!({
            "type": "array",
            "items": { "$ref": "#/definitions/x" }
        }))
        .expect_err("nested unsupported keyword must be rejected");
        assert!(err.contains("$ref"), "{err}");
        assert!(err.contains("items"), "{err}");
    }

    #[test]
    fn a_non_object_schema_is_rejected() {
        assert!(check_schema(&json!(true)).is_err());
        assert!(check_schema(&json!("string")).is_err());
    }

    #[test]
    fn an_unsupported_type_value_is_rejected() {
        let err = check_schema(&json!({ "type": "any" })).expect_err("type must be supported");
        assert!(err.contains("any"), "{err}");
    }
}
