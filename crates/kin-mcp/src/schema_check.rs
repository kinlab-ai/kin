// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Test support: grade a JSON value against a served schema the way a
//! validating client grades it.

use serde_json::Value;

/// A JSON Schema check over exactly the keywords the typed requests emit, so a
/// probe is graded against the served schema the way a validating client
/// grades it. An unknown keyword fails the check, so a new keyword cannot be
/// served without this learning it.
pub(crate) fn schema_errors(schema: &Value, instance: &Value, at: &str, errors: &mut Vec<String>) {
    let Some(object) = schema.as_object() else {
        errors.push(format!("{at}: schema is not an object"));
        return;
    };
    for key in object.keys() {
        assert!(
            [
                "type",
                "properties",
                "required",
                "items",
                "enum",
                "minimum",
                "maximum",
                "minLength",
                "maxLength",
                "minItems",
                "maxItems",
                "additionalProperties",
                "minProperties",
                "maxProperties",
                "anyOf",
                "oneOf",
                "description",
                "default",
                "title",
                "format",
                "pattern",
                "const",
                "$schema",
            ]
            .contains(&key.as_str()),
            "{at}: the probe checker does not know the keyword {key}"
        );
    }
    let kinds: Vec<&str> = match object.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if !kinds.is_empty() {
        let fits = kinds.iter().any(|kind| match *kind {
            "object" => instance.is_object(),
            "array" => instance.is_array(),
            "string" => instance.is_string(),
            "integer" => instance.is_i64() || instance.is_u64(),
            "number" => instance.is_number(),
            "boolean" => instance.is_boolean(),
            "null" => instance.is_null(),
            _ => true,
        });
        if !fits {
            errors.push(format!("{at}: not a {}", kinds.join(" or ")));
            return;
        }
    }
    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        if !values.contains(instance) {
            errors.push(format!("{at}: not in enum"));
        }
    }
    if let Some(constant) = object.get("const") {
        if constant != instance {
            errors.push(format!("{at}: not the const"));
        }
    }
    if let Some(number) = instance.as_f64() {
        if object
            .get("minimum")
            .and_then(Value::as_f64)
            .is_some_and(|minimum| number < minimum)
        {
            errors.push(format!("{at}: below minimum"));
        }
        if object
            .get("maximum")
            .and_then(Value::as_f64)
            .is_some_and(|maximum| number > maximum)
        {
            errors.push(format!("{at}: above maximum"));
        }
    }
    if let Some(text) = instance.as_str() {
        let length = text.chars().count() as u64;
        if object
            .get("minLength")
            .and_then(Value::as_u64)
            .is_some_and(|minimum| length < minimum)
        {
            errors.push(format!("{at}: too short"));
        }
        if object
            .get("maxLength")
            .and_then(Value::as_u64)
            .is_some_and(|maximum| length > maximum)
        {
            errors.push(format!("{at}: too long"));
        }
    }
    if let Some(items) = instance.as_array() {
        let count = items.len() as u64;
        if object
            .get("minItems")
            .and_then(Value::as_u64)
            .is_some_and(|minimum| count < minimum)
        {
            errors.push(format!("{at}: too few items"));
        }
        if object
            .get("maxItems")
            .and_then(Value::as_u64)
            .is_some_and(|maximum| count > maximum)
        {
            errors.push(format!("{at}: too many items"));
        }
        if let Some(item) = object.get("items") {
            for (index, element) in items.iter().enumerate() {
                schema_errors(item, element, &format!("{at}[{index}]"), errors);
            }
        }
    }
    if let Some(fields) = instance.as_object() {
        let count = fields.len() as u64;
        if object
            .get("minProperties")
            .and_then(Value::as_u64)
            .is_some_and(|minimum| count < minimum)
        {
            errors.push(format!("{at}: too few properties"));
        }
        if object
            .get("maxProperties")
            .and_then(Value::as_u64)
            .is_some_and(|maximum| count > maximum)
        {
            errors.push(format!("{at}: too many properties"));
        }
        if let Some(required) = object.get("required").and_then(Value::as_array) {
            for name in required.iter().filter_map(Value::as_str) {
                if !fields.contains_key(name) {
                    errors.push(format!("{at}: missing {name}"));
                }
            }
        }
        let properties = object.get("properties").and_then(Value::as_object);
        for (name, value) in fields {
            match properties.and_then(|properties| properties.get(name)) {
                Some(property) => schema_errors(property, value, &format!("{at}.{name}"), errors),
                None => match object.get("additionalProperties") {
                    Some(Value::Bool(false)) => errors.push(format!("{at}: {name} is not allowed")),
                    Some(extra @ Value::Object(_)) => {
                        schema_errors(extra, value, &format!("{at}.{name}"), errors)
                    }
                    _ => {}
                },
            }
        }
    }
    for (combinator, need) in [("anyOf", 1usize), ("oneOf", 1usize)] {
        if let Some(branches) = object.get(combinator).and_then(Value::as_array) {
            let passing = branches
                .iter()
                .filter(|branch| {
                    let mut branch_errors = Vec::new();
                    schema_errors(branch, instance, at, &mut branch_errors);
                    branch_errors.is_empty()
                })
                .count();
            let ok = if combinator == "oneOf" {
                passing == need
            } else {
                passing >= need
            };
            if !ok {
                errors.push(format!("{at}: no {combinator} branch fits"));
            }
        }
    }
}

/// Every way `instance` breaks `schema`, empty when it fits.
pub(crate) fn errors(schema: &Value, instance: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    schema_errors(schema, instance, "$", &mut errors);
    errors
}
