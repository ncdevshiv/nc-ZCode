//! hook 配置的写回（`packages/shared/src/workspace-hook-mutation.ts` 的 Rust 版）。
//!
//! 只支持一件事：把审阅快照里某条声明的 `enabled` 写回**可编辑的**
//! `.zcode/config.json`。所有守卫都是"不做猜测"：
//! - 目标必须是 snapshot 里 `editable` 的 `.zcode/config.json`，否则 mismatch；
//! - 磁盘上的声明必须与审阅时**逐字摘要一致**（用快照里的 resolvedTimeout/MaxOutput
//!   作默认值重算），否则 mismatch——审阅之后有人改过命令，就不能把信任/开关落到
//!   新命令上；
//! - 读/解析失败是 `config_unreadable`，写失败是 `config_write_failed`：两者必须
//!   能区分，否则用户会对"读不出来"反复重试"写入"。
//!
//! 写回保留原文件的键顺序（workspace 已开启 serde_json 的 `preserve_order`），
//! 输出 `JSON.stringify(value, null, 2) + "\n"`，与 TS 的原子写一致。

use std::fs;
use std::io::Write;
use std::path::Path;

use serde_json::Value;

use super::config::HookDefinition;
use super::digest::{declaration_digest, BundleSnapshot, DeclarationDigestInput};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MutationError {
    /// 配置读不出来 / 不是合法 JSON（不要报成写入失败）。
    ConfigUnreadable(String),
    /// 快照与磁盘不一致（review 之后被改过、目标不是可编辑配置等）。
    SnapshotMismatch(&'static str),
    WriteFailed(String),
}

impl MutationError {
    pub fn reason_code(&self) -> &'static str {
        match self {
            MutationError::ConfigUnreadable(_) => "workspace_hooks_config_unreadable",
            MutationError::SnapshotMismatch(_) => "workspace_hooks_snapshot_mismatch",
            MutationError::WriteFailed(_) => "workspace_hooks_config_write_failed",
        }
    }
}

