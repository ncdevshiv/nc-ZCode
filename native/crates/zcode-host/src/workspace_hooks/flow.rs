//! 评审 flow 的状态机（`core/src/hooks/workspace-hook-review-flow.ts` 的 Rust 版）。
//!
//! 一次评审的生命周期：`pending` → `resolved` / `cancelled` / `superseded` /
//! `timed_out`。生成号（generation）是防伪关键：**同一 flow 的每次重开都必须推进
//! generation**，旧 generation 的应答一律 `workspace_hooks_review_superseded`——
//! 否则用户点"信任"时可能授权到一份已经被改过的声明。
//!
//! 超时不在这里起定时器：宿主持有唯一 ticker，调用 [`ReviewFlowRegistry::expire`]
//! 保证"截止时间到"只在一处判定。TS 用 setTimeout 是因为每个 flow 独立计时；
//! 本 host 把它做成纯函数便于测试，也不会出现"定时器在会话删除后还在跑"。
//!
//! 与 TS 的差异（记录在 README）：TS 的 snapshot 身份还包含 taskId/runId/
//! remoteSessionId。本 host 没有这三层任务身份，只校验 sessionId / workspaceIdentity /
//! bundleDigest；命令 payload 仍按协议接收这些字段（忽略而不拒绝）。

use super::trust::WORKSPACE_HOOK_REVIEW_TIMEOUT_MS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowState {
    Pending,
    Resolved,
    Superseded,
    Cancelled,
    TimedOut,
}

impl FlowState {
    pub fn as_str(self) -> &'static str {
        match self {
            FlowState::Pending => "pending",
            FlowState::Resolved => "resolved",
            FlowState::Superseded => "superseded",
            FlowState::Cancelled => "cancelled",
            FlowState::TimedOut => "timed_out",
        }
    }
}

/// 一次评审请求的不可变身份 + 内容索引。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowRequest {
    pub review_flow_id: String,
    pub generation: u32,
    pub interaction_id: String,
    pub session_id: String,
    pub workspace_identity: String,
    pub bundle_digest: String,
    pub item_ids: Vec<String>,
    pub created_at: i64,
    pub deadline_at: i64,
}

impl FlowRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.review_flow_id.trim().is_empty()
            || self.interaction_id.trim().is_empty()
            || self.session_id.trim().is_empty()
            || self.workspace_identity.trim().is_empty()
        {
            return Err("workspace_hooks_snapshot_mismatch");
        }
        if self.generation == 0 {
            return Err("workspace_hooks_snapshot_mismatch");
        }
        if !is_sha256(&self.bundle_digest) {
            return Err("workspace_hooks_snapshot_mismatch");
        }
        if self.deadline_at < self.created_at {
            return Err("workspace_hooks_snapshot_mismatch");
        }
        let mut seen = std::collections::HashSet::new();
        if self.item_ids.iter().any(|id| !seen.insert(id)) {
            return Err("workspace_hooks_snapshot_mismatch");
        }
        Ok(())
    }
}

pub fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// 命令侧的目标身份（`workspaceHookReviewCommandTargetSchema` 的本 host 子集）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewTarget {
    pub session_id: String,
    pub workspace_identity: String,
    pub bundle_digest: String,
    pub review_flow_id: String,
    pub generation: u32,
    pub interaction_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowAction {
    TrustSelected,
    NoChange,
}

