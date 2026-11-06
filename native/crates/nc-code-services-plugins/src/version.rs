//! Plugin update comparison — port of `adapters/src/plugins/version-compare.ts`.
//!
//! TypeScript uses `semver.coerce(...)` first, so `v1.2`, `release-1.2.3` and
//! `1.2.3-rc.1` all compare as their first numeric triple. That coercion is
//! reimplemented here instead of pulling a semver dependency.

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Triple(u64, u64, u64);

fn coerce(value: &str) -> Option<Triple> {
    let bytes = value.as_bytes();
    let start = bytes.iter().position(|byte| byte.is_ascii_digit())?;
    let mut parts: Vec<u64> = Vec::new();
    let mut index = start;
    while index < bytes.len() && parts.len() < 3 {
        if !bytes[index].is_ascii_digit() {
            break;
        }
        let mut number: u64 = 0;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            number = number
                .saturating_mul(10)
                .saturating_add(u64::from(bytes[index] - b'0'));
            index += 1;
        }
        parts.push(number);
        if index < bytes.len() && bytes[index] == b'.' {
            index += 1;
        } else {
            break;
        }
    }
    while parts.len() < 3 {
        parts.push(0);
    }
    Some(Triple(parts[0], parts[1], parts[2]))
}

/// `comparePluginVersions`: `"none" | "update-available" | "version-changed"`.
pub fn compare_plugin_versions(installed: Option<&str>, latest: Option<&str>) -> &'static str {
    let (Some(installed), Some(latest)) = (installed, latest) else {
        return "none";
    };
    match (coerce(installed), coerce(latest)) {
        (Some(installed), Some(latest)) => {
            if latest > installed {
                "update-available"
            } else {
                "none"
            }
        }
        _ => {
            if installed == latest {
                "none"
            } else {
                "version-changed"
            }
        }
    }
}

/// `comparePluginUpdate`: pick the axis the catalog actually provides.
pub fn compare_plugin_update(
    installed_version: Option<&str>,
    installed_sha: Option<&str>,
    latest_version: Option<&str>,
    latest_sha: Option<&str>,
) -> &'static str {
    if let Some(latest_version) = latest_version {
        return compare_plugin_versions(installed_version, Some(latest_version));
    }
    if let Some(latest_sha) = latest_sha {
        return match installed_sha {
            None => "version-changed",
            Some(installed_sha) if installed_sha == latest_sha => "none",
            Some(_) => "update-available",
        };
    }
    "none"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_comparison_uses_the_first_numeric_triple() {
        assert_eq!(
            compare_plugin_versions(Some("1.2.0"), Some("1.10.0")),
            "update-available"
        );
        assert_eq!(
            compare_plugin_versions(Some("v2.0.0"), Some("2.0.0")),
            "none"
        );
        assert_eq!(
            compare_plugin_versions(Some("release-1.2"), Some("1.3.0")),
            "update-available"
        );
        assert_eq!(
            compare_plugin_versions(Some("no-digits"), Some("other")),
            "version-changed"
        );
        assert_eq!(compare_plugin_versions(None, Some("1.0.0")), "none");
    }

    #[test]
    fn sha_axis_reports_changed_identity() {
        assert_eq!(
            compare_plugin_update(Some("1.0.0"), Some("a"), None, Some("a")),
            "none"
        );
        assert_eq!(
            compare_plugin_update(Some("1.0.0"), Some("a"), None, Some("b")),
            "update-available"
        );
        assert_eq!(
            compare_plugin_update(Some("1.0.0"), None, None, Some("b")),
            "version-changed"
        );
        assert_eq!(
            compare_plugin_update(Some("1.0.0"), None, None, None),
            "none"
        );
    }
}
