//! Workspace file index: walker, packed column codec and fuzzy search.
//!
//! Packing and search scoring mirror `workspaceFileEntriesCodec.ts` and
//! `workspaceFileSearch.ts`; the walker uses the `.zcodeignore` rules from `ignore.rs`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nc_code_service_core::ServiceError;
use serde_json::Value;
use tokio::sync::Mutex;

use super::ignore::IgnoreRules;

const CACHE_TTL: Duration = Duration::from_secs(60);
const SEARCH_LIMIT: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceEntry {
    pub name: String,
    pub path: String,
    pub relative_path: String,
    pub entry_type: &'static str,
}

impl WorkspaceEntry {
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "name": self.name,
            "path": self.path,
            "relativePath": self.relative_path,
            "type": self.entry_type,
        })
    }
}

pub struct Index {
    pub entries: Vec<WorkspaceEntry>,
    pub packed: String,
    signature: String,
    at: Instant,
}

impl Index {
    fn fresh(&self, signature: &str) -> bool {
        self.signature == signature && self.at.elapsed() < CACHE_TTL
    }
}

#[derive(Default)]
pub struct IndexCache {
    cache: Mutex<HashMap<String, Arc<Index>>>,
}

impl IndexCache {
    pub async fn ensure(&self, root: &Path, refresh: bool) -> Result<Arc<Index>, ServiceError> {
        let signature = super::ignore::fingerprint(root).await;
        let key = root.to_string_lossy().into_owned();
        if !refresh {
            let cache = self.cache.lock().await;
            if let Some(index) = cache.get(&key) {
                if index.fresh(&signature) {
                    return Ok(Arc::clone(index));
                }
            }
        }
        let rules = super::ignore::load_rules(root).await?;
        let entries = walk(root, &rules).await?;
        let packed = pack(&entries);
        let index = Arc::new(Index {
            entries,
            packed,
            signature,
            at: Instant::now(),
        });
        let mut cache = self.cache.lock().await;
        cache.insert(key, Arc::clone(&index));
        while cache.len() > 4 {
            let Some(oldest) = cache.keys().next().cloned() else {
                break;
            };
            cache.remove(&oldest);
        }
        Ok(index)
    }
}

async fn walk(root: &Path, rules: &IgnoreRules) -> Result<Vec<WorkspaceEntry>, ServiceError> {
    let mut entries = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let mut reader = match tokio::fs::read_dir(&directory).await {
            Ok(reader) => reader,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                continue
            }
            Err(error) => return Err(ServiceError::new(format!("walk workspace failed: {error}"))),
        };
        while let Some(entry) = reader
            .next_entry()
            .await
            .map_err(|error| ServiceError::new(format!("walk workspace failed: {error}")))?
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == super::ignore::IGNORE_FILE_NAME {
                continue;
            }
            let path = entry.path();
            let relative_path = relative_path(root, &path);
            let file_type = match entry.file_type().await {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            let is_dir = if file_type.is_symlink() {
                tokio::fs::metadata(&path)
                    .await
                    .map(|metadata| metadata.is_dir())
                    .unwrap_or(false)
            } else {
                file_type.is_dir()
            };
            let entry_type = if is_dir { "directory" } else { "file" };
            if rules.is_ignored(&relative_path, is_dir) {
                continue;
            }
            entries.push(WorkspaceEntry {
                name,
                path: path.to_string_lossy().into_owned(),
                relative_path: relative_path.clone(),
                entry_type,
            });
            if is_dir && !file_type.is_symlink() {
                pending.push(path);
            }
        }
    }
    entries.sort_by(|left, right| {
        let left_dir = left.entry_type == "directory";
        let right_dir = right.entry_type == "directory";
        right_dir
            .cmp(&left_dir)
            .then_with(|| left.relative_path.cmp(&right.relative_path))
    });
    Ok(entries)
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn escape_field(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
}

