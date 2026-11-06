//! `WebSearch`: uses **nc-tools' `net.search`** as the search backend (TS uses provider-native search).
//!
//! This is where "reuse nc-tools" lands: nc's `net.search` already runs multi-engine retrieval + RRF fusion
//! (keyless: hn/wikipedia/ddg/mojeek/bing-rss/gnews-rss/stackexchange/openalex/arxiv/
//! npm/crates; with keys brave/tavily/serper are merged in), with SSRF policy, caching and optional local
//! reranking - capability-wise wider than TS's single-provider native search (the "combined capability").
//!
//! Contract aligned with `contracts/src/tools/websearch.ts`: input `{query, allowed_domains,
//! blocked_domains}` (the last two are mutually exclusive); output `{query, results[], sources[], durationMs}`
//! (strict: no `summary`/`modelUsage` fields, since this host has no such facts to report).
//!
//! Differences from TS (recorded in `specs/rust-native-crates.md` S3):
//! - TS uses the provider-native tool behind `model.properties.supportsNativeWebSearch`; this host's
//!   provider only offers chat completion, so nc's multi-engine backend is used instead. **What comes back are real retrieval results**,
//!   not a model-generated paraphrase - hence no `summary`, only `results` and `sources`.
//! - Domain filtering is applied locally (nc does no allowed/blocked filtering).
use super::{require_string, Tool, ToolContext, ToolContract, ToolError, ToolErrorCode};
use nct_core::kernel::Kernel;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Instant;
const DEFAULT_MAX_RESULTS: u64 = 8;
const MAX_MAX_RESULTS: u64 = 25;
const SEARCH_TIMEOUT_MS: u64 = 20_000;
const TOOL_TIMEOUT_MS: u64 = 60_000;
// TS `metadata.maxOutputBytes` (handlers/websearch.ts) is 20 000: the row-side/display budget.
// The body (context) budget is computed separately; see TS_MODEL_BUDGET_BYTES below.
const MAX_OUTPUT_BYTES: usize = 20_000;
/// TS WebSearch's body budget: `resultBudget.inlineBytes` 10_000 < `maxModelBytes` 20_000.
const TS_MODEL_BUDGET_BYTES: usize = 10_000;
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
/// The current month (**UTC**, at most one day off from TS's local month; this crate pulls in no timezone library,
/// and the "current month" in the description only guides searches for recent information, which needs no timezone precision).
fn current_month_label() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0) as i64;
    let days = now.div_euclid(86_400);
    let (year, month, _day) = civil_from_days(days);
    format!("{} {}", MONTHS[(month as usize).clamp(1, 12) - 1], year)
}
/// Howard Hinnant's civil_from_days (proleptic Gregorian).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
/// Only http/https are accepted: `file:`/`data:` and friends must not appear in search results, nor be projected to the model.
fn host_of(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    parsed
        .host_str()
        .map(|host| host.to_ascii_lowercase())
        .filter(|host| !host.is_empty())
}
fn domain_matches(host: &str, domain: &str) -> bool {
    let domain = domain.trim().trim_start_matches('.').to_ascii_lowercase();
    !domain.is_empty() && (host == domain || host.ends_with(&format!(".{domain}")))
}
fn read_string_list(input: &Value, key: &str) -> Result<Option<Vec<String>>, ToolError> {
    let Some(value) = input.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let Some(items) = value.as_array() else {
        return Err(ToolError::new(
            ToolErrorCode::InvalidInput,
            format!("{key}: expected an array of domain strings"),
        ));
    };
    let mut domains = Vec::with_capacity(items.len());
    for item in items {
        let Some(domain) = item.as_str() else {
            return Err(ToolError::new(
                ToolErrorCode::InvalidInput,
                format!("{key}: expected an array of domain strings"),
            ));
        };
        domains.push(domain.to_string());
    }
    Ok(Some(domains))
}
/// Maps nc's `net.search` `SearchResult` to TS's `WebSearchOutput` (the strict field set).
///
/// A pure function: filtering (domains), deduplication (url), projection (title/url). The unit tests assert against it directly.
pub fn map_search_output(
    query: &str,
    search: &Value,
    allowed: &[String],
    blocked: &[String],
    fallback_duration_ms: u64,
) -> Value {
    let mut items: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(results) = search.get("results").and_then(Value::as_array) {
        for hit in results {
            let Some(url) = hit.get("url").and_then(Value::as_str) else {
                continue;
            };
            if url.trim().is_empty() {
                continue;
            }
            let Some(host) = host_of(url) else {
                continue;
            };
            if !allowed.is_empty() && !allowed.iter().any(|domain| domain_matches(&host, domain)) {
                continue;
            }
            if blocked.iter().any(|domain| domain_matches(&host, domain)) {
                continue;
            }
            if !seen.insert(url.to_string()) {
                continue;
            }
            let mut item = json!({ "url": url });
            if let Some(title) = hit.get("title").and_then(Value::as_str) {
                if !title.trim().is_empty() {
                    item["title"] = json!(title);
                }
            }
            items.push(item);
        }
    }
    let sources = items.clone();
    let duration_ms = search
        .get("durationMs")
        .and_then(Value::as_u64)
        .unwrap_or(fallback_duration_ms);
    json!({
        "query": query,
        "results": items,
        "sources": sources,
        "durationMs": duration_ms,
    })
}
pub struct WebSearchTool {
    kernel: Option<Arc<Kernel>>,
}
impl WebSearchTool {
    pub fn new(kernel: Option<Arc<Kernel>>) -> Self {
        Self { kernel }
    }
}
impl Tool for WebSearchTool {
    fn contract(&self) -> ToolContract {
        let month = current_month_label();
        ToolContract {
            name: "WebSearch".to_string(),
            description: format!(
                "Search the web. Returns result blocks with titles and URLs.\n\n\
                 - The current month is {month} — use this when searching for recent information.\n\
                 - `allowed_domains` / `blocked_domains` filter results.\n\
                 - After answering from results, end with a \"Sources:\" list of the URLs you used as markdown links."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "The search query to use"},
                    "allowed_domains": {"type": "array", "items": {"type": "string"}, "description": "Only include search results from these domains"},
                    "blocked_domains": {"type": "array", "items": {"type": "string"}, "description": "Never include search results from these domains"},
                    "maxUses": {"type": "integer", "description": "Maximum results to return (1-8, default 8)"},
                },
                "required": ["query"],
            }),
            read_only: true,
            destructive: false,
            concurrent_safe: true,
            // An outbound call; TS declares "network" (the same closed set as WebFetch).
            side_effect_scope: super::SCOPE_NETWORK,
            timeout_ms: TOOL_TIMEOUT_MS,
            max_output_bytes: MAX_OUTPUT_BYTES,
            // TS's WebSearch: inline 10_000 < model 20_000 -> the body budget is 10_000.
            max_model_bytes: TS_MODEL_BUDGET_BYTES,
            preview_direction: "head",
        }
    }
    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let query = require_string(input, "query")?.trim().to_string();
        if query.chars().count() < 2 {
            return Err(ToolError::new(
                ToolErrorCode::InvalidInput,
                "query: must be at least 2 characters",
            ));
        }
        let allowed = read_string_list(input, "allowed_domains")?.unwrap_or_default();
        let blocked = read_string_list(input, "blocked_domains")?.unwrap_or_default();
        if !allowed.is_empty() && !blocked.is_empty() {
            return Err(ToolError::new(
                ToolErrorCode::InvalidInput,
                "allowed_domains and blocked_domains cannot both be specified",
            ));
        }
        let max_results = input
            .get("maxUses")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_RESULTS)
            .clamp(1, MAX_MAX_RESULTS);
        // The cancel bit must be checked **before the network retrieval is issued**: otherwise an already cancelled call would really go online.
        if ctx.is_cancelled() {
            return Err(ToolError::new(ToolErrorCode::Cancelled, "turn cancelled"));
        }
        let Some(kernel) = &self.kernel else {
            return Err(ToolError::new(
                ToolErrorCode::ExecutionFailed,
                "WebSearch: no nc-tools net.search backend is configured for this host; composition must inject a kernel (no fake results)",
            ));
        };
        let started = Instant::now();
        let outcome = kernel.call(
            "net.search",
            &json!({
                "query": query,
                "maxResults": max_results,
                "timeoutMs": SEARCH_TIMEOUT_MS,
            }),
        );
        if ctx.is_cancelled() {
            return Err(ToolError::new(ToolErrorCode::Cancelled, "turn cancelled"));
        }
        if !outcome.ok {
            let error = outcome.error.unwrap_or_else(|| {
                nct_core::ToolError::new(nct_core::errors::codes::INTERNAL, "net.search failed")
            });
            return Err(ToolError::new(
                ToolErrorCode::ExecutionFailed,
                format!("WebSearch: [{}] {}", error.code, error.message),
            ));
        }
        let search = outcome.result.unwrap_or(Value::Null);
        let output = map_search_output(
            &query,
            &search,
            &allowed,
            &blocked,
            started.elapsed().as_millis() as u64,
        );
        Ok(output.to_string())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn search_fixture() -> Value {
        json!({
            "query": "rust async",
            "durationMs": 42,
            "results": [
                {"title": "Async Rust", "url": "https://rust-lang.org/async"},
                {"title": "Docs", "url": "https://docs.rs/tokio"},
                {"title": "Blog", "url": "https://blog.example.com/async-rust"},
                {"url": "https://docs.rs/tokio"},
                {"title": "No url"}
            ]
        })
    }
    #[test]
    fn projects_results_and_dedupes_by_url() {
        let output = map_search_output("rust async", &search_fixture(), &[], &[], 1);
        assert_eq!(output["query"], json!("rust async"));
        assert_eq!(output["durationMs"], json!(42));
        let results = output["results"].as_array().unwrap();
        assert_eq!(
            results.len(),
            3,
            "duplicate URL must be dropped: {results:?}"
        );
        assert_eq!(results[0]["title"], json!("Async Rust"));
        assert_eq!(results[0]["url"], json!("https://rust-lang.org/async"));
        assert_eq!(output["sources"], output["results"]);
    }
    #[test]
    fn allowed_domains_restrict_and_blocked_domains_exclude() {
        let allowed = vec!["docs.rs".to_string()];
        let filtered = map_search_output("q", &search_fixture(), &allowed, &[], 1);
        let urls: Vec<&str> = filtered["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["url"].as_str())
            .collect();
        assert_eq!(urls, vec!["https://docs.rs/tokio"]);
        let blocked = vec!["example.com".to_string()];
        let filtered = map_search_output("q", &search_fixture(), &[], &blocked, 1);
        let urls: Vec<&str> = filtered["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["url"].as_str())
            .collect();
        assert_eq!(
            urls,
            vec!["https://rust-lang.org/async", "https://docs.rs/tokio"]
        );
    }
    #[test]
    fn subdomain_matching_is_suffix_safe() {
        assert!(domain_matches("docs.rs", "docs.rs"));
        assert!(domain_matches("api.docs.rs", "docs.rs"));
        assert!(domain_matches("api.docs.rs", ".docs.rs"));
        assert!(!domain_matches("notdocs.rs", "docs.rs"));
        assert!(!domain_matches("example.com", "ample.com"));
    }
    #[test]
    fn non_http_urls_are_dropped_rather_than_projected() {
        let search = json!({"results": [
            {"title": "local", "url": "file:
        ///etc/passwd"},
            {"title": "inline", "url": "data:text/html,hi"},
            {"title": "ok", "url": "https://example.com/page"}
        ]});
        let output = map_search_output("q", &search, &[], &[], 0);
        let urls: Vec<&str> = output["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["url"].as_str())
            .collect();
        assert_eq!(urls, vec!["https://example.com/page"]);
    }
    #[test]
    fn month_label_is_regenerated_and_well_formed() {
        let label = current_month_label();
        let (month, year) = label.split_once(' ').expect("month year");
        assert!(MONTHS.contains(&month), "{label}");
        assert_eq!(year.len(), 4, "{label}");
    }

    /// End-to-end over the **real network**: run nc `net.search` (multi-engine) and assert real URLs come back.

    /// Ignored by default: search failures depend on the machine/network environment and cannot gate every commit;

    /// when needed, run it explicitly:

    /// `cargo test -p nc-code-host --lib -- web_search::tests::live_search --ignored --nocapture`
    #[test]
    #[ignore = "needs internet access (real multi-engine search, no fixture)"]
    fn live_search_returns_real_http_results() {
        let dir = std::env::temp_dir().join(format!("zcode-websearch-live-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let kernel = Arc::new(nct_mcp::build_kernel(dir.clone()).expect("kernel"));
        let tool = WebSearchTool::new(Some(kernel));
        let ctx = ToolContext {
            workspace_root: dir.clone(),
            full_access: true,
            interactive: false,
            trace_id: "t".to_string(),
            turn_id: "turn".to_string(),
            session_id: "session-1".to_string(),
            model_ref: None,
            read_state: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            todos: crate::tools::todo::new_todo_store(),
            provider_config: None,
            tool_call_id: None,
            subagent: None,
            ask_user: None,
            plan_mode: false,
            plan: None,
            background: None,
            automation: None,
            browser: None,
            computer: None,
            offpeak: None,
            sessions: None,
            workflow: None,
        };
        let output = tool
            .execute(&json!({"query": "rust programming language"}), &ctx)
            .expect("live search");
        let parsed: Value = serde_json::from_str(&output).expect("json");
        let results = parsed["results"].as_array().expect("results");
        assert!(
            !results.is_empty(),
            "a real search must return hits: {parsed}"
        );
        for item in results {
            let url = item["url"].as_str().expect("url");
            assert!(
                url.starts_with("http://") || url.starts_with("https://"),
                "{url}"
            );
        }
        eprintln!("live search returned {} results", results.len());
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn invalid_inputs_are_rejected_before_the_kernel_is_called() {
        let kernel = Arc::new(nct_mcp::build_kernel(std::env::temp_dir()).expect("kernel"));
        let tool = WebSearchTool::new(Some(kernel));
        let ctx = ToolContext {
            workspace_root: std::env::temp_dir(),
            full_access: true,
            interactive: false,
            trace_id: "t".to_string(),
            turn_id: "turn".to_string(),
            session_id: "session-1".to_string(),
            model_ref: None,
            read_state: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            todos: crate::tools::todo::new_todo_store(),
            provider_config: None,
            tool_call_id: None,
            subagent: None,
            ask_user: None,
            plan_mode: false,
            plan: None,
            background: None,
            automation: None,
            browser: None,
            computer: None,
            offpeak: None,
            sessions: None,
            workflow: None,
        };
        let error = tool
            .execute(&json!({"query": "x"}), &ctx)
            .expect_err("short query");
        assert_eq!(error.code, ToolErrorCode::InvalidInput);
        let error = tool
            .execute(
                &json!({"query": "rust", "allowed_domains": ["a.com"], "blocked_domains": ["b.com"]}),
                &ctx,
            )
            .expect_err("both filters");
        assert!(error.message.contains("cannot both be specified"));
        // The cancel bit takes precedence over the network call.
        ctx.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        let error = tool
            .execute(&json!({"query": "rust"}), &ctx)
            .expect_err("cancelled");
        assert_eq!(error.code, ToolErrorCode::Cancelled);
    }
}
