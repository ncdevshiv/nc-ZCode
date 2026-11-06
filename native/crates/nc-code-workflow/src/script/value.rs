//! JSON value semantics for the NWS interpreter.
//!
//! NWS values are `serde_json::Value` all the way down: an ask result, a world read, a
//! template, the return value. This module is the whole dynamic part of an otherwise
//! static language — truthiness, the two coercions `+` and comparison need, and member
//! access — written once so the interpreter stays a tree walk.
//!
//! Where the semantics deliberately differ from JavaScript:
//! - a missing object member is `null`, not `undefined` (NWS has no `undefined`);
//! - arithmetic other than `+` requires numbers on both sides instead of coercing strings,
//!   because a coerced `"3" * "4"` is a bug the author should see as one named error;
//! - a runtime failure carries the source position in its message, since there is no
//!   JavaScript stack to unwind.

use serde_json::{json, Value};

use crate::types::{WorkflowError, WorkflowErrorCode};

use super::ast::Span;
use super::ast::{BinaryOp, UnaryOp};

/// A runtime failure inside the script (as opposed to an engine rejection, which keeps
/// its own structured code).
pub fn script_error(span: Span, message: impl Into<String>) -> WorkflowError {
    WorkflowError::new(
        WorkflowErrorCode::DriverError,
        format!("L{}:C{} {}", span.line, span.column, message.into()),
    )
}

/// A computed number as a JSON value.
///
/// Integral results become JSON integers: `1 + 2` must render as `3`, not `3.0`, because
/// the journal hashes the canonical rendering of a node's input and the TypeScript path
/// hashed `3`. `serde_json` keeps an `f64` of `3.0` as a float, so the shape is fixed here
/// rather than at each call site.
fn number_out(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_991.0 {
        return json!(value as i64);
    }
    json!(value)
}

/// Truthiness, JavaScript-shaped: `null`, `false`, `0`, `NaN` and `""` are falsy; every
/// array and object is truthy, including an empty one.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number
            .as_f64()
            .is_some_and(|number| number != 0.0 && !number.is_nan()),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// The string a value contributes to a template, a `+` concatenation or a log line.
///
/// Strings pass through unchanged; everything else is the JSON rendering, so an array of
/// paths interleaves the way the author wrote it and a number keeps its shortest form.
pub fn display(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The number an arithmetic operand must be; anything else is the author's bug to see.
fn number(span: Span, value: &Value, role: &str) -> Result<f64, WorkflowError> {
    match value {
        Value::Number(number) => number.as_f64().ok_or_else(|| {
            script_error(
                span,
                format!("{role} is a number too large for arithmetic: {number}"),
            )
        }),
        other => Err(script_error(
            span,
            format!(
                "{role} must be a number for this operator, got {}",
                kind_of(other)
            ),
        )),
    }
}

pub fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

pub fn apply_unary(op: UnaryOp, value: Value, span: Span) -> Result<Value, WorkflowError> {
    match op {
        UnaryOp::Not => Ok(Value::Bool(!truthy(&value))),
        UnaryOp::Negate => number(span, &value, "the operand of `-`").map(|n| number_out(-n)),
    }
}

/// `&&` and `||` return an operand, not a boolean, exactly as JavaScript does: that is
/// what makes `let title = args.title || "untitled"` read the way authors expect.
pub fn apply_binary(
    op: BinaryOp,
    left: Value,
    right: Value,
    span: Span,
) -> Result<Value, WorkflowError> {
    match op {
        BinaryOp::And => Ok(if truthy(&left) { right } else { left }),
        BinaryOp::Or => Ok(if truthy(&left) { left } else { right }),
        BinaryOp::Equal => Ok(Value::Bool(equal(&left, &right))),
        BinaryOp::NotEqual => Ok(Value::Bool(!equal(&left, &right))),
        BinaryOp::Add => Ok(match (&left, &right) {
            // `+` concatenates when either side is a string, and adds otherwise — the one
            // operator with two behaviors, because both are unavoidable in a script that
            // builds prompts.
            (Value::String(_), _) | (_, Value::String(_)) => {
                json!(format!("{}{}", display(&left), display(&right)))
            }
            _ => {
                let a = number(span, &left, "the left operand of `+`")?;
                let b = number(span, &right, "the right operand of `+`")?;
                number_out(a + b)
            }
        }),
        BinaryOp::Subtract | BinaryOp::Multiply | BinaryOp::Divide | BinaryOp::Remainder => {
            let a = number(span, &left, "the left operand")?;
            let b = number(span, &right, "the right operand")?;
            Ok(match op {
                BinaryOp::Subtract => number_out(a - b),
                BinaryOp::Multiply => number_out(a * b),
                BinaryOp::Divide => number_out(a / b),
                BinaryOp::Remainder => number_out(a % b),
                _ => unreachable!("arithmetic operators handled above"),
            })
        }
        BinaryOp::Less | BinaryOp::LessEqual | BinaryOp::Greater | BinaryOp::GreaterEqual => {
            // Both numbers or both strings; a mixed comparison would be JavaScript's
            // coercion surprise, and a script that hits it is a script with a bug.
            let ordering = match (&left, &right) {
                (Value::Number(_), Value::Number(_)) => {
                    let a = number(span, &left, "the left operand of a comparison")?;
                    let b = number(span, &right, "the right operand of a comparison")?;
                    a.partial_cmp(&b)
                }
                (Value::String(a), Value::String(b)) => a.as_str().partial_cmp(b.as_str()),
                (Value::String(_), Value::Number(_)) | (Value::Number(_), Value::String(_)) => {
                    return Err(script_error(
                        span,
                        format!(
                            "comparison mixes a string ({left}) with a number ({right}); NWS \
                             does not coerce for `<`/`>` — convert explicitly"
                        ),
                    ));
                }
                _ => {
                    return Err(script_error(
                        span,
                        format!(
                            "comparison needs two numbers or two strings, got {} and {}",
                            kind_of(&left),
                            kind_of(&right)
                        ),
                    ));
                }
            };
            let Some(ordering) = ordering else {
                return Err(script_error(
                    span,
                    format!("cannot compare {left} with {right}"),
                ));
            };
            Ok(Value::Bool(match op {
                BinaryOp::Less => ordering.is_lt(),
                BinaryOp::LessEqual => ordering.is_le(),
                BinaryOp::Greater => ordering.is_gt(),
                BinaryOp::GreaterEqual => ordering.is_ge(),
                _ => unreachable!("comparison operators handled above"),
            }))
        }
    }
}

/// Deep equality with JSON semantics; numbers compare by value so `1` equals `1.0`.
fn equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        _ => left == right,
    }
}

