# -*- coding: utf-8 -*-
# Fix 7 test failures of zcode-services-extensions at one time:
# 3 real implementation defects (plain folding lacks spaces, folded block doubles blank lines, ICU number weight),
# 4 platform/clock assertions that are fragile or contrary to TS. Number of assertion hits per replacement.
import io

edits = {}

edits["native/crates/zcode-services-extensions/src/yaml_lite.rs"] = [
    # A) plain multi-line folding: continuation lines are spliced without separating spaces ("first line" + "second line").
    (
        " single.push_str(&fold_plain_lines(&continuation));",
        """ let folded = fold_plain_lines(&continuation);
        if !single.is_empty() && !folded.is_empty() && !single.ends_with(' ') && !single.ends_with('\\n') {
            // TS plain scalar folding rule: Connect continuation lines with a single space (multiLineIndentation).
            single.push(' ');
        }
        single.push_str(&folded);""",
    ),
    # B) folded block: inserting a newline when out already ends with a newline will enlarge the YAML empty line into two newlines.
    (
        """ if position > 0 {
                if line.is_empty() || out.ends_with('\\n') {
                    out.push('\\n');
                } else {
                    out.push(' ');
                }
            }""",
        """ if position > 0 && !out.is_empty() {
                if line.is_empty() {
                    if !out.ends_with('\\n') {
                        out.push('\\n');
                    }
                } else if !out.ends_with('\\n') {
                    out.push(' ');
                }
                // When out ends with \\n (the previous blank line has been wrapped), no delimiter is added.
                // Otherwise a single empty line in the folded scalar will be output as two newlines.
            }""",
    ),
]

edits["native/crates/zcode-services-extensions/src/text.rs"] = [
    # C) ICU en: Numbers at the same position are compared numerically (skill-2 < skill-10), and will be inverted character by character.
    (
        """ match left_folded.cmp(&right_folded) {
            Ordering::Equal => {}
            other => return other,
        }""",
        """ // The numerical weight of ICU "en" is based on the numerical ratio ("2" < "10"). The character-by-character ratio will judge 2 to be greater.if left_char.is_ascii_digit() && right_char.is_ascii_digit() {
            let (left_run, left_len) = digits_run(&left_chars, index);
            let (right_run, right_len) = digits_run(&right_chars, index);
            if let (Ok(left_number), Ok(right_number)) =
                (left_run.parse::<u128>(), right_run.parse::<u128>())
            {
                if left_number != right_number {
                    return left_number.cmp(&right_number);
                }
                // The values are the same: After consuming the entire number, continue the comparison from the next character.
                let consumed = left_len.max(right_len);
                let left_tail: String = left_chars[index + consumed..].iter().collect();
                let right_tail: String = right_chars[index + consumed..].iter().collect();
                return locale_en_cmp(&left_tail, &right_tail);
            }
        }
        match left_folded.cmp(&right_folded) {
            Ordering::Equal => {}
            other => return other,
        }""",
    ),
    (
        "fn fold(value: char) -> String {",
        """/// Contiguous string of ASCII digits starting from index (returns string and length).
fn digits_run(chars: &[char], index: usize) -> (String, usize) {
    let mut text = String::new();
    let mut length = 0;
    for character in chars[index..].iter() {
        if !character.is_ascii_digit() {
            break;
        }
        text.push(*character);
        length += 1;
    }
    (text, length.max(1))
}

fn fold(value: char) -> String {""",
    ),
]

edits["native/crates/zcode-services-extensions/src/model_selection.rs"] = [
    # The options.reasoningLevel of TS model-selection.ts:10 is .optional(), {} is legal.
    (
        ' assert_eq!(selection(json!({"providerId": "a", "modelId": "b", "options": {}})), None);',""" // TS `modelSelectionSchema`'s options.reasoningLevel is `.optional()`
        // (packages/shared/src/model-selection.ts:10): empty options legal = no gear.
        assert_eq!(
            selection(json!({"providerId": "a", "modelId": "b", "options": {}}))
                .map(|value| value.reasoning_level),
            Some(None)
        );""",
    ),
]

