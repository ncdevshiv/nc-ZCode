//! Workspace Hooks 的**领域层**：配置发现/解析、声明与 bundle 摘要、信任评估。
//!
//! 对照 TS：
//! - `packages/shared/src/workspace-hook-config.ts`（格式、门禁、超时/输出上限）
//! - `packages/shared/src/workspace-hook-digest.ts`（canonical payload + sha256）
//! - `packages/shared/src/zcode-protocol-v4/workspace-hook-review.ts`（投影形状）
//! - `core/src/hooks/workspace-hook-trust-evaluation.ts`（信任状态机）
//!
//! 分层原因（与 goal 子系统同一纪律）：摘要与信任判定是纯函数，先做成可单测、
//! 可被命令面/准入面/评审流共用的模块；磁盘与交互编排在后续模块里，不在这里。
//!
//! **摘要必须与 TS 逐字节一致**：`sha256(JSON.stringify(payload))`。测试里钉死了
//! 用 Node 的 `JSON.stringify`+`createHash` 算出的参考值——摘要分叉等于所有已授信
//! 的 hook 在升级后集体失效（用户被重新提示一遍全部 Hook），这不是"格式差异"，
//! 是产品回归。

pub mod admission;
pub mod config;
pub mod digest;
pub mod display;
pub mod flow;
pub mod mutation;
pub mod runtime;
pub mod store;
pub mod trust;

pub use admission::{
    review_target_from_payload, rfc3339_utc, AdmissionError, RequestReviewOutcome,
    SnapshotEvaluation, WorkspaceHookAdmission,
};
pub use config::{
    configured_gates, discover_config_paths, read_sources, resolve_runtime_root,
    resolve_timeout_ms, ConfigError, ConfigFileKind, ConfigPathRef, Gates, HookDefinition,
    HookEventName, HookEvents, HooksConfig, MatcherConfig, RootGates, RuntimeRoot, ShellConfig,
    SourceInput, SourceReadError, DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_TIMEOUT_MS, EVENT_NAMES,
    WORKSPACE_HOOK_DIGEST_SCHEMA_VERSION,
};
pub use digest::{
    build_bundle, declaration_digest, resolve_entries, BundleSnapshot, CanonicalEntry, EntryType,
    SourceFileSnapshot,
};
pub use display::{
    execution_descriptor, quote_command_part, sanitize_hook_display_text, HookExecutionDescriptor,
};
pub use flow::{
    review_deadline, FlowAction, FlowError, FlowOutcome, FlowRequest, FlowState, OpenOutcome,
    ResolveResult, ReviewFlowRegistry, ReviewTarget,
};
pub use mutation::{write_configured_toggle, MutationError};
pub use store::{TrustStore, TrustStoreLoad, TRUST_STORE_FILE};
pub use trust::{
    create_trust_records, evaluate_entry, trust_key, AdmissionClass, EffectiveState, HookPolicy,
    PolicyMode, TrustRecord, TrustState, TrustStoreFile, TrustStoreStatus,
};
