//! Browser executable discovery: registry-free, probing installed Chromium-family browsers through the
//! standard install paths + environment variables. Mirrors the Windows candidate set of TS
//! `apps/nc-code-cli/packages/adapters/src/browser/executable.ts` (executable.ts:29-37), extended with
//! Edge's LOCALAPPDATA path.
//!
//! Difference from TS: the `chromium.executablePath()` candidate bundled with Playwright does not exist
//! here (removing playwright-core and its browser downloads is precisely R3's goal), so the candidate
//! order is the final order: explicit path → Chrome(32/64) → Edge(32/64 + LOCALAPPDATA) → Chromium.

use std::path::{Path, PathBuf};

/// One executable browser discovery result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserExecutable {
    /// The normalized absolute path of the executable.
    pub path: PathBuf,
    /// The browser family (diagnostics and logs only; the CDP protocol is identical for all three).
    pub family: BrowserFamily,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserFamily {
    Chrome,
    Edge,
    Chromium,
}

impl BrowserFamily {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chrome => "chrome",
            Self::Edge => "edge",
            Self::Chromium => "chromium",
        }
    }
}

/// The Windows standard install-directory candidates (mirrors executable.ts:29-37, plus Edge's LOCALAPPDATA).
fn windows_candidates(env: &dyn Fn(&str) -> Option<String>) -> Vec<(BrowserFamily, PathBuf)> {
    let mut candidates = Vec::new();
    for root_key in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
        let Some(root) = env(root_key) else { continue };
        if root.trim().is_empty() {
            continue;
        }
        let root = Path::new(root.trim());
        candidates.push((
            BrowserFamily::Chrome,
            root.join("Google")
                .join("Chrome")
                .join("Application")
                .join("chrome.exe"),
        ));
        candidates.push((
            BrowserFamily::Chromium,
            root.join("Chromium").join("Application").join("chrome.exe"),
        ));
        candidates.push((
            BrowserFamily::Edge,
            root.join("Microsoft")
                .join("Edge")
                .join("Application")
                .join("msedge.exe"),
        ));
    }
    candidates
}

/// The non-Windows candidates (macOS/Linux kept for cross-platform test environments; unusable from a Windows host).
#[cfg(not(windows))]
fn unix_candidates() -> Vec<(BrowserFamily, PathBuf)> {
    let mut candidates = Vec::new();
    for (family, path) in [
        (BrowserFamily::Chrome, "/usr/bin/google-chrome-stable"),
        (BrowserFamily::Chrome, "/usr/bin/google-chrome"),
        (BrowserFamily::Chromium, "/usr/bin/chromium"),
        (BrowserFamily::Chromium, "/usr/bin/chromium-browser"),
        (BrowserFamily::Chromium, "/snap/bin/chromium"),
        (
            BrowserFamily::Chrome,
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        ),
        (
            BrowserFamily::Chromium,
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ),
        (
            BrowserFamily::Edge,
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        ),
    ] {
        candidates.push((family, PathBuf::from(path)));
    }
    candidates
}

fn is_usable_executable(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) => {
            if !meta.is_file() {
                return false;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                return meta.permissions().mode() & 0o111 != 0;
            }
            #[cfg(not(unix))]
            {
                true
            }
        }
        Err(_) => false,
    }
}

/// Validate an explicit path: it must exist and be a file (mirrors executable.ts:49-62, keeping the error message actionable).
pub fn validate_explicit(path: &Path) -> Result<BrowserExecutable, String> {
    if !path.is_absolute() {
        return Err(format!(
            "Browser executable path must be absolute: {path:?}"
        ));
    }
    if !is_usable_executable(path) {
        return Err(format!(
            "Browser executable is missing or not executable: {path:?}"
        ));
    }
    let family = family_for_executable_name(path);
    Ok(BrowserExecutable {
        path: path.to_path_buf(),
        family,
    })
}

fn family_for_executable_name(path: &Path) -> BrowserFamily {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if name.contains("msedge") {
        BrowserFamily::Edge
    } else if name.contains("chromium") {
        BrowserFamily::Chromium
    } else {
        BrowserFamily::Chrome
    }
}

