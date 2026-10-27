//! 会话级准入状态：把快照、信任台账、策略、撤销集与评审 flow 收成一个所有者。
//!
//! 对照 TS：
//! - `core/src/hooks/workspace-hook-trust-coordinator.ts`（记录/策略/撤销 → 评估）
//! - `bootstrap/src/app/workspace-hook-review-request.ts`（评审请求 payload）
//! - `bootstrap/src/app/workspace-hook-review-controller.ts`（request/respond/toggle/revoke）
//! - `product-projection.ts`（`workspaceHookAdmission` 投影：pending 清零 → null）
//!
//! 这个模块不投影帧、不碰 session：它只回答"现在有哪些 hook、能不能跑、评审到哪一步"，
//! 由会话层把结果变成 `state.updated` / `pendingInteractions`。这样评估逻辑可以单测，
//! 也不会出现"命令面与投影面各算一遍"的分叉。
//!
//! 本 host 的 payload 身份：TS 的 `taskId`/`runId`/`remoteSessionId` 没有对应物，
//! 评审请求里用 sessionId 填充前两者、省略第三项（协议允许），命令侧只校验
//! sessionId/workspaceIdentity/bundleDigest。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::config::{read_sources, resolve_runtime_root, RootGates, RuntimeRoot, SourceReadError};
use super::digest::{build_bundle, BundleSnapshot};
use super::flow::{
    review_deadline, FlowOutcome, FlowRequest, FlowState, OpenOutcome, ResolveResult,
    ReviewFlowRegistry, ReviewTarget, REASON_SNAPSHOT_MISMATCH,
};
use super::mutation::{write_configured_toggle, MutationError};
use super::store::TrustStore;
use super::trust::{
    create_trust_records, evaluate_entry, trust_key, AdmissionClass, EffectiveState,
    EvaluationContext, HookPolicy, PolicyMode, TrustRecord, TrustStoreStatus,
};

/// 一次评估的结果（`WorkspaceHookSnapshotEvaluation`）。
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotEvaluation {
    pub workspace_identity: String,
    pub bundle_digest: String,
    pub policy: HookPolicy,
    pub store_status: TrustStoreStatus,
    pub items: Vec<EffectiveState>,
}

