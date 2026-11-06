//! Branch / commit-graph / index record parsers.
//!
//! Port of the record parsers in `packages/services/src/git/repo/gitCliRepo.ts`
//! (`parseBranchRefRecords:350`, `parseGitGraphRecords:472`, `parseGitIndexEntries:519`).
//! The object key order and the branch sort are observable, so both are written to match
//! the TS output byte for byte.

use nc_code_service_core::ServiceError;
use serde_json::{json, Value};

/// `parseBranchRefRecords` (`gitCliRepo.ts:350`) for
/// `git for-each-ref refs/heads --format=%(refname:short)%00%(upstream:short)%00%(objectname)%00%(committerdate:unix)`.
pub fn parse_branch_ref_records(stdout: &str, current_branch_name: Option<&str>) -> Vec<Value> {
    let mut records: Vec<(String, Value)> = Vec::new();
    for line in stdout.replace("\r\n", "\n").split('\n') {
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\0').collect();
        let name = fields.first().copied().unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let upstream = fields.get(1).copied().unwrap_or_default();
        let hash = fields.get(2).copied().unwrap_or_default();
        let timestamp = fields.get(3).copied().unwrap_or_default();
        let commit_timestamp_ms = if timestamp.is_empty() {
            None
        } else {
            parse_leading_integer(timestamp).map(|seconds| seconds * 1000)
        };
        let is_current = current_branch_name == Some(name);
        records.push((
            name.to_string(),
            json!({
                "name": name,
                "isCurrent": is_current,
                "upstreamName": if upstream.is_empty() { Value::Null } else { json!(upstream) },
                "commitHash": if hash.is_empty() { Value::Null } else { json!(hash) },
                "commitTimestampMs": commit_timestamp_ms.map_or(Value::Null, |value| json!(value)),
            }),
        ));
    }
    records.sort_by(|left, right| {
        let left_is_current = left.1["isCurrent"].as_bool().unwrap_or(false);
        let right_is_current = right.1["isCurrent"].as_bool().unwrap_or(false);
        right_is_current
            .cmp(&left_is_current)
            .then_with(|| {
                let left_timestamp = left.1["commitTimestampMs"].as_i64().unwrap_or(i64::MIN);
                let right_timestamp = right.1["commitTimestampMs"].as_i64().unwrap_or(i64::MIN);
                right_timestamp.cmp(&left_timestamp)
            })
            .then_with(|| locale_compare(&left.0, &right.0))
    });
    records.into_iter().map(|(_, value)| value).collect()
}

/// `String.prototype.localeCompare` for branch names, which TS uses to break ties
/// (`gitCliRepo.ts:382`). A byte compare would put `Z` before `a`; the locale rule is
/// case-insensitive first with the raw value as the tie-break.
fn locale_compare(left: &str, right: &str) -> std::cmp::Ordering {
    left.to_lowercase()
        .cmp(&right.to_lowercase())
        .then_with(|| left.cmp(right))
}

/// JS `Number.parseInt(value, 10)`.
pub fn parse_leading_integer(value: &str) -> Option<i64> {
    let digits: String = value
        .trim_start()
        .chars()
        .take_while(|character| {
            character.is_ascii_digit() || *character == '-' || *character == '+'
        })
        .collect();
    digits.parse().ok()
}

/// `normalizeGitGraphMaxCount` (`gitCliRepo.ts:386`).
pub fn normalize_graph_max_count(max_count: Option<i64>) -> i64 {
    match max_count {
        None => 100,
        Some(value) => value.clamp(1, 200),
    }
}

/// `normalizeGitGraphSkip` (`gitCliRepo.ts:394`).
pub fn normalize_graph_skip(skip: Option<i64>) -> i64 {
    skip.unwrap_or(0).max(0)
}

/// `parseGitGraphDecorationRef` (`gitCliRepo.ts:410`).
fn parse_graph_decoration_ref(raw_ref: &str) -> Option<Value> {
    let reference = raw_ref.trim();
    if reference.is_empty() {
        return None;
    }
    if reference == "HEAD" {
        return Some(json!({"name": "HEAD", "kind": "head"}));
    }
    if let Some(tag_ref) = reference.strip_prefix("tag: ") {
        let tag_ref = tag_ref.trim();
        let name = tag_ref.strip_prefix("refs/tags/").unwrap_or(tag_ref);
        return (!name.is_empty()).then(|| json!({"name": name, "kind": "tag"}));
    }
    for (prefix, kind) in [
        ("refs/heads/", "branch"),
        ("refs/remotes/", "remote"),
        ("refs/tags/", "tag"),
    ] {
        if let Some(name) = reference.strip_prefix(prefix) {
            return (!name.is_empty()).then(|| json!({"name": name, "kind": kind}));
        }
    }
    let kind = if reference.contains('/') {
        "remote"
    } else {
        "branch"
    };
    Some(json!({"name": reference, "kind": kind}))
}

