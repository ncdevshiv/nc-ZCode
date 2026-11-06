//! Renderer telemetry → OTLP/HTTP exporter for the Tauri shell.
//!
//! The Electron main process delivered renderer events to ARMS. The Tauri shell
//! replaces that vendor path with an OTLP/HTTP JSON exporter configured through
//! the standard OpenTelemetry environment variables:
//!
//! - `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT`, `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`,
//!   `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` (full signal URLs, used as-is), or
//! - `OTEL_EXPORTER_OTLP_ENDPOINT` + `/v1/logs`, `/v1/traces`, `/v1/metrics`.
//!
//! Without an endpoint telemetry is disabled and events are dropped; nothing is
//! written to disk and no network request is attempted.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{json, Map, Value};
use tokio::sync::mpsc;

const BATCH_SIZE: usize = 64;
const FLUSH_INTERVAL: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CHANNEL_CAPACITY: usize = 1024;
const DEFAULT_SERVICE_NAME: &str = "zcode-desktop";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryRendererContext {
    pub client_timezone: String,
    pub client_language: String,
    pub screen_resolution: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RendererTelemetryEvent {
    pub element_name: String,
    pub event_region: String,
    pub event_type: String,
    #[serde(default)]
    pub event_text: Option<String>,
    #[serde(default)]
    pub event_extra_detail: Map<String, Value>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub talk_id: Option<String>,
    #[serde(default)]
    pub message_id: Option<String>,
    pub context: TelemetryRendererContext,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArmsCustomEvent {
    pub name: String,
    pub group: String,
    #[serde(default)]
    pub value: Option<f64>,
    #[serde(default)]
    pub properties: Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RendererActionTraceResource {
    pub service_name: String,
    pub service_version: String,
    pub deployment_environment: String,
    pub renderer_instance_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RendererActionTraceSpan {
    pub trace_id: String,
    pub span_id: String,
    pub name: String,
    pub start_time_unix_ms: f64,
    pub end_time_unix_ms: f64,
    pub status: String,
    #[serde(default)]
    pub attributes: Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RendererActionTraceBatch {
    pub version: u8,
    pub renderer_instance_id: String,
    pub sequence: u64,
    pub dropped_since_last_flush: u64,
    pub resource: RendererActionTraceResource,
    pub spans: Vec<RendererActionTraceSpan>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RendererHeapSample {
    pub heap_used_kb: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalTtftRecord {
    pub observation_id: String,
    #[serde(default)]
    pub command_id: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub outcome: Option<String>,
    pub start: f64,
    pub end: f64,
    #[serde(default)]
    pub first_output_kind: Option<String>,
    #[serde(default)]
    pub quality: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub send_mode: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalTtftBatch {
    pub version: u8,
    pub renderer_instance_id: String,
    pub sequence: u64,
    pub records: Vec<LocalTtftRecord>,
    pub dropped: u64,
}

#[derive(Debug, Clone)]
pub enum TelemetryRecord {
    Event(RendererTelemetryEvent),
    Arms(ArmsCustomEvent),
    ActionTrace(RendererActionTraceBatch),
    HeapSample(RendererHeapSample),
    LocalTtft(LocalTtftBatch),
}

#[derive(Clone)]
pub struct TelemetryExporter {
    sender: mpsc::Sender<TelemetryRecord>,
}

impl TelemetryExporter {
    /// Queue an event without blocking the command handler.
    pub fn record(&self, record: TelemetryRecord) {
        // A full channel means the exporter is behind; dropping telemetry is
        // preferable to blocking the UI thread.
        let _ = self.sender.try_send(record);
    }
}

#[derive(Clone, Default)]
struct TelemetryConfig {
    logs: Option<String>,
    traces: Option<String>,
    metrics: Option<String>,
}

impl TelemetryConfig {
    fn from_env() -> Self {
        let base = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok();
        Self {
            logs: resolve_signal_endpoint(
                std::env::var("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT").ok(),
                base.clone(),
                "/v1/logs",
            ),
            traces: resolve_signal_endpoint(
                std::env::var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT").ok(),
                base.clone(),
                "/v1/traces",
            ),
            metrics: resolve_signal_endpoint(
                std::env::var("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT").ok(),
                base,
                "/v1/metrics",
            ),
        }
    }

    fn is_empty(&self) -> bool {
        self.logs.is_none() && self.traces.is_none() && self.metrics.is_none()
    }
}

/// Start the exporter task. Returns a handle that can be managed as Tauri state.
pub fn start(app_version: &'static str) -> TelemetryExporter {
    let (sender, receiver) = mpsc::channel(CHANNEL_CAPACITY);
    let config = TelemetryConfig::from_env();
    tauri::async_runtime::spawn(run(receiver, config, app_version));
    TelemetryExporter { sender }
}

fn resolve_signal_endpoint(
    specific: Option<String>,
    base: Option<String>,
    path: &str,
) -> Option<String> {
    resolve_endpoint(specific, false, path).or_else(|| resolve_endpoint(base, true, path))
}

fn resolve_endpoint(value: Option<String>, append_path: bool, path: &str) -> Option<String> {
    let value = value?.trim().to_string();
    if value.is_empty() {
        return None;
    }
    let mut url = reqwest::Url::parse(&value).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    if append_path && !url.path().trim_end_matches('/').ends_with(path) {
        let next = format!("{}{}", url.path().trim_end_matches('/'), path);
        url.set_path(&next);
    }
    Some(url.to_string())
}

async fn run(
    mut receiver: mpsc::Receiver<TelemetryRecord>,
    config: TelemetryConfig,
    app_version: &'static str,
) {
    if config.is_empty() {
        // Telemetry is disabled: drain so senders never see backpressure.
        while receiver.recv().await.is_some() {}
        return;
    }
    let client = match reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build() {
        Ok(client) => client,
        Err(_) => return,
    };
    let mut batch: Vec<TelemetryRecord> = Vec::new();
    let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            record = receiver.recv() => {
                match record {
                    Some(record) => {
                        batch.push(record);
                        if batch.len() >= BATCH_SIZE {
                            flush(&client, &config, &mut batch, app_version).await;
                        }
                    }
                    None => {
                        flush(&client, &config, &mut batch, app_version).await;
                        return;
                    }
                }
            }
            _ = ticker.tick() => {
                if !batch.is_empty() {
                    flush(&client, &config, &mut batch, app_version).await;
                }
            }
        }
    }
}

async fn flush(
    client: &reqwest::Client,
    config: &TelemetryConfig,
    batch: &mut Vec<TelemetryRecord>,
    app_version: &'static str,
) {
    if batch.is_empty() {
        return;
    }
    let mut logs = Vec::new();
    let mut traces = Vec::new();
    let mut metrics = Vec::new();
    for record in std::mem::take(batch) {
        match record {
            TelemetryRecord::Event(_) | TelemetryRecord::Arms(_) => logs.push(record),
            TelemetryRecord::ActionTrace(_) => traces.push(record),
            TelemetryRecord::HeapSample(_) | TelemetryRecord::LocalTtft(_) => metrics.push(record),
        }
    }
    if let Some(endpoint) = &config.logs {
        if !logs.is_empty() {
            post(client, endpoint, build_logs_payload(&logs, app_version)).await;
        }
    }
    if let Some(endpoint) = &config.traces {
        if !traces.is_empty() {
            post(client, endpoint, build_traces_payload(&traces, app_version)).await;
        }
    }
    if let Some(endpoint) = &config.metrics {
        if !metrics.is_empty() {
            post(
                client,
                endpoint,
                build_metrics_payload(&metrics, app_version),
            )
            .await;
        }
    }
}

async fn post(client: &reqwest::Client, endpoint: &str, payload: Value) {
    // Telemetry must never surface as an application error.
    let _ = client
        .post(endpoint)
        .header("content-type", "application/json")
        .body(payload.to_string())
        .send()
        .await;
}

fn string_attribute(key: &str, value: &str) -> Value {
    json!({ "key": key, "value": { "stringValue": value } })
}

fn int_attribute(key: &str, value: i64) -> Value {
    json!({ "key": key, "value": { "intValue": value } })
}

fn double_attribute(key: &str, value: f64) -> Value {
    json!({ "key": key, "value": { "doubleValue": value } })
}

fn bool_attribute(key: &str, value: bool) -> Value {
    json!({ "key": key, "value": { "boolValue": value } })
}

fn json_attribute(key: &str, value: &Value) -> Option<Value> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(string_attribute(key, text)),
        Value::Bool(flag) => Some(bool_attribute(key, *flag)),
        Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                Some(int_attribute(key, int))
            } else {
                number.as_f64().map(|double| double_attribute(key, double))
            }
        }
        other => Some(string_attribute(key, &other.to_string())),
    }
}

fn push_string(attributes: &mut Vec<Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
        attributes.push(string_attribute(key, value));
    }
}

fn push_detail(attributes: &mut Vec<Value>, detail: &Map<String, Value>) {
    for (key, value) in detail {
        let rendered = match value {
            Value::String(text) => text.clone(),
            Value::Null => continue,
            other => other.to_string(),
        };
        attributes.push(string_attribute(&format!("detail.{key}"), &rendered));
    }
}

fn resource_attributes(app_version: &str) -> Vec<Value> {
    vec![
        string_attribute(
            "service.name",
            std::env::var("OTEL_SERVICE_NAME")
                .ok()
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(DEFAULT_SERVICE_NAME),
        ),
        string_attribute("service.version", app_version),
        string_attribute("telemetry.sdk.name", "zcode-tauri"),
        string_attribute("os.type", std::env::consts::OS),
        string_attribute("host.arch", std::env::consts::ARCH),
    ]
}

fn event_log_record(event: &RendererTelemetryEvent) -> Value {
    let mut attributes = Vec::new();
    push_string(&mut attributes, "elementName", Some(&event.element_name));
    push_string(&mut attributes, "eventRegion", Some(&event.event_region));
    push_string(&mut attributes, "eventType", Some(&event.event_type));
    push_string(&mut attributes, "eventText", event.event_text.as_deref());
    push_string(&mut attributes, "userId", event.user_id.as_deref());
    push_string(&mut attributes, "talkId", event.talk_id.as_deref());
    push_string(&mut attributes, "messageId", event.message_id.as_deref());
    push_string(
        &mut attributes,
        "clientTimezone",
        Some(&event.context.client_timezone),
    );
    push_string(
        &mut attributes,
        "clientLanguage",
        Some(&event.context.client_language),
    );
    push_string(
        &mut attributes,
        "screenResolution",
        Some(&event.context.screen_resolution),
    );
    push_detail(&mut attributes, &event.event_extra_detail);
    log_record(&event.event_type, attributes)
}

fn arms_log_record(event: &ArmsCustomEvent) -> Value {
    let mut attributes = Vec::new();
    push_string(&mut attributes, "group", Some(&event.group));
    if let Some(value) = event.value {
        if value.fract() == 0.0 && value.abs() < i64::MAX as f64 {
            attributes.push(int_attribute("value", value as i64));
        } else {
            attributes.push(double_attribute("value", value));
        }
    }
    for (key, value) in &event.properties {
        let rendered = match value {
            Value::String(text) => text.clone(),
            Value::Null => continue,
            other => other.to_string(),
        };
        attributes.push(string_attribute(&format!("property.{key}"), &rendered));
    }
    log_record(&event.name, attributes)
}

fn log_record(body: &str, attributes: Vec<Value>) -> Value {
    json!({
        "timeUnixNano": now_nanos(),
        "severityText": "INFO",
        "body": { "stringValue": body },
        "attributes": attributes,
    })
}

fn now_nanos() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

fn ms_to_nanos(value: f64) -> String {
    if !value.is_finite() || value <= 0.0 {
        return "0".to_string();
    }
    format!("{:.0}", value * 1_000_000.0)
}

fn span_status_code(status: &str) -> u8 {
    match status {
        "ok" => 1,
        "error" => 2,
        _ => 0,
    }
}

fn build_logs_payload(records: &[TelemetryRecord], app_version: &str) -> Value {
    let log_records: Vec<Value> = records
        .iter()
        .filter_map(|record| match record {
            TelemetryRecord::Event(event) => Some(event_log_record(event)),
            TelemetryRecord::Arms(event) => Some(arms_log_record(event)),
            _ => None,
        })
        .collect();
    json!({
        "resourceLogs": [{
            "resource": { "attributes": resource_attributes(app_version) },
            "scopeLogs": [{
                "scope": { "name": "zcode.renderer" },
                "logRecords": log_records,
            }],
        }],
    })
}

fn build_traces_payload(records: &[TelemetryRecord], app_version: &str) -> Value {
    let resource_spans: Vec<Value> = records
        .iter()
        .filter_map(|record| {
            let TelemetryRecord::ActionTrace(batch) = record else {
                return None;
            };
            let spans: Vec<Value> = batch
                .spans
                .iter()
                .map(|span| {
                    let attributes: Vec<Value> = span
                        .attributes
                        .iter()
                        .filter_map(|(key, value)| json_attribute(key, value))
                        .collect();
                    json!({
                        "traceId": span.trace_id,
                        "spanId": span.span_id,
                        "name": span.name,
                        "kind": 1,
                        "startTimeUnixNano": ms_to_nanos(span.start_time_unix_ms),
                        "endTimeUnixNano": ms_to_nanos(span.end_time_unix_ms),
                        "status": { "code": span_status_code(&span.status) },
                        "attributes": attributes,
                    })
                })
                .collect();
            Some(json!({
                "resource": {
                    "attributes": [
                        string_attribute("service.name", &batch.resource.service_name),
                        string_attribute("service.version", &batch.resource.service_version),
                        string_attribute(
                            "deployment.environment",
                            &batch.resource.deployment_environment,
                        ),
                        string_attribute(
                            "renderer.instance.id",
                            &batch.resource.renderer_instance_id,
                        ),
                        string_attribute(
                            "renderer.trace.instance.id",
                            &batch.renderer_instance_id,
                        ),
                        string_attribute("renderer.trace.batch.version", &batch.version.to_string()),
                        string_attribute("renderer.trace.batch.sequence", &batch.sequence.to_string()),
                        string_attribute(
                            "renderer.trace.batch.dropped",
                            &batch.dropped_since_last_flush.to_string(),
                        ),
                        string_attribute("telemetry.sdk.name", "zcode-tauri"),
                        string_attribute("telemetry.sdk.version", app_version),
                    ],
                },
                "scopeSpans": [{
                    "scope": { "name": "zcode.renderer" },
                    "spans": spans,
                }],
            }))
        })
        .collect();
    json!({ "resourceSpans": resource_spans })
}

fn build_metrics_payload(records: &[TelemetryRecord], app_version: &str) -> Value {
    let mut heap_points = Vec::new();
    let mut ttft_points = Vec::new();
    for record in records {
        match record {
            TelemetryRecord::HeapSample(sample) => {
                heap_points.push(json!({
                    "timeUnixNano": now_nanos(),
                    "asDouble": sample.heap_used_kb,
                    "attributes": [string_attribute("source", "renderer")],
                }));
            }
            TelemetryRecord::LocalTtft(batch) => {
                for item in &batch.records {
                    let mut attributes = Vec::new();
                    push_string(&mut attributes, "observationId", Some(&item.observation_id));
                    push_string(&mut attributes, "commandId", item.command_id.as_deref());
                    push_string(&mut attributes, "kind", item.kind.as_deref());
                    push_string(&mut attributes, "outcome", item.outcome.as_deref());
                    push_string(
                        &mut attributes,
                        "firstOutputKind",
                        item.first_output_kind.as_deref(),
                    );
                    push_string(&mut attributes, "quality", item.quality.as_deref());
                    push_string(&mut attributes, "provider", item.provider.as_deref());
                    push_string(&mut attributes, "model", item.model.as_deref());
                    push_string(&mut attributes, "sendMode", item.send_mode.as_deref());
                    push_string(
                        &mut attributes,
                        "rendererInstanceId",
                        Some(&batch.renderer_instance_id),
                    );
                    attributes.push(int_attribute("sequence", batch.sequence as i64));
                    attributes.push(int_attribute("dropped", batch.dropped as i64));
                    attributes.push(int_attribute("batchVersion", batch.version as i64));
                    ttft_points.push(json!({
                        "timeUnixNano": now_nanos(),
                        "asDouble": (item.end - item.start).max(0.0),
                        "attributes": attributes,
                    }));
                }
            }
            _ => {}
        }
    }
    let mut metrics = Vec::new();
    if !heap_points.is_empty() {
        metrics.push(json!({
            "name": "renderer.heap.used",
            "unit": "kBy",
            "gauge": { "dataPoints": heap_points },
        }));
    }
    if !ttft_points.is_empty() {
        metrics.push(json!({
            "name": "renderer.ttft",
            "unit": "ms",
            "gauge": { "dataPoints": ttft_points },
        }));
    }
    json!({
        "resourceMetrics": [{
            "resource": { "attributes": resource_attributes(app_version) },
            "scopeMetrics": [{
                "scope": { "name": "zcode.renderer" },
                "metrics": metrics,
            }],
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_event() -> RendererTelemetryEvent {
        RendererTelemetryEvent {
            element_name: "send_button".to_string(),
            event_region: "chat".to_string(),
            event_type: "ui_click".to_string(),
            event_text: Some("Send".to_string()),
            event_extra_detail: Map::from_iter([(
                "model".to_string(),
                Value::String("glm-5".to_string()),
            )]),
            user_id: Some("user-1".to_string()),
            talk_id: None,
            message_id: None,
            context: TelemetryRendererContext {
                client_timezone: "Asia/Shanghai".to_string(),
                client_language: "zh-CN".to_string(),
                screen_resolution: "1920x1080".to_string(),
            },
        }
    }

    fn sample_trace_batch() -> RendererActionTraceBatch {
        RendererActionTraceBatch {
            version: 1,
            renderer_instance_id: "renderer-1".to_string(),
            sequence: 7,
            dropped_since_last_flush: 2,
            resource: RendererActionTraceResource {
                service_name: "zcode-desktop-renderer".to_string(),
                service_version: "3.14.0".to_string(),
                deployment_environment: "test".to_string(),
                renderer_instance_id: "renderer-1".to_string(),
            },
            spans: vec![RendererActionTraceSpan {
                trace_id: "0123456789abcdef0123456789abcdef".to_string(),
                span_id: "0123456789abcdef".to_string(),
                name: "ui_action".to_string(),
                start_time_unix_ms: 1_700_000_000_000.0,
                end_time_unix_ms: 1_700_000_000_042.0,
                status: "error".to_string(),
                attributes: Map::from_iter([
                    (
                        "feature_id".to_string(),
                        Value::String("settings".to_string()),
                    ),
                    ("action".to_string(), Value::String("save".to_string())),
                    ("configured".to_string(), Value::Bool(true)),
                ]),
            }],
        }
    }

    fn sample_ttft_batch() -> LocalTtftBatch {
        LocalTtftBatch {
            version: 1,
            renderer_instance_id: "renderer-1".to_string(),
            sequence: 3,
            dropped: 1,
            records: vec![LocalTtftRecord {
                observation_id: "obs-1".to_string(),
                command_id: Some("cmd-1".to_string()),
                kind: Some("first_text".to_string()),
                outcome: Some("success".to_string()),
                start: 100.0,
                end: 250.0,
                first_output_kind: Some("text".to_string()),
                quality: Some("complete".to_string()),
                provider: Some("zai".to_string()),
                model: Some("glm-5".to_string()),
                send_mode: Some("idle".to_string()),
            }],
        }
    }

    fn attribute_value(record: &Value, key: &str) -> Option<Value> {
        record
            .get("attributes")?
            .as_array()?
            .iter()
            .find(|item| item.get("key").and_then(Value::as_str) == Some(key))?
            .get("value")
            .cloned()
    }

    fn metric<'a>(payload: &'a Value, name: &str) -> &'a Value {
        payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["name"] == name)
            .unwrap()
    }

    #[test]
    fn maps_renderer_event_to_otlp_log_record() {
        let payload = build_logs_payload(&[TelemetryRecord::Event(sample_event())], "3.14.0");
        let record = &payload["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
        assert_eq!(record["body"]["stringValue"], "ui_click");
        assert_eq!(
            attribute_value(record, "elementName").unwrap()["stringValue"],
            "send_button"
        );
        assert_eq!(
            attribute_value(record, "detail.model").unwrap()["stringValue"],
            "glm-5"
        );
        assert_eq!(
            attribute_value(record, "clientTimezone").unwrap()["stringValue"],
            "Asia/Shanghai"
        );
        assert!(record["timeUnixNano"]
            .as_str()
            .is_some_and(|value| !value.is_empty()));
        let resource = &payload["resourceLogs"][0]["resource"]["attributes"];
        assert!(resource.as_array().unwrap().iter().any(
            |item| item["key"] == "service.version" && item["value"]["stringValue"] == "3.14.0"
        ));
    }

    #[test]
    fn maps_arms_event_with_int_value_and_properties() {
        let event = ArmsCustomEvent {
            name: "task_started".to_string(),
            group: "task".to_string(),
            value: Some(3.0),
            properties: Map::from_iter([
                ("mode".to_string(), Value::String("yolo".to_string())),
                ("count".to_string(), Value::from(2)),
            ]),
        };
        let payload = build_logs_payload(&[TelemetryRecord::Arms(event)], "3.14.0");
        let record = &payload["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
        assert_eq!(record["body"]["stringValue"], "task_started");
        assert_eq!(attribute_value(record, "value").unwrap()["intValue"], 3);
        assert_eq!(
            attribute_value(record, "property.mode").unwrap()["stringValue"],
            "yolo"
        );
        assert_eq!(
            attribute_value(record, "property.count").unwrap()["stringValue"],
            "2"
        );
    }

    #[test]
    fn maps_action_trace_batch_to_otlp_spans() {
        let payload = build_traces_payload(
            &[TelemetryRecord::ActionTrace(sample_trace_batch())],
            "3.14.0",
        );
        let resource_span = &payload["resourceSpans"][0];
        assert_eq!(
            resource_span["resource"]["attributes"][0]["value"]["stringValue"],
            "zcode-desktop-renderer"
        );
        let span = &resource_span["scopeSpans"][0]["spans"][0];
        assert_eq!(span["traceId"], "0123456789abcdef0123456789abcdef");
        assert_eq!(span["spanId"], "0123456789abcdef");
        assert_eq!(span["name"], "ui_action");
        assert_eq!(span["startTimeUnixNano"], "1700000000000000000");
        // JS timestamps are f64 milliseconds; nanosecond conversion can differ
        // by a few hundred ns from the decimal literal.
        let end_nanos: i128 = span["endTimeUnixNano"].as_str().unwrap().parse().unwrap();
        assert!((end_nanos - 1_700_000_000_042_000_000i128).abs() < 1_000);
        assert_eq!(span["status"]["code"], 2);
        assert_eq!(
            attribute_value(span, "feature_id").unwrap()["stringValue"],
            "settings"
        );
        assert_eq!(
            attribute_value(span, "configured").unwrap()["boolValue"],
            true
        );
    }

    #[test]
    fn maps_heap_and_ttft_to_otlp_metrics() {
        let payload = build_metrics_payload(
            &[
                TelemetryRecord::HeapSample(RendererHeapSample {
                    heap_used_kb: 512.0,
                }),
                TelemetryRecord::LocalTtft(sample_ttft_batch()),
            ],
            "3.14.0",
        );
        let heap = metric(&payload, "renderer.heap.used");
        assert_eq!(heap["gauge"]["dataPoints"][0]["asDouble"], 512.0);
        let ttft = metric(&payload, "renderer.ttft");
        assert_eq!(ttft["unit"], "ms");
        let point = &ttft["gauge"]["dataPoints"][0];
        assert_eq!(point["asDouble"], 150.0);
        assert_eq!(
            attribute_value(point, "outcome").unwrap()["stringValue"],
            "success"
        );
        assert_eq!(
            attribute_value(point, "rendererInstanceId").unwrap()["stringValue"],
            "renderer-1"
        );
    }

    #[test]
    fn resolves_otlp_signal_endpoints() {
        assert_eq!(
            resolve_signal_endpoint(
                None,
                Some("http://collector:4318".to_string()),
                "/v1/traces"
            )
            .as_deref(),
            Some("http://collector:4318/v1/traces")
        );
        assert_eq!(
            resolve_signal_endpoint(
                Some("http://collector:4318/custom/traces".to_string()),
                Some("http://base:4318".to_string()),
                "/v1/traces"
            )
            .as_deref(),
            Some("http://collector:4318/custom/traces")
        );
        assert_eq!(
            resolve_signal_endpoint(None, Some("file:///tmp".to_string()), "/v1/metrics"),
            None
        );
        assert_eq!(resolve_signal_endpoint(None, None, "/v1/logs"), None);
    }

    #[test]
    fn empty_detail_values_are_skipped() {
        let mut event = sample_event();
        event
            .event_extra_detail
            .insert("empty".to_string(), Value::Null);
        let payload = build_logs_payload(&[TelemetryRecord::Event(event)], "3.14.0");
        let record = &payload["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
        assert!(attribute_value(record, "detail.empty").is_none());
    }

    #[tokio::test]
    async fn posts_otlp_payload_to_configured_endpoint() {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let read = tokio::time::timeout(Duration::from_secs(2), socket.read(&mut buffer))
                    .await
                    .expect("request timeout")
                    .unwrap();
                if read == 0 {
                    break;
                }
                received.extend_from_slice(&buffer[..read]);
                if String::from_utf8_lossy(&received).contains("ui_click") {
                    break;
                }
            }
            String::from_utf8_lossy(&received).to_string()
        });
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap();
        let config = TelemetryConfig {
            logs: Some(format!("http://{address}/v1/logs")),
            ..Default::default()
        };
        let mut batch = vec![TelemetryRecord::Event(sample_event())];
        flush(&client, &config, &mut batch, "3.14.0").await;
        assert!(batch.is_empty());
        let request = server.await.unwrap();
        assert!(request.starts_with("POST /v1/logs"));
        assert!(request.contains("\"resourceLogs\""));
        assert!(request.contains("ui_click"));
    }
}