/// Discover the first usable installed browser in candidate order; with no result return an empty Vec (the caller decides fail-closed).
pub fn discover_installed_browsers() -> Vec<BrowserExecutable> {
    let mut found = Vec::new();
    let env_lookup = |key: &str| std::env::var(key).ok();
    #[cfg(windows)]
    let candidates = windows_candidates(&env_lookup);
    #[cfg(not(windows))]
    let candidates = {
        let _ = env_lookup;
        unix_candidates()
    };
    for (family, path) in candidates {
        if found
            .iter()
            .any(|item: &BrowserExecutable| item.path == path)
        {
            continue; // the same file under several ProgramFiles roots is not reported twice
        }
        if is_usable_executable(&path) {
            found.push(BrowserExecutable { path, family });
        }
    }
    found
}

/// Discover the first usable browser; when none exists return an actionable error message (aligned with
/// executable.ts:84-87, but with the Playwright hint dropped in favour of installing one or passing an explicit path).
pub fn discover_installed_browser() -> Result<BrowserExecutable, String> {
    discover_installed_browsers()
        .into_iter()
        .next()
        .ok_or_else(|| {
            "No installed Chrome, Edge or Chromium executable was found. \
             Install a Chromium-family browser or pass an explicit absolute executable path."
                .to_string()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_candidate_layout_mirrors_ts() {
        // Inject a fake env and verify the candidate generation order and path shapes match executable.ts:29-37.
        let candidates = windows_candidates(&|key| match key {
            "PROGRAMFILES" => Some("C:\\PF".to_string()),
            "PROGRAMFILES(X86)" => Some("C:\\PF86".to_string()),
            "LOCALAPPDATA" => Some("C:\\LA".to_string()),
            _ => None,
        });
        let paths: Vec<String> = candidates
            .iter()
            .map(|(_, p)| p.to_string_lossy().replace('\\', "/"))
            .collect();
        assert!(paths.contains(&"C:/PF/Google/Chrome/Application/chrome.exe".to_string()));
        assert!(paths.contains(&"C:/PF86/Google/Chrome/Application/chrome.exe".to_string()));
        assert!(paths.contains(&"C:/PF/Microsoft/Edge/Application/msedge.exe".to_string()));
        assert!(paths.contains(&"C:/LA/Microsoft/Edge/Application/msedge.exe".to_string()));
        assert!(paths.contains(&"C:/PF/Chromium/Application/chrome.exe".to_string()));
        // Chrome comes before Edge (the family order mirrors the order inside the TS flatMap)
        assert_eq!(candidates[0].0, BrowserFamily::Chrome);
        assert_eq!(candidates[1].0, BrowserFamily::Chromium);
        assert_eq!(candidates[2].0, BrowserFamily::Edge);
    }

    #[test]
    fn empty_env_root_is_skipped() {
        let candidates = windows_candidates(&|key| match key {
            "PROGRAMFILES" => Some("   ".to_string()),
            "PROGRAMFILES(X86)" => None,
            "LOCALAPPDATA" => Some("C:\\LA".to_string()),
            _ => None,
        });
        assert!(candidates.iter().all(|(_, p)| p.starts_with("C:\\LA")));
    }

    #[test]
    fn family_from_name() {
        assert_eq!(
            family_for_executable_name(Path::new("C:\\x\\msedge.exe")),
            BrowserFamily::Edge
        );
        assert_eq!(
            family_for_executable_name(Path::new("/usr/bin/chromium-browser")),
            BrowserFamily::Chromium
        );
        assert_eq!(
            family_for_executable_name(Path::new("/tmp/chrome.exe")),
            BrowserFamily::Chrome
        );
    }

    #[test]
    fn validate_explicit_requires_absolute_existing_file() {
        assert!(validate_explicit(Path::new("relative/chrome.exe")).is_err());
        assert!(validate_explicit(Path::new("/nonexistent/definitely/no-chrome.exe")).is_err());
        // This crate's Cargo.toml is certainly an existing file
        let this_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let manifest = this_dir.join("Cargo.toml");
        let resolved = validate_explicit(&manifest).expect("existing file path");
        assert_eq!(resolved.family, BrowserFamily::Chrome); // the name-inference fallback, diagnostics only
    }

    #[test]
    fn discovery_reports_machine_or_empty_vec() {
        // The machine may or may not have a browser; only assert no panic and deduplication.
        let found = discover_installed_browsers();
        let mut paths = found.iter().map(|b| b.path.clone()).collect::<Vec<_>>();
        let before = paths.len();
        paths.sort();
        paths.dedup();
        assert_eq!(
            paths.len(),
            before,
            "discovery should not return duplicate paths"
        );
    }
}