/// 把某条声明的 `enabled` 写回 `.zcode/config.json`（只接受可编辑目标）。
pub fn write_configured_toggle(
    config_path: &Path,
    snapshot: &BundleSnapshot,
    review_item_id: &str,
    enabled: bool,
) -> Result<(), MutationError> {
    let config_path = super::config::absolute_lexical(config_path);
    let some_entry = snapshot
        .hooks
        .iter()
        .find(|entry| entry.review_item_id == review_item_id);
    let source = some_entry.and_then(|entry| snapshot.source_files.get(entry.source_file_index));
    let (Some(entry), Some(source)) = (some_entry, source) else {
        return Err(MutationError::SnapshotMismatch(
            "Workspace Hook toggle target is not the current editable project config",
        ));
    };
    if !entry.editable
        || !source.editable
        || source.config_file_kind != super::config::ConfigFileKind::ZcodeConfigJson
        || super::config::absolute_lexical(Path::new(&source.canonical_path)) != config_path
    {
        return Err(MutationError::SnapshotMismatch(
            "Workspace Hook toggle target is not the current editable project config",
        ));
    }

    let text = fs::read_to_string(&config_path)
        .map_err(|error| MutationError::ConfigUnreadable(format!("read: {error}")))?;
    let mut root: Value = serde_json::from_str(&text)
        .map_err(|error| MutationError::ConfigUnreadable(format!("parse: {error}")))?;
    if !root.is_object() {
        return Err(MutationError::SnapshotMismatch(
            "Workspace Hook config root is not an object",
        ));
    }
    let raw_hooks = root
        .get_mut("hooks")
        .filter(|hooks| hooks.is_object())
        .ok_or(MutationError::SnapshotMismatch(
            "Workspace Hook config has no hooks object",
        ))?;
    // 整段 hooks 必须仍然符合 schema（未知键/坏类型都不能被写回掩盖）。
    super::config::HooksConfig::parse(raw_hooks).map_err(|_| {
        MutationError::SnapshotMismatch("Workspace Hook config no longer matches its schema")
    })?;
    let events = raw_hooks
        .get_mut("events")
        .filter(|events| events.is_object())
        .ok_or(MutationError::SnapshotMismatch(
            "Workspace Hook config has no events object",
        ))?;
    let matchers = events
        .get_mut(entry.event.as_str())
        .and_then(Value::as_array_mut)
        .ok_or(MutationError::SnapshotMismatch(
            "Workspace Hook event no longer exists",
        ))?;
    let matcher = matchers
        .get_mut(entry.matcher_index)
        .filter(|matcher| matcher.is_object())
        .ok_or(MutationError::SnapshotMismatch(
            "Workspace Hook matcher no longer exists",
        ))?;
    // matcher 的展示值要在可变借用 hooks 数组之前取出来（借用检查器会拒绝反过来写）。
    let matcher_value = matcher
        .get("matcher")
        .and_then(Value::as_str)
        .map(str::to_string);
    let declarations = matcher
        .get_mut("hooks")
        .and_then(Value::as_array_mut)
        .ok_or(MutationError::SnapshotMismatch(
            "Workspace Hook matcher no longer exists",
        ))?;
    let raw_declaration = declarations
        .get_mut(entry.hook_index)
        .filter(|declaration| declaration.is_object())
        .ok_or(MutationError::SnapshotMismatch(
            "Workspace Hook declaration no longer exists",
        ))?;
    let parsed_declaration: HookDefinition = serde_json::from_value(raw_declaration.clone())
        .map_err(|_| {
            MutationError::SnapshotMismatch(
                "Workspace Hook declaration no longer matches its schema",
            )
        })?;

    // 重算摘要：用快照里的 resolved 值作默认值——校验的是**声明本体**没有变。
    // （已知边界与 TS 相同：只改 root 级 timeoutMs/maxOutputBytes 时本守卫看不到。）
    let current_digest = declaration_digest(&DeclarationDigestInput {
        source_relative_path: &entry.source_relative_path,
        source_discovery_order: source.discovery_order,
        event: entry.event,
        matcher: matcher_value.as_deref(),
        matcher_index: entry.matcher_index,
        hook_index: entry.hook_index,
        hook: &parsed_declaration,
        resolved_timeout_ms: entry.resolved_timeout_ms,
        resolved_max_output_bytes: entry.resolved_max_output_bytes,
    });
    if current_digest != entry.hook_declaration_digest {
        return Err(MutationError::SnapshotMismatch(
            "Workspace Hook declaration changed after review",
        ));
    }

    if let Some(object) = raw_declaration.as_object_mut() {
        object.insert("enabled".to_string(), Value::Bool(enabled));
    }
    write_atomic_config(&config_path, &root)
}