impl FlowAction {
    pub fn as_str(self) -> &'static str {
        match self {
            FlowAction::TrustSelected => "trust_selected",
            FlowAction::NoChange => "no_change",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowOutcome {
    pub action: FlowAction,
    pub review_item_ids: Vec<String>,
    pub reason_code: Option<&'static str>,
}

impl FlowOutcome {
    fn no_change(reason_code: Option<&'static str>) -> Self {
        Self {
            action: FlowAction::NoChange,
            review_item_ids: Vec::new(),
            reason_code,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveResult {
    Accepted { review_item_ids: Vec<String> },
    Rejected { reason_code: &'static str },
}

pub const REASON_REVIEW_SUPERSEDED: &str = "workspace_hooks_review_superseded";
pub const REASON_SNAPSHOT_MISMATCH: &str = "workspace_hooks_snapshot_mismatch";
pub const REASON_INTERACTION_TIMEOUT: &str = "workspace_hooks_interaction_timeout";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenOutcome {
    Opened,
    /// 同一 generation 的重复请求：幂等复用，不新建 flow。
    Reused,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowError {
    pub message: String,
}

struct CurrentFlow {
    request: FlowRequest,
    state: FlowState,
    superseded_by: Option<String>,
    outcome: Option<FlowOutcome>,
}

/// 单会话的评审 flow 注册表。
#[derive(Default)]
pub struct ReviewFlowRegistry {
    current: Option<CurrentFlow>,
}

impl ReviewFlowRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 目标是否命中当前 flow 的 generation（不检查 pending 状态）。
    pub fn generation_matches(&self, target: &ReviewTarget) -> bool {
        self.current
            .as_ref()
            .is_some_and(|current| matches_generation(&current.request, target))
    }

    pub fn state(&self) -> Option<FlowState> {
        self.current.as_ref().map(|flow| flow.state)
    }

    pub fn request(&self) -> Option<&FlowRequest> {
        self.current.as_ref().map(|flow| &flow.request)
    }

    pub fn superseded_by(&self) -> Option<&str> {
        self.current
            .as_ref()
            .and_then(|flow| flow.superseded_by.as_deref())
    }

    /// 打开（或幂等复用）一个 pending flow。
    pub fn open(&mut self, request: FlowRequest) -> Result<OpenOutcome, FlowError> {
        request.validate().map_err(|reason| FlowError {
            message: format!("invalid workspace hook review request: {reason}"),
        })?;
        if let Some(current) = &self.current {
            if current.state == FlowState::Pending {
                if same_generation(&current.request, &request) {
                    return Ok(OpenOutcome::Reused);
                }
                return Err(FlowError {
                    message: "A pending Workspace Hook review must be superseded explicitly"
                        .to_string(),
                });
            }
        }
        self.current = Some(CurrentFlow {
            request,
            state: FlowState::Pending,
            superseded_by: None,
            outcome: None,
        });
        Ok(OpenOutcome::Opened)
    }

    /// 显式替换：旧 flow `superseded`，新 flow 接管（generation 必须前进）。
    pub fn supersede(
        &mut self,
        target: &ReviewTarget,
        replacement: FlowRequest,
    ) -> Result<(), FlowError> {
        replacement.validate().map_err(|reason| FlowError {
            message: format!("invalid replacement request: {reason}"),
        })?;
        let Some(current) = &self.current else {
            return Err(FlowError {
                message: "Cannot supersede an unknown Workspace Hook review generation".into(),
            });
        };
        if !matches_generation(&current.request, target) {
            return Err(FlowError {
                message: "Cannot supersede an unknown Workspace Hook review generation".into(),
            });
        }
        if replacement.session_id != current.request.session_id
            || replacement.review_flow_id != current.request.review_flow_id
            || replacement.generation <= current.request.generation
        {
            return Err(FlowError {
                message: "Replacement must advance the same Workspace Hook review flow".into(),
            });
        }
        self.settle(
            FlowState::Superseded,
            Some(replacement.interaction_id.clone()),
            FlowOutcome::no_change(Some(REASON_REVIEW_SUPERSEDED)),
        );
        self.current = Some(CurrentFlow {
            request: replacement,
            state: FlowState::Pending,
            superseded_by: None,
            outcome: None,
        });
        Ok(())
    }

    /// 只校验不改状态（命令在真正落盘授权前先确认这次应答仍然有效）。
    pub fn validate(&self, target: &ReviewTarget, review_item_ids: &[String]) -> ResolveResult {
        let Some(current) = &self.current else {
            return ResolveResult::Rejected {
                reason_code: REASON_REVIEW_SUPERSEDED,
            };
        };
        if !matches_generation(&current.request, target) || current.state != FlowState::Pending {
            return ResolveResult::Rejected {
                reason_code: REASON_REVIEW_SUPERSEDED,
            };
        }
        if !matches_snapshot(&current.request, target) {
            return ResolveResult::Rejected {
                reason_code: REASON_SNAPSHOT_MISMATCH,
            };
        }
        let known: std::collections::HashSet<&str> = current
            .request
            .item_ids
            .iter()
            .map(String::as_str)
            .collect();
        let unique = unique_ids(review_item_ids);
        if unique.iter().any(|id| !known.contains(id.as_str())) {
            return ResolveResult::Rejected {
                reason_code: REASON_SNAPSHOT_MISMATCH,
            };
        }
        if unique.is_empty() {
            return ResolveResult::Rejected {
                reason_code: REASON_SNAPSHOT_MISMATCH,
            };
        }
        ResolveResult::Accepted {
            review_item_ids: unique,
        }
    }

    /// 用户应答：校验通过后 flow 落成 `resolved` 并产出可观测的 outcome。
    pub fn resolve(&mut self, target: &ReviewTarget, review_item_ids: &[String]) -> ResolveResult {
        let validation = self.validate(target, review_item_ids);
        let ResolveResult::Accepted {
            review_item_ids: accepted,
        } = &validation
        else {
            return validation;
        };
        let outcome = FlowOutcome {
            action: FlowAction::TrustSelected,
            review_item_ids: accepted.clone(),
            reason_code: None,
        };
        self.settle(FlowState::Resolved, None, outcome);
        validation
    }

    /// 失败（写盘失败/策略拒绝等）：flow 落成 `cancelled`，reasonCode 透传。
    pub fn fail(&mut self, target: &ReviewTarget, reason_code: &'static str) -> bool {
        if !self.flow_matches_pending(target) {
            return false;
        }
        self.settle(
            FlowState::Cancelled,
            None,
            FlowOutcome::no_change(Some(reason_code)),
        );
        true
    }

    /// 关闭但保留决定权（用户取消面板/切走）：`resolved` + `no_change`。
    pub fn close_without_decision(&mut self, target: &ReviewTarget) -> bool {
        if !self.flow_matches_pending(target) {
            return false;
        }
        self.settle(FlowState::Resolved, None, FlowOutcome::no_change(None));
        true
    }

    /// 截止时间到：`timed_out` + `workspace_hooks_interaction_timeout`。
    pub fn expire(&mut self, now_ms: i64) -> Option<FlowOutcome> {
        let current = self.current.as_ref()?;
        if current.state != FlowState::Pending || now_ms < current.request.deadline_at {
            return None;
        }
        self.settle(
            FlowState::TimedOut,
            None,
            FlowOutcome::no_change(Some(REASON_INTERACTION_TIMEOUT)),
        );
        self.current.as_ref()?.outcome.clone()
    }

    /// 取走 outcome（宿主把它交给监管逻辑：落信任 or 只记 telemetry）。
    pub fn take_outcome(&mut self) -> Option<FlowOutcome> {
        self.current.as_mut()?.outcome.take()
    }

    pub fn outcome(&self) -> Option<&FlowOutcome> {
        self.current.as_ref()?.outcome.as_ref()
    }

    fn flow_matches_pending(&self, target: &ReviewTarget) -> bool {
        self.current.as_ref().is_some_and(|current| {
            current.state == FlowState::Pending
                && matches_generation(&current.request, target)
                && matches_snapshot(&current.request, target)
        })
    }

    fn settle(&mut self, state: FlowState, superseded_by: Option<String>, outcome: FlowOutcome) {
        let Some(current) = self.current.as_mut() else {
            return;
        };
        if current.state != FlowState::Pending {
            return;
        }
        current.state = state;
        current.superseded_by = superseded_by;
        current.outcome = Some(outcome);
    }
}

fn same_generation(left: &FlowRequest, right: &FlowRequest) -> bool {
    left.review_flow_id == right.review_flow_id
        && left.generation == right.generation
        && left.interaction_id == right.interaction_id
        && left.bundle_digest == right.bundle_digest
}

fn matches_generation(request: &FlowRequest, target: &ReviewTarget) -> bool {
    request.review_flow_id == target.review_flow_id
        && request.generation == target.generation
        && request.interaction_id == target.interaction_id
}

fn matches_snapshot(request: &FlowRequest, target: &ReviewTarget) -> bool {
    request.session_id == target.session_id
        && request.workspace_identity == target.workspace_identity
        && request.bundle_digest == target.bundle_digest
}

fn unique_ids(ids: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    ids.iter()
        .filter(|id| seen.insert((*id).clone()))
        .cloned()
        .collect()
}

/// 评审默认有效期与 TS 一致（10 分钟）。
pub fn review_deadline(created_at_ms: i64) -> i64 {
    created_at_ms.saturating_add(WORKSPACE_HOOK_REVIEW_TIMEOUT_MS as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(generation: u32, interaction: &str, digest_char: char) -> FlowRequest {
        FlowRequest {
            review_flow_id: "flow-1".into(),
            generation,
            interaction_id: interaction.into(),
            session_id: "session-1".into(),
            workspace_identity: "ws-1".into(),
            bundle_digest: digest_char.to_string().repeat(64),
            item_ids: vec![
                "workspace-hook-0-Stop-0-0".into(),
                "workspace-hook-0-Stop-0-1".into(),
            ],
            created_at: 1_000,
            deadline_at: review_deadline(1_000),
        }
    }

    fn target(generation: u32, interaction: &str, digest_char: char) -> ReviewTarget {
        ReviewTarget {
            session_id: "session-1".into(),
            workspace_identity: "ws-1".into(),
            bundle_digest: digest_char.to_string().repeat(64),
            review_flow_id: "flow-1".into(),
            generation,
            interaction_id: interaction.into(),
        }
    }

    #[test]
    fn open_reuses_the_same_generation_and_refuses_to_silently_replace_it() {
        let mut registry = ReviewFlowRegistry::new();
        assert_eq!(
            registry.open(request(1, "i1", 'a')).unwrap(),
            OpenOutcome::Opened
        );
        assert_eq!(
            registry.open(request(1, "i1", 'a')).unwrap(),
            OpenOutcome::Reused
        );
        let error = registry
            .open(request(2, "i2", 'a'))
            .expect_err("a different generation needs an explicit supersede");
        assert!(error.message.contains("superseded explicitly"));
        assert_eq!(registry.state(), Some(FlowState::Pending));
    }

    #[test]
    fn supersede_settles_the_old_flow_and_advances_the_generation() {
        let mut registry = ReviewFlowRegistry::new();
        registry.open(request(1, "i1", 'a')).unwrap();
        registry
            .supersede(&target(1, "i1", 'a'), request(2, "i2", 'b'))
            .expect("supersede");
        assert_eq!(registry.state(), Some(FlowState::Pending));
        assert_eq!(registry.request().unwrap().generation, 2);
        assert_eq!(
            registry.take_outcome(),
            None,
            "新 flow 还没有 outcome；旧 flow 的 outcome 已被替换"
        );
        // 旧 generation 的应答一律 superseded。
        match registry.resolve(&target(1, "i1", 'a'), &["workspace-hook-0-Stop-0-0".into()]) {
            ResolveResult::Rejected { reason_code } => {
                assert_eq!(reason_code, REASON_REVIEW_SUPERSEDED)
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn supersede_requires_an_advancing_generation_in_the_same_flow() {
        let mut registry = ReviewFlowRegistry::new();
        registry.open(request(1, "i1", 'a')).unwrap();
        let mut replacement = request(1, "i2", 'b');
        replacement.generation = 1;
        assert!(registry
            .supersede(&target(1, "i1", 'a'), replacement)
            .is_err());
        let mut different_flow = request(2, "i2", 'b');
        different_flow.review_flow_id = "flow-2".into();
        assert!(registry
            .supersede(&target(1, "i1", 'a'), different_flow)
            .is_err());
    }

    #[test]
    fn validation_rejects_stale_generations_snapshot_mismatches_and_unknown_items() {
        let mut registry = ReviewFlowRegistry::new();
        registry.open(request(1, "i1", 'a')).unwrap();
        let good = vec!["workspace-hook-0-Stop-0-0".to_string()];

        assert!(matches!(
            registry.validate(&target(1, "i1", 'a'), &good),
            ResolveResult::Accepted { .. }
        ));
        assert_eq!(
            registry.validate(&target(2, "i1", 'a'), &good),
            ResolveResult::Rejected {
                reason_code: REASON_REVIEW_SUPERSEDED
            }
        );
        assert_eq!(
            registry.validate(&target(1, "i1", 'b'), &good),
            ResolveResult::Rejected {
                reason_code: REASON_SNAPSHOT_MISMATCH
            }
        );
        assert_eq!(
            registry.validate(
                &target(1, "i1", 'a'),
                &["workspace-hook-9-unknown".to_string()]
            ),
            ResolveResult::Rejected {
                reason_code: REASON_SNAPSHOT_MISMATCH
            }
        );
        assert_eq!(
            registry.validate(&target(1, "i1", 'a'), &[]),
            ResolveResult::Rejected {
                reason_code: REASON_SNAPSHOT_MISMATCH
            },
            "空决定不能当作信任全部"
        );
    }

    #[test]
    fn resolve_dedupes_ids_and_settles_the_flow_once() {
        let mut registry = ReviewFlowRegistry::new();
        registry.open(request(1, "i1", 'a')).unwrap();
        let decision = vec![
            "workspace-hook-0-Stop-0-1".to_string(),
            "workspace-hook-0-Stop-0-1".to_string(),
            "workspace-hook-0-Stop-0-0".to_string(),
        ];
        let result = registry.resolve(&target(1, "i1", 'a'), &decision);
        assert_eq!(
            result,
            ResolveResult::Accepted {
                review_item_ids: vec![
                    "workspace-hook-0-Stop-0-1".to_string(),
                    "workspace-hook-0-Stop-0-0".to_string()
                ]
            }
        );
        assert_eq!(registry.state(), Some(FlowState::Resolved));
        let outcome = registry.take_outcome().expect("outcome");
        assert_eq!(outcome.action, FlowAction::TrustSelected);
        assert_eq!(outcome.review_item_ids.len(), 2);
        assert_eq!(registry.take_outcome(), None, "outcome 只能被取一次");
        // 第二次应答不再被接受。
        assert_eq!(
            registry.resolve(&target(1, "i1", 'a'), &decision),
            ResolveResult::Rejected {
                reason_code: REASON_REVIEW_SUPERSEDED
            }
        );
        assert_eq!(registry.state(), Some(FlowState::Resolved), "状态不被覆盖");
    }

    #[test]
    fn failure_and_timeout_land_as_no_change_with_typed_reasons() {
        let mut registry = ReviewFlowRegistry::new();
        registry.open(request(1, "i1", 'a')).unwrap();
        assert!(registry.fail(&target(1, "i1", 'a'), "workspace_hooks_config_write_failed"));
        assert_eq!(registry.state(), Some(FlowState::Cancelled));
        let outcome = registry.take_outcome().unwrap();
        assert_eq!(outcome.action, FlowAction::NoChange);
        assert_eq!(
            outcome.reason_code,
            Some("workspace_hooks_config_write_failed")
        );

        let mut registry = ReviewFlowRegistry::new();
        registry.open(request(1, "i1", 'a')).unwrap();
        assert!(registry.expire(1_000 + 60_000).is_none(), "截止前不收口");
        assert_eq!(registry.state(), Some(FlowState::Pending));
        let outcome = registry
            .expire(review_deadline(1_000))
            .expect("deadline triggers timeout");
        assert_eq!(outcome.reason_code, Some(REASON_INTERACTION_TIMEOUT));
        assert_eq!(registry.state(), Some(FlowState::TimedOut));
    }

    #[test]
    fn close_without_decision_keeps_no_reason() {
        let mut registry = ReviewFlowRegistry::new();
        registry.open(request(1, "i1", 'a')).unwrap();
        assert!(registry.close_without_decision(&target(1, "i1", 'a')));
        let outcome = registry.take_outcome().unwrap();
        assert_eq!(outcome.action, FlowAction::NoChange);
        assert_eq!(outcome.reason_code, None);
        assert_eq!(registry.state(), Some(FlowState::Resolved));
    }
}
