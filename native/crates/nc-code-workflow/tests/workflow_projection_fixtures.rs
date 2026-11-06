//! Golden fixture verification for compile/analysis projections (Rust, TS-compiler-free).
//!
//! Uses real fixture scripts under `tests/fixtures/` and verifies evidence-based
//! projection sections produced by `analyze_ts_workflow` / compile. Assertions focus
//! on honest, derivable facts (site IDs vs golden JSON when present, typed/untyped
//! asks, actor/ask presence, taint explicitly not fabricated). Deep semantic parity
//! is incremental; this covers all available `workflow_*.ts` + `workflow_*.json` pairs.

use std::path::PathBuf;

use serde_json::Value;

use nc_code_workflow::compile::analysis::analyze_ts_workflow;
use nc_code_workflow::compile::ts_workflow::compile_ts_workflow;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn read_fixture_text(name: &str) -> String {
    std::fs::read_to_string(fixtures_dir().join(name)).expect("read fixture")
}

fn read_fixture_json(name: &str) -> Value {
    let text = read_fixture_text(name);
    serde_json::from_str(&text).expect("fixture json")
}

fn workflow_ts_json_pairs() -> Vec<(String, String, String)> {
    let mut pairs = Vec::new();
    let dir = fixtures_dir();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("ts") {
                continue;
            }
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if !stem.starts_with("workflow_") {
                continue;
            }
            let json_path = dir.join(format!("{stem}.json"));
            if json_path.is_file() {
                pairs.push((
                    stem.to_string(),
                    format!("{stem}.ts"),
                    format!("{stem}.json"),
                ));
            }
        }
    }
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    pairs
}

fn assert_common_projections_honest(p: &Value, site_count: usize, label: &str) {
    assert!(
        p["causality"]["steps"].as_array().is_some(),
        "{label}: causality.steps"
    );
    assert!(
        p["flow"]["nodes"].as_array().is_some(),
        "{label}: flow.nodes"
    );
    assert!(
        p["handoff"]["participants"].is_array(),
        "{label}: handoff.participants"
    );
    assert!(
        p["handoff"]["handoffs"].is_array(),
        "{label}: handoff.handoffs"
    );
    assert!(
        p["callGraph"]["nodes"].is_array(),
        "{label}: callGraph.nodes"
    );
    assert!(
        p["callGraph"]["edges"].is_array(),
        "{label}: callGraph.edges"
    );
    assert!(p["interpret"]["asks"].is_array(), "{label}: interpret.asks");
    assert_eq!(
        p["state"]["sites"].as_u64().unwrap() as usize,
        site_count,
        "{label}: state.sites"
    );
    assert!(
        p["state"]["hasActors"].is_boolean(),
        "{label}: state.hasActors"
    );
    assert!(p["state"]["hasAsks"].is_boolean(), "{label}: state.hasAsks");
    // Honesty: taint not fabricated.
    assert_eq!(
        p["taint"]["computed"], false,
        "{label}: taint must not be fabricated"
    );
    let reason = p["taint"]["reason"].as_str().unwrap_or("");
    assert!(
        reason.contains("no fabricated") || reason.contains("not fully implemented"),
        "{label}: taint reason must be honest: {reason}"
    );
}

#[test]
fn all_workflow_ts_json_fixtures_compile_and_project_evidence() {
    let pairs = workflow_ts_json_pairs();
    assert!(
        !pairs.is_empty(),
        "expected workflow_*.ts+json fixtures in tests/fixtures"
    );
    for (stem, ts_name, json_name) in pairs {
        let script = read_fixture_text(&ts_name);
        let golden = read_fixture_json(&json_name);
        let golden_site_ids: Vec<String> = golden["siteIds"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();

        let compiled = compile_ts_workflow(&script)
            .unwrap_or_else(|e| panic!("{stem}: compile failed: {e:?}"));
        assert_eq!(
            compiled.site_ids, golden_site_ids,
            "{stem}: Rust compile must produce the same site table as golden JSON"
        );

        // Ask specs typedness parity with golden when provided.
        if let Some(golden_asks) = golden.get("askSpecs").and_then(|v| v.as_object()) {
            for (ask_id, spec) in golden_asks {
                let typed = spec.get("typed").and_then(|v| v.as_bool());
                if let (Some(expected), Some(actual)) = (typed, compiled.ask_specs.get(ask_id)) {
                    assert_eq!(
                        actual.typed, expected,
                        "{stem}: ask {ask_id} typed mismatch"
                    );
                }
            }
        }

        let analysis = analyze_ts_workflow(&script);
        assert_eq!(analysis["ok"], true, "{stem}: analysis ok: {analysis}");
        assert_eq!(
            analysis["siteIds"], golden["siteIds"],
            "{stem}: analysis siteIds"
        );
        assert!(
            analysis["scriptHash"].is_string(),
            "{stem}: scriptHash present"
        );
        assert!(analysis["askSpecs"].is_object(), "{stem}: askSpecs present");
        assert!(
            analysis["projections"].is_object(),
            "{stem}: projections present"
        );

        let p = &analysis["projections"];
        assert_common_projections_honest(p, golden_site_ids.len(), &stem);

        // Evidence-based call-graph honesty: multi-actor cases must not fabricate ownership.
        let actor_count = golden_site_ids
            .iter()
            .filter(|id| id.starts_with("actor#"))
            .count();
        let completeness = p["callGraph"]["completeness"].as_str().unwrap_or("");
        if actor_count > 1 {
            assert!(
                completeness == "partial-no-fabricated-ownership"
                    || completeness == "evidence-derived",
                "{stem}: multi-actor callGraph completeness must be honest: {completeness}"
            );
        } else if actor_count == 1 {
            assert_eq!(
                completeness, "evidence-derived",
                "{stem}: single-actor completeness"
            );
        }
    }
}

#[test]
fn linear_fixture_has_expected_evidence_details() {
    let script = read_fixture_text("workflow_linear.ts");
    let analysis = analyze_ts_workflow(&script);
    let p = &analysis["projections"];
    assert_eq!(p["state"]["hasArtifacts"], true);
    assert_eq!(p["state"]["hasWorldReads"], true);
    assert_eq!(p["callGraph"]["completeness"], "evidence-derived");
    let asks = p["interpret"]["asks"].as_array().unwrap();
    let typed_ask = asks.iter().find(|a| a["id"] == "ask#1").expect("ask#1");
    assert_eq!(typed_ask["typed"], true);
    assert_eq!(typed_ask["mode"], "typed");
    assert!(!p["handoff"]["handoffs"].as_array().unwrap().is_empty());
}

#[test]
fn fanout_fixture_has_expected_evidence_details() {
    let script = read_fixture_text("workflow_fanout.ts");
    let analysis = analyze_ts_workflow(&script);
    let p = &analysis["projections"];
    assert_eq!(p["state"]["hasActors"], true);
    assert_eq!(p["state"]["hasAsks"], true);
    let completeness = p["callGraph"]["completeness"].as_str().unwrap_or("");
    assert!(
        completeness == "partial-no-fabricated-ownership" || completeness == "evidence-derived",
        "fanout completeness honest: {completeness}"
    );
}
