# -*- coding: utf-8 -*-
# zcode-services-terminal Fixes for 8 test failures: 5 real implementation flaws + 3 assertions contrary to TS.
import io

failures = 0


def patch(path, pairs):
    global failures
    text = io.open(path, encoding="utf-8").read()
    for old, new, label in pairs:
        hits = text.count(old)
        if hits != 1:
            print("MISS(%d) [%s] in %s" % (hits, label, path))
            failures += 1
            continue
        text = text.replace(old, new)
        print("ok [%s]" % label)
    io.open(path, "w", encoding="utf-8", newline="").write(text)


profile = "native/crates/zcode-services-terminal/src/profile.rs"
terminal = "native/crates/zcode-services-terminal/src/terminal.rs"
utf8 = "native/crates/zcode-services-terminal/src/utf8_stream.rs"

patch(
    utf8,
    [
        #Real defect: illegal bytes are skipped directly, and no U+FFFD is output
        # (doc comment Promise alignment Node Buffer.toString("utf8")).
        (
            """ match error.error_len() {
                        // Invalid bytes: one replacement character per invalid sequence.
                        Some(length) => offset += length,""",
            """ match error.error_len() {
                        // Invalid bytes: one replacement character per invalid sequence.
                        Some(length) => {
                            out.push('\\u{FFFD}');
                            offset += length;
                        }""",
            "utf8 emits U+FFFD for invalid sequences",
        ),
    ],
)

patch(
    profile,
    [
        # Real defect: Block comment scanning stops at '*', and then '/' is spit out as a normal character → JSON has one more slash.
        (
            """ '/' if next == Some('*') => {
                index += 2;
                while index < chars.len() && !(chars[index] == '*' && chars.get(index + 1) == Some(&'/'))
                {index += 1;
                }
                index += 1;
            }""",
            """ '/' if next == Some('*') => {
                index += 2;
                while index < chars.len() && !(chars[index] == '*' && chars.get(index + 1) == Some(&'/'))
                {
                    index += 1;
                }
                // Loop stops at '*': skip it together with '/' (otherwise the slash will be left as a normal character).
                index += 2;
            }""",
            "block comment consumes both delimiters",
        ),
        #Real defect: profiles is an object {list, defaults}, take it as an array → list will always be empty,
        # The defaultProfile branch can never be reached (TS: parsed.profiles.list, terminalProfile.ts:230-246).
        (
            """ let profiles = parsed.get("profiles");
        let list = profiles.and_then(Value::as_array).cloned().unwrap_or_default();""",
            """ let profiles = parsed.get("profiles");
        // profiles are {list, defaults} objects, not arrays (terminalProfile.ts:230-231).
        let list = profiles
            .and_then(|value| value.get("list"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();""",
            "read profiles.list (object shape)",
        ),
        # Real bug: Top-level non-objects (arrays/scalars) should be skipped - the detector only reads the configuration as an object.
        (
            """pub fn parse_toml(raw: &str) -> Option<Value> {
    let parsed: toml::Value = raw.parse().ok()?;
    serde_json::to_value(parsed).ok()
}""",
            """pub fn parse_toml(raw: &str) -> Option<Value> {
    let parsed: toml::Value = raw.parse().ok()?;
    let value = serde_json::to_value(parsed).ok()?;
    // Configure the reader to only accept top-level objects (arrays/scalars with no lookup keys → skipped).
    matches!(value, Value::Object(_)).then_some(value)
}""",
            "toml parser keeps objects only",
        ),
        ("""pub fn parse_yaml(raw: &str) -> Option<Value> {
    let parsed: serde_yaml::Value = serde_yaml::from_str(raw).ok()?;
    json_from_yaml(&parsed)
}""",
            """pub fn parse_yaml(raw: &str) -> Option<Value> {
    let parsed: serde_yaml::Value = serde_yaml::from_str(raw).ok()?;
    let value = json_from_yaml(&parsed)?;
    // Same as above: top-level arrays/scalars are not returned as configuration objects.
    matches!(value, Value::Object(_)).then_some(value)
}""",
            "yaml parser keeps objects only",
        ),
        # Assertion is contradictory: first assert that the key list does not contain windowsPty, and then assert that it exists. This call is Windows,
        # TS only adds this key in win32 (terminal.ts create result), so the key list must contain it.
        (
            """ let keys: Vec<&String> = value.as_object().expect("object").keys().collect();
        assert_eq!(keys, vec!["id", "shell", "fontFamily", "fontFamilySource"]);""",
            """ let keys: Vec<&String> = value.as_object().expect("object").keys().collect();
        assert_eq!(
            keys,
            vec![
                "id",
                "shell",
                "fontFamily",
                "fontFamilySource",
                "windowsPty"
            ]
        );""",
            "create_result keys include win32-only windowsPty",
        ),
        # Assertion is contrary to TS: dedupeFontFamilyStack puts primary first (terminalProfile.ts:65-78),
        # So the result of primary="monospace" starts with monospace, not the order of fallbacks.
        (
            """ assert_eq!(
            dedupe_font_family_stack("monospace"),
            FONT_FAMILY_FALLBACKS.join(", ")
        );""",
            """ // TS dedupeFontFamilyStack puts the primary first, and then fills the gap fallback (terminalProfile.ts:65-78).
        let expected = std::iter::once("monospace".to_string())
            .chain(
                FONT_FAMILY_FALLBACKS
                    .iter().filter(|entry| *entry != "monospace")
                    .map(|entry| entry.to_string()),
            )
            .collect::<Vec<_>>()
            .join(", ");
        assert_eq!(dedupe_font_family_stack("monospace"), expected);""",
            "font stack expectation matches TS dedupe order",
        ),
    ],
)

patch(
    terminal,
    [
        # The write contract of TS is {id: string; data: string} (terminalService.ts:415-416),
        # data is required; passing undefined to node-pty will throw an error instead of "empty writing".
        (
            """ assert_eq!(
            decode_write_params(&json!({"id": "3"})).unwrap().data,
            "",
            "a missing data payload is an empty write, like node-pty"
        );""",
            """ // TS: data of write(params: {id, data}) is required (terminalService.ts:415);
        // Missing data cannot be written as empty (undefined will be thrown into node-pty), which is explicitly rejected here.
        assert!(decode_write_params(&json!({"id": "3"})).is_err());""",
            "missing write data is rejected per TS contract",
        ),
    ],
)

if failures:
    raise SystemExit("FAILED: %d replacement(s)" % failures)
print("all replacements landed")