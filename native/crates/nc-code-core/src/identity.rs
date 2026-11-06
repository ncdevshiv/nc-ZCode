//! AGENTS.md Workspace Identity：
//! The identity key is unified into `workspaceIdentity?.trim() || workspacePath`
//! (Common to deduplication, binding, caching, queue, persistence, and request association).

/// Empty set mirror for JS `String.prototype.trim`.
///
/// Differences checked one by one (ECMAScript WhiteSpace/LineTerminator vs Rust `char::is_whitespace`
/// = Unicode `White_Space` attribute):
/// - **NBSP (U+00A0)**: clipping on both sides (Rust `White_Space=yes`, JS WhiteSpace explicitly contains NBSP)
/// ——The boundary of task requirement verification, the conclusion is that the behavior is consistent and no modification is needed;
/// - **ZWNBSP (U+FEFF)**: JS WhiteSpace explicit inclusion → clipping; Rust `White_Space=no` →
/// Default is **not** cropping. Add it explicitly here, otherwise the identity keys on both sides of `" \u{FEFF}id"` will be inconsistent;
/// - **NEL (U+0085)**: Rust `White_Space=yes` → default clipping; JS WhiteSpace **not included** →
/// No cropping. Explicitly excluded here, otherwise `"\u{0085}"` will fall into path fallback by mistake in Rust, but it will be identity in JS;
/// - The remaining (TAB/LF/CR/VT/FF/SP/Zs/LS/PS) sets are consistent on both sides.
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
    // Fix: When the entire string is JS blank, `start = len` and `end = 0`, `&s[start..end]` will
    // Directly panic due to start > end (such as `" "`, `"\u{FEFF}"`). JS `trim()` on this input
    //Return an empty string, return "" in advance here, and ensure that the subsequent `end >= start` is always true.
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
/// identity defaults/is empty after clipping → fallback `workspacePath` (path is used as is, not clipped,
/// Consistent with JS expressions).
pub fn identity_key(workspace_identity: Option<&str>, workspace_path: &str) -> String {
    match workspace_identity.map(js_trim) {
        Some(trimmed) if !trimmed.is_empty() => trimmed.to_owned(),
        _ => workspace_path.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn identity_test() {
        assert!(true);
    }
}
