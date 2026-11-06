//! Node version resolution: probe the raw output of `node --version` and normalize it into a comparable structure.

use std::fmt;

/// The three numeric segments inside `v24.14.0` / `24.14.0` / `v23.0.0-nightly20240101abcdef`.
/// The prerelease suffix is ignored: the kernel only cares about the major-version capability line (see `MIN_SUPPORTED_NODE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl NodeVersion {
    /// Parse the output of `node --version`.
    ///
    /// Accepts only the `v?MAJOR.MINOR.PATCH` prefix (Node's own stable format);
    /// a parse failure is always handled as "unconfirmable" and fails closed at the caller,
    /// never assuming a capability is available while the version is unknown.
    pub fn parse(raw: &str) -> Option<NodeVersion> {
        let trimmed = raw.trim();
        let body = trimmed.strip_prefix('v').unwrap_or(trimmed);
        let mut parts = body.split('.');
        let major = parts.next()?.parse::<u32>().ok()?;
        let minor = parts.next()?.parse::<u32>().ok()?;
        let patch_raw = parts.next()?;
        // Prerelease/build suffixes (`-nightly…`, `+lts`) are cut off the numeric segment.
        let patch_digits: String = patch_raw
            .chars()
            .take_while(|char| char.is_ascii_digit())
            .collect();
        if patch_digits.is_empty() {
            return None;
        }
        let patch = patch_digits.parse::<u32>().ok()?;
        Some(NodeVersion {
            major,
            minor,
            patch,
        })
    }

    /// Whether this version meets the minimum capability line required by the kernel driver.
    pub fn supports_kernel(&self, min: NodeVersion) -> bool {
        *self >= min
    }
}

impl fmt::Display for NodeVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "v{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[cfg(test)]
mod tests {
    use super::NodeVersion;

    #[test]
    fn parses_the_node_version_flag_output() {
        assert_eq!(
            NodeVersion::parse("v24.14.0\n"),
            Some(NodeVersion {
                major: 24,
                minor: 14,
                patch: 0
            })
        );
        assert_eq!(
            NodeVersion::parse("  26.12.1 "),
            Some(NodeVersion {
                major: 26,
                minor: 12,
                patch: 1
            })
        );
    }

    #[test]
    fn strips_prerelease_and_build_suffixes() {
        assert_eq!(
            NodeVersion::parse("v23.0.0-nightly20240101a1b2c3d"),
            Some(NodeVersion {
                major: 23,
                minor: 0,
                patch: 0
            })
        );
        assert_eq!(
            NodeVersion::parse("v22.11.0+experimental"),
            Some(NodeVersion {
                major: 22,
                minor: 11,
                patch: 0
            })
        );
    }

    #[test]
    fn rejects_unparseable_output_instead_of_guessing() {
        // fail-closed: a structurally incomplete string (missing patch, non-numeric, empty) always yields None.
        assert_eq!(NodeVersion::parse("v24.14"), None);
        assert_eq!(NodeVersion::parse("node"), None);
        assert_eq!(NodeVersion::parse(""), None);
        assert_eq!(NodeVersion::parse("v24..1"), None);
        assert_eq!(NodeVersion::parse("v24.1."), None);
    }

    #[test]
    fn orders_versions_for_candidate_ranking() {
        let old = NodeVersion {
            major: 18,
            minor: 0,
            patch: 0,
        };
        let new = NodeVersion {
            major: 24,
            minor: 0,
            patch: 0,
        };
        assert!(old < new);
        assert!(new.supports_kernel(old));
        assert_eq!(old.to_string(), "v18.0.0");
    }
}
