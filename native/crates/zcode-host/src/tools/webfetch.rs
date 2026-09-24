//! WebFetch：抓取 URL 内容（替代 TS `core/src/tool/handlers/webfetch-*`）。
//!
//! 契约对齐 TS `contracts/tools/webfetch.ts`：输入 `{url, prompt}`，输出
//! `{content, url, finalUrl, status}`。
//!
//! **egress 守卫**（与 TS `webfetch-url.ts` 同纪律，README 差异表记录）：拒绝
//! 私网/环回/链路本地/元数据主机与非 http(s) 协议；重定向逐跳重验。这是外部
//! 输入边界，必须挡——本地网络与云元数据端点都不该被模型随手读到。
//!
//! 抓取走 zcode-provider 已验证的 HTTP 栈（reqwest），不引入新依赖。

use std::net::IpAddr;
use std::time::Duration;

use serde_json::{json, Value};
use url::Url;

use crate::tools::{string_schema, Tool, ToolContext, ToolContract, ToolError, ToolErrorCode};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_REDIRECTS: usize = 5;
const MAX_CONTENT_BYTES: usize = 5 * 1024 * 1024;

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::new(ToolErrorCode::InvalidInput, message)
}

/// URL 解析 + egress 守卫（私网/环回/链路本地/元数据/非 http(s) 一律拒绝）。
pub fn normalize_fetch_url(raw: &str) -> Result<Url, ToolError> {
    let url = Url::parse(raw.trim()).map_err(|error| invalid(format!("url: {error}")))?;
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(invalid(format!("unsupported URL scheme: {other}"))),
    }
    // IPv6 字面量在 host_str() 里带方括号（"[::1]"）：剥掉括号再判定，
    // 否则字符串判定先行、IP 判定永不执行（[::1] 会漏过守卫）。
    let host = url
        .host_str()
        .ok_or_else(|| invalid("url: missing host"))?
        .trim_matches(['[', ']'])
        .to_ascii_lowercase();
    if is_blocked_hostname(&host) {
        return Err(invalid(
            "url: private/loopback/metadata hosts are not fetchable",
        ));
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_blocked_ip(ip) {
            return Err(invalid(
                "url: private/loopback/link-local addresses are not fetchable",
            ));
        }
    }
    Ok(url)
}

fn is_blocked_hostname(host: &str) -> bool {
    host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host == "metadata.google.internal"
        || host == "metadata"
}

fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
            // 云元数据 169.254.169.254 已由 is_link_local 覆盖。
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // 唯一本地地址 fc00::/7 与链路本地 fe80::/10。
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

pub struct WebFetchTool;

impl Tool for WebFetchTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "WebFetch".to_string(),
            description: "Fetch a URL over HTTP(S) and return its readable content. Private, loopback, link-local and cloud-metadata hosts are refused. Redirects are re-checked per hop.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": string_schema("The URL to fetch content from"),
                    "prompt": string_schema("What you want from the page (guides extraction)"),
                },
                "required": ["url", "prompt"],
            }),
            read_only: true,
            destructive: false,
            concurrent_safe: true,
            side_effect_scope: crate::tools::SCOPE_NONE,
            timeout_ms: DEFAULT_TIMEOUT_MS + 5_000,
            max_output_bytes: 1024 * 1024,
        }
    }

    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let raw_url = input
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("url: required string"))?;
        // prompt 是模型侧的提取指令，host 侧不据此二次检索（原样透传给模型）。
        let _prompt = input
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("prompt: required string"))?;

        let content = fetch_with_redirects(raw_url, ctx)?;
        Ok(json!({
            "content": content,
            "url": raw_url,
            "finalUrl": raw_url,
            "status": 200,
        })
        .to_string())
    }
}

fn fetch_with_redirects(raw_url: &str, ctx: &ToolContext) -> Result<String, ToolError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(DEFAULT_TIMEOUT_MS))
        .user_agent("zcode-host/1.0")
        .build()
        .map_err(|error| {
            ToolError::new(
                ToolErrorCode::ExecutionFailed,
                format!("http client init: {error}"),
            )
        })?;

    let mut current = normalize_fetch_url(raw_url)?;
    for _ in 0..=MAX_REDIRECTS {
        if ctx.is_cancelled() {
            return Err(ToolError::new(ToolErrorCode::Cancelled, "turn cancelled"));
        }
        let response = client.get(current.as_str()).send().map_err(|error| {
            ToolError::new(
                ToolErrorCode::ExecutionFailed,
                format!("fetch failed: {error}"),
            )
        })?;
        let status = response.status();
        if status.is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| {
                    ToolError::new(
                        ToolErrorCode::ExecutionFailed,
                        "redirect without Location header",
                    )
                })?;
            current = current.join(location).map_err(|error| {
                ToolError::new(
                    ToolErrorCode::ExecutionFailed,
                    format!("invalid redirect target: {error}"),
                )
            })?;
            normalize_fetch_url(current.as_str())?;
            continue;
        }
        if !status.is_success() {
            return Err(ToolError::new(
                ToolErrorCode::ExecutionFailed,
                format!("HTTP {}", status.as_u16()),
            ));
        }
        let body = response.text().map_err(|error| {
            ToolError::new(
                ToolErrorCode::ExecutionFailed,
                format!("read body: {error}"),
            )
        })?;
        if body.len() > MAX_CONTENT_BYTES {
            return Err(ToolError::new(
                ToolErrorCode::ExecutionFailed,
                format!("content exceeds {MAX_CONTENT_BYTES} bytes"),
            ));
        }
        return Ok(body);
    }
    Err(ToolError::new(
        ToolErrorCode::ExecutionFailed,
        format!("exceeded {MAX_REDIRECTS} redirects"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_non_http_schemes() {
        assert!(normalize_fetch_url("file:///etc/passwd").is_err());
        assert!(normalize_fetch_url("ftp://example.com").is_err());
        assert!(normalize_fetch_url("not a url").is_err());
    }

    #[test]
    fn refuses_private_loopback_and_metadata_hosts() {
        for url in [
            "http://localhost:8080/x",
            "http://127.0.0.1/x",
            "http://10.0.0.5/x",
            "http://192.168.1.1/x",
            "http://169.254.169.254/latest/meta-data",
            "http://metadata.google.internal/",
            "http://foo.local/",
            "http://[::1]/x",
        ] {
            assert!(
                normalize_fetch_url(url).is_err(),
                "must refuse private/metadata target: {url}"
            );
        }
    }

    #[test]
    fn allows_public_http_and_https() {
        assert!(normalize_fetch_url("https://example.com/docs").is_ok());
        assert!(normalize_fetch_url("http://93.184.216.34/x").is_ok());
    }
}
