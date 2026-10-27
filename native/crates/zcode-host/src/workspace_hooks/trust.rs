//! 信任状态机与信任台账（`workspace-hook-trust-evaluation.ts` +
//! `workspace-hook-trust-store-file.ts` 的 Rust 版）。
//!
//! 信任存储是**权限边界**：同一份 `workspace-hook-trust-v1.json`，runtime、CLI、UI
//! 必须得出同一个结论。因此这里的解析是严格的（未知字段 = 文件无效 → fail-closed
//! 全部阻断），不做"尽量读出什么算什么"的宽容解析。
//!
//! 状态机（与 TS 逐条一致）：
//! - policy.deny → blocked_policy（最高优先级）
//! - 存储损坏 → blocked_untrusted（不信任任何记录）
//! - allow_trusted_only → 有记录才 trusted_persistent，否则 blocked_policy
//! - 有记录 → trusted_persistent
//! - 无记录但被撤销过 → revoked
//! - 无记录但同一"槽位"（事件/源/下标/matcher）有旧摘要的授权 → stale_digest
//! - 其余 → pending_trust

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::config::HookEventName;
use super::digest::CanonicalEntry;

pub const WORKSPACE_HOOK_TRUST_STORE_SCHEMA_VERSION: u32 = 1;
pub const WORKSPACE_HOOK_REVIEW_TIMEOUT_MS: u64 = 10 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustState {
    NotApplicable,
    PendingTrust,
    TrustedPersistent,
    BlockedUntrusted,
    BlockedPolicy,
    Revoked,
    StaleDigest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionClass {
    NotApplicable,
    Admitted,
    Pending,
    Blocked,
}

impl AdmissionClass {
    pub fn as_str(self) -> &'static str {
        match self {
            AdmissionClass::NotApplicable => "not_applicable",
            AdmissionClass::Admitted => "admitted",
            AdmissionClass::Pending => "pending",
            AdmissionClass::Blocked => "blocked",
        }
    }
}

