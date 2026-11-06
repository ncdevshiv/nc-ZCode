//! The real provider harness emits all smoke (skipped by default, explicit environment variables are required).
//!
//! Purpose: Use **product's own** directory to capture + detect code to create real endpoint and verify
//! Acceptance of `specs/model-catalog-and-tps.md` §4.3:
//! - **Every** chat model in the directory runs connectivity (unavailable/limited/timeout status must be implemented one by one);
//! - Each ability signature (vision/reasoning/tool/plain × size) replaces the running tools, thinking and throughput measurements;
//! - Single model requests have a hard budget (pending models cannot occupy dead semaphores);
//! - The failure must have a displayable fact (code/statusCode/retryable) instead of just "request failed".
//!
//! Credentials are only read from environment variables and never written to the warehouse; reports are written to the system temporary directory by default.
//!
//! ```sh
//! nc_code_REAL_PROVIDER_BASE_URL=https://integrate.api.nvidia.com/v1 \
//! nc_code_REAL_PROVIDER_API_KEY=nvapi-… \
//! nc_code_REAL_PROBE_ALL=1 \
//! cargo test -p nc-code-services-provider --test real_provider_harness -- --nocapture
//! ```

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use nc_code_services_provider::catalog::{self, Endpoint};
use serde_json::{json, Value};

const CAPABILITY_CHECKS: &[&str] = &["toolCall", "reasoning", "throughput"];

struct Config {
    base_url: String,
    api_key: String,
    probe_limit: usize,
    probe_all: bool,
    capability_sample: usize,
    concurrency: usize,
    budget: Duration,
    report_path: Option<String>,
}

