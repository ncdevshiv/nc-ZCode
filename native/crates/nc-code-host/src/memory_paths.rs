//! Memory file path safety (ported from TS `core/src/memory/memory-file-path.ts`).
//!
//! Memory writes are constrained to a contained, non-sensitive path under the
//! memory root. This prevents prompt-injected memory extraction from mutating
//! repository internals (`.git`, hooks, `.zcode`, `node_modules`, etc.).

use std::path::{Component, Path, PathBuf};

const SENSITIVE_SEGMENTS: &[&str] = &[
    ".git",
    "hooks",
    ".husky",
    ".githooks",
    "node_modules",
    ".vscode",
    ".idea",
    "head",
    "config",
    "objects",
    "refs",
    ".zcode",
    "skills",
    "commands",
    "agents",
    ".cargo",
    ".devcontainer",
    ".yarn",
    ".mvn",
];

/// Resolve a path under `root_dir` and ensure it stays contained.
/// Returns `None` if the path escapes, is absolute in a disallowed way, or is empty.
pub fn resolve_contained(root_dir: &Path, candidate: &Path) -> Option<PathBuf> {
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root_dir.join(candidate)
    };
    let normalized = normalize_path(&joined);
    let root = normalize_path(root_dir);
    if !normalized.starts_with(&root) {
        return None;
    }
    let rel = normalized.strip_prefix(&root).ok()?;
    if rel.as_os_str().is_empty() {
        return None;
    }
    Some(normalized)
}

/// Resolve a contained memory file path and reject sensitive segments.
/// Only `.md` memory mutations are considered safe by default; callers may
/// enforce extension checks separately.
pub fn resolve_safe_memory_file(root_dir: &Path, candidate: &Path) -> Option<PathBuf> {
    let resolved = resolve_contained(root_dir, candidate)?;
    let rel = resolved.strip_prefix(normalize_path(root_dir)).ok()?;
    if contains_sensitive_segment(rel) {
        return None;
    }
    Some(resolved)
}

/// Normalize a path lexically (no filesystem access).
fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let popped = out.pop();
                if !popped {
                    // keep leading .. only for relative paths that started there;
                    // absolute paths must not escape root.
                    if !out.has_root() {
                        out.push(component.as_os_str());
                    }
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn contains_sensitive_segment(relative: &Path) -> bool {
    for component in relative.components() {
        let Component::Normal(seg) = component else {
            continue;
        };
        let seg = seg.to_string_lossy();
        let normalized = normalize_sensitive_segment(&seg);
        if SENSITIVE_SEGMENTS.iter().any(|s| *s == normalized) {
            return true;
        }
    }
    false
}

fn normalize_sensitive_segment(segment: &str) -> String {
    let lower = segment.to_lowercase();
    // Strip zero-width/control-ish chars used in TS normalization, then NTFS ADS/ trailing dots/spaces.
    let filtered: String = lower
        .chars()
        .filter(|ch| {
            !matches!(
                *ch,
                '\u{200c}'..='\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{206a}'..='\u{206f}'
                    | '\u{feff}'
            )
        })
        .collect();
    let before_ads = filtered.split(':').next().unwrap_or("").to_string();
    before_ads.trim_end_matches(['.', ' ']).to_string()
}

pub fn is_contained_markdown_mutation(root_dir: &Path, candidate: &Path) -> bool {
    let Some(path) = resolve_safe_memory_file(root_dir, candidate) else {
        return false;
    };
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("md"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contained_path_resolves_under_root() {
        let root = Path::new("/tmp/mem");
        let p = resolve_contained(root, Path::new("notes/a.md")).unwrap();
        assert!(p.starts_with("/tmp/mem") || p.to_string_lossy().contains("mem"));
    }

    #[test]
    fn traversal_is_rejected() {
        let root = Path::new("/tmp/mem");
        assert!(resolve_contained(root, Path::new("../etc/passwd")).is_none());
        assert!(resolve_safe_memory_file(root, Path::new("a/../../etc/passwd")).is_none());
    }

    #[test]
    fn sensitive_segments_rejected() {
        let root = Path::new("/tmp/mem");
        assert!(resolve_safe_memory_file(root, Path::new(".git/config")).is_none());
        assert!(resolve_safe_memory_file(root, Path::new("node_modules/x.md")).is_none());
        assert!(resolve_safe_memory_file(root, Path::new(".zcode/hooks.md")).is_none());
    }

    #[test]
    fn ntfs_ads_and_trailing_normalization_rejected() {
        let root = Path::new("C:/mem");
        assert!(normalize_sensitive_segment(".git:$DATA") == ".git");
        assert!(resolve_safe_memory_file(root, Path::new(".git:$DATA/config")).is_none());
    }

    #[test]
    fn markdown_mutation_requires_md_extension() {
        let root = Path::new("/tmp/mem");
        assert!(is_contained_markdown_mutation(root, Path::new("note.md")));
        assert!(!is_contained_markdown_mutation(root, Path::new("note.txt")));
    }
}