impl TrustState {
    pub fn as_str(self) -> &'static str {
        match self {
            TrustState::NotApplicable => "not_applicable",
            TrustState::PendingTrust => "pending_trust",
            TrustState::TrustedPersistent => "trusted_persistent",
            TrustState::BlockedUntrusted => "blocked_untrusted",
            TrustState::BlockedPolicy => "blocked_policy",
            TrustState::Revoked => "revoked",
            TrustState::StaleDigest => "stale_digest",
        }
    }

    /// `WORKSPACE_HOOK_STATE_ADMISSION_MAP`。
    pub fn admission(self) -> (AdmissionClass, bool, &'static str) {
        match self {
            TrustState::NotApplicable => (
                AdmissionClass::NotApplicable,
                false,
                "workspace_hooks_not_applicable",
            ),
            TrustState::PendingTrust => (
                AdmissionClass::Pending,
                false,
                "workspace_hooks_pending_trust",
            ),
            TrustState::TrustedPersistent => (
                AdmissionClass::Admitted,
                true,
                "workspace_hooks_trusted_persistent",
            ),
            TrustState::BlockedUntrusted => (
                AdmissionClass::Blocked,
                false,
                "workspace_hooks_blocked_untrusted",
            ),
            TrustState::BlockedPolicy => (
                AdmissionClass::Blocked,
                false,
                "workspace_hooks_blocked_by_policy",
            ),
            TrustState::Revoked => (AdmissionClass::Pending, false, "workspace_hooks_revoked"),
            TrustState::StaleDigest => (
                AdmissionClass::Pending,
                false,
                "workspace_hook_declaration_changed",
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyMode {
    Deny,
    UserDecides,
    AllowTrustedOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookPolicy {
    pub mode: PolicyMode,
    pub reason: Option<String>,
    pub policy_revision: String,
}

impl HookPolicy {
    pub fn user_decides() -> Self {
        Self {
            mode: PolicyMode::UserDecides,
            reason: None,
            policy_revision: "builtin:user-decides:v1".to_string(),
        }
    }

    /// `workspaceHookPolicySchema`：structured 模式严格（未知键 = 无效）。
    pub fn parse(value: &Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or_else(|| "policy must be an object".to_string())?;
        let mode = object
            .get("mode")
            .and_then(Value::as_str)
            .ok_or_else(|| "policy.mode: required".to_string())?;
        let policy_revision = object
            .get("policyRevision")
            .and_then(Value::as_str)
            .filter(|revision| !revision.trim().is_empty())
            .ok_or_else(|| "policy.policyRevision: required".to_string())?
            .to_string();
        let reason = match object.get("reason") {
            None => None,
            Some(Value::String(reason)) if !reason.trim().is_empty() => Some(reason.clone()),
            Some(_) => return Err("policy.reason: must be a non-empty string".to_string()),
        };
        let allowed: &[&str] = match mode {
            "deny" => &["mode", "reason", "policyRevision"],
            "user_decides" => &["mode", "policyRevision"],
            "allow_trusted_only" => &["mode", "reason", "policyRevision"],
            other => return Err(format!("policy.mode: unsupported {other}")),
        };
        for key in object.keys() {
            if !allowed.contains(&key.as_str()) {
                return Err(format!("policy: unknown field {key}"));
            }
        }
        if mode == "deny" && reason.is_none() {
            return Err("policy.reason: required for deny".to_string());
        }
        Ok(Self {
            mode: match mode {
                "deny" => PolicyMode::Deny,
                "allow_trusted_only" => PolicyMode::AllowTrustedOnly,
                _ => PolicyMode::UserDecides,
            },
            reason,
            policy_revision,
        })
    }
}

/// 一条授权记录（`workspaceHookTrustRecordSchema`；strict）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrustRecord {
    pub workspace_identity: String,
    pub hook_declaration_digest: String,
    pub digest_algorithm: String,
    pub decision: String,
    pub granted_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_digest_at_grant: Option<String>,
    pub event_at_grant: HookEventName,
    pub display_command_at_grant: String,
    pub source_path_at_grant: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_discovery_order_at_grant: Option<usize>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_nullable_string"
    )]
    pub matcher_at_grant: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher_index_at_grant: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_index_at_grant: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_version_at_grant: Option<String>,
}

impl TrustRecord {
    pub fn validate(&self) -> Result<(), String> {
        if self.workspace_identity.trim().is_empty() {
            return Err("record.workspaceIdentity: must not be empty".into());
        }
        if !is_sha256(&self.hook_declaration_digest) {
            return Err("record.hookDeclarationDigest: expected 64 lowercase hex chars".into());
        }
        if self.digest_algorithm != "sha256" {
            return Err("record.digestAlgorithm: must be 'sha256'".into());
        }
        if self.decision != "trusted" {
            return Err("record.decision: must be 'trusted'".into());
        }
        if self.display_command_at_grant.trim().is_empty()
            || self.source_path_at_grant.trim().is_empty()
        {
            return Err("record.displayCommandAtGrant/sourcePathAtGrant: must not be empty".into());
        }
        Ok(())
    }
}

/// `null` 与"字段缺席"是两件事：TS 的 stale 槽位判定把 `undefined` 和 `null` 分开
/// 比较，所以这里用 double option 保留区别（`#[serde(default)]` 负责缺席）。
fn deserialize_nullable_string<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(Some)
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// 信任台账文件（`workspace-hook-trust-v1.json`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrustStoreFile {
    pub schema_version: u32,
    pub records: Vec<TrustRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustStoreStatus {
    Ok,
    /// 文件语法/结构非法：fail-closed，任何记录都不可信。
    Corrupt,
}

impl TrustStoreFile {
    /// 解析 + 校验（唯一键）。语法错误与结构错误等价：都不可信。
    pub fn parse(content: &str) -> Result<Self, String> {
        let file: TrustStoreFile = serde_json::from_str(content)
            .map_err(|error| format!("invalid trust store: {error}"))?;
        if file.schema_version != WORKSPACE_HOOK_TRUST_STORE_SCHEMA_VERSION {
            return Err(format!(
                "unsupported trust store schemaVersion: {}",
                file.schema_version
            ));
        }
        for record in &file.records {
            record.validate()?;
        }
        let mut keys = HashSet::new();
        for record in &file.records {
            if !keys.insert(trust_key(
                &record.workspace_identity,
                &record.hook_declaration_digest,
            )) {
                return Err("duplicate workspaceIdentity+digest key in trust store".into());
            }
        }
        Ok(file)
    }

    pub fn to_json_string(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }
}

/// `trustKey`：`workspaceIdentity\u{0}declarationDigest`。
pub fn trust_key(workspace_identity: &str, hook_declaration_digest: &str) -> String {
    format!("{workspace_identity}\u{0}{hook_declaration_digest}")
}

/// `evaluateWorkspaceHookEntry` 的投影结果（`workspaceHookEffectiveStateSchema`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveState {
    pub review_item_id: String,
    pub trust_state: TrustState,
    pub admission_class: AdmissionClass,
    pub effective_runnable: bool,
    pub reason_code: &'static str,
    pub workspace_identity: String,
    pub bundle_digest: String,
    pub hook_declaration_digest: String,
    pub source_path: String,
}

