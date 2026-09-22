//! workspaceIdentity 身份 key 行为测试。
//!
//! 语义来自 AGENTS.md：`workspaceIdentity?.trim() || workspacePath`
//! （身份 key 统一用于去重、绑定、缓存、队列、持久化、请求关联）。
//! 仓库内无对应 TS 测试文件（已 grep `*.test.ts`/`*.spec.ts` 核实为 0），
//! 断言按 JS `String.prototype.trim` 的 ECMAScript 空白集逐边界编写，
//! 关键差异（NBSP / ZWNBSP / NEL）在 `js_trim` 模块注释中有据可查。

use zcode_core::{identity_key, js_trim};

// ── 1. identity_key 全矩阵 ──

#[test]
fn identity_key_matrix() {
    let path = "/work/repo";

    // 缺省 / 空 / 纯空白 → trim 后为空 → JS `||` 回退 workspacePath。
    assert_eq!(identity_key(None, path), path);
    assert_eq!(identity_key(Some(""), path), path);
    assert_eq!(identity_key(Some("   "), path), path);
    assert_eq!(identity_key(Some(" \t\r\n\u{000B}\u{000C} "), path), path);

    // 有值 → trim 后的身份 key（两侧空白去除，中间不动）。
    assert_eq!(identity_key(Some("  padded  "), path), "padded");
    assert_eq!(identity_key(Some("\t\nrepo\r\n"), path), "repo");
    assert_eq!(identity_key(Some("a b"), path), "a b");

    // NBSP (U+00A0)：JS WhiteSpace 显式包含，Rust White_Space 也含 → 两侧行为一致。
    assert_eq!(identity_key(Some("\u{00A0}"), path), path); // 仅 NBSP → 回退
    assert_eq!(identity_key(Some("\u{00A0}id\u{00A0}"), path), "id");
    assert_eq!(identity_key(Some("  id\u{00A0}"), path), "id");

    // ZWNBSP (U+FEFF)：JS trim 裁剪；Rust 默认不裁 → js_trim 显式补齐。
    assert_eq!(identity_key(Some("\u{FEFF}"), path), path); // 仅 BOM → 回退
    assert_eq!(identity_key(Some("\u{FEFF}id\u{FEFF}"), path), "id");
    assert_eq!(identity_key(Some(" \u{FEFF}id"), path), "id");

    // NEL (U+0085)：JS WhiteSpace **不含** → 不裁剪（Rust 默认会裁 → js_trim 显式排除）。
    // 结果非空 → 作为身份 key，绝不落到 path 回退（否则与 JS 两侧身份不一致）。
    assert_eq!(identity_key(Some("\u{0085}"), path), "\u{0085}");
    assert_eq!(identity_key(Some("  \u{0085}  "), path), "\u{0085}");
    assert_eq!(
        identity_key(Some("\u{0085}id\u{0085}"), path),
        "\u{0085}id\u{0085}"
    );

    // 普通 unicode 身份不受 trim 影响（只去首尾空白）。
    assert_eq!(identity_key(Some("  项目/目录  "), path), "项目/目录");
    assert_eq!(identity_key(Some("café"), path), "café");

    // workspacePath 原样使用：**不** trim（与 JS 表达式一致）。
    assert_eq!(identity_key(None, "  /w/x  "), "  /w/x  ");
    assert_eq!(identity_key(Some(""), "  /w/x  "), "  /w/x  ");
}

// ── 2. js_trim 直测（JS trim 空白集镜像） ──

#[test]
fn js_trim_matches_js_whitespace_semantics() {
    // 常规空白（TAB/LF/CR/SP 与 Zs）两侧一致。
    assert_eq!(js_trim(" a "), "a");
    assert_eq!(js_trim("\t\n\r  x  \r\n\t"), "x");
    assert_eq!(js_trim(" \t\r\n "), "");
    assert_eq!(js_trim(""), "");

    // NBSP：JS 与 Rust 都裁剪 → 一致。
    assert_eq!(js_trim("\u{00A0}x\u{00A0}"), "x");

    // ZWNBSP (U+FEFF)：JS WhiteSpace 包含 → 裁剪（含首尾混合空白场景）。
    assert_eq!(js_trim("\u{FEFF}x"), "x");
    assert_eq!(js_trim("x\u{FEFF}"), "x");
    assert_eq!(js_trim("  \u{FEFF}x\u{FEFF}  "), "x");
    // 内部 BOM 不动（trim 只作用于两端）。
    assert_eq!(js_trim("a\u{FEFF}b"), "a\u{FEFF}b");

    // NEL (U+0085)：JS 不裁剪 → 保留（两端含 NEL 的串整体原样）。
    assert_eq!(js_trim("\u{0085}x\u{0085}"), "\u{0085}x\u{0085}");
    assert_eq!(js_trim(" \u{0085}x "), "\u{0085}x"); // 外侧空格裁掉，NEL 保留

    // LS/PS (U+2028/U+2029)：JS LineTerminator → trim 裁剪；Rust White_Space 同 → 一致。
    assert_eq!(js_trim("\u{2028}x\u{2029}"), "x");

    // ZWSP (U+200B)：两侧 White_Space 均为 no → 不裁剪。
    assert_eq!(js_trim("x\u{200B}y"), "x\u{200B}y");
}
