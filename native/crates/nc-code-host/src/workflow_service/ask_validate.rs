//! The typed-ask result validator (port of `apps/nc-code-cli/packages/dynamic-workflow/src/schema/validate.ts`).
//!
//! The engine makes validation **injected** ([`nc_code_workflow::types::ValidateFn`]): the core depends on
//! no JSON Schema implementation and judges by whatever the host hands it. TS injects the adapter
//! package's `validate` from `dynamic-workflow-run-launch.ts`; this crate has no JSON Schema dependency
//! (and should not gain a general-purpose one for a validator that is exactly "the subset
//! synthesizeAskSchemas emits"), so that one pure function is ported here clause by clause:
//!
//! - The keywords covered are precisely the ones synthesize emits: `$ref`/`$defs`, `const`, `enum`,
//!   `anyOf`, `type` (including an array of types), `properties`/`required`/`additionalProperties`,
//!   `items`/`prefixItems`/`minItems`/`maxItems`, `minLength`/`maxLength`/`pattern`,
//!   `minimum`/`maximum`/`exclusiveMinimum`/`exclusiveMaximum`. `additionalProperties: true` and an
//!   empty schema `{}` always pass.
//! - Violation text is **byte-for-byte the same shape** as TS: `<path>: expected <expected>, got <got>`,
//!   because it enters the actor's `submit_result` rejection tool_result verbatim, and that line is
//!   what the model's repair loop reads.
//!
//! Known divergences (all confined to extreme inputs, none changing the fact that a valid answer
//! passes):
//! - string length counts UTF-16 code units (matching JS `.length`), but `describeValue`'s 20-character
//!   cut never splits a surrogate pair (JS `slice` does), so the description of a very long emoji string
//!   can be one code unit shorter;
//! - `const`/`enum` equality uses `serde_json::Value`'s `PartialEq`, which distinguishes `1` from
//!   `1.0` where JS `===` does not. synthesize only emits literal enums (strings), so it is unreachable
//!   in practice;
//! - an uncompilable `pattern` passes (TS lets `new RegExp` throw and kills the ask; synthesize never
//!   emits an illegal pattern, and letting the host's own compile failure veto a legitimate answer is worse).

use nc_code_workflow::types::Violation;
use regex::Regex;
use serde_json::Value;

/// One validation: an empty list means it passed. A `None` schema (an untyped site) is not validated.
pub(crate) fn validate(schema: Option<&Value>, value: &Value) -> Vec<Violation> {
    let mut violations = Vec::new();
    if let Some(schema) = schema {
        check(schema, value, "$", schema, &mut violations);
    }
    violations
}

fn check(schema: &Value, value: &Value, path: &str, root: &Value, out: &mut Vec<Violation>) {
    let Some(schema) = schema.as_object() else {
        // A non-object schema is not a synthesize product; pass it rather than condemning a valid answer.
        return;
    };

    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        match resolve_ref(root, reference) {
            Some(resolved) => check(resolved, value, path, root, out),
            None => out.push(Violation {
                path: path.to_string(),
                expected: format!("schema {reference}"),
                got: "unresolved $ref".to_string(),
            }),
        }
        return;
    }

    if let Some(constant) = schema.get("const") {
        if constant != value {
            out.push(Violation {
                path: path.to_string(),
                expected: describe_value(constant),
                got: describe_value(value),
            });
        }
        return;
    }

    if let Some(Value::Array(candidates)) = schema.get("enum") {
        if !candidates.iter().any(|candidate| candidate == value) {
            let expected = candidates
                .iter()
                .map(describe_value)
                .collect::<Vec<_>>()
                .join(", ");
            out.push(Violation {
                path: path.to_string(),
                expected: format!("one of {expected}"),
                got: describe_value(value),
            });
        }
        return;
    }

    if let Some(Value::Array(branches)) = schema.get("anyOf") {
        let matched = branches.iter().any(|branch| {
            let mut trial = Vec::new();
            check(branch, value, path, root, &mut trial);
            trial.is_empty()
        });
        if !matched {
            out.push(Violation {
                path: path.to_string(),
                expected: format!("one of {} variants", branches.len()),
                got: describe_value(value),
            });
        }
        return;
    }

    if let Some(declared) = schema.get("type") {
        if !type_matches(declared, value) {
            out.push(Violation {
                path: path.to_string(),
                expected: type_name(declared),
                got: describe_value(value),
            });
            return;
        }
    }

    if schema.get("type").and_then(Value::as_str) == Some("object") {
        if let Value::Object(object) = value {
            check_object(schema, object, path, root, out);
        }
    }
    if schema.get("type").and_then(Value::as_str) == Some("array") {
        if let Value::Array(items) = value {
            check_array(schema, items, path, root, out);
        }
    }
    if let Value::String(text) = value {
        check_string(schema, text, path, out);
    }
    if value.is_number() {
        check_number(schema, value, path, out);
    }
}