/// `atomicWriteWorkspaceHookConfig`：临时文件（同名目录）→ fsync → rename；
/// 内容 = `JSON.stringify(value, null, 2) + "\n"`。
fn write_atomic_config(path: &Path, value: &Value) -> Result<(), MutationError> {
    let mut rendered = serde_json::to_string_pretty(value)
        .map_err(|error| MutationError::WriteFailed(error.to_string()))?;
    rendered.push('\n');
    let directory = path.parent().ok_or_else(|| {
        MutationError::WriteFailed("config path has no parent directory".to_string())
    })?;
    fs::create_dir_all(directory)
        .map_err(|error| MutationError::WriteFailed(format!("create dir: {error}")))?;
    let temp = directory.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "config.json".to_string()),
        std::process::id(),
        crate::store::now_ms()
    ));
    let write_result = (|| -> std::io::Result<()> {
        let mut file = fs::File::create(&temp)?;
        file.write_all(rendered.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp);
        return Err(MutationError::WriteFailed(error.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_hooks::config::{
        read_sources, RuntimeRoot, DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_TIMEOUT_MS,
    };
    use crate::workspace_hooks::digest::build_bundle;
    use serde_json::json;

    const CONFIG: &str = r#"{
  "theme": "dark",
  "hooks": {
    "enabled": true,
    "events": {
      "PreToolUse": [
        {"matcher": "Bash", "hooks": [
          {"type": "command", "command": "node check.js"},
          {"type": "command", "command": "npm run lint", "enabled": false}
        ]}
      ]
    }
  }
}
"#;

    fn workspace(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let base =
            std::env::temp_dir().join(format!("zcode-hook-mutation-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join(".zcode")).unwrap();
        let config = base.join(".zcode").join("config.json");
        fs::write(&config, CONFIG).unwrap();
        (base, config)
    }

    fn snapshot_of(workspace: &Path) -> BundleSnapshot {
        let (sources, errors) = read_sources(workspace, None);
        assert!(errors.is_empty());
        let runtime = RuntimeRoot {
            enabled: true,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        };
        build_bundle(
            "ws-1",
            workspace,
            &sources,
            &runtime,
            "2026-01-01T00:00:00.000Z",
        )
        .expect("bundle")
    }

    #[test]
    fn toggle_writes_only_the_target_and_preserves_everything_else() {
        let (workspace, config) = workspace("write");
        let snapshot = snapshot_of(&workspace);
        let target = snapshot
            .hooks
            .iter()
            .find(|entry| entry.command == "node check.js")
            .expect("target entry");
        assert!(target.editable);
        write_configured_toggle(&config, &snapshot, &target.review_item_id, false).expect("write");

        let written: Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(written["theme"], json!("dark"), "无关的顶层键必须保留");
        assert_eq!(
            written["hooks"]["events"]["PreToolUse"][0]["hooks"][0]["enabled"],
            json!(false)
        );
        assert_eq!(
            written["hooks"]["events"]["PreToolUse"][0]["hooks"][1]["enabled"],
            json!(false),
            "其它声明的开关不动"
        );
        // 键顺序保留（preserve_order）：theme 仍在 hooks 前。
        let text = fs::read_to_string(&config).unwrap();
        assert!(text.find("theme").unwrap() < text.find("hooks").unwrap());
        assert!(text.ends_with("}\n"), "输出带尾随换行");

        // 重新发现的快照里该条 configuredEnabled 变 false，摘要不变。
        let refreshed = snapshot_of(&workspace);
        let refreshed_target = refreshed
            .hooks
            .iter()
            .find(|entry| entry.review_item_id == target.review_item_id)
            .unwrap();
        assert!(!refreshed_target.configured_enabled);
        assert_eq!(
            refreshed_target.hook_declaration_digest, target.hook_declaration_digest,
            "开关不进声明摘要（信任跟声明走，不跟开关走）"
        );
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn a_changed_declaration_after_review_is_rejected() {
        let (workspace, config) = workspace("stale");
        let snapshot = snapshot_of(&workspace);
        let target = snapshot.hooks[0].clone();
        // 审阅之后把命令改掉。
        let text = fs::read_to_string(&config)
            .unwrap()
            .replace("node check.js", "node check.js --exfiltrate");
        fs::write(&config, text).unwrap();
        let error =
            write_configured_toggle(&config, &snapshot, &target.review_item_id, false).unwrap_err();
        assert_eq!(error.reason_code(), "workspace_hooks_snapshot_mismatch");
        assert!(matches!(error, MutationError::SnapshotMismatch(_)));
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn unreadable_or_invalid_config_is_reported_as_unreadable_not_write_failed() {
        let (workspace, config) = workspace("bad");
        let snapshot = snapshot_of(&workspace);
        let target = snapshot.hooks[0].clone();
        fs::write(&config, "{ not json").unwrap();
        let error =
            write_configured_toggle(&config, &snapshot, &target.review_item_id, false).unwrap_err();
        assert_eq!(error.reason_code(), "workspace_hooks_config_unreadable");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn only_the_workspace_editable_config_is_a_valid_toggle_target() {
        let base =
            std::env::temp_dir().join(format!("zcode-hook-mutation-root-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        fs::write(
            base.join("zcode.json"),
            r#"{"hooks":{"events":{"Stop":[{"hooks":[{"type":"command","command":"echo root"}]}]}}}"#,
        )
        .unwrap();
        let snapshot = snapshot_of(&base);
        let error = write_configured_toggle(
            &base.join("zcode.json"),
            &snapshot,
            &snapshot.hooks[0].review_item_id,
            false,
        )
        .unwrap_err();
        assert_eq!(
            error.reason_code(),
            "workspace_hooks_snapshot_mismatch",
            "根目录 zcode.json 不是可编辑目标"
        );
        let _ = fs::remove_dir_all(&base);
    }
}
