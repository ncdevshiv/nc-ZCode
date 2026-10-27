//! hook 的展示元数据（`core/src/hooks/display-metadata.ts` 的 Rust 版）。
//!
//! 这里只有两件事，但都是安全相关的：
//! 1. **凭据打码**：hook 命令会展示给用户（评审界面/时间线），URL userinfo、
//!    `Authorization: Bearer …`、`?access_token=…`、`password=…`、`--api-key …`
//!    等形状必须替换成 `••••`，否则密钥会进 UI、截图与日志。
//! 2. **展示用的命令拼接**：`process` 型的 argv 逐段引用后拼接（含空格的参数要加引号），
//!    `command` 型原样展示；`async` 的 command hook 展示为 `background`。
//!
//! 测试里的期望值**不是手写的**——用 `node --experimental-strip-types` 直接跑真实的
//! `display-metadata.ts` 得到的输出，逐条钉在下面。打码规则改一个字都会失配。

use regex::Regex;
use std::sync::OnceLock;

const MASK: &str = "••••";
const SENSITIVE_KEY: &str =
    "(?:access[_-]?token|api[_-]?key|credential|password|private[_-]?key|secret|token)";

fn sensitive_identifier() -> String {
    format!("(?:[A-Za-z0-9]+[_-])*{SENSITIVE_KEY}(?:[_-][A-Za-z0-9]+)*")
}

struct MaskPatterns {
    url_userinfo: Regex,
    authorization_header: Regex,
    query_param: Regex,
    authorization_assignment: Regex,
    quoted_assignment: Regex,
    bare_assignment: Regex,
    flag_value: Regex,
}

fn patterns() -> &'static MaskPatterns {
    static PATTERNS: OnceLock<MaskPatterns> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let identifier = sensitive_identifier();
        MaskPatterns {
            // ([a-z][a-z0-9+.-]*:\/\/)[^\s/:@]+:[^\s/@]+@  →  协议://••••:••••@
            url_userinfo: Regex::new(r"(?i)([a-z][a-z0-9+.\-]*://)[^\s/:@]+:[^\s/@]+@")
                .expect("valid regex"),
            // Authorization: Bearer <token>
            authorization_header: Regex::new(
                r#"(?i)(authorization\s*:\s*(?:basic|bearer)\s+)(?:"[^"]*"|'[^']*'|[^\s,;"']+)"#,
            )
            .expect("valid regex"),
            query_param: Regex::new(&format!(
                r#"(?i)([?&](?:authorization|{identifier})=)[^&\s"']+"#
            ))
            .expect("valid regex"),
            authorization_assignment: Regex::new(
                r#"(?i)(\bauthorization\b\s*=\s*)(?:"[^"]*"|'[^']*'|[^\s,"']+)"#,
            )
            .expect("valid regex"),
            quoted_assignment: Regex::new(&format!(
                r#"(?i)((?:"{identifier}"|'{identifier}')\s*[:=]\s*)(?:"[^"]*"|'[^']*'|[^\s,"']+)"#
            ))
            .expect("valid regex"),
            bare_assignment: Regex::new(&format!(
                r#"(?im)(^|[^A-Za-z0-9_])({identifier}\s*[:=]\s*)(?:"[^"]*"|'[^']*'|[^\s,"']+)"#
            ))
            .expect("valid regex"),
            flag_value: Regex::new(&format!(
                r#"(?i)((?:--)?{identifier})(\s+)(?:"[^"]*"|'[^']*'|[^\s]+)"#
            ))
            .expect("valid regex"),
        }
    })
}

/// `sanitizeHookDisplayText`：把展示文本里的凭据替换成 `••••`。
pub fn sanitize_hook_display_text(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    let patterns = patterns();
    let mut text = patterns
        .url_userinfo
        .replace_all(value, format!("$1{MASK}:{MASK}@"))
        .into_owned();
    text = patterns
        .authorization_header
        .replace_all(&text, format!("$1{MASK}"))
        .into_owned();
    text = patterns
        .query_param
        .replace_all(&text, format!("$1{MASK}"))
        .into_owned();
    text = patterns
        .authorization_assignment
        .replace_all(&text, format!("$1{MASK}"))
        .into_owned();
    text = patterns
        .quoted_assignment
        .replace_all(&text, format!("$1{MASK}"))
        .into_owned();
    text = patterns
        .bare_assignment
        .replace_all(&text, format!("$1$2{MASK}"))
        .into_owned();
    patterns
        .flag_value
        .replace_all(&text, format!("$1$2{MASK}"))
        .into_owned()
}