fn check_object(
    schema: &serde_json::Map<String, Value>,
    value: &serde_json::Map<String, Value>,
    path: &str,
    root: &Value,
    out: &mut Vec<Violation>,
) {
    if let Some(Value::Array(required)) = schema.get("required") {
        for key in required.iter().filter_map(Value::as_str) {
            if !value.contains_key(key) {
                out.push(Violation {
                    path: format!("{path}.{key}"),
                    expected: "present".to_string(),
                    got: "missing".to_string(),
                });
            }
        }
    }
    let empty = serde_json::Map::new();
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    for (key, property) in value {
        if let Some(property_schema) = properties.get(key) {
            check(
                property_schema,
                property,
                &format!("{path}.{key}"),
                root,
                out,
            );
            continue;
        }
        match schema.get("additionalProperties") {
            Some(Value::Bool(false)) => out.push(Violation {
                path: format!("{path}.{key}"),
                expected: "no additional property".to_string(),
                got: describe_value(property),
            }),
            Some(additional) if !additional.is_boolean() && !additional.is_null() => {
                check(additional, property, &format!("{path}.{key}"), root, out);
            }
            _ => {}
        }
    }
}

fn check_array(
    schema: &serde_json::Map<String, Value>,
    value: &[Value],
    path: &str,
    root: &Value,
    out: &mut Vec<Violation>,
) {
    if let Some(minimum) = schema.get("minItems").and_then(Value::as_u64) {
        if value.len() < minimum as usize {
            out.push(Violation {
                path: path.to_string(),
                expected: format!("at least {minimum} items"),
                got: format!("array({})", value.len()),
            });
        }
    }
    if let Some(maximum) = schema.get("maxItems").and_then(Value::as_u64) {
        if value.len() > maximum as usize {
            out.push(Violation {
                path: path.to_string(),
                expected: format!("at most {maximum} items"),
                got: format!("array({})", value.len()),
            });
        }
    }
    if let Some(Value::Array(prefix)) = schema.get("prefixItems") {
        // zip stops naturally at a position the schema has but the value does not (the same meaning as
        // TS's `index < value.length`).
        for (index, (item_schema, item)) in prefix.iter().zip(value.iter()).enumerate() {
            check(item_schema, item, &format!("{path}[{index}]"), root, out);
        }
        if let Some(items) = schema.get("items") {
            for (index, item) in value.iter().enumerate().skip(prefix.len()) {
                check(items, item, &format!("{path}[{index}]"), root, out);
            }
        }
        return;
    }
    if let Some(items) = schema.get("items") {
        for (index, item) in value.iter().enumerate() {
            check(items, item, &format!("{path}[{index}]"), root, out);
        }
    }
}

fn check_string(
    schema: &serde_json::Map<String, Value>,
    value: &str,
    path: &str,
    out: &mut Vec<Violation>,
) {
    // JS `.length` counts UTF-16 code units; the same measure is used here, otherwise one and the
    // same string would get a different minLength verdict on the two hosts.
    let length = value.encode_utf16().count();
    if let Some(minimum) = schema.get("minLength").and_then(Value::as_u64) {
        if length < minimum as usize {
            out.push(Violation {
                path: path.to_string(),
                expected: format!("string length >= {minimum}"),
                got: format!("length {length}"),
            });
        }
    }
    if let Some(maximum) = schema.get("maxLength").and_then(Value::as_u64) {
        if length > maximum as usize {
            out.push(Violation {
                path: path.to_string(),
                expected: format!("string length <= {maximum}"),
                got: format!("length {length}"),
            });
        }
    }
    if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) {
        // See the file header: an uncompilable pattern passes (TS throws, synthesize never emits one).
        if let Ok(regex) = Regex::new(pattern) {
            if !regex.is_match(value) {
                out.push(Violation {
                    path: path.to_string(),
                    expected: format!("match /{pattern}/"),
                    got: describe_value(&Value::String(value.to_string())),
                });
            }
        }
    }
}