pub fn pack(entries: &[WorkspaceEntry]) -> String {
    entries
        .iter()
        .map(|entry| {
            format!(
                "{}\t{}",
                entry.entry_type,
                escape_field(&entry.relative_path)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn search(index: &Index, query: &str, limit: usize) -> Vec<WorkspaceEntry> {
    let normalized_query = query.trim().to_lowercase();
    let effective_limit = limit.min(SEARCH_LIMIT);
    if effective_limit == 0 {
        return Vec::new();
    }
    if normalized_query.is_empty() {
        let mut candidates = index.entries.clone();
        candidates.sort_by_key(|entry| {
            if entry.entry_type == "directory" {
                1
            } else {
                0
            }
        });
        candidates.truncate(effective_limit);
        return candidates;
    }
    let mut scored: Vec<(u32, usize, WorkspaceEntry)> = Vec::new();
    for (position, entry) in index.entries.iter().enumerate() {
        let Some(score) = score(
            &entry.name,
            &entry.relative_path,
            &entry.path,
            &normalized_query,
        ) else {
            continue;
        };
        scored.push((score, position, entry.clone()));
        if scored.len() > effective_limit * 4 {
            scored.sort_by(compare_scored);
            scored.truncate(effective_limit);
        }
    }
    scored.sort_by(compare_scored);
    scored.truncate(effective_limit);
    scored.into_iter().map(|(_, _, entry)| entry).collect()
}

fn compare_scored(
    left: &(u32, usize, WorkspaceEntry),
    right: &(u32, usize, WorkspaceEntry),
) -> std::cmp::Ordering {
    left.0
        .cmp(&right.0)
        .then_with(|| left.1.cmp(&right.1))
        .then_with(|| left.2.name.cmp(&right.2.name))
}

fn score(name: &str, relative_path: &str, path: &str, query: &str) -> Option<u32> {
    let name_score = score_text(name, query);
    let relative_score = score_text(relative_path, query);
    let path_score = score_text(path, query);
    let keyword = [
        relative_score.map(|score| score + 300),
        path_score.map(|score| score + 300),
    ]
    .into_iter()
    .flatten()
    .min();
    [name_score, relative_score.map(|score| score + 25), keyword]
        .into_iter()
        .flatten()
        .min()
}

fn score_text(text: &str, query: &str) -> Option<u32> {
    let text = text.trim().to_lowercase();
    if text.is_empty() {
        return None;
    }
    if text.starts_with(query) {
        return Some((text.chars().count() - query.chars().count()) as u32);
    }
    if let Some(index) = text.find(query) {
        return Some(100 + text[..index].chars().count() as u32);
    }
    let mut score = 200u32;
    let mut search_start = 0usize;
    for character in query.chars() {
        let found = text[search_start..].find(character)?;
        score += (found) as u32;
        search_start += found + character.len_utf8();
    }
    Some(score + (text.chars().count().saturating_sub(query.chars().count())) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, relative: &str, kind: &'static str) -> WorkspaceEntry {
        WorkspaceEntry {
            name: name.to_string(),
            path: format!("G:/work/{relative}"),
            relative_path: relative.to_string(),
            entry_type: kind,
        }
    }

    #[test]
    fn packing_escapes_tabs_and_newlines() {
        let packed = pack(&[
            entry("a", "dir/a.ts", "file"),
            entry("b\tc", "dir/b\tc", "file"),
            entry("d", "dir/d\ne", "file"),
        ]);
        assert_eq!(packed, "file\tdir/a.ts\nfile\tdir/b\\tc\nfile\tdir/d\\ne");
    }

    #[test]
    fn search_prefers_exact_name_prefix_then_path_matches() {
        let index = Index {
            entries: vec![
                entry("alpha.ts", "src/alpha.ts", "file"),
                entry("alpha", "src/alpha", "directory"),
                entry("beta.ts", "alpha/beta.ts", "file"),
                entry("unrelated.ts", "other/unrelated.ts", "file"),
            ],
            packed: String::new(),
            signature: String::new(),
            at: Instant::now(),
        };
        let results = search(&index, "alpha", 10);
        assert_eq!(results[0].name, "alpha");
        assert!(results.iter().any(|entry| entry.name == "beta.ts"));
        assert!(!results.iter().any(|entry| entry.name == "unrelated.ts"));
    }
}
