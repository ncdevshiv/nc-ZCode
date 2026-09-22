//! parity：用 `tools/gen-jsdiff-fixtures.mjs` 以真实 npm diff@9 生成的
//! fixtures 逐字节比对四个方法的输出（JSON 以字符串序序列化后比较，
//! 连键序一起校验）。

use serde_json::{json, Value};

const FIXTURES: &str = include_str!("fixtures/jsdiff-parity.json");

fn fixtures() -> Value {
    serde_json::from_str(FIXTURES).expect("fixtures 必须是合法 JSON")
}

fn cases(kind: &str) -> Vec<Value> {
    fixtures()[kind]
        .as_array()
        .unwrap_or_else(|| panic!("{kind} 数组缺失"))
        .clone()
}

/// case.params 与 sidecar 参数面同形，直接送进同一个 handle 入口。
fn run(method: &str, case: &Value) -> Value {
    let name = case["name"].as_str().unwrap_or("<unnamed>");
    zcode_diff::handle(method, &case["params"])
        .unwrap_or_else(|err| panic!("{method}/{name} 意外失败: {}", err.body.message))
}

fn assert_value_json_eq(got: &Value, expected: &Value, label: &str) {
    let got_text = serde_json::to_string(got).expect("got json");
    let expected_text = serde_json::to_string(expected).expect("expected json");
    assert_eq!(got_text, expected_text, "{label}");
}

#[test]
fn baseline_is_real_jsdiff_9() {
    let all = fixtures();
    let version = all["jsdiff"].as_str().expect("jsdiff 版本");
    assert!(version.starts_with("9."), "基线必须来自 diff@9: {version}");
    assert!(cases("unified").len() >= 20, "unified 用例不足");
    assert!(cases("structured").len() >= 20, "structured 用例不足");
    assert!(cases("chunks").len() >= 20, "chunks 用例不足");
    assert!(cases("apply").len() >= 10, "apply 用例不足");
}

#[test]
fn unified_patches_match_jsdiff_byte_for_byte() {
    for case in cases("unified") {
        let name = case["name"].as_str().expect("name");
        let got = run("diff_unified", &case);
        match &case["expected"] {
            Value::Null => assert_eq!(got, json!({"patch": null}), "unified/{name}"),
            expected => {
                let patch = got["patch"].as_str().expect("patch 必须是字符串");
                assert_eq!(
                    patch,
                    expected.as_str().expect("expected 字符串"),
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
        let got = run("structured_patch", &case);
        assert_value_json_eq(&got, &case["expected"], &format!("structured/{name}"));
    }
}

#[test]
fn line_chunks_match_jsdiff_including_boolean_explicitness() {
    for case in cases("chunks") {
        let name = case["name"].as_str().expect("name");
        let got = run("diff_lines", &case);
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
    assert!(failures >= 1, "fixtures 必须覆盖失败路径");
    for case in &cases {
        let name = case["name"].as_str().expect("name");
        let got = run("apply_patch", case);
        assert_value_json_eq(&got, &case["expected"], &format!("apply/{name}"));
    }
}

#[test]
fn roundtrip_structured_then_apply_recovers_target() {
    // 真实组合路径：structured_patch 产出的 hunks 直接回灌 apply_patch。
    // 两处变更相距 18 行 > 2*context(4) → 必然拆成两个 hunk。
    let old: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    let mut rows: Vec<String> = (1..=20).map(|i| format!("line {i}")).collect();
    rows[0] = "line ONE".into();
    rows[19] = "line TWENTY".into();
    let new: String = rows.iter().map(|row| format!("{row}\n")).collect();

    // 直接送 params（run() 面向 fixture 的 {params, expected} 形状）。
    let patch = zcode_diff::handle(
        "structured_patch",
        &json!({"old": old, "new": new, "oldLabel": "f.txt", "newLabel": "f.txt"}),
    )
    .unwrap_or_else(|err| panic!("structured 意外失败: {}", err.body.message));
    let hunks = patch["hunks"].as_array().expect("hunks").clone();
    assert_eq!(hunks.len(), 2, "远距变更应拆两个 hunk: {patch}");

    let applied = zcode_diff::handle("apply_patch", &json!({"source": old, "hunks": hunks}))
        .unwrap_or_else(|err| panic!("apply 意外失败: {}", err.body.message));
    assert_eq!(
        applied,
        json!({"applied": true, "result": new}),
        "补丁必须可逆还原目标文本"
    );
}