/// Member access: `.length` on strings and arrays, a key on objects, `null` for anything
/// a JavaScript author would have called `undefined`.
pub fn member(value: &Value, property: &str, span: Span) -> Result<Value, WorkflowError> {
    match value {
        Value::Object(map) => Ok(map.get(property).cloned().unwrap_or(Value::Null)),
        Value::Array(items) => match property {
            "length" => Ok(json!(items.len())),
            _ => Err(script_error(
                span,
                format!(
                    "an array has no member `{property}`: use [i] for an element, .length \
                     for the count, or Promise.all([…]) to await several calls"
                ),
            )),
        },
        Value::String(text) => match property {
            "length" => Ok(json!(text.chars().count())),
            "trim" | "toLowerCase" | "toUpperCase" | "split" | "includes" | "startsWith"
            | "endsWith" | "replace" => Err(script_error(
                span,
                format!(
                    "`{property}` is a string method and NWS has no methods: compare with \
                     `==`, concatenate with `+`, or ask a subagent to shape the text"
                ),
            )),
            _ => Err(script_error(
                span,
                format!("a string has no member `{property}`"),
            )),
        },
        Value::Null => Err(script_error(
            span,
            format!("cannot read `{property}` of null: the value was not there"),
        )),
        _ => Err(script_error(
            span,
            format!(
                "cannot read `{property}` of {} — only an object has members",
                kind_of(value)
            ),
        )),
    }
}

/// Index access on arrays (by position, out of range is `null`), objects (by key) and
/// strings (by character index).
pub fn index(object: &Value, index: &Value, span: Span) -> Result<Value, WorkflowError> {
    match (object, index) {
        (Value::Array(items), Value::Number(number)) => {
            let Some(position) = number.as_f64() else {
                return Err(script_error(span, "an array index must be a number"));
            };
            if position < 0.0 || position.fract() != 0.0 {
                return Err(script_error(
                    span,
                    format!("an array index must be a non-negative integer, got {position}"),
                ));
            }
            Ok(items.get(position as usize).cloned().unwrap_or(Value::Null))
        }
        (Value::Object(map), Value::String(key)) => {
            Ok(map.get(key).cloned().unwrap_or(Value::Null))
        }
        (Value::String(text), Value::Number(number)) => {
            let Some(position) = number.as_f64() else {
                return Err(script_error(span, "a string index must be a number"));
            };
            let characters: Vec<char> = text.chars().collect();
            if position < 0.0 || position.fract() != 0.0 {
                return Err(script_error(
                    span,
                    format!("a string index must be a non-negative integer, got {position}"),
                ));
            }
            Ok(characters
                .get(position as usize)
                .map(|c| json!(c.to_string()))
                .unwrap_or(Value::Null))
        }
        (Value::Array(_), other) => Err(script_error(
            span,
            format!("an array index must be a number, got {}", kind_of(other)),
        )),
        (other, _) => Err(script_error(
            span,
            format!("cannot index into {}", kind_of(other)),
        )),
    }
}

/// The engine's `create_actor` name argument: a string, or absent.
pub fn actor_name(value: &Value, span: Span) -> Result<Option<String>, WorkflowError> {
    match value {
        Value::Null => Ok(None),
        Value::String(text) => Ok(Some(text.clone())),
        other => Err(script_error(
            span,
            format!(
                "an actor name must be a string, got {} — `agent(\"planner\", \"persona\")`",
                kind_of(other)
            ),
        )),
    }
}

