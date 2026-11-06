//! Facade registry data - Rust equivalent of `facade/registry.ts`.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorldReadRow {
    pub container: &'static str,
    pub member: &'static str,
    pub op: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactRow {
    pub container: &'static str,
    pub member: &'static str,
    pub op: &'static str,
    pub family: &'static str, // "content" | "preset"
}

pub const WORLD_READ_REGISTRY: &[WorldReadRow] = &[
    WorldReadRow {
        container: "files",
        member: "glob",
        op: "glob",
    },
    WorldReadRow {
        container: "files",
        member: "read",
        op: "read",
    },
    WorldReadRow {
        container: "files",
        member: "grep",
        op: "grep",
    },
    WorldReadRow {
        container: "git",
        member: "changedFiles",
        op: "git-changed-files",
    },
    WorldReadRow {
        container: "git",
        member: "diff",
        op: "git-diff",
    },
    WorldReadRow {
        container: "git",
        member: "status",
        op: "git-status",
    },
    WorldReadRow {
        container: "git",
        member: "log",
        op: "git-log",
    },
    WorldReadRow {
        container: "world",
        member: "run",
        op: "run",
    },
];

pub const ARTIFACT_REGISTRY: &[ArtifactRow] = &[
    ArtifactRow {
        container: "artifact",
        member: "file",
        op: "file",
        family: "content",
    },
    ArtifactRow {
        container: "artifact",
        member: "markdown",
        op: "markdown",
        family: "content",
    },
    ArtifactRow {
        container: "artifact",
        member: "chart",
        op: "chart",
        family: "preset",
    },
    ArtifactRow {
        container: "artifact",
        member: "table",
        op: "table",
        family: "preset",
    },
    ArtifactRow {
        container: "artifact",
        member: "metrics",
        op: "metrics",
        family: "preset",
    },
    ArtifactRow {
        container: "artifact",
        member: "board",
        op: "board",
        family: "preset",
    },
];

pub fn world_read_op(container: &str, member: &str) -> Option<&'static str> {
    WORLD_READ_REGISTRY
        .iter()
        .find(|r| r.container == container && r.member == member)
        .map(|r| r.op)
}

pub fn artifact_row(container: &str, member: &str) -> Option<&'static ArtifactRow> {
    ARTIFACT_REGISTRY
        .iter()
        .find(|r| r.container == container && r.member == member)
}

pub fn artifact_family(op: &str) -> Option<&'static str> {
    ARTIFACT_REGISTRY
        .iter()
        .find(|r| r.op == op)
        .map(|r| r.family)
}

pub fn is_artifact_preset_op(op: &str) -> bool {
    artifact_family(op) == Some("preset")
}

pub fn is_site_producing(container: Option<&str>, member: &str) -> bool {
    match container {
        None => member == "agent" || member == "report",
        Some("Agent") if member == "ask" => true,
        Some(c) => world_read_op(c, member).is_some() || artifact_row(c, member).is_some(),
    }
}

pub fn site_member_names() -> impl Iterator<Item = &'static str> {
    std::iter::once("ask")
        .chain(WORLD_READ_REGISTRY.iter().map(|r| r.member))
        .chain(ARTIFACT_REGISTRY.iter().map(|r| r.member))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn world_read_lookups() {
        assert_eq!(world_read_op("files", "read"), Some("read"));
        assert_eq!(world_read_op("files", "glob"), Some("glob"));
        assert_eq!(world_read_op("git", "log"), Some("git-log"));
        assert_eq!(
            world_read_op("git", "changedFiles"),
            Some("git-changed-files")
        );
        assert_eq!(world_read_op("world", "run"), Some("run"));
        assert_eq!(world_read_op("files", "nope"), None);
        assert_eq!(world_read_op("unknown", "read"), None);
    }

    #[test]
    fn artifact_lookups() {
        let r = artifact_row("artifact", "file").unwrap();
        assert_eq!(r.op, "file");
        assert_eq!(r.family, "content");
        assert!(is_artifact_preset_op("chart"));
        assert!(is_artifact_preset_op("board"));
        assert!(!is_artifact_preset_op("file"));
        assert!(!is_artifact_preset_op("markdown"));
        assert_eq!(artifact_family("table"), Some("preset"));
        assert_eq!(artifact_family("nope"), None);
    }

    #[test]
    fn site_producing_membership() {
        assert!(is_site_producing(None, "agent"));
        assert!(is_site_producing(None, "report"));
        assert!(!is_site_producing(None, "log"));
        assert!(!is_site_producing(None, "phase"));
        assert!(is_site_producing(Some("Agent"), "ask"));
        assert!(is_site_producing(Some("files"), "read"));
        assert!(is_site_producing(Some("world"), "run"));
        assert!(is_site_producing(Some("artifact"), "chart"));
        assert!(!is_site_producing(Some("files"), "write"));
    }

    #[test]
    fn registry_counts_match_ts() {
        assert_eq!(WORLD_READ_REGISTRY.len(), 8);
        assert_eq!(ARTIFACT_REGISTRY.len(), 6);
        let names: std::collections::HashSet<_> = site_member_names().collect();
        assert!(names.contains("ask"));
        assert!(names.contains("glob"));
        assert!(names.contains("run"));
        assert!(names.contains("file"));
        assert!(names.contains("chart"));
    }
}