/// `addGitGraphRef` (`gitCliRepo.ts:402`) — dedup by (kind, name).
fn add_graph_ref(refs: &mut Vec<Value>, value: Value) {
    let duplicate = refs
        .iter()
        .any(|candidate| candidate["kind"] == value["kind"] && candidate["name"] == value["name"]);
    if !duplicate {
        refs.push(value);
    }
}

/// `parseGitGraphRefs` (`gitCliRepo.ts:445`).
fn parse_graph_refs(raw_decorations: &str) -> Vec<Value> {
    let mut refs = Vec::new();
    for raw_decoration in raw_decorations.split(',') {
        let decoration = raw_decoration.trim();
        if decoration.is_empty() {
            continue;
        }
        if let Some(pointed) = decoration.strip_prefix("HEAD -> ") {
            add_graph_ref(&mut refs, json!({"name": "HEAD", "kind": "head"}));
            if let Some(value) = parse_graph_decoration_ref(pointed) {
                add_graph_ref(&mut refs, value);
            }
            continue;
        }
        if let Some(value) = parse_graph_decoration_ref(decoration) {
            add_graph_ref(&mut refs, value);
        }
    }
    refs
}

/// `parseGitGraphRecords` (`gitCliRepo.ts:472`) for the
/// `%H%x00%P%x00%an%x00%at%x00%s%x00%D%x1e` format.
pub fn parse_graph_records(stdout: &str) -> Vec<Value> {
    stdout
        .split('\x1e')
        .map(|record| record.trim())
        .filter(|record| !record.is_empty())
        .filter_map(|record| {
            let fields: Vec<&str> = record.split('\0').collect();
            let hash = fields.first().copied().unwrap_or_default();
            if hash.is_empty() {
                return None;
            }
            let parents = fields.get(1).copied().unwrap_or_default();
            let author_name = fields.get(2).copied().unwrap_or_default();
            let authored_at = fields.get(3).copied().unwrap_or_default();
            let subject = fields.get(4).copied().unwrap_or_default();
            let decorations = fields.get(5).copied().unwrap_or_default();
            let authored_at_ms = if authored_at.is_empty() {
                None
            } else {
                parse_leading_integer(authored_at).map(|seconds| seconds * 1000)
            };
            Some(json!({
                "hash": hash,
                "parents": parents
                    .split(' ')
                    .filter(|token| !token.is_empty())
                    .map(Value::from)
                    .collect::<Vec<Value>>(),
                "refs": parse_graph_refs(decorations),
                "subject": subject,
                "authorName": if author_name.is_empty() { Value::Null } else { json!(author_name) },
                "authoredAtMs": authored_at_ms.map_or(Value::Null, |value| json!(value)),
            }))
        })
        .collect()
}

#[derive(Debug)]
pub struct IndexEntry {
    pub mode: String,
    pub object_hash: String,
    pub stage: String,
    pub path: String,
}