impl SnapshotEvaluation {
    pub fn pending_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.admission_class == AdmissionClass::Pending)
            .count()
    }

    pub fn runnable(&self) -> Vec<&EffectiveState> {
        self.items
            .iter()
            .filter(|item| item.effective_runnable)
            .collect()
    }

    pub fn item(&self, review_item_id: &str) -> Option<&EffectiveState> {
        self.items
            .iter()
            .find(|item| item.review_item_id == review_item_id)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AdmissionError {
    /// 配置读不出来：既不评审也不执行（fail-closed）。
    ConfigUnreadable(SourceReadError),
    /// 快照/身份对不上。
    SnapshotMismatch(&'static str),
    /// 企业策略禁止持久信任。
    BlockedByPolicy,
    /// 台账损坏。
    TrustStoreCorrupt,
    /// 没有任何待审项（安全 no-op）。
    NoPendingItems,
    /// 写盘失败。
    WriteFailed(String),
}

impl AdmissionError {
    pub fn reason_code(&self) -> &'static str {
        match self {
            AdmissionError::ConfigUnreadable(_) => "workspace_hooks_config_unreadable",
            AdmissionError::SnapshotMismatch(_) => "workspace_hooks_snapshot_mismatch",
            AdmissionError::BlockedByPolicy => "workspace_hooks_blocked_by_policy",
            AdmissionError::TrustStoreCorrupt => "workspace_hooks_trust_store_corrupt",
            AdmissionError::NoPendingItems => "workspace_hooks_no_enabled_hooks",
            AdmissionError::WriteFailed(_) => "workspace_hooks_config_write_failed",
        }
    }
}

struct Inner {
    snapshot: Option<BundleSnapshot>,
    evaluation: Option<SnapshotEvaluation>,
}

/// 一个会话的 hook 准入状态（快照/台账/策略/撤销/评审 flow 的唯一所有者）。
pub struct WorkspaceHookAdmission {
    workspace_path: PathBuf,
    workspace_identity: String,
    workspace_label: String,
    store: Arc<TrustStore>,
    policy: Mutex<HookPolicy>,
    revoked: Mutex<HashSet<String>>,
    inner: Mutex<Inner>,
    flow: Mutex<ReviewFlowRegistry>,
}

impl WorkspaceHookAdmission {
    pub fn new(
        workspace_path: PathBuf,
        workspace_identity: String,
        workspace_label: String,
        store: Arc<TrustStore>,
    ) -> Self {
        Self {
            workspace_path,
            workspace_identity,
            workspace_label,
            store,
            policy: Mutex::new(HookPolicy::user_decides()),
            revoked: Mutex::new(HashSet::new()),
            inner: Mutex::new(Inner {
                snapshot: None,
                evaluation: None,
            }),
            flow: Mutex::new(ReviewFlowRegistry::new()),
        }
    }

    pub fn workspace_identity(&self) -> &str {
        &self.workspace_identity
    }

    pub fn snapshot(&self) -> Option<BundleSnapshot> {
        self.lock_inner().snapshot.clone()
    }

    pub fn evaluation(&self) -> Option<SnapshotEvaluation> {
        self.lock_inner().evaluation.clone()
    }

    pub fn flow_state(&self) -> Option<FlowState> {
        self.lock_flow().state()
    }

    pub fn set_policy(&self, policy: HookPolicy) {
        *self
            .policy
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = policy;
        self.reevaluate();
    }

    /// 从磁盘重新发现 + 重新评估。配置读失败是硬错误（不拿旧快照凑合）。
    pub fn refresh(&self, discovered_at: &str) -> Result<(), AdmissionError> {
        let (sources, errors) = read_sources(&self.workspace_path, None);
        if let Some(first) = errors.into_iter().next() {
            return Err(AdmissionError::ConfigUnreadable(first));
        }
        let roots: Vec<RootGates> = sources
            .iter()
            .map(|source| RootGates {
                enabled: source.hooks.enabled,
                timeout_ms: source.hooks.timeout_ms,
                max_output_bytes: source.hooks.max_output_bytes,
            })
            .collect();
        let runtime: RuntimeRoot = resolve_runtime_root(&roots);
        let snapshot = build_bundle(
            &self.workspace_identity,
            &self.workspace_path,
            &sources,
            &runtime,
            discovered_at,
        );
        let evaluation = snapshot
            .as_ref()
            .map(|snapshot| self.evaluate_snapshot(snapshot));
        let mut inner = self.lock_inner();
        inner.snapshot = snapshot;
        inner.evaluation = evaluation;
        Ok(())
    }

    /// 用当前台账/策略/撤销集重算评估（授权、撤销之后调用）。
    pub fn reevaluate(&self) -> Option<SnapshotEvaluation> {
        let snapshot = self.lock_inner().snapshot.clone()?;
        let evaluation = self.evaluate_snapshot(&snapshot);
        self.lock_inner().evaluation = Some(evaluation.clone());
        Some(evaluation)
    }

    fn evaluate_snapshot(&self, snapshot: &BundleSnapshot) -> SnapshotEvaluation {
        let (store_status, records) = self.store.records();
        let records_by_key: HashMap<String, TrustRecord> = records
            .into_iter()
            .map(|record| {
                (
                    trust_key(&record.workspace_identity, &record.hook_declaration_digest),
                    record,
                )
            })
            .collect();
        let revoked = self
            .revoked
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let policy = self
            .policy
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone());
        let items = snapshot
            .hooks
            .iter()
            .map(|entry| {
                let source_path = snapshot
                    .source_files
                    .get(entry.source_file_index)
                    .map(|source| source.canonical_path.clone())
                    .unwrap_or_else(|| entry.source_relative_path.clone());
                evaluate_entry(
                    entry,
                    &EvaluationContext {
                        workspace_identity: &snapshot.workspace_identity,
                        bundle_digest: &snapshot.bundle_digest,
                        source_canonical_path: &source_path,
                        policy: &policy,
                        records: &records_by_key,
                        revoked_keys: &revoked,
                        store_status,
                    },
                )
            })
            .collect();
        SnapshotEvaluation {
            workspace_identity: snapshot.workspace_identity.clone(),
            bundle_digest: snapshot.bundle_digest.clone(),
            policy,
            store_status,
            items,
        }
    }

    /// `workspaceHookAdmission` 投影：pending 清零 → null（提示条消失）。
    pub fn admission_state(&self) -> Option<Value> {
        let evaluation = self.lock_inner().evaluation.clone()?;
        let pending_count = evaluation.pending_count();
        if pending_count == 0 {
            return None;
        }
        let mut state = json!({
            "pendingCount": pending_count,
            "bundleDigest": evaluation.bundle_digest,
        });
        if !self.workspace_identity.is_empty() {
            state["workspaceIdentity"] = json!(self.workspace_identity);
        }
        Some(state)
    }

    /// 构造评审请求 payload（`workspaceHookReviewRequestPayloadSchema` 的投影）。
    pub fn build_review_request(
        &self,
        session_id: &str,
        review_flow_id: &str,
        generation: u32,
        interaction_id: &str,
        now_ms: i64,
    ) -> Result<(FlowRequest, Value), AdmissionError> {
        let inner = self.lock_inner();
        let Some(snapshot) = &inner.snapshot else {
            return Err(AdmissionError::NoPendingItems);
        };
        let Some(evaluation) = &inner.evaluation else {
            return Err(AdmissionError::NoPendingItems);
        };
        let items: Vec<Value> = snapshot
            .hooks
            .iter()
            .map(|entry| {
                let state = evaluation
                    .item(&entry.review_item_id)
                    .map(|item| item.trust_state.as_str())
                    .unwrap_or("blocked_untrusted");
                let descriptor = super::display::execution_descriptor(
                    entry.entry_type.as_str(),
                    &entry.command,
                    entry.args.as_deref(),
                    entry.asynchronous == Some(true),
                    entry.resolved_timeout_ms,
                );
                let mut item = json!({
                    "reviewItemId": entry.review_item_id,
                    "event": entry.event.as_str(),
                    "type": entry.entry_type.as_str(),
                    "displayName": match &entry.matcher {
                        Some(matcher) => format!("{} · {matcher}", entry.event.as_str()),
                        None => entry.event.as_str().to_string(),
                    },
                    "displayCommand": descriptor.command_display,
                    "sourcePath": entry.source_relative_path,
                    "resolvedTimeoutMs": entry.resolved_timeout_ms,
                    "resolvedMaxOutputBytes": entry.resolved_max_output_bytes,
                    "executionMode": descriptor.execution_mode,
                    "configuredEnabled": entry.configured_enabled,
                    "editable": entry.editable,
                    "trustState": state,
                });
                if let Some(matcher) = &entry.matcher {
                    item["matcher"] = json!(matcher);
                }
                item
            })
            .collect();
        let pending_count = items
            .iter()
            .filter(|item| {
                matches!(
                    item["trustState"].as_str(),
                    Some("pending_trust") | Some("revoked") | Some("stale_digest")
                )
            })
            .count();
        let event_count = items
            .iter()
            .filter_map(|item| item["event"].as_str())
            .collect::<std::collections::HashSet<_>>()
            .len();
        let payload = json!({
            "kind": "workspaceHookReview",
            "reviewFlowId": review_flow_id,
            "generation": generation,
            "interactionId": interaction_id,
            "sessionId": session_id,
            "taskId": session_id,
            "runId": session_id,
            "workspaceIdentity": snapshot.workspace_identity,
            "workspaceLabel": self.workspace_label,
            "bundleDigest": snapshot.bundle_digest,
            "createdAt": now_ms,
            "deadlineAt": review_deadline(now_ms),
            "sourceFiles": snapshot.source_files.iter().map(|source| {
                json!({
                    "path": source.canonical_path,
                    "displayPath": display_source_path(&source.base_dir, &source.canonical_path),
                    "editable": source.editable,
                })
            }).collect::<Vec<_>>(),
            "summary": {
                "eventCount": event_count,
                "hookCount": items.len(),
                "pendingCount": pending_count,
            },
            "items": items,
            "warningCode": "workspace_hooks_execute_code",
        });
        let request = FlowRequest {
            review_flow_id: review_flow_id.to_string(),
            generation,
            interaction_id: interaction_id.to_string(),
            session_id: session_id.to_string(),
            workspace_identity: snapshot.workspace_identity.clone(),
            bundle_digest: snapshot.bundle_digest.clone(),
            item_ids: snapshot
                .hooks
                .iter()
                .map(|entry| entry.review_item_id.clone())
                .collect(),
            created_at: now_ms,
            deadline_at: review_deadline(now_ms),
        };
        Ok((request, payload))
    }

    /// `requestWorkspaceHookReview`：按需开评审（无待审项 → 安全 no-op）。
    pub fn request_review(
        &self,
        session_id: &str,
        review_flow_id: &str,
        generation: u32,
        interaction_id: &str,
        now_ms: i64,
    ) -> Result<RequestReviewOutcome, AdmissionError> {
        let Some(evaluation) = self.evaluation() else {
            return Err(AdmissionError::NoPendingItems);
        };
        if evaluation.pending_count() == 0 {
            if evaluation
                .items
                .iter()
                .any(|item| item.trust_state == super::trust::TrustState::BlockedPolicy)
            {
                return Err(AdmissionError::BlockedByPolicy);
            }
            if evaluation.store_status == TrustStoreStatus::Corrupt {
                return Err(AdmissionError::TrustStoreCorrupt);
            }
            return Ok(RequestReviewOutcome {
                reused: false,
                payload: None,
            });
        }
        let (request, payload) = self.build_review_request(
            session_id,
            review_flow_id,
            generation,
            interaction_id,
            now_ms,
        )?;
        let outcome = self
            .lock_flow()
            .open(request)
            .map_err(|error| AdmissionError::WriteFailed(error.message))?;
        // 复用不重发 payload：旧的 pendingInteraction 仍在客户端，deadline 不能被重置
        // （否则用户每点一次"去审核"就延期 10 分钟）。
        let reused = outcome == OpenOutcome::Reused;
        Ok(RequestReviewOutcome {
            reused,
            payload: if reused { None } else { Some(payload) },
        })
    }

    /// 校验命令携带的评审 target（快照身份 + pending + generation）。
    pub fn validate_review_target(&self, target: &ReviewTarget) -> Result<(), AdmissionError> {
        self.ensure_snapshot_matches(target)?;
        let flow = self.lock_flow();
        if flow.state() != Some(FlowState::Pending) {
            return Err(AdmissionError::SnapshotMismatch(
                super::flow::REASON_REVIEW_SUPERSEDED,
            ));
        }
        if !flow.generation_matches(target) {
            return Err(AdmissionError::SnapshotMismatch(
                super::flow::REASON_REVIEW_SUPERSEDED,
            ));
        }
        Ok(())
    }

    /// `respondWorkspaceHookReview`：校验 → 落信任 → 收口 flow。
    pub fn respond(
        &self,
        target: &ReviewTarget,
        review_item_ids: &[String],
    ) -> Result<FlowOutcome, AdmissionError> {
        self.ensure_snapshot_matches(target)?;
        let policy_allows = self
            .policy
            .lock()
            .map(|policy| policy.mode == PolicyMode::UserDecides)
            .unwrap_or(false);
        if !policy_allows {
            let _ = self
                .lock_flow()
                .fail(target, "workspace_hooks_blocked_by_policy");
            return Err(AdmissionError::BlockedByPolicy);
        }
        let mut flow = self.lock_flow();
        let accepted = match flow.validate(target, review_item_ids) {
            ResolveResult::Accepted { review_item_ids } => review_item_ids,
            ResolveResult::Rejected { reason_code } => {
                return Err(AdmissionError::SnapshotMismatch(reason_code));
            }
        };
        let Some(snapshot) = self.snapshot() else {
            return Err(AdmissionError::SnapshotMismatch(REASON_SNAPSHOT_MISMATCH));
        };
        let records = create_trust_records(
            &snapshot.hooks,
            &accepted,
            &snapshot.workspace_identity,
            &snapshot.bundle_digest,
            &time_stamp(),
            None,
        )
        .map_err(AdmissionError::WriteFailed)?;
        if let Err(error) = self.store.upsert(records) {
            let _ = flow.fail(target, "workspace_hooks_trust_store_corrupt");
            return Err(AdmissionError::WriteFailed(error.message));
        }
        let resolved = flow.resolve(target, &accepted);
        drop(flow);
        let _ = self.reevaluate();
        match resolved {
            ResolveResult::Accepted { .. } => {
                Ok(self.lock_flow().take_outcome().unwrap_or(FlowOutcome {
                    action: super::flow::FlowAction::TrustSelected,
                    review_item_ids: accepted.clone(),
                    reason_code: None,
                }))
            }
            ResolveResult::Rejected { reason_code } => {
                Err(AdmissionError::SnapshotMismatch(reason_code))
            }
        }
    }

    /// `toggleWorkspaceHookReviewItem`：写回可编辑配置的 `enabled`，然后重建快照。
    pub fn toggle(
        &self,
        target: &ReviewTarget,
        review_item_id: &str,
        enabled: bool,
    ) -> Result<(), AdmissionError> {
        self.ensure_snapshot_matches(target)?;
        let snapshot = self.snapshot().ok_or(AdmissionError::SnapshotMismatch(
            "Workspace Hook bundle no longer exists",
        ))?;
        let config_path = self.workspace_path.join(".zcode").join("config.json");
        write_configured_toggle(&config_path, &snapshot, review_item_id, enabled).map_err(
            |error| match error {
                MutationError::ConfigUnreadable(message) => {
                    AdmissionError::ConfigUnreadable(SourceReadError {
                        path: config_path.clone(),
                        message,
                        reason_code: "workspace_hooks_config_unreadable",
                    })
                }
                MutationError::SnapshotMismatch(reason) => AdmissionError::SnapshotMismatch(reason),
                MutationError::WriteFailed(message) => AdmissionError::WriteFailed(message),
            },
        )?;
        self.refresh(&time_stamp())?;
        // 声明的 enabled 变了但摘要不变（开关不进摘要）：pending 评审可以继续；
        // 但 configuredEnabled 已变，界面要重建快照。flow 是否需要 supersede 由调用方
        // 根据 `flow_bundle_stale()` 决定（bundleDigest 不变 → 不需要）。
        Ok(())
    }

    /// `revokeWorkspaceHookTrust`：删记录 + 记撤销 + 重新评估。
    /// 返回 true 表示有 pending flow 的条目状态变了，调用方应 supersede 后重开。
    pub fn revoke(
        &self,
        workspace_identity: &str,
        hook_declaration_digests: &[String],
    ) -> Result<bool, AdmissionError> {
        if workspace_identity != self.workspace_identity {
            return Err(AdmissionError::SnapshotMismatch(REASON_SNAPSHOT_MISMATCH));
        }
        self.store
            .remove(workspace_identity, hook_declaration_digests)
            .map_err(|error| AdmissionError::WriteFailed(error.message))?;
        {
            let mut revoked = self
                .revoked
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for digest in hook_declaration_digests {
                revoked.insert(trust_key(workspace_identity, digest));
            }
        }
        let _ = self.reevaluate();
        Ok(self.flow_is_pending())
    }

    /// 到点收口（宿主 ticker 调用）。返回 outcome 供会话层落状态。
    pub fn expire(&self, now_ms: i64) -> Option<FlowOutcome> {
        self.lock_flow().expire(now_ms)
    }

    /// 主动关闭（用户切走面板）：`resolved` + `no_change`。
    pub fn close_review(&self, target: &ReviewTarget) -> bool {
        self.lock_flow().close_without_decision(target)
    }

    /// 没有待审项（例如已全部信任）时，把仍在 pending 的 flow 收口成 `no_change`。
    pub fn close_review_if_pending(&self) -> bool {
        let request = {
            let flow = self.lock_flow();
            if flow.state() != Some(FlowState::Pending) {
                return false;
            }
            flow.request().cloned()
        };
        let Some(request) = request else {
            return false;
        };
        let target = ReviewTarget {
            session_id: request.session_id,
            workspace_identity: request.workspace_identity,
            bundle_digest: request.bundle_digest,
            review_flow_id: request.review_flow_id,
            generation: request.generation,
            interaction_id: request.interaction_id,
        };
        self.lock_flow().close_without_decision(&target)
    }

    /// pending flow 的快照与当前快照是否已经不一致（需要 supersede 重开）。
    pub fn flow_bundle_stale(&self) -> bool {
        let Some(snapshot) = self.snapshot() else {
            return false;
        };
        let flow = self.lock_flow();
        flow.request()
            .is_some_and(|request| request.bundle_digest != snapshot.bundle_digest)
    }

    pub fn flow_is_pending(&self) -> bool {
        self.lock_flow().state() == Some(FlowState::Pending)
    }

    /// supersede 旧 flow 并换成新的请求（配置/撤销改变后重开评审）。
    pub fn supersede_review(
        &self,
        target: &ReviewTarget,
        session_id: &str,
        review_flow_id: &str,
        generation: u32,
        interaction_id: &str,
        now_ms: i64,
    ) -> Result<Value, AdmissionError> {
        let (replacement, payload) = self.build_review_request(
            session_id,
            review_flow_id,
            generation,
            interaction_id,
            now_ms,
        )?;
        self.lock_flow()
            .supersede(target, replacement)
            .map_err(|error| AdmissionError::WriteFailed(error.message))?;
        Ok(payload)
    }

    fn ensure_snapshot_matches(&self, target: &ReviewTarget) -> Result<(), AdmissionError> {
        let Some(snapshot) = self.snapshot() else {
            return Err(AdmissionError::SnapshotMismatch(REASON_SNAPSHOT_MISMATCH));
        };
        if target.session_id.is_empty() {
            return Err(AdmissionError::SnapshotMismatch(REASON_SNAPSHOT_MISMATCH));
        }
        if snapshot.workspace_identity != target.workspace_identity
            || snapshot.bundle_digest != target.bundle_digest
        {
            return Err(AdmissionError::SnapshotMismatch(REASON_SNAPSHOT_MISMATCH));
        }
        Ok(())
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_flow(&self) -> std::sync::MutexGuard<'_, ReviewFlowRegistry> {
        self.flow
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[derive(Debug, Clone)]
pub struct RequestReviewOutcome {
    pub reused: bool,
    pub payload: Option<Value>,
}

/// 评审 flow 入口需要的稳定身份（由会话层提供，避免在这里依赖命令类型）。
pub fn review_target_from_payload(value: &Value) -> Option<ReviewTarget> {
    Some(ReviewTarget {
        session_id: value.get("sessionId")?.as_str()?.to_string(),
        workspace_identity: value.get("workspaceIdentity")?.as_str()?.to_string(),
        bundle_digest: value.get("bundleDigest")?.as_str()?.to_string(),
        review_flow_id: value.get("reviewFlowId")?.as_str()?.to_string(),
        generation: u32::try_from(value.get("generation")?.as_u64()?).ok()?,
        interaction_id: value.get("interactionId")?.as_str()?.to_string(),
    })
}

fn display_source_path(base_dir: &str, canonical_path: &str) -> String {
    let base = Path::new(base_dir);
    let path = Path::new(canonical_path);
    path.strip_prefix(base)
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        .ok()
        .filter(|relative| !relative.is_empty())
        .unwrap_or_else(|| {
            path.file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| canonical_path.to_string())
        })
}

fn time_stamp() -> String {
    // grantedAt 是 ISO 字符串；本 host 不引 chrono，用毫秒时间戳的可读形式即可
    // （schema 只要求 string().datetime()——这里输出 RFC3339 UTC）。
    let now = crate::store::now_ms();
    rfc3339_utc(now)
}

/// 毫秒 → `YYYY-MM-DDTHH:MM:SS.sssZ`（纯整数运算，无时区数据库依赖）。
pub fn rfc3339_utc(now_ms: i64) -> String {
    let seconds = now_ms.div_euclid(1000);
    let millis = now_ms.rem_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = second_of_day / 3600;
    let minute = (second_of_day % 3600) / 60;
    let second = second_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Howard Hinnant 的 civil_from_days（与 JS `Date#toISOString` 同日历）。
fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_hooks::flow::{FlowAction, FlowState, REASON_INTERACTION_TIMEOUT};
    use crate::workspace_hooks::store::TRUST_STORE_FILE;
    use crate::workspace_hooks::trust::TrustState;
    use serde_json::json;

    const CONFIG: &str = r#"{
  "hooks": {
    "enabled": true,
    "events": {
      "PreToolUse": [
        {"matcher": "Bash", "hooks": [{"type": "command", "command": "node check.js"}]}
      ]
    }
  }
}
"#;

    struct Fixture {
        base: PathBuf,
        admission: WorkspaceHookAdmission,
    }

    fn fixture(tag: &str) -> Fixture {
        let base =
            std::env::temp_dir().join(format!("zcode-hook-admission-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join(".zcode")).unwrap();
        std::fs::write(base.join(".zcode").join("config.json"), CONFIG).unwrap();
        let store = Arc::new(TrustStore::new(
            base.join("security").join(TRUST_STORE_FILE),
        ));
        let admission = WorkspaceHookAdmission::new(
            base.clone(),
            "ws-admission".to_string(),
            "admission".to_string(),
            store,
        );
        admission
            .refresh("2026-01-01T00:00:00.000Z")
            .expect("refresh");
        Fixture { base, admission }
    }

    fn review_target(request: &FlowRequest) -> ReviewTarget {
        ReviewTarget {
            session_id: request.session_id.clone(),
            workspace_identity: request.workspace_identity.clone(),
            bundle_digest: request.bundle_digest.clone(),
            review_flow_id: request.review_flow_id.clone(),
            generation: request.generation,
            interaction_id: request.interaction_id.clone(),
        }
    }

    #[test]
    fn pending_admission_projects_state_and_review_payload() {
        let fixture = fixture("pending");
        let evaluation = fixture.admission.evaluation().expect("evaluation");
        assert_eq!(evaluation.pending_count(), 1);
        assert_eq!(evaluation.items[0].trust_state, TrustState::PendingTrust);

        let state = fixture
            .admission
            .admission_state()
            .expect("admission state");
        assert_eq!(state["pendingCount"], json!(1));
        assert_eq!(state["workspaceIdentity"], json!("ws-admission"));
        assert!(state["bundleDigest"].as_str().unwrap().len() == 64);

        let (request, payload) = fixture
            .admission
            .build_review_request("session-1", "flow-1", 1, "interaction-1", 1_000)
            .expect("payload");
        assert_eq!(payload["kind"], json!("workspaceHookReview"));
        assert_eq!(payload["summary"]["hookCount"], json!(1));
        assert_eq!(payload["summary"]["eventCount"], json!(1));
        assert_eq!(payload["summary"]["pendingCount"], json!(1));
        assert_eq!(
            payload["warningCode"],
            json!("workspace_hooks_execute_code")
        );
        assert_eq!(
            payload["items"][0]["displayName"],
            json!("PreToolUse · Bash")
        );
        assert_eq!(
            payload["items"][0]["displayCommand"],
            json!("node check.js")
        );
        assert_eq!(payload["items"][0]["executionMode"], json!("foreground"));
        assert_eq!(payload["items"][0]["trustState"], json!("pending_trust"));
        assert_eq!(payload["deadlineAt"], json!(review_deadline(1_000)));
        assert_eq!(request.item_ids.len(), 1);

        let _ = std::fs::remove_dir_all(&fixture.base);
    }

    #[test]
    fn respond_grants_trust_persists_it_and_clears_the_admission_banner() {
        let fixture = fixture("respond");
        let (request, _) = fixture
            .admission
            .build_review_request("session-1", "flow-1", 1, "interaction-1", 1_000)
            .expect("payload");
        let target = review_target(&request);
        fixture
            .admission
            .lock_flow()
            .open(request.clone())
            .expect("open");

        let outcome = fixture
            .admission
            .respond(&target, std::slice::from_ref(&request.item_ids[0]))
            .expect("respond");
        assert_eq!(outcome.action, FlowAction::TrustSelected);
        assert_eq!(outcome.review_item_ids.len(), 1);
        assert_eq!(fixture.admission.flow_state(), Some(FlowState::Resolved));
        let evaluation = fixture.admission.evaluation().expect("evaluation");
        assert_eq!(evaluation.pending_count(), 0);
        assert!(evaluation.items[0].effective_runnable);
        assert!(
            fixture.admission.admission_state().is_none(),
            "pending 清零后投影必须是 null"
        );

        // 台账落盘（重启后仍然 trusted）。
        let reloaded = TrustStore::new(fixture.base.join("security").join(TRUST_STORE_FILE));
        let (status, records) = reloaded.records();
        assert_eq!(status, TrustStoreStatus::Ok);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].workspace_identity, "ws-admission");

        let _ = std::fs::remove_dir_all(&fixture.base);
    }

    #[test]
    fn stale_generation_or_unknown_items_are_rejected_without_writing() {
        let fixture = fixture("stale");
        let (request, _) = fixture
            .admission
            .build_review_request("session-1", "flow-1", 1, "interaction-1", 1_000)
            .unwrap();
        fixture.admission.lock_flow().open(request.clone()).unwrap();
        let mut stale = review_target(&request);
        stale.generation = 2;
        let error = fixture
            .admission
            .respond(&stale, std::slice::from_ref(&request.item_ids[0]))
            .unwrap_err();
        assert_eq!(error.reason_code(), "workspace_hooks_snapshot_mismatch");
        let error = fixture
            .admission
            .respond(&review_target(&request), &["nope".to_string()])
            .unwrap_err();
        assert_eq!(error.reason_code(), "workspace_hooks_snapshot_mismatch");
        let store = TrustStore::new(fixture.base.join("security").join(TRUST_STORE_FILE));
        assert!(store.records().1.is_empty(), "拒绝的应答不得写台账");
        let _ = std::fs::remove_dir_all(&fixture.base);
    }

    #[test]
    fn revoke_removes_the_record_and_marks_the_item_revoked() {
        let fixture = fixture("revoke");
        let (request, _) = fixture
            .admission
            .build_review_request("session-1", "flow-1", 1, "interaction-1", 1_000)
            .unwrap();
        let target = review_target(&request);
        fixture.admission.lock_flow().open(request.clone()).unwrap();
        fixture
            .admission
            .respond(&target, std::slice::from_ref(&request.item_ids[0]))
            .expect("respond");
        let digest = fixture.admission.snapshot().unwrap().hooks[0]
            .hook_declaration_digest
            .clone();
        let pending = fixture
            .admission
            .revoke("ws-admission", std::slice::from_ref(&digest))
            .expect("revoke");
        assert!(!pending, "没有 pending flow，不需要 supersede");
        let evaluation = fixture.admission.evaluation().unwrap();
        assert_eq!(evaluation.items[0].trust_state, TrustState::Revoked);
        assert_eq!(evaluation.pending_count(), 1);
        let store = TrustStore::new(fixture.base.join("security").join(TRUST_STORE_FILE));
        assert!(store.records().1.is_empty());
        let _ = std::fs::remove_dir_all(&fixture.base);
    }

    #[test]
    fn policy_deny_blocks_both_review_and_grant() {
        let fixture = fixture("policy");
        fixture.admission.set_policy(HookPolicy {
            mode: PolicyMode::Deny,
            reason: Some("managed".into()),
            policy_revision: "r1".into(),
        });
        let evaluation = fixture.admission.evaluation().unwrap();
        assert_eq!(evaluation.items[0].trust_state, TrustState::BlockedPolicy);
        assert_eq!(evaluation.pending_count(), 0);
        let error = fixture
            .admission
            .request_review("session-1", "flow-1", 1, "interaction-1", 1_000)
            .unwrap_err();
        assert_eq!(error.reason_code(), "workspace_hooks_blocked_by_policy");

        let (request, _) = {
            fixture.admission.set_policy(HookPolicy::user_decides());
            fixture
                .admission
                .build_review_request("session-1", "flow-1", 1, "interaction-1", 1_000)
                .unwrap()
        };
        let target = review_target(&request);
        fixture.admission.lock_flow().open(request.clone()).unwrap();
        fixture.admission.set_policy(HookPolicy {
            mode: PolicyMode::Deny,
            reason: Some("managed".into()),
            policy_revision: "r1".into(),
        });
        let error = fixture
            .admission
            .respond(&target, &request.item_ids)
            .unwrap_err();
        assert_eq!(error.reason_code(), "workspace_hooks_blocked_by_policy");
        let store = TrustStore::new(fixture.base.join("security").join(TRUST_STORE_FILE));
        assert!(store.records().1.is_empty());
        let _ = std::fs::remove_dir_all(&fixture.base);
    }

    #[test]
    fn expire_settles_the_flow_without_granting() {
        let fixture = fixture("expire");
        let (request, _) = fixture
            .admission
            .build_review_request("session-1", "flow-1", 1, "interaction-1", 1_000)
            .unwrap();
        fixture.admission.lock_flow().open(request.clone()).unwrap();
        assert!(fixture.admission.expire(1_001).is_none());
        let outcome = fixture
            .admission
            .expire(request.deadline_at)
            .expect("timeout");
        assert_eq!(outcome.reason_code, Some(REASON_INTERACTION_TIMEOUT));
        assert_eq!(fixture.admission.flow_state(), Some(FlowState::TimedOut));
        let store = TrustStore::new(fixture.base.join("security").join(TRUST_STORE_FILE));
        assert!(store.records().1.is_empty());
        let _ = std::fs::remove_dir_all(&fixture.base);
    }

    #[test]
    fn toggle_writes_the_config_and_refreshes_the_snapshot() {
        let fixture = fixture("toggle");
        let (request, _) = fixture
            .admission
            .build_review_request("session-1", "flow-1", 1, "interaction-1", 1_000)
            .unwrap();
        let target = review_target(&request);
        fixture.admission.lock_flow().open(request.clone()).unwrap();
        fixture
            .admission
            .toggle(&target, &request.item_ids[0], false)
            .expect("toggle");
        let evaluation = fixture.admission.evaluation().unwrap();
        assert!(!evaluation.items[0].effective_runnable);
        assert_eq!(
            evaluation.items[0].trust_state,
            TrustState::PendingTrust,
            "关掉开关不改变信任状态"
        );
        let config: Value = serde_json::from_str(
            &std::fs::read_to_string(fixture.base.join(".zcode").join("config.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            config["hooks"]["events"]["PreToolUse"][0]["hooks"][0]["enabled"],
            json!(false)
        );
        // enabled 是 bundle 摘要的输入（declarationEnabled/configuredEnabled），
        // 所以开关一变，pending flow 的快照就过期了——调用方必须 supersede 重开评审，
        // 否则用户对旧 bundle 的决定会落到新 bundle 上（TS 的 refreshPendingFlow 同理）。
        assert!(fixture.admission.flow_bundle_stale());
        let mut stale_target = review_target(&request);
        let generation = request.generation + 1;
        let payload = fixture
            .admission
            .supersede_review(
                &stale_target,
                "session-1",
                "flow-1",
                generation,
                "interaction-2",
                request.created_at + 1,
            )
            .expect("supersede");
        assert_eq!(payload["generation"], json!(generation));
        assert_eq!(payload["items"][0]["configuredEnabled"], json!(false));
        assert_eq!(payload["items"][0]["trustState"], json!("pending_trust"));
        assert!(!fixture.admission.flow_bundle_stale());
        // 旧 generation 的应答被拒绝。
        stale_target.generation = request.generation;
        let error = fixture
            .admission
            .respond(&stale_target, &request.item_ids)
            .unwrap_err();
        assert_eq!(error.reason_code(), "workspace_hooks_snapshot_mismatch");
        let _ = std::fs::remove_dir_all(&fixture.base);
    }

    #[test]
    fn rfc3339_matches_the_javascript_to_iso_string_shape() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339_utc(1_700_000_000_000), "2023-11-14T22:13:20.000Z");
        assert_eq!(rfc3339_utc(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        // 闰日边界。
        assert_eq!(rfc3339_utc(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }
}