impl EffectiveState {
    pub fn to_json(&self, entry: &CanonicalEntry) -> Value {
        json!({
            "reviewItemId": self.review_item_id,
            "sourceRootEnabled": entry.source_root_enabled,
            "declarationEnabled": entry.declaration_enabled,
            "runtimeHooksEnabled": entry.runtime_hooks_enabled,
            "configuredEnabled": entry.configured_enabled,
            "editable": entry.editable,
            "trustState": self.trust_state.as_str(),
            "admissionClass": self.admission_class.as_str(),
            "effectiveRunnable": self.effective_runnable,
            "workspaceIdentity": self.workspace_identity,
            "bundleDigest": self.bundle_digest,
            "hookDeclarationDigest": self.hook_declaration_digest,
            "sourcePaths": [self.source_path],
            "reasonCode": self.reason_code,
        })
    }
}

/// 一次评估的只读输入。收成结构是刻意的：`records`/`revoked`/`policy`/`status`
/// 必须来自同一份信任快照，散成位置参数时很容易把两个快照的字段混用。
pub struct EvaluationContext<'a> {
    pub workspace_identity: &'a str,
    pub bundle_digest: &'a str,
    pub source_canonical_path: &'a str,
    pub policy: &'a HookPolicy,
    pub records: &'a HashMap<String, TrustRecord>,
    pub revoked_keys: &'a HashSet<String>,
    pub store_status: TrustStoreStatus,
}

/// 评估一条声明。`records` 以 [`trust_key`] 为键；`revoked_keys` 存同一 key 的撤销集。
pub fn evaluate_entry(entry: &CanonicalEntry, context: &EvaluationContext<'_>) -> EffectiveState {
    let EvaluationContext {
        workspace_identity,
        bundle_digest,
        source_canonical_path,
        policy,
        records,
        revoked_keys,
        store_status,
    } = context;
    let store_status = *store_status;
    let key = trust_key(workspace_identity, &entry.hook_declaration_digest);
    let persistent = records.get(&key);
    let trust_state = if policy.mode == PolicyMode::Deny {
        TrustState::BlockedPolicy
    } else if store_status == TrustStoreStatus::Corrupt {
        TrustState::BlockedUntrusted
    } else if policy.mode == PolicyMode::AllowTrustedOnly {
        if persistent.is_some() {
            TrustState::TrustedPersistent
        } else {
            TrustState::BlockedPolicy
        }
    } else if persistent.is_some() {
        TrustState::TrustedPersistent
    } else if revoked_keys.contains(&key) {
        TrustState::Revoked
    } else if has_stale_slot_record(workspace_identity, entry, records.values()) {
        TrustState::StaleDigest
    } else {
        TrustState::PendingTrust
    };
    let (admission_class, runnable_by_state, reason_code) = trust_state.admission();
    EffectiveState {
        review_item_id: entry.review_item_id.clone(),
        trust_state,
        admission_class,
        effective_runnable: runnable_by_state
            && entry.configured_enabled
            && policy.mode != PolicyMode::Deny,
        reason_code,
        workspace_identity: workspace_identity.to_string(),
        bundle_digest: bundle_digest.to_string(),
        hook_declaration_digest: entry.hook_declaration_digest.clone(),
        source_path: source_canonical_path.to_string(),
    }
}