/// `parseGitIndexEntries` (`gitCliRepo.ts:519`) for `git ls-files --stage -z`.
pub fn parse_index_entries(stdout: &str) -> Result<Vec<IndexEntry>, ServiceError> {
    let mut entries = Vec::new();
    for record in stdout.split('\0').filter(|record| !record.is_empty()) {
        let Some(tab_index) = record.find('\t') else {
            return Err(ServiceError::message(
                "Failed to parse staged Git index entry.",
            ));
        };
        let tokens: Vec<&str> = record[..tab_index].split_whitespace().collect();
        let path = crate::path::normalize_git_path(&record[tab_index + 1..]);
        if tokens.len() != 3 || path.is_empty() {
            return Err(ServiceError::message(
                "Failed to parse staged Git index entry.",
            ));
        }
        entries.push(IndexEntry {
            mode: tokens[0].to_string(),
            object_hash: tokens[1].to_string(),
            stage: tokens[2].to_string(),
            path,
        });
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_records_sort_current_then_newest_then_name() {
        let stdout = "main\0origin/main\0aaaa\x001700000000\n\
             feature\0\0bbbb\x001800000000\n\
             older\0origin/older\0cccc\x001600000000\n\
             azero\0\0dddd\x001700000000\n";
        let branches = parse_branch_ref_records(stdout, Some("main"));
        assert_eq!(branches.len(), 4);
        assert_eq!(branches[0]["name"], json!("main"));
        assert_eq!(branches[0]["isCurrent"], json!(true));
        assert_eq!(branches[0]["upstreamName"], json!("origin/main"));
        assert_eq!(branches[1]["name"], json!("feature"));
        // Same timestamp as `main`, so the locale tie-break puts `azero` next.
        assert_eq!(branches[2]["name"], json!("azero"));
        assert_eq!(branches[3]["name"], json!("older"));
        assert_eq!(branches[1]["upstreamName"], Value::Null);
        assert_eq!(branches[1]["commitTimestampMs"], json!(1800000000000i64));
        assert_eq!(branches[3]["commitTimestampMs"], json!(1600000000000i64));
    }

    #[test]
    fn locale_compare_is_case_insensitive() {
        // gitCliRepo.ts:382 uses left.name.localeCompare(right.name):
        // "alpha" < "Beta" (first letter a<b); "a" and "A" tie at the first level, and ICU en puts uppercase first -> "a" > "A".
        assert_eq!(locale_compare("alpha", "Beta"), std::cmp::Ordering::Less);
        assert_eq!(locale_compare("a", "A"), std::cmp::Ordering::Greater);
    }

    #[test]
    fn detached_head_marks_no_branch_as_current() {
        let branches = parse_branch_ref_records("main\0\0aaaa\x001\n", None);
        assert_eq!(branches[0]["isCurrent"], json!(false));
        assert_eq!(branches[0]["commitHash"], json!("aaaa"));
    }

    #[test]
    fn parse_leading_integer_matches_parse_int() {
        assert_eq!(parse_leading_integer("1700000000"), Some(1700000000));
        assert_eq!(parse_leading_integer("-5x"), Some(-5));
        assert_eq!(parse_leading_integer(""), None);
        assert_eq!(parse_leading_integer("abc"), None);
    }

    #[test]
    fn graph_records_decode_fields_and_decorations() {
        let stdout = "abc\0def ghi\0Test User\x001700000000\0feat: x\0HEAD -> main, tag: v1, origin/main\x1e\ndef\0\0Bot\0\0empty subject\0\x1e";
        let commits = parse_graph_records(stdout);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0]["hash"], json!("abc"));
        assert_eq!(commits[0]["parents"], json!(["def", "ghi"]));
        assert_eq!(commits[0]["authoredAtMs"], json!(1700000000000i64));
        assert_eq!(
            commits[0]["refs"],
            json!([
                {"name": "HEAD", "kind": "head"},
                {"name": "main", "kind": "branch"},
                {"name": "v1", "kind": "tag"},
                {"name": "origin/main", "kind": "remote"},
            ])
        );
        assert_eq!(commits[1]["parents"], json!([]));
        assert_eq!(commits[1]["authoredAtMs"], Value::Null);
        assert_eq!(commits[1]["refs"], json!([]));
    }

    #[test]
    fn decoration_ref_kinds() {
        assert_eq!(
            parse_graph_decoration_ref("refs/heads/x"),
            Some(json!({"name": "x", "kind": "branch"}))
        );
        assert_eq!(
            parse_graph_decoration_ref("refs/remotes/origin/x"),
            Some(json!({"name": "origin/x", "kind": "remote"}))
        );
        assert_eq!(
            parse_graph_decoration_ref("refs/tags/v2"),
            Some(json!({"name": "v2", "kind": "tag"}))
        );
        assert_eq!(
            parse_graph_decoration_ref("tag: refs/tags/v3"),
            Some(json!({"name": "v3", "kind": "tag"}))
        );
        assert_eq!(
            parse_graph_decoration_ref("origin/x"),
            Some(json!({"name": "origin/x", "kind": "remote"}))
        );
        assert_eq!(
            parse_graph_decoration_ref("plain"),
            Some(json!({"name": "plain", "kind": "branch"}))
        );
        assert_eq!(parse_graph_decoration_ref("  "), None);
    }

    #[test]
    fn graph_pagination_bounds_match_ts() {
        assert_eq!(normalize_graph_max_count(None), 100);
        assert_eq!(normalize_graph_max_count(Some(0)), 1);
        assert_eq!(normalize_graph_max_count(Some(999)), 200);
        assert_eq!(normalize_graph_skip(None), 0);
        assert_eq!(normalize_graph_skip(Some(-5)), 0);
        assert_eq!(normalize_graph_skip(Some(7)), 7);
    }

    #[test]
    fn index_entries_reject_malformed_records() {
        let entries = parse_index_entries("100644 abcdef 0\tsrc/a.txt\0").expect("entry");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].mode, "100644");
        assert_eq!(entries[0].stage, "0");
        assert_eq!(entries[0].path, "src/a.txt");
        assert_eq!(
            parse_index_entries("100644 abcdef 0\t")
                .unwrap_err()
                .message,
            "Failed to parse staged Git index entry."
        );
        assert_eq!(
            parse_index_entries("no-tab").unwrap_err().message,
            "Failed to parse staged Git index entry."
        );
    }

    #[test]
    fn leading_integer_takes_the_numeric_prefix_like_number_parseint() {
        // On the JS side it is Number.parseInt(value, 10): tolerate leading whitespace, take the leading digits, return null for non-numbers.
        assert_eq!(parse_leading_integer("  42"), Some(42));
        assert_eq!(parse_leading_integer("12abc"), Some(12));
        assert_eq!(parse_leading_integer("+7"), Some(7));
        assert_eq!(parse_leading_integer("-"), None);
        assert_eq!(parse_leading_integer("  "), None);
    }
}
