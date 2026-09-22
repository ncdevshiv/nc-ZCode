//! reqwest 实现的 `ProviderTransport`（S2 唯一生产传输）。
//!
//! - idle 窗口在每次 `open`/`next_chunk` 调用上独立计时：对齐 TS
//!   `readNextWithStreamIdleTimeout`（每次 `iterator.next()` 都以完整 timeoutMs
//!   与读取竞争）。
//! - 错误链文本映射为 Node 风格码，复用 `classify` 的超时/网络/TLS/代理探针
//!   （TS 侧读的是 undici/fetch 的 `error.code`，reqwest 无该字段，只能从错误链还原）。

use crate::transport::{
    ProviderTransport, TransportFailure, TransportRequest, TransportResponseHead,
};
use std::future::Future;

pub struct HttpTransport {
    client: reqwest::Client,
    /// 当前 attempt 的活动响应（open 后由 read_body / next_chunk 消费一次）。
    active: Option<reqwest::Response>,
}

impl HttpTransport {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            active: None,
        }
    }
}

impl Default for HttpTransport {
    fn default() -> Self {
        Self::new()
    }
}

/// 响应头 →（小写名, 原值）对；无法按 UTF-8 解码的头值以空串占位（保留名值对位次，
/// Retry-After/Content-Type 均为 ASCII，不受影响）。
fn response_head(response: &reqwest::Response) -> TransportResponseHead {
    TransportResponseHead {
        status: response.status().as_u16(),
        headers: response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_ascii_lowercase(),
                    value.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect(),
    }
}

/// 包装任意 `Result<T, reqwest::Error>` future：idle 窗口超时 → IdleTimeout。
async fn guard_idle<T, F>(idle_timeout_ms: Option<u64>, future: F) -> Result<T, TransportFailure>
where
    F: Future<Output = Result<T, reqwest::Error>>,
{
    let Some(window_ms) = idle_timeout_ms.filter(|window| *window > 0) else {
        return future.await.map_err(map_reqwest_failure);
    };
    match tokio::time::timeout(std::time::Duration::from_millis(window_ms), future).await {
        Ok(result) => result.map_err(map_reqwest_failure),
        Err(_) => Err(TransportFailure::IdleTimeout {
            timeout_ms: window_ms,
        }),
    }
}

impl ProviderTransport for HttpTransport {
    // trait 返回类型显式带 `+ Send`；`async fn` 语法无法表达该边界，保持 RPITIT 形态。
    #[allow(clippy::manual_async_fn)]
    fn open(
        &mut self,
        request: TransportRequest,
        idle_timeout_ms: Option<u64>,
    ) -> impl Future<Output = Result<TransportResponseHead, TransportFailure>> + Send {
        async move {
            let mut send = self.client.post(&request.url).body(request.body);
            for (name, value) in &request.headers {
                let header_name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|error| {
                        TransportFailure::network(
                            None,
                            format!("invalid request header name: {error}"),
                        )
                    })?;
                let header_value = reqwest::header::HeaderValue::from_bytes(value.as_bytes())
                    .map_err(|error| {
                        TransportFailure::network(
                            None,
                            format!("invalid request header value: {error}"),
                        )
                    })?;
                send = send.header(header_name, header_value);
            }
            let response = guard_idle(idle_timeout_ms, send.send()).await?;
            let head = response_head(&response);
            self.active = Some(response);
            Ok(head)
        }
    }

    // 同上：trait 要求 `+ Send` 返回边界，保持 RPITIT 形态。
    #[allow(clippy::manual_async_fn)]
    fn read_body(&mut self) -> impl Future<Output = Result<Vec<u8>, TransportFailure>> + Send {
        async move {
            let response = self
                .active
                .take()
                .ok_or_else(|| TransportFailure::network(None, "response body already consumed"))?;
            // 错误体读取不做 idle 窗口（TS getResponseError 的 body 读取同样不参与
            // readNext 竞争；窗口只约束流式读取）。
            let bytes = response.bytes().await.map_err(map_reqwest_failure)?;
            Ok(bytes.to_vec())
        }
    }

    // 同上：trait 要求 `+ Send` 返回边界，保持 RPITIT 形态。
    #[allow(clippy::manual_async_fn)]
    fn next_chunk(
        &mut self,
        idle_timeout_ms: Option<u64>,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, TransportFailure>> + Send {
        async move {
            let response = self
                .active
                .as_mut()
                .ok_or_else(|| TransportFailure::network(None, "no active response stream"))?;
            let chunk = guard_idle(idle_timeout_ms, response.chunk()).await?;
            Ok(chunk.map(|bytes| bytes.to_vec()))
        }
    }
}