edits["native/crates/zcode-services-extensions/src/user_config.rs"] = [
    # AppPaths on Windows parses "/tmp/..." into a path with a drive letter, and POSIX literal assertions must fail.
    (
        """ assert_eq!(
            resolve_cli_storage_root(&app_paths, &resolve_config_path(&app_paths, "~/custom")),
            PathBuf::from("/tmp/zcode-ext-unit/home/custom/cli")
        );""",
        """ // AppPaths on Windows will complement the current drive letter (G:\\tmp\\...): assert the tail by component, and do not lock POSIX literals.
        let resolved =
            resolve_cli_storage_root(&app_paths, &resolve_config_path(&app_paths, "~/custom"));
        assert!(
            resolved.ends_with(PathBuf::from("home/custom/cli")),
            "{resolved:?}"
        );""",
    ),
]

edits["native/crates/zcode-services-extensions/src/plugins.rs"] = [
    # Same as above: absolute_lexical adds the current drive letter to the root path.
    (
        """ assert_eq!(
            resolve_inside(&root, "commands"),
            Some(PathBuf::from("/plugins/demo/commands"))
        );""",
        """ let resolved = resolve_inside(&root, "commands");
        // absolute_lexical will add the drive letter to the root path in Windows: assert by component, without locking POSIX literals.
        assert!(
            resolved
                .as_ref()
                .is_some_and(|path| path.ends_with(PathBuf::from("/plugins/demo/commands"))),
            "{resolved:?}"
        );""",
    ),
]

edits["native/crates/zcode-services-extensions/src/memory.rs"] = [
    # The list is sorted in descending order by updatedAt (the same millisecond is used by id), and the hard-coded ranking will jitter with the clock.
    (""" let service = MemoryService::new(paths);
        let listed = service.list_project_memories().await.unwrap();
        assert_eq!(listed.len(), 2, "{listed:?}");
        assert_eq!(listed[0]["id"], json!("demo-workspace-0123456789abcdef"));
        assert_eq!(listed[0]["label"], json!("demo-workspace"));
        let files = listed[0]["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0]["kind"], json!("index"));
        assert_eq!(files[0]["name"], json!("MEMORY.md"));
        assert_eq!(files[1]["name"], json!("notes.md"));
        assert_eq!(files[1]["size"], json!(10));
        // updatedAt desc: both have entries, the empty-memory workspace ranks by id.
        assert_eq!(listed[1]["id"], json!("empty-workspace-0123456789abcdeg"));""",
        """ let service = MemoryService::new(paths);
        let listed = service.list_project_memories().await.unwrap();
        assert_eq!(listed.len(), 2, "{listed:?}");
        //The sorting rule is updatedAt descending order, and the same millisecond is in ascending order by id (memoryService.ts:196);
        //The actual milliseconds for file writing vary randomly, so the rule itself is asserted, not the fixed ranking.
        let ids: Vec<&str> = listed
            .iter()
            .map(|entry| entry["id"].as_str().unwrap_or_default())
            .collect();
        assert!(ids.contains(&"demo-workspace-0123456789abcdef"), "{ids:?}");
        assert!(ids.contains(&"empty-workspace-0123456789abcdeg"), "{ids:?}");
        assert!(
            listed[0]["updatedAt"].as_f64().unwrap_or_default()
                >= listed[1]["updatedAt"].as_f64().unwrap_or_default(),
            "{listed:?}"
        );
        let demo = listed
            .iter()
            .find(|entry| entry["id"] == json!("demo-workspace-0123456789abcdef"))
            .expect("demo entry");assert_eq!(demo["label"], json!("demo-workspace"));
        let files = demo["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        // index must be ranked before ordinary files (memoryService.ts:106-107).
        assert_eq!(files[0]["kind"], json!("index"));
        assert_eq!(files[0]["name"], json!("MEMORY.md"));
        assert_eq!(files[1]["name"], json!("notes.md"));
        assert_eq!(files[1]["size"], json!(10));""",
    ),
]

failures = 0
for path, pairs in edits.items():
    text = io.open(path, encoding="utf-8").read()
    for old, new in pairs:
        hits = text.count(old)
        if hits != 1:
            print("MISS(%d) in %s: %s" % (hits, path, old[:70]))
            failures += 1
            continue
        text = text.replace(old, new)
    io.open(path, "w", encoding="utf-8", newline="").write(text)
    print("patched", path.split("/")[-1], len(pairs))

if failures:
    raise SystemExit("FAILED: %d replacement(s)" % failures)
print("all replacements landed")