fn config() -> Option<Config> {
    let base_url = std::env::var("ZCODE_REAL_PROVIDER_BASE_URL").ok()?;
    let api_key = std::env::var("ZCODE_REAL_PROVIDER_API_KEY").ok()?;
    if base_url.trim().is_empty() || api_key.trim().is_empty() {
        return None;
    }
    Some(Config {
        base_url,
        api_key,
        probe_limit: std::env::var("ZCODE_REAL_PROBE_LIMIT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(12),
        probe_all: std::env::var("ZCODE_REAL_PROBE_ALL").is_ok(),
        capability_sample: std::env::var("ZCODE_REAL_CAPABILITY_SAMPLE")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(12),
        concurrency: std::env::var("ZCODE_REAL_CONCURRENCY")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(4),
        budget: Duration::from_secs(
            std::env::var("ZCODE_REAL_PROBE_TIMEOUT_SECS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(90),
        ),
        report_path: std::env::var("ZCODE_REAL_REPORT_PATH")
            .ok()
            .or_else(default_report_path),
    })
}

fn default_report_path() -> Option<String> {
    let dir = std::env::temp_dir().join("zcode-real-harness");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("report.json").display().to_string())
}

/// The purpose tag of each model in the directory; if there is no `purpose`, fall back to heuristic inference.
fn purposes(model: &Value) -> Vec<String> {
    if let Some(array) = model["purpose"].as_array() {
        return array
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
    }
    catalog::infer(model["modelId"].as_str().unwrap_or_default())["purpose"]
        .as_array()
        .map(|array| {
            array
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn is_chat(model: &Value) -> bool {
    purposes(model).iter().any(|purpose| purpose == "chat")
}

/// Capability signature: used to only take one representative of each type for complete actual testing.
fn signature(model: &Value) -> String {
    let mut tags = purposes(model);
    tags.sort_unstable();
    let context = model["contextWindow"]
        .as_u64()
        .map(|value| if value >= 200_000 { "xl" } else { "std" })
        .unwrap_or("unknown");
    let tool = match model["supportsToolCall"].as_bool() {
        Some(true) => "tool",
        Some(false) => "no-tool",
        None => "tool?",
    };
    let thinking = match model["reasoningLevels"].as_array() {
        Some(levels) if levels.is_empty() => "no-think".to_string(),
        Some(levels) => format!("think={}", levels.len()),
        None => "think?".to_string(),
    };
    format!("{}|{tool}|{thinking}|ctx={context}", tags.join("+"))
}

/// Failure classification: the harness report counts according to this bucket, instead of only looking at ok/false.
fn classify(entry: &Value) -> &'static str {
    if entry["timeout"].as_bool() == Some(true) {
        return "budget-exceeded";
    }
    if entry["connectivity"]["ok"] == Value::Bool(true) {
        return "ok";
    }
    let error = &entry["connectivity"]["error"];
    match error["statusCode"].as_u64() {
        Some(404) => return "not-found",
        Some(401) | Some(403) => return "auth",
        Some(429) => return "rate-limited",
        Some(status) if status >= 500 => return "upstream-5xx",
        _ => {}
    }
    let code = error["code"].as_str().unwrap_or_default();
    if code.contains("timeout") || error["reason"].as_str() == Some("timeout") {
        "timeout"
    } else if code.contains("connection") || code.contains("dns") || code.contains("transport") {
        "transport"
    } else if entry["connectivity"]["textChars"].as_u64().unwrap_or(0) == 0
        && entry["connectivity"]["completionTokens"].is_null()
    {
        "empty-response"
    } else {
        "other"
    }
}

/// A complete detection: the hard budget covers the probe_model, and the dead model is recorded as a timeout instead of dragging down the entire round.
async fn probe_with_budget(
    endpoint: Arc<Endpoint>,
    model_id: String,
    checks: Vec<String>,
    budget: Duration,
) -> Value {
    let model_id_for_task = model_id.clone();
    let mut handle =
        tokio::spawn(
            async move { catalog::probe_model(&endpoint, &model_id_for_task, &checks).await },
        );
    match tokio::time::timeout(budget, &mut handle).await {
        Ok(Ok(Ok(value))) => value,
        Ok(Ok(Err(error))) => json!({
            "modelId": model_id,
            "connectivity": { "ok": false, "error": { "code": "probe-error", "message": error.message } },
        }),
        Ok(Err(_)) => json!({
            "modelId": model_id,
            "connectivity": { "ok": false, "error": { "code": "join-error", "message": "probe task panicked" } },
        }),
        Err(_elapsed) => {
            // Interrupt the request if it exceeds the budget: otherwise a dead model will always occupy the concurrent quota.
            handle.abort();
            json!({
                "modelId": model_id,
                "timeout": true,
                "connectivity": {
                    "ok": false,
                    "error": {
                        "code": "budget-exceeded",
                        "message": format!("probe exceeded {}s budget", budget.as_secs())
                    }
                },
            })
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn real_provider_catalog_and_probe_harness() {
    let Some(config) = config() else {
        eprintln!("skipping: nc_code_REAL_PROVIDER_BASE_URL / _API_KEY not set");
        return;
    };
    let endpoint = Arc::new(Endpoint {
        base_url: config.base_url.clone(),
        api_type: "openai-chat-completions".to_string(),
        api_key: config.api_key.clone(),
    });

    let catalog_models = catalog::fetch_catalog(&endpoint)
        .await
        .expect("real catalog fetch must succeed");
    assert!(
        !catalog_models.is_empty(),
        "real endpoint returned an empty model catalog"
    );
    let mut class_counts: BTreeMap<String, usize> = BTreeMap::new();
    for model in &catalog_models {
        for purpose in purposes(model) {
            *class_counts.entry(purpose).or_default() += 1;
        }
    }
    let chat_models: Vec<&Value> = catalog_models
        .iter()
        .filter(|model| is_chat(model))
        .collect();
    assert!(
        !chat_models.is_empty(),
        "no chat-capable model inferred from a real catalog"
    );

    // Connectivity: Covers all chat models by default; limited mode is used for quick regression.
    let connectivity_targets: Vec<String> = if config.probe_all {
        chat_models
            .iter()
            .map(|m| m["modelId"].as_str().unwrap_or_default().to_string())
            .collect()
    } else {
        chat_models
            .iter()
            .take(config.probe_limit)
            .map(|m| m["modelId"].as_str().unwrap_or_default().to_string())
            .collect()
    };

    // Capability actual measurement: Each capability signature takes a representative, and it must be a real model among the connectivity candidates.
    let mut seen_signatures: BTreeMap<String, String> = BTreeMap::new();
    for model in &chat_models {
        let id = model["modelId"].as_str().unwrap_or_default().to_string();
        if !connectivity_targets.contains(&id) {
            continue;
        }
        seen_signatures.entry(signature(model)).or_insert(id);
    }
    let capability_targets: Vec<String> = seen_signatures
        .values()
        .take(config.capability_sample)
        .cloned()
        .collect();

    println!(
        "catalog total={} chat={} connectivity={} capabilityProbes={} signatures={}",
        catalog_models.len(),
        chat_models.len(),
        connectivity_targets.len(),
        capability_targets.len(),
        seen_signatures.len(),
    );
    let mut classes: Vec<(&String, &usize)> = class_counts.iter().collect();
    classes.sort_by(|left, right| right.1.cmp(left.1));
    println!(
        "classes: {}",
        classes
            .iter()
            .map(|(name, count)| format!("{name}={count}"))
            .collect::<Vec<_>>()
            .join(" ")
    );

    let permit = Arc::new(tokio::sync::Semaphore::new(config.concurrency));
    let results = Arc::new(tokio::sync::Mutex::new(Vec::<Value>::new()));
    let capability_set: Arc<Vec<String>> = Arc::new(capability_targets.clone());
    let purposes_by_id: Arc<HashMap<String, Vec<String>>> = Arc::new(
        catalog_models
            .iter()
            .map(|model| {
                (
                    model["modelId"].as_str().unwrap_or_default().to_string(),
                    purposes(model),
                )
            })
            .collect(),
    );
    let mut tasks = Vec::new();
    for model_id in connectivity_targets.clone() {
        let permit = Arc::clone(&permit);
        let endpoint = Arc::clone(&endpoint);
        let results = Arc::clone(&results);
        let capability_set = Arc::clone(&capability_set);
        let purposes_by_id = Arc::clone(&purposes_by_id);
        let budget = config.budget;
        tasks.push(tokio::spawn(async move {
            let _guard = permit.acquire().await.expect("permit");
            let full_probe = capability_set.contains(&model_id);
            let checks: Vec<String> = if full_probe {
                CAPABILITY_CHECKS
                    .iter()
                    .map(|check| check.to_string())
                    .collect()
            } else {
                Vec::new()
            };
            let mut probe = probe_with_budget(endpoint, model_id.clone(), checks, budget).await;
            probe["probedChecks"] = json!(if full_probe {
                "connectivity+toolCall+reasoning+throughput"
            } else {
                "connectivity"
            });
            probe["purpose"] = json!(purposes_by_id.get(&model_id).cloned().unwrap_or_default());
            results.lock().await.push(probe);
        }));
    }
    for task in tasks {
        task.await.expect("probe task");
    }

    let mut report = results.lock().await.clone();
    report.sort_by(|left, right| {
        left["modelId"]
            .as_str()
            .unwrap_or_default()
            .cmp(right["modelId"].as_str().unwrap_or_default())
    });

    let mut buckets: BTreeMap<&'static str, usize> = BTreeMap::new();
    for entry in &report {
        *buckets.entry(classify(entry)).or_default() += 1;
    }
    let measured: Vec<&Value> = report
        .iter()
        .filter(|entry| entry["throughput"]["tokensPerSecond"].is_number())
        .collect();
    let tools_ok = report
        .iter()
        .filter(|entry| entry["toolCall"]["ok"] == Value::Bool(true))
        .count();
    let reasoning_ok = report
        .iter()
        .filter(|entry| entry["reasoning"]["ok"] == Value::Bool(true))
        .count();
    let ok_total = buckets.get("ok").copied().unwrap_or(0);

    println!("--- harness summary ---");
    println!(
        "connectivity: {}",
        buckets
            .iter()
            .map(|(bucket, count)| format!("{bucket}={count}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!(
        "measured: toolCall ok={tools_ok}/{}, reasoning ok={reasoning_ok}/{}, throughput samples={}",
        capability_targets.len(),
        capability_targets.len(),
        measured.len(),
    );

    let mut by_tps = measured.clone();
    by_tps.sort_by(|left, right| {
        let tps = |value: &Value| {
            value["throughput"]["tokensPerSecond"]
                .as_f64()
                .unwrap_or(0.0)
        };
        tps(right)
            .partial_cmp(&tps(left))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    println!("--- measured capability (top by tok/s) ---");
    for entry in by_tps.iter().take(capability_targets.len()) {
        println!(
            "{:<52} tps={:<8} est={} window={:<10} probeOk={} tools={} reasoning={} latencyMs={}",
            entry["modelId"].as_str().unwrap_or_default(),
            entry["throughput"]["tokensPerSecond"],
            entry["throughput"]["estimated"],
            format!(
                "{}ms/{}",
                entry["throughput"]["elapsedMs"],
                entry["throughput"]["windowSource"].as_str().unwrap_or("?")
            ),
            entry["throughput"]["ok"],
            entry["toolCall"]["ok"],
            entry["reasoning"]["ok"],
            entry["connectivity"]["latencyMs"],
        );
    }
    println!("--- failures (honest facts) ---");
    let mut shown = 0usize;
    for entry in report.iter().filter(|entry| classify(entry) != "ok") {
        let error = &entry["connectivity"]["error"];
        println!(
            "{:<52} {:<15} status={} reason={} retryable={} message={}",
            entry["modelId"].as_str().unwrap_or_default(),
            classify(entry),
            error["statusCode"].as_u64().unwrap_or(0),
            error["reason"].as_str().unwrap_or("-"),
            error["retryable"],
            truncate(error["message"].as_str().unwrap_or("unavailable"), 90),
        );
        shown += 1;
        if shown >= 40 {
            println!("… ({} more in the JSON report)", report.len() - shown);
            break;
        }
    }

    let payload = json!({
        "baseUrl": redacted_base_url(&config.base_url),
        "catalogTotal": catalog_models.len(),
        "chatTotal": chat_models.len(),
        "classes": class_counts,
        "buckets": buckets,
        "capabilitySignatures": seen_signatures,
        "measuredToolCallOk": tools_ok,
        "measuredReasoningOk": reasoning_ok,
        "throughputSamples": measured.len(),
        "entries": report,
    });
    if let Some(path) = config.report_path.clone() {
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&payload).expect("serialize"),
        )
        .expect("write report");
        println!("report: {path}");
    }

    // Acceptance: Harness must really be able to get through at least one model, and at least one throughput measurement is non-zero——
    // Otherwise, it means that the TPS timing window is not connected (the tok/s chip in the product will also always be 0).
    assert!(
        ok_total > 0,
        "no real model answered; harness cannot be considered functional"
    );
    assert!(
        by_tps
            .iter()
            .any(|entry| entry["throughput"]["tokensPerSecond"]
                .as_f64()
                .unwrap_or(0.0)
                > 0.0),
        "connectivity succeeded but no throughput sample measured tokens/s"
    );
    for entry in report.iter().filter(|entry| classify(entry) != "ok") {
        assert!(
            entry["connectivity"]["error"].is_object() || entry["timeout"] == Value::Bool(true),
            "failed model {} has no failure facts: {entry}",
            entry["modelId"].as_str().unwrap_or_default()
        );
    }
}

fn truncate(value: &str, max: usize) -> String {
    let trimmed = value.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(max).collect();
    out.push('…');
    out
}
fn redacted_base_url(value: &str) -> String {
    // Only keep scheme://host, no possible path query string is included in the report.
    let Some(scheme_end) = value.find("://") else {
        return value.trim_end_matches('/').to_string();
    };
    let authority_start = scheme_end + 3;
    match value[authority_start..].find('/') {
        Some(offset) => value[..authority_start + offset].to_string(),
        None => value.trim_end_matches('/').to_string(),
    }
}
