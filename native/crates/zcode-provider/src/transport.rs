//! 执行器的 I/O 与时间边界（对齐 TS `model-execution.ts` 的 fetch 读取面与
//! `runner-retry.ts` 的 sleep 注入点）。
//!
//! 两个注入点把外部世界挡在 `executor` 之外：
//!
//! ```text
//! StreamRequest ─▶ execute()
//!                    │  ProviderTransport（一次 attempt：open → 错误体 / 分块读取）
//!                    │  AttemptClock（退避等待 + 抖动随机数）
//!                    ▼
//!              Vec<ModelStreamEvent> + 可选终态失败
//! ```
//!
//! 测试注入脚本传输 + 假时钟：零网络、零真实等待；生产实现是 `http_transport::HttpTransport`
//! 与 `RealClock`。

use std::future::Future;

/// 一次 POST 请求的全部出站信息（URL/头/体由调用方用 `request`模块构造完成后传入）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// 响应首行：状态 +全部头（名称小写化保留原值，供 Retry-After / Content-Type 读取）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportResponseHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

impl TransportResponseHead {
    /// 取首个匹配头（名称大小写不敏感）。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// 传输层失败。`code` 使用 Node 风格错误码（ECONNREFUSED/…）以复用 `classify`
/// 的超时/网络/TLS/代理探针——TS 侧这些探针读的就是 undici/fetch 的 `error.code`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportFailure {
    /// 连接/读取通用超时（非 idle 窗口）。
    Timeout,
    /// 单次 open/next_chunk 的 idle 窗口超时；`timeout_ms` 为本窗口配置值，
    /// 用于还原 TS `ModelStreamIdleTimeoutError` 的文案
    /// （`Model stream stalled: no event received for {ms}ms.`）。
    IdleTimeout { timeout_ms: u64 },
    /// 其他传输错误。
    Network {
        code: Option<String>,
        message: String,
    },
}

impl TransportFailure {
    pub fn network(code: Option<&str>, message: impl Into<String>) -> Self {
        Self::Network {
            code: code.map(str::to_string),
            message: message.into(),
        }
    }
}

/// 传输契约（TS fetch 读取面的收敛）：
///
/// - `open` 每个 attempt 调一次；返回状态与全部响应头。
/// - 非 2xx 错误体 / 非 SSE 响应体走 `read_body`（整体缓冲读取）。
/// - 2xx SSE 流走 `next_chunk` 循环直到 `Ok(None)`。
/// - `idle_timeout_ms` 为 None 或 <=0 表示该次调用禁用 idle 窗口；窗口按**每次调用**
///   独立计时（对齐 TS 每次 `readNext` 都以完整 timeoutMs 与读取竞争的语义）。
pub trait ProviderTransport {
    fn open(
        &mut self,
        request: TransportRequest,
        idle_timeout_ms: Option<u64>,
    ) -> impl Future<Output = Result<TransportResponseHead, TransportFailure>> + Send;

    fn read_body(&mut self) -> impl Future<Output = Result<Vec<u8>, TransportFailure>> + Send;

    fn next_chunk(
        &mut self,
        idle_timeout_ms: Option<u64>,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, TransportFailure>> + Send;
}

/// attempt 时钟：`now_ms`/`sleep_ms` 驱动退避与诊断，`jitter_random` 提供
/// `calculate_retry_delay` 的抖动源（TS 侧为 `Math.random()` 注入）。
pub trait AttemptClock {
    fn now_ms(&self) -> u64;

    fn sleep_ms(&self, delay_ms: u64) -> impl Future<Output = ()> + Send;

    /// [0, 1) 的抖动随机数。
    fn jitter_random(&self) -> f64;
}

/// 生产时钟：UNIX epoch 毫秒 + tokio sleep + 系统时间纳秒派生抖动。
///
/// 抖动只服务退避分散（把同时重试的客户端打散），不需要 CSPRNG；
/// 用纳秒派生可避免为它引入额外随机源依赖。
#[derive(Debug, Default, Clone, Copy)]
pub struct RealClock;

impl AttemptClock for RealClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0)
    }

    fn sleep_ms(&self, delay_ms: u64) -> impl Future<Output = ()> + Send {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms))
    }

    fn jitter_random(&self) -> f64 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.subsec_nanos())
            .unwrap_or(0);
        f64::from(nanos) / 1_000_000_000.0
    }
}
