//! workspaceIdentity identity key behavior test.
//!
//! Semantics from AGENTS.md: `workspaceIdentity?.trim() || workspacePath`
//! (Identity key is used uniformly for deduplication, binding, caching, queuing, persistence, and request association).
//! There is no corresponding TS test file in the warehouse (verified by grep `*.test.ts`/`*.spec.ts` to be 0),
//! Assertions are written boundary by boundary according to the ECMAScript whitespace set of JS `String.prototype.trim`,
//! The key differences (NBSP / ZWNBSP / NEL) are documented in the `js_trim` module comments.

use nc_code_core::{identity_key, js_trim};

// ── 1. identity_key full matrix ──

#[test]
fn identity_key_matrix() {
    let path = "/work/repo";

    // Default / empty / pure blank → empty after trim → JS `||` fallback workspacePath.
    assert_eq!(identity_key(None, path), path);
    assert_eq!(identity_key(Some(""), path), path);
    assert_eq!(identity_key(Some("   "), path), path);
    assert_eq!(identity_key(Some(" \t\r\n\u{000B}\u{000C} "), path), path);

    // Value → Identity key after trimming (blanks on both sides are removed, and the middle is left unchanged).
    assert_eq!(identity_key(Some("  padded  "), path), "padded");
    assert_eq!(identity_key(Some("\t\nrepo\r\n"), path), "repo");
    assert_eq!(identity_key(Some("a b"), path), "a b");

    // NBSP (U+00A0): JS WhiteSpace is included explicitly, Rust White_Space is also included → behaves the same on both sides.
    assert_eq!(identity_key(Some("\u{00A0}"), path), path); // 仅 NBSP → 回退
    assert_eq!(identity_key(Some("\u{00A0}id\u{00A0}"), path), "id");
    assert_eq!(identity_key(Some("  id\u{00A0}"), path), "id");

    // ZWNBSP (U+FEFF): JS trim; Rust does not trim by default → js_trim explicitly completes.
    assert_eq!(identity_key(Some("\u{FEFF}"), path), path); // 仅 BOM → 回退
    assert_eq!(identity_key(Some("\u{FEFF}id\u{FEFF}"), path), "id");
    assert_eq!(identity_key(Some(" \u{FEFF}id"), path), "id");

    // NEL (U+0085): JS WhiteSpace **not included** → no trimming (Rust will trim by default → js_trim explicitly excluded).
    // The result is non-empty → as an identity key, it will never fall back to the path fallback (otherwise it will be inconsistent with the identity on both sides of JS).
    assert_eq!(identity_key(Some("\u{0085}"), path), "\u{0085}");
    assert_eq!(identity_key(Some("  \u{0085}  "), path), "\u{0085}");
    assert_eq!(
        identity_key(Some("\u{0085}id\u{0085}"), path),
        "\u{0085}id\u{0085}"
    );

    // Ordinary unicode identity is not affected by trim (only leading and trailing blanks are removed).
    assert_eq!(
        identity_key(Some("Project/Directory"), path),
        "Project/Directory"
    );
    assert_eq!(identity_key(Some("café"), path), "café");

    // workspacePath used as-is: **not** trim (consistent with JS expressions).
    assert_eq!(identity_key(None, "  /w/x  "), "  /w/x  ");
    assert_eq!(identity_key(Some(""), "  /w/x  "), "  /w/x  ");
}

// ── 2. js_trim direct test (JS trim blank set mirror) ──

#[test]
fn js_trim_matches_js_whitespace_semantics() {
    // Regular whitespace (TAB/LF/CR/SP and Zs) are consistent on both sides.
    assert_eq!(js_trim(" a "), "a");
    assert_eq!(js_trim("\t\n\r  x  \r\n\t"), "x");
    assert_eq!(js_trim(" \t\r\n "), "");
    assert_eq!(js_trim(""), "");

    // NBSP: Both JS and Rust are tailored → consistent.
    assert_eq!(js_trim("\u{00A0}x\u{00A0}"), "x");

    // ZWNBSP (U+FEFF): JS WhiteSpace includes → cropping (including mixed first and last blank scenes).
    assert_eq!(js_trim("\u{FEFF}x"), "x");
    assert_eq!(js_trim("x\u{FEFF}"), "x");
    assert_eq!(js_trim("  \u{FEFF}x\u{FEFF}  "), "x");
    //The internal BOM does not move (trim only acts on both ends).
    assert_eq!(js_trim("a\u{FEFF}b"), "a\u{FEFF}b");

    // NEL (U+0085): JS does not cut → retain (the string containing NEL at both ends remains intact).
    assert_eq!(js_trim("\u{0085}x\u{0085}"), "\u{0085}x\u{0085}");
    assert_eq!(js_trim(" \u{0085}x "), "\u{0085}x"); // 外侧空格裁掉，NEL 保留

    // LS/PS (U+2028/U+2029): JS LineTerminator → trim; Rust White_Space is the same as →.
    assert_eq!(js_trim("\u{2028}x\u{2029}"), "x");

    // ZWSP (U+200B): White_Space on both sides is no → no cropping.
    assert_eq!(js_trim("x\u{200B}y"), "x\u{200B}y");
}