/// 同一"槽位"（事件+源+下标）上存在**别的摘要**的授权 = 声明被改过。
fn has_stale_slot_record<'a>(
    workspace_identity: &str,
    entry: &CanonicalEntry,
    records: impl Iterator<Item = &'a TrustRecord>,
) -> bool {
    for record in records {
        if record.workspace_identity != workspace_identity {
            continue;
        }
        if record.hook_declaration_digest == entry.hook_declaration_digest {
            continue;
        }
        let (Some(order), Some(matcher_index), Some(hook_index)) = (
            record.source_discovery_order_at_grant,
            record.matcher_index_at_grant,
            record.hook_index_at_grant,
        ) else {
            continue;
        };
        if record.event_at_grant == entry.event
            && record.source_path_at_grant == entry.source_relative_path
            && order == entry.source_discovery_order
            && record.matcher_at_grant.is_some()
            && record.matcher_at_grant.clone().flatten() == entry.matcher
            && matcher_index == entry.matcher_index
            && hook_index == entry.hook_index
        {
            return true;
        }
    }
    false
}

/// 从 bundle 的选中条目生成授权记录（`createWorkspaceHookTrustRecords`）。
pub fn create_trust_records(
    entries: &[CanonicalEntry],
    review_item_ids: &[String],
    workspace_identity: &str,
    bundle_digest: &str,
    granted_at: &str,
    app_version: Option<&str>,
) -> Result<Vec<TrustRecord>, String> {
    let mut unique = Vec::new();
    for id in review_item_ids {
        if !unique.contains(id) {
            unique.push(id.clone());
        }
    }
    let mut records = Vec::new();
    for review_item_id in unique {
        let entry = entries
            .iter()
            .find(|entry| entry.review_item_id == review_item_id)
            .ok_or_else(|| format!("Unknown Workspace Hook review item: {review_item_id}"))?;
        let display_command = match entry.entry_type {
            super::digest::EntryType::Process => {
                let mut parts = vec![entry.command.clone()];
                parts.extend(entry.args.clone().unwrap_or_default());
                parts.join(" ")
            }
            super::digest::EntryType::Command => entry.command.clone(),
        };
        records.push(TrustRecord {
            workspace_identity: workspace_identity.to_string(),
            hook_declaration_digest: entry.hook_declaration_digest.clone(),
            digest_algorithm: "sha256".to_string(),
            decision: "trusted".to_string(),
            granted_at: granted_at.to_string(),
            last_used_at: None,
            bundle_digest_at_grant: Some(bundle_digest.to_string()),
            event_at_grant: entry.event,
            display_command_at_grant: display_command,
            source_path_at_grant: entry.source_relative_path.clone(),
            source_discovery_order_at_grant: Some(entry.source_discovery_order),
            matcher_at_grant: Some(entry.matcher.clone()),
            matcher_index_at_grant: Some(entry.matcher_index),
            hook_index_at_grant: Some(entry.hook_index),
            app_version_at_grant: app_version.map(str::to_string),
        });
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_hooks::config::{
        read_sources, RuntimeRoot, DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_TIMEOUT_MS,
    };
    use crate::workspace_hooks::digest::build_bundle;
    use serde_json::json;
    use std::path::PathBuf;

    fn fixture(
        tag: &str,
    ) -> (
        PathBuf,
        Vec<crate::workspace_hooks::digest::CanonicalEntry>,
        String,
    ) {
        let base =
            std::env::temp_dir().join(format!("zcode-hook-trust-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join(".zcode")).unwrap();
        std::fs::write(
            base.join(".zcode").join("config.json"),
            r#"{"hooks":{"enabled":true,"events":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"node check.js"}]}]}}}"#,
        )
        .unwrap();
        let (sources, errors) = read_sources(&base, None);
        assert!(errors.is_empty());
        let runtime = RuntimeRoot {
            enabled: true,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        };
        let bundle = build_bundle(
            "ws-1",
            &base,
            &sources,
            &runtime,
            "2026-01-01T00:00:00.000Z",
        )
        .unwrap();
        (base, bundle.hooks, bundle.bundle_digest)
    }

    fn record_for(
        entry: &crate::workspace_hooks::digest::CanonicalEntry,
        bundle_digest: &str,
    ) -> TrustRecord {
        create_trust_records(
            std::slice::from_ref(entry),
            std::slice::from_ref(&entry.review_item_id),
            "ws-1",
            bundle_digest,
            "2026-01-01T00:00:01.000Z",
            Some("9.9.9"),
        )
        .unwrap()
        .remove(0)
    }

    fn records_of(record: TrustRecord) -> HashMap<String, TrustRecord> {
        let key = trust_key(&record.workspace_identity, &record.hook_declaration_digest);
        HashMap::from([(key, record)])
    }

    #[test]
    fn trust_state_machine_covers_every_branch() {
        let (base, entries, bundle_digest) = fixture("states");
        let entry = &entries[0];
        let policy = HookPolicy::user_decides();
        let source_path = base
            .join(".zcode")
            .join("config.json")
            .to_string_lossy()
            .to_string();

        let pending = evaluate_entry(
            entry,
            &EvaluationContext {
                workspace_identity: "ws-1",
                bundle_digest: &bundle_digest,
                source_canonical_path: &source_path,
                policy: &policy,
                records: &HashMap::new(),
                revoked_keys: &HashSet::new(),
                store_status: TrustStoreStatus::Ok,
            },
        );
        assert_eq!(pending.trust_state, TrustState::PendingTrust);
        assert_eq!(pending.admission_class, AdmissionClass::Pending);
        assert!(!pending.effective_runnable);

        let trusted_records = records_of(record_for(entry, &bundle_digest));
        let trusted = evaluate_entry(
            entry,
            &EvaluationContext {
                workspace_identity: "ws-1",
                bundle_digest: &bundle_digest,
                source_canonical_path: &source_path,
                policy: &policy,
                records: &trusted_records,
                revoked_keys: &HashSet::new(),
                store_status: TrustStoreStatus::Ok,
            },
        );
        assert_eq!(trusted.trust_state, TrustState::TrustedPersistent);
        assert!(
            trusted.effective_runnable,
            "admitted + configured → runnable"
        );
        assert_eq!(trusted.reason_code, "workspace_hooks_trusted_persistent");

        let revoked = evaluate_entry(
            entry,
            &EvaluationContext {
                workspace_identity: "ws-1",
                bundle_digest: &bundle_digest,
                source_canonical_path: &source_path,
                policy: &policy,
                records: &HashMap::new(),
                revoked_keys: &HashSet::from([trust_key("ws-1", &entry.hook_declaration_digest)]),
                store_status: TrustStoreStatus::Ok,
            },
        );
        assert_eq!(revoked.trust_state, TrustState::Revoked);

        let corrupt = evaluate_entry(
            entry,
            &EvaluationContext {
                workspace_identity: "ws-1",
                bundle_digest: &bundle_digest,
                source_canonical_path: &source_path,
                policy: &policy,
                records: &trusted_records,
                revoked_keys: &HashSet::new(),
                store_status: TrustStoreStatus::Corrupt,
            },
        );
        assert_eq!(
            corrupt.trust_state,
            TrustState::BlockedUntrusted,
            "损坏的 store 必须无视已有记录"
        );

        let deny = HookPolicy {
            mode: PolicyMode::Deny,
            reason: Some("policy".into()),
            policy_revision: "rev".into(),
        };
        let blocked = evaluate_entry(
            entry,
            &EvaluationContext {
                workspace_identity: "ws-1",
                bundle_digest: &bundle_digest,
                source_canonical_path: &source_path,
                policy: &deny,
                records: &trusted_records,
                revoked_keys: &HashSet::new(),
                store_status: TrustStoreStatus::Ok,
            },
        );
        assert_eq!(blocked.trust_state, TrustState::BlockedPolicy);
        assert!(!blocked.effective_runnable, "deny 下永不 runnable");

        let trusted_only = HookPolicy {
            mode: PolicyMode::AllowTrustedOnly,
            reason: None,
            policy_revision: "rev".into(),
        };
        let not_trusted = evaluate_entry(
            entry,
            &EvaluationContext {
                workspace_identity: "ws-1",
                bundle_digest: &bundle_digest,
                source_canonical_path: &source_path,
                policy: &trusted_only,
                records: &HashMap::new(),
                revoked_keys: &HashSet::new(),
                store_status: TrustStoreStatus::Ok,
            },
        );
        assert_eq!(not_trusted.trust_state, TrustState::BlockedPolicy);
        let is_trusted = evaluate_entry(
            entry,
            &EvaluationContext {
                workspace_identity: "ws-1",
                bundle_digest: &bundle_digest,
                source_canonical_path: &source_path,
                policy: &trusted_only,
                records: &trusted_records,
                revoked_keys: &HashSet::new(),
                store_status: TrustStoreStatus::Ok,
            },
        );
        assert_eq!(is_trusted.trust_state, TrustState::TrustedPersistent);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_changed_declaration_in_the_same_slot_reads_as_stale_digest() {
        let (base, entries, bundle_digest) = fixture("stale");
        let entry = &entries[0];
        let mut record = record_for(entry, &bundle_digest);
        // 模拟"槽位没变、命令改过"的旧授权：换一个合法但不同的摘要。
        record.hook_declaration_digest = "a".repeat(64);
        let state = evaluate_entry(
            entry,
            &EvaluationContext {
                workspace_identity: "ws-1",
                bundle_digest: &bundle_digest,
                source_canonical_path: &base.join(".zcode").join("config.json").to_string_lossy(),
                policy: &HookPolicy::user_decides(),
                records: &HashMap::new(),
                revoked_keys: &HashSet::new(),
                store_status: TrustStoreStatus::Ok,
            },
        );
        let stale = evaluate_entry(
            entry,
            &EvaluationContext {
                workspace_identity: "ws-1",
                bundle_digest: &bundle_digest,
                source_canonical_path: &base.join(".zcode").join("config.json").to_string_lossy(),
                policy: &HookPolicy::user_decides(),
                records: &records_of(record),
                revoked_keys: &HashSet::new(),
                store_status: TrustStoreStatus::Ok,
            },
        );
        assert_eq!(
            state.trust_state,
            TrustState::PendingTrust,
            "没有记录时是 pending"
        );
        assert_eq!(stale.trust_state, TrustState::StaleDigest);
        assert_eq!(stale.reason_code, "workspace_hook_declaration_changed");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn trust_store_parsing_is_strict_and_fail_closed() {
        let file = TrustStoreFile {
            schema_version: WORKSPACE_HOOK_TRUST_STORE_SCHEMA_VERSION,
            records: vec![TrustRecord {
                workspace_identity: "ws-1".into(),
                hook_declaration_digest: "b".repeat(64),
                digest_algorithm: "sha256".into(),
                decision: "trusted".into(),
                granted_at: "2026-01-01T00:00:00.000Z".into(),
                last_used_at: None,
                bundle_digest_at_grant: Some("c".repeat(64)),
                event_at_grant: HookEventName::PreToolUse,
                display_command_at_grant: "node check.js".into(),
                source_path_at_grant: ".zcode/config.json".into(),
                source_discovery_order_at_grant: Some(0),
                matcher_at_grant: Some(Some("Bash".into())),
                matcher_index_at_grant: Some(0),
                hook_index_at_grant: Some(0),
                app_version_at_grant: None,
            }],
        };
        let text = file.to_json_string();
        let parsed = TrustStoreFile::parse(&text).expect("round trip");
        assert_eq!(parsed, file);

        // 未知字段 = 文件无效（strict）。
        let mut value: Value = serde_json::from_str(&text).unwrap();
        value["records"][0]["extra"] = json!(true);
        assert!(TrustStoreFile::parse(&value.to_string()).is_err());

        // 重复 key = 文件无效。
        let mut duplicate = file.clone();
        duplicate.records.push(file.records[0].clone());
        assert!(TrustStoreFile::parse(&duplicate.to_json_string()).is_err());

        // 坏摘要形状 = 文件无效。
        let mut bad_digest = file.clone();
        bad_digest.records[0].hook_declaration_digest = "XYZ".into();
        assert!(TrustStoreFile::parse(&bad_digest.to_json_string()).is_err());

        // 语法错误。
        assert!(TrustStoreFile::parse("{ not json").is_err());
    }

    #[test]
    fn policies_parse_strictly() {
        let deny = HookPolicy::parse(&json!({
            "mode": "deny", "reason": "company policy", "policyRevision": "r1"
        }))
        .unwrap();
        assert_eq!(deny.mode, PolicyMode::Deny);
        assert!(HookPolicy::parse(&json!({"mode": "deny", "policyRevision": "r1"})).is_err());
        assert!(HookPolicy::parse(&json!({
            "mode": "deny", "reason": "x", "policyRevision": "r1", "extra": 1
        }))
        .is_err());
        assert!(HookPolicy::parse(&json!({"mode": "user_decides"})).is_err());
        assert!(HookPolicy::parse(&json!({"mode": "wat", "policyRevision": "r1"})).is_err());
        let trusted_only = HookPolicy::parse(&json!({
            "mode": "allow_trusted_only", "policyRevision": "r2"
        }))
        .unwrap();
        assert_eq!(trusted_only.mode, PolicyMode::AllowTrustedOnly);
        assert_eq!(
            HookPolicy::user_decides().policy_revision,
            "builtin:user-decides:v1"
        );
    }

    #[test]
    fn trust_records_require_known_review_items_and_dedupe_ids() {
        let (base, entries, bundle_digest) = fixture("records");
        let entry = &entries[0];
        let granted = create_trust_records(
            &entries,
            &[entry.review_item_id.clone(), entry.review_item_id.clone()],
            "ws-1",
            &bundle_digest,
            "2026-01-01T00:00:00.000Z",
            None,
        )
        .unwrap();
        assert_eq!(granted.len(), 1, "重复 id 只授权一次");
        assert_eq!(granted[0].display_command_at_grant, "node check.js");
        assert_eq!(granted[0].source_discovery_order_at_grant, Some(0));
        assert_eq!(granted[0].matcher_at_grant, Some(Some("Bash".to_string())));
        assert!(create_trust_records(
            &entries,
            &["workspace-hook-9-nope".to_string()],
            "ws-1",
            &bundle_digest,
            "t",
            None
        )
        .is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}