/// The instruction an ask carries: everything is rendered to text, so a template or a
/// concatenation of parts is legal and a number becomes its digits.
pub fn instructions(value: &Value) -> String {
    display(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn span() -> Span {
        Span::new(1, 1)
    }

    #[test]
    fn truthiness_matches_javascript_for_json_values() {
        assert!(!truthy(&Value::Null));
        assert!(!truthy(&json!(0)));
        assert!(!truthy(&json!("")));
        assert!(!truthy(&json!(false)));
        assert!(truthy(&json!([])));
        assert!(truthy(&json!({})));
        assert!(truthy(&json!("0")));
        assert!(truthy(&json!(1)));
    }

    #[test]
    fn plus_concatenates_when_either_side_is_text() {
        assert_eq!(
            apply_binary(BinaryOp::Add, json!("a"), json!("b"), span()).unwrap(),
            json!("ab")
        );
        assert_eq!(
            apply_binary(BinaryOp::Add, json!(1), json!(2), span()).unwrap(),
            json!(3)
        );
        assert_eq!(
            apply_binary(BinaryOp::Multiply, json!(1.5), json!(2), span()).unwrap(),
            json!(3),
            "an integral product renders as an integer, the way JSON.stringify(1.5 * 2) does"
        );
        assert_eq!(
            apply_binary(BinaryOp::Divide, json!(1), json!(2), span()).unwrap(),
            json!(0.5),
            "a fractional quotient stays a float"
        );
        assert_eq!(
            apply_binary(BinaryOp::Add, json!("steps: "), json!(["a", "b"]), span()).unwrap(),
            json!(r#"steps: ["a","b"]"#)
        );
    }

    #[test]
    fn other_arithmetic_refuses_text() {
        let error = apply_binary(BinaryOp::Multiply, json!("3"), json!(4), span())
            .expect_err("no coercion outside `+`");
        assert_eq!(error.code, WorkflowErrorCode::DriverError);
        assert!(error.message.contains("must be a number"), "{error}");
    }

    #[test]
    fn logical_operators_return_an_operand() {
        assert_eq!(
            apply_binary(BinaryOp::Or, Value::Null, json!("fallback"), span()).unwrap(),
            json!("fallback")
        );
        assert_eq!(
            apply_binary(BinaryOp::And, json!("kept"), json!(true), span()).unwrap(),
            json!(true)
        );
        assert_eq!(
            apply_binary(BinaryOp::Equal, json!(1), json!(1.0), span()).unwrap(),
            json!(true)
        );
    }

    #[test]
    fn comparison_needs_two_values_of_one_kind() {
        assert_eq!(
            apply_binary(BinaryOp::Less, json!("a"), json!("b"), span()).unwrap(),
            json!(true)
        );
        let error = apply_binary(BinaryOp::Less, json!("2"), json!(3), span())
            .expect_err("mixed comparison is refused");
        assert!(error.message.contains("mixes"), "{error}");
    }

    #[test]
    fn member_access_covers_the_shapes_a_script_needs() {
        assert_eq!(
            member(&json!(["a", "b"]), "length", span()).unwrap(),
            json!(2)
        );
        assert_eq!(member(&json!("héllo"), "length", span()).unwrap(), json!(5));
        assert_eq!(
            member(&json!({ "steps": ["a"] }), "steps", span()).unwrap(),
            json!(["a"])
        );
        // A key that is not there is null, not an error: the author's `|| fallback` works.
        assert_eq!(
            member(&json!({ "a": 1 }), "b", span()).unwrap(),
            Value::Null
        );
        let error = member(&json!("text"), "trim", span()).expect_err("no string methods");
        assert!(error.message.contains("no methods"), "{error}");
    }

    #[test]
    fn index_access_reads_arrays_objects_and_strings() {
        assert_eq!(
            index(&json!(["a", "b"]), &json!(1), span()).unwrap(),
            json!("b")
        );
        assert_eq!(
            index(&json!(["a"]), &json!(5), span()).unwrap(),
            Value::Null
        );
        assert_eq!(
            index(&json!({ "k": 3 }), &json!("k"), span()).unwrap(),
            json!(3)
        );
        assert_eq!(
            index(&json!("héllo"), &json!(1), span()).unwrap(),
            json!("é")
        );
        let error = index(&json!(3), &json!(0), span()).expect_err("numbers do not index");
        assert!(error.message.contains("cannot index"), "{error}");
    }

    #[test]
    fn display_is_the_json_rendering_except_for_plain_text() {
        assert_eq!(display(&json!("text")), "text");
        assert_eq!(display(&Value::Null), "");
        assert_eq!(display(&json!(12)), "12");
        assert_eq!(display(&json!(["a", "b"])), r#"["a","b"]"#);
    }

    #[test]
    fn an_actor_name_is_text_or_absent() {
        assert_eq!(
            actor_name(&json!("planner"), span()).unwrap().as_deref(),
            Some("planner")
        );
        assert_eq!(actor_name(&Value::Null, span()).unwrap(), None);
        assert!(actor_name(&json!(3), span()).is_err());
    }
}
