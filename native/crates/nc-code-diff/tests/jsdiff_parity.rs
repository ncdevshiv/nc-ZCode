//! parity: generated with real npm diff@9 using `tools/gen-jsdiff-fixtures.mjs`
//! Fixtures compares the output of the four methods byte by byte (JSON is serialized in string order and compared,
//! Check together with the key sequence).

use serde_json::{json, Value};

const FIXTURES: &str = include_str!("fixtures/jsdiff-parity.json");

fn fixtures() -> Value {
    serde_json::from_str(FIXTURES).expect("fixtures must be valid JSON")
}

fn cases(kind: &str) -> Vec<Value> {
    fixtures()[kind]
        .as_array()
        .unwrap_or_else(|| panic!("{kind} array is missing"))
        .clone()
}

/// case.params has the same shape as the sidecar parameter surface and is directly sent to the same handle entry.
fn run(method: &str, case: &Value) -> Value {
    let name = case["name"].as_str().unwrap_or("<unnamed>");
    nc_code_diff::handle(method, &case["params"])
        .unwrap_or_else(|err| panic!("{method}/{name} failed unexpectedly: {}", err.body.message))
}

fn assert_value_json_eq(got: &Value, expected: &Value, label: &str) {
    let got_text = serde_json::to_string(got).expect("got json");
    let expected_text = serde_json::to_string(expected).expect("expected json");
    assert_eq!(got_text, expected_text, "{label}");
}

#[test]
fn baseline_is_real_jsdiff_9() {
    let all = fixtures();
    let version = all["jsdiff"].as_str().expect("jsdiff version");
    assert!(
        version.starts_with("9."),
        "Baseline must be from diff@9: {version}"
    );
    assert!(cases("unified").len() >= 20, "Not enough unified use cases");
    assert!(
        cases("structured").len() >= 20,
        "Insufficient structured use cases"
    );
    assert!(
        cases("chunks").len() >= 20,
        "Insufficient use cases for chunks"
    );
    assert!(cases("apply").len() >= 10, "Insufficient apply use cases");
}

#[test]
fn unified_patches_match_jsdiff_byte_for_byte() {
    for case in cases("unified") {
        let name = case["name"].as_str().expect("name");
        let got = run("diff.unified", &case);
        match &case["expected"] {
            Value::Null => assert_eq!(got, json!({"patch": null}), "unified/{name}"),
            expected => {
                let patch = got["patch"].as_str().expect("patch must be a string");
                assert_eq!(
                    patch,
                    expected.as_str().expect("expected string"),
                    "unified/{name}"
                );
            }
        }
    }
}

#[test]
fn structured_patches_match_jsdiff_including_key_order() {
    for case in cases("structured") {
        let name = case["name"].as_str().expect("name");
        let got = run("structuredPatch", &case);
        assert_value_json_eq(&got, &case["expected"], &format!("structured/{name}"));
    }
}

#[test]
fn line_chunks_match_jsdiff_including_boolean_explicitness() {
    for case in cases("chunks") {
        let name = case["name"].as_str().expect("name");
        let got = run("diff.lines", &case);
        assert_value_json_eq(&got["chunks"], &case["expected"], &format!("chunks/{name}"));
    }
}

#[test]
fn apply_results_match_jsdiff_success_and_failure() {
    let cases = cases("apply");
    let failures = cases
        .iter()
        .filter(|case| case["expected"]["applied"] == json!(false))
        .count();
    assert!(failures >= 1, "fixtures must cover failure paths");
    for case in &cases {
        let name = case["name"].as_str().expect("name");
        let got = run("diff.applyPatch", case);
        assert_value_json_eq(&got, &case["expected"], &format!("apply/{name}"));
    }
}

#[test]
fn roundtrip_structured_then_apply_recovers_target() {
    //Real combination path: The hunks produced by structured_patch are directly fed back into apply_patch.
    // The two changes are 18 lines apart > 2*context(4) → must be split into two hunks.
    let old: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    let mut rows: Vec<String> = (1..=20).map(|i| format!("line {i}")).collect();
    rows[0] = "line ONE".into();
    rows[19] = "line TWENTY".into();
    let new: String = rows.iter().map(|row| format!("{row}\n")).collect();

    // Send params directly (run() faces the {params, expected} shape of the fixture).
    let patch = nc_code_diff::handle(
        "structuredPatch",
        &json!({"old": old, "new": new, "oldLabel": "f.txt", "newLabel": "f.txt"}),
    )
    .unwrap_or_else(|err| panic!("structured failed unexpectedly: {}", err.body.message));
    let hunks = patch["hunks"].as_array().expect("hunks").clone();
    assert_eq!(
        hunks.len(),
        2,
        "Remote changes should be split into two hunks: {patch}"
    );

    let applied = nc_code_diff::handle("diff.applyPatch", &json!({"source": old, "hunks": hunks}))
        .unwrap_or_else(|err| panic!("apply failed unexpectedly: {}", err.body.message));
    assert_eq!(
        applied,
        json!({"applied": true, "result": new}),
        "The patch must reversibly restore the target text"
    );
}