fn check_number(
    schema: &serde_json::Map<String, Value>,
    value: &Value,
    path: &str,
    out: &mut Vec<Violation>,
) {
    let Some(number) = value.as_f64() else {
        return;
    };
    if schema.get("type").and_then(Value::as_str) == Some("integer") && number.fract() != 0.0 {
        out.push(Violation {
            path: path.to_string(),
            expected: "integer".to_string(),
            got: format!("number {number}"),
        });
    }
    if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64) {
        if number < minimum {
            out.push(Violation {
                path: path.to_string(),
                expected: format!(">= {minimum}"),
                got: format!("number {number}"),
            });
        }
    }
    if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64) {
        if number > maximum {
            out.push(Violation {
                path: path.to_string(),
                expected: format!("<= {maximum}"),
                got: format!("number {number}"),
            });
        }
    }
    if let Some(minimum) = schema.get("exclusiveMinimum").and_then(Value::as_f64) {
        if number <= minimum {
            out.push(Violation {
                path: path.to_string(),
                expected: format!("> {minimum}"),
                got: format!("number {number}"),
            });
        }
    }
    if let Some(maximum) = schema.get("exclusiveMaximum").and_then(Value::as_f64) {
        if number >= maximum {
            out.push(Violation {
                path: path.to_string(),
                expected: format!("< {maximum}"),
                got: format!("number {number}"),
            });
        }
    }
}

fn type_matches(declared: &Value, value: &Value) -> bool {
    match declared {
        Value::String(name) => matches_one(name, value),
        Value::Array(names) => names
            .iter()
            .filter_map(Value::as_str)
            .any(|name| matches_one(name, value)),
        // A non-string `type` is not a synthesize product.
        _ => true,
    }
}

fn matches_one(name: &str, value: &Value) -> bool {
    match name {
        "string" => value.is_string(),
        // serde_json numbers are always finite, so `Number.isFinite` would be constantly true.
        "number" | "integer" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => true,
    }
}

fn type_name(declared: &Value) -> String {
    match declared {
        Value::String(name) => name.clone(),
        Value::Array(names) => names
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" | "),
        _ => "value".to_string(),
    }
}

fn resolve_ref<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let name = reference.strip_prefix("#/$defs/")?;
    root.get("$defs")?.as_object()?.get(name)
}

/// A short description of a value (TS `describeValue`): its type plus a condensed rendering.
fn describe_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Array(items) => format!("array({})", items.len()),
        Value::String(text) => {
            let shown = truncate_utf16(text, 20);
            format!("string {}", Value::String(shown))
        }
        Value::Number(number) => format!("number {}", js_number(number)),
        Value::Bool(flag) => format!("boolean {flag}"),
        Value::Object(_) => "object".to_string(),
    }
}

/// Truncate to `max` UTF-16 code units (never splitting a surrogate pair), appending an ellipsis when long.
fn truncate_utf16(text: &str, max: usize) -> String {
    let mut units = 0usize;
    let mut out = String::new();
    for character in text.chars() {
        let width = character.len_utf16();
        if units + width > max {
            out.push('…');
            return out;
        }
        units += width;
        out.push(character);
    }
    out
}