/// `quoteCommandPart`：安全字符集内的参数原样输出，否则按 JSON 字符串字面量引用。
pub fn quote_command_part(value: &str) -> String {
    static SAFE: OnceLock<Regex> = OnceLock::new();
    let safe = SAFE.get_or_init(|| Regex::new(r"^[A-Za-z0-9_./:@%+=,-]+$").expect("valid regex"));
    if safe.is_match(value) {
        value.to_string()
    } else {
        serde_json::to_string(value).unwrap_or_else(|_| format!("\"{value}\""))
    }
}

/// hook 的执行展示描述（评审条目与 `hookInvocation` 行共用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookExecutionDescriptor {
    pub command_display: String,
    pub execution_mode: &'static str,
    pub execution_type: &'static str,
    pub timeout_ms: u64,
}

/// `createHookExecutionDescriptor`：process 逐段引用、command 原样，再过打码。
pub fn execution_descriptor(
    entry_type: &str,
    command: &str,
    args: Option<&[String]>,
    asynchronous: bool,
    timeout_ms: u64,
) -> HookExecutionDescriptor {
    let execution_type = if entry_type == "process" {
        "process"
    } else {
        "command"
    };
    let raw = if entry_type == "process" {
        let mut parts = vec![quote_command_part(command)];
        parts.extend(
            args.unwrap_or(&[])
                .iter()
                .map(|arg| quote_command_part(arg)),
        );
        parts.join(" ")
    } else {
        command.to_string()
    };
    HookExecutionDescriptor {
        command_display: sanitize_hook_display_text(&raw),
        execution_mode: if entry_type == "command" && asynchronous {
            "background"
        } else {
            "foreground"
        },
        execution_type,
        timeout_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 参考值来自真实的 `display-metadata.ts`（node --experimental-strip-types 直接执行）。
    #[test]
    fn masking_matches_the_typescript_implementation() {
        let cases = [
            (
                "curl \"https://user:pass@example.com/x\"",
                "curl \"https://••••:••••@example.com/x\"",
            ),
            ("Authorization: Bearer abc123", "Authorization: Bearer ••••"),
            (
                "curl \"https://x/?access_token=abc&y=1\"",
                "curl \"https://x/?access_token=••••\"",
            ),
            ("--api-key secret", "--api-key ••••"),
            ("password='hunter2'", "password=••••"),
            ("echo (a+b)", "echo (a+b)"),
            (
                "MY_SECRET=topsecret node check.js",
                "MY_SECRET=•••• node check.js",
            ),
            ("node script.js --token=abc", "node script.js --token=••••"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                sanitize_hook_display_text(input),
                expected,
                "masking mismatch for {input}"
            );
        }
        assert_eq!(sanitize_hook_display_text(""), "");
    }

    #[test]
    fn command_parts_are_quoted_only_when_they_need_it() {
        assert_eq!(quote_command_part("git"), "git");
        assert_eq!(quote_command_part("src/a.ts"), "src/a.ts");
        assert_eq!(quote_command_part("--porcelain"), "--porcelain");
        assert_eq!(quote_command_part("two words"), "\"two words\"");
        assert_eq!(quote_command_part("quo\"te"), "\"quo\\\"te\"");
    }

    /// 参考值同样来自真实 TS 的 `createHookExecutionDescriptor`。
    #[test]
    fn descriptor_shapes_match_the_typescript_implementation() {
        let process = execution_descriptor(
            "process",
            "git",
            Some(&["status".to_string(), "--porcelain".to_string()]),
            false,
            60_000,
        );
        assert_eq!(process.command_display, "git status --porcelain");
        assert_eq!(process.execution_mode, "foreground");
        assert_eq!(process.execution_type, "process");
        assert_eq!(process.timeout_ms, 60_000);

        let background = execution_descriptor("command", "npm test", None, true, 1_500);
        assert_eq!(background.command_display, "npm test");
        assert_eq!(background.execution_mode, "background");
        assert_eq!(background.execution_type, "command");
        assert_eq!(background.timeout_ms, 1_500);

        let masked = execution_descriptor("command", "deploy --token=abc123", None, false, 1_000);
        assert_eq!(masked.command_display, "deploy --token=••••");
    }
}