/// reqwest 错误 →传输失败：超时单列，其余按错误链文本映射 Node 风格码。
pub fn map_reqwest_failure(error: reqwest::Error) -> TransportFailure {
    if error.is_timeout() {
        return TransportFailure::Timeout;
    }
    let mut chain = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(inner) = source {
        chain.push_str("; ");
        chain.push_str(&inner.to_string());
        source = inner.source();
    }
    TransportFailure::Network {
        code: map_error_code(&chain),
        message: chain,
    }
}

/// 错误链文本 → Node 风格码（对齐 TS `failure-inspection.ts` 消费的 error.code 族）。
///
/// 判定顺序：超时 → TLS → 代理 → 网络族；文案同时覆盖 Unix（"connection refused"）
/// 与 Windows（"actively refused"、"No such host is known."）措辞。
/// 顺序有意：TLS 握手超时归超时族（可重试），而非 TLS 族。
pub fn map_error_code(chain_text: &str) -> Option<String> {
    let text = chain_text.to_ascii_lowercase();
    if text.contains("timed out") || text.contains("timeout") {
        return Some(code("ETIMEDOUT"));
    }
    if text.contains("certificate")
        || text.contains("handshake")
        || text.contains(" tls")
        || text.contains("tls ")
        || text.contains("ssl")
    {
        // is_tls_code 按包含 CERT/TLS 判定；统一码即可命中 TlsError 族。
        return Some(code("TLS_VALIDATION_FAILED"));
    }
    if text.contains("proxy") {
        return Some(code("PROXY_CONNECTION_FAILED"));
    }
    if text.contains("connection refused") || text.contains("actively refused") {
        return Some(code("ECONNREFUSED"));
    }
    if text.contains("connection reset")
        || text.contains("broken pipe")
        || text.contains("forcibly closed")
        || text.contains("connection closed")
    {
        return Some(code("ECONNRESET"));
    }
    if text.contains("temporary failure in name resolution") || text.contains("try again") {
        return Some(code("EAI_AGAIN"));
    }
    if text.contains("lookup address information")
        || text.contains("name or service not known")
        || text.contains("no such host")
        || text.contains("nodename nor servname")
        || text.contains("unknown host")
    {
        return Some(code("ENOTFOUND"));
    }
    if text.contains("network is unreachable") {
        return Some(code("ENETUNREACH"));
    }
    if text.contains("host is unreachable") || text.contains("no route to host") {
        return Some(code("EHOSTUNREACH"));
    }
    None
}

/// 错误码字面量集中一处，避免散落拼写分叉。
fn code(value: &str) -> String {
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::map_error_code;

    #[test]
    fn maps_unix_and_windows_network_wordings() {
        let cases = [
            ("connection refused", "ECONNREFUSED"),
            (
                "No connection could be made because the target machine actively refused it",
                "ECONNREFUSED",
            ),
            ("connection reset by peer", "ECONNRESET"),
            (
                "An existing connection was forcibly closed by the remote host",
                "ECONNRESET",
            ),
            ("broken pipe", "ECONNRESET"),
            (
                "failed to lookup address information: Name or service not known",
                "ENOTFOUND",
            ),
            ("No such host is known.", "ENOTFOUND"),
            ("temporary failure in name resolution", "EAI_AGAIN"),
            ("network is unreachable", "ENETUNREACH"),
            ("No route to host", "EHOSTUNREACH"),
            ("operation timed out", "ETIMEDOUT"),
        ];
        for (text, expected) in cases {
            assert_eq!(
                map_error_code(text).as_deref(),
                Some(expected),
                "input: {text}"
            );
        }
    }

    #[test]
    fn maps_tls_and_proxy_families() {
        assert_eq!(
            map_error_code("invalid peer certificate: UnknownIssuer").as_deref(),
            Some("TLS_VALIDATION_FAILED")
        );
        assert_eq!(
            map_error_code("The certificate chain was issued by an authority that is not trusted")
                .as_deref(),
            Some("TLS_VALIDATION_FAILED")
        );
        assert_eq!(
            map_error_code("error sending request: proxy connect refused").as_deref(),
            Some("PROXY_CONNECTION_FAILED")
        );
    }

    #[test]
    fn timeout_family_wins_over_tls_wording() {
        // TLS 握手超时应归超时族（可重试），顺序先于 TLS 判定。
        assert_eq!(
            map_error_code("tls handshake timed out").as_deref(),
            Some("ETIMEDOUT")
        );
    }

    #[test]
    fn unknown_text_maps_to_no_code() {
        assert_eq!(map_error_code("something unprecedented"), None);
    }
}