/// Approximation of JS `String(number)`: an integral value carries no `.0` (Rust's `f64` Display happens to agree).
fn js_number(number: &serde_json::Number) -> String {
    match (number.as_i64(), number.as_u64()) {
        (Some(value), _) => value.to_string(),
        (_, Some(value)) => value.to_string(),
        _ => format!("{}", number.as_f64().unwrap_or(0.0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    /// `path → expected` (the assertion looks at these two columns only; `got` is covered by the describe tests).
    fn violations_by_path(violations: &[Violation]) -> BTreeMap<String, String> {
        violations
            .iter()
            .map(|violation| (violation.path.clone(), violation.expected.clone()))
            .collect()
    }

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "steps": { "type": "array", "minItems": 1, "items": { "type": "string" } },
                "rationale": { "type": "string", "minLength": 3 }
            },
            "required": ["steps", "rationale"],
            "additionalProperties": false
        })
    }

    #[test]
    fn valid_answers_pass_and_unknown_schema_is_permissive() {
        assert!(validate(
            Some(&schema()),
            &json!({"steps": ["a"], "rationale": "why"})
        )
        .is_empty());
        // An empty schema and an absent schema both pass (the permissive schema of `unknown`).
        assert!(validate(Some(&json!({})), &json!({"anything": [1]})).is_empty());
        assert!(validate(None, &json!(null)).is_empty());
    }

    #[test]
    fn violations_name_path_expected_and_got() {
        let violations = validate(
            Some(&schema()),
            &json!({"steps": ["a", 2], "rationale": "x", "extra": true}),
        );
        let by_path = violations_by_path(&violations);
        assert_eq!(by_path.get("$.steps[1]"), Some(&"string".to_string()));
        assert_eq!(
            by_path.get("$.rationale"),
            Some(&"string length >= 3".to_string())
        );
        assert_eq!(
            by_path.get("$.extra"),
            Some(&"no additional property".to_string())
        );

        let missing = validate(Some(&schema()), &json!({"steps": []}));
        let by_path = violations_by_path(&missing);
        assert_eq!(by_path.get("$.rationale"), Some(&"present".to_string()));
        assert_eq!(
            by_path.get("$.steps"),
            Some(&"at least 1 items".to_string())
        );
    }

    #[test]
    fn enum_const_anyof_and_refs_are_checked() {
        let schema = json!({
            "type": "object",
            "properties": {
                "mode": { "enum": ["fast", "slow"] },
                "flag": { "const": true },
                "either": { "anyOf": [{ "type": "string" }, { "type": "null" }] },
                "nested": { "$ref": "#/$defs/Item" }
            },
            "$defs": { "Item": { "type": "object", "required": ["id"] } }
        });
        let violations = validate(
            Some(&schema),
            &json!({"mode": "medium", "flag": false, "either": 3, "nested": {}}),
        );
        let by_path = violations_by_path(&violations);
        assert_eq!(
            by_path.get("$.mode"),
            Some(&"one of string \"fast\", string \"slow\"".to_string())
        );
        assert_eq!(by_path.get("$.flag"), Some(&"boolean true".to_string()));
        assert_eq!(
            by_path.get("$.either"),
            Some(&"one of 2 variants".to_string())
        );
        assert_eq!(by_path.get("$.nested.id"), Some(&"present".to_string()));
        assert!(validate(
            Some(&schema),
            &json!({"mode": "fast", "flag": true, "either": null, "nested": {"id": 1}})
        )
        .is_empty());
    }

    #[test]
    fn integer_type_and_numeric_bounds_use_js_semantics() {
        let schema = json!({"type": "integer", "minimum": 1, "maximum": 4});
        assert!(validate(Some(&schema), &json!(2)).is_empty());
        assert!(
            validate(Some(&schema), &json!(2.0)).is_empty(),
            "Number.isInteger(2.0) is true"
        );
        let violations = validate(Some(&schema), &json!(2.5));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].expected, "integer");
        let violations = validate(Some(&schema), &json!(9));
        assert_eq!(violations[0].expected, "<= 4");
    }

    #[test]
    fn string_lengths_count_utf16_units_like_javascript() {
        let schema = json!({"type": "string", "minLength": 2});
        // One emoji is one character but two UTF-16 code units: JS judges it as satisfying minLength 2.
        assert!(validate(Some(&schema), &json!("😀")).is_empty());
        assert_eq!(validate(Some(&schema), &json!("x")).len(), 1);
    }

    #[test]
    fn describe_truncates_long_strings() {
        let long = "a".repeat(30);
        let text = describe_value(&Value::String(long));
        assert!(text.starts_with("string \"aaaaaaaaaaaaaaaaaaaa…"), "{text}");
        assert_eq!(describe_value(&json!([1, 2])), "array(2)");
    }
}
