//! AGENTS.md Workspace Identity：
//! 身份 key 统一为 `workspaceIdentity?.trim() || workspacePath`
//! （去重、绑定、缓存、队列、持久化、请求关联通用）。

/// JS `String.prototype.trim` 的空白集镜像。
///
/// 差异已逐项核对（ECMAScript WhiteSpace/LineTerminator vs Rust `char::is_whitespace`
/// = Unicode `White_Space` 属性）：
/// - **NBSP (U+00A0)**：两侧都裁剪（Rust `White_Space=yes`，JS WhiteSpace 显式含 NBSP）
///   ——任务要求验证的边界，结论是行为一致，无需修正；
/// - **ZWNBSP (U+FEFF)**：JS WhiteSpace 显式包含 → 裁剪；Rust `White_Space=no` →
///   默认**不**裁剪。这里显式补上，否则 `" \u{FEFF}id"` 两侧身份 key 不一致；
/// - **NEL (U+0085)**：Rust `White_Space=yes` → 默认裁剪；JS WhiteSpace **不含** →
///   不裁剪。这里显式排除，否则 `"\u{0085}"` 在 Rust 会误落到 path 回退、在 JS 却是 identity；
/// - 其余（TAB/LF/CR/VT/FF/SP/Zs/LS/PS）两侧集合一致。
pub fn js_trim(s: &str) -> &str {
    let is_js_whitespace = |c: char| -> bool {
        if c == '\u{FEFF}' {
            return true;
        }
        if c == '\u{0085}' {
            return false;
        }
        c.is_whitespace()
    };
    // 修复：全串皆为 JS 空白时，`start = len` 而 `end = 0`，`&s[start..end]` 会
    // 因 start > end 直接 panic（如 `"   "`、`"\u{FEFF}"`）。JS `trim()` 对该输入
    // 返回空串，这里提前返回 ""，同时保证后续 `end >= start` 恒成立。
    let start = match s.find(|c| !is_js_whitespace(c)) {
        Some(index) => index,
        None => return "",
    };
    let end = s
        .rfind(|c| !is_js_whitespace(c))
        .map(|i| i + s[i..].chars().next().map_or(0, char::len_utf8))
        .unwrap_or(0);
    &s[start..end]
}

/// `workspaceKey = workspaceIdentity?.trim() || workspacePath`：
/// identity 缺省/裁剪后为空 → 回退 `workspacePath`（path 原样使用，不裁剪，
/// 与 JS 表达式一致）。
pub fn identity_key(workspace_identity: Option<&str>, workspace_path: &str) -> String {
    match workspace_identity.map(js_trim) {
        Some(trimmed) if !trimmed.is_empty() => trimmed.to_owned(),
        _ => workspace_path.to_owned(),
    }
}
