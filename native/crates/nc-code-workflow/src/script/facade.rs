//! The NWS facade registry: the one table of callable facade members.
//!
//! Mirrors the TypeScript front-end's `(declaring container, member) -> op` design
//! (`facade/registry.ts`): adding a world-read primitive is adding a row here, not
//! editing the resolver, the lowering and the engine. The keys carry a container on
//! purpose — `git.log` and the top-level `log()` share a member name, and a bare-name
//! table would either mint a world-read site for every progress message or drop
//! `git.log`'s site entirely.
//!
//! Arity bounds and literal-argument requirements live on the row too, because the
//! rule that needs them (a compile diagnostic) is the rule that reads the table.

use crate::types::ArtifactOp;

/// A world-read primitive: `(container, member)` → Boundary A op.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorldReadRow {
    pub container: &'static str,
    pub member: &'static str,
    /// Boundary A op name; not always the member name (`git.log` → `git-log`).
    pub op: &'static str,
    pub min_args: usize,
    pub max_args: usize,
    /// The first argument must be a compile-time string literal.
    pub literal_first_arg: bool,
    /// Whether the primitive is a `world.run` (journal node kind `world-run`,
    /// and the compile-time command authorization gate).
    pub is_run: bool,
}

/// `world.read`-family registry. Adding a primitive is adding a row.
pub const WORLD_READ_ROWS: &[WorldReadRow] = &[
    WorldReadRow {
        container: "files",
        member: "glob",
        op: "glob",
        min_args: 1,
        max_args: 1,
        literal_first_arg: false,
        is_run: false,
    },
    WorldReadRow {
        container: "files",
        member: "read",
        op: "read",
        min_args: 1,
        max_args: 1,
        literal_first_arg: false,
        is_run: false,
    },
    WorldReadRow {
        container: "files",
        member: "grep",
        op: "grep",
        min_args: 1,
        max_args: 2,
        literal_first_arg: false,
        is_run: false,
    },
    WorldReadRow {
        container: "git",
        member: "changedFiles",
        op: "git-changed-files",
        min_args: 0,
        max_args: 1,
        literal_first_arg: false,
        is_run: false,
    },
    WorldReadRow {
        container: "git",
        member: "diff",
        op: "git-diff",
        min_args: 0,
        max_args: 2,
        literal_first_arg: false,
        is_run: false,
    },
    WorldReadRow {
        container: "git",
        member: "status",
        op: "git-status",
        min_args: 0,
        max_args: 0,
        literal_first_arg: false,
        is_run: false,
    },
    WorldReadRow {
        container: "git",
        member: "log",
        op: "git-log",
        min_args: 0,
        max_args: 1,
        literal_first_arg: false,
        is_run: false,
    },
    // `world.run` shares the table and the site-identity mechanism with the reads: the
    // difference between a read and an effect is the authorization surface (a literal
    // command plus the confirmation window) and the journal node kind, not the site.
    WorldReadRow {
        container: "world",
        member: "run",
        op: "run",
        min_args: 1,
        max_args: 3,
        literal_first_arg: true,
        is_run: true,
    },
];

/// An `artifact.*` member: content publishes (effect, through the driver) or a preset
/// declaration (synchronous, no driver). The family decides the lowering, the journal
/// node kind and whether an over-cap call rejects the node or fails the run, so it
/// lives on the row rather than being re-derived at three call sites.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArtifactRow {
    pub member: &'static str,
    pub op: ArtifactOp,
    pub min_args: usize,
    pub max_args: usize,
    /// Whether the third argument is the card options object.
    pub takes_options: bool,
}

/// Artifact registry: the two families in declaration order.
pub const ARTIFACT_ROWS: &[ArtifactRow] = &[
    ArtifactRow {
        member: "file",
        op: ArtifactOp::File,
        min_args: 2,
        max_args: 3,
        takes_options: true,
    },
    ArtifactRow {
        member: "markdown",
        op: ArtifactOp::Markdown,
        min_args: 2,
        max_args: 3,
        takes_options: true,
    },
    ArtifactRow {
        member: "chart",
        op: ArtifactOp::Chart,
        min_args: 2,
        max_args: 2,
        takes_options: false,
    },
    ArtifactRow {
        member: "table",
        op: ArtifactOp::Table,
        min_args: 2,
        max_args: 2,
        takes_options: false,
    },
    ArtifactRow {
        member: "metrics",
        op: ArtifactOp::Metrics,
        min_args: 2,
        max_args: 2,
        takes_options: false,
    },
    ArtifactRow {
        member: "board",
        op: ArtifactOp::Board,
        min_args: 2,
        max_args: 2,
        takes_options: false,
    },
];

/// Look up a world-read row by declaring container and member name.
pub fn world_read_row(container: &str, member: &str) -> Option<&'static WorldReadRow> {
    WORLD_READ_ROWS
        .iter()
        .find(|row| row.container == container && row.member == member)
}

/// Look up an artifact row by member name.
pub fn artifact_row(member: &str) -> Option<&'static ArtifactRow> {
    ARTIFACT_ROWS.iter().find(|row| row.member == member)
}

/// The artifact id charset: `[A-Za-z0-9_.-]`, non-empty, at most
/// `ARTIFACT_CAPS.max_id_length` characters — the same rule the TypeScript compiler
/// enforced, because the set an NWS run can publish is fixed at submit time.
pub fn is_valid_artifact_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= crate::types::ARTIFACT_CAPS.max_id_length
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Member names that exist on a facade container, for a diagnostic that says what the
/// author could have written instead.
pub fn world_read_members(container: &str) -> Vec<&'static str> {
    WORLD_READ_ROWS
        .iter()
        .filter(|row| row.container == container)
        .map(|row| row.member)
        .collect()
}

/// Is this identifier one of the facade container namespaces? Facade containers are
/// values only as receivers of their members, so a bare use is a misuse.
pub fn is_facade_container(name: &str) -> bool {
    matches!(name, "files" | "git" | "world" | "artifact")
}

/// The join construct: `Promise.all([...])`.
pub const PROMISE_CONTAINER: &str = "Promise";
pub const PROMISE_ALL_MEMBER: &str = "all";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn world_read_rows_key_on_container() {
        // `git.log` and the top-level `log()` must not meet in this table.
        assert_eq!(
            world_read_row("git", "log").map(|row| row.op),
            Some("git-log")
        );
        assert_eq!(world_read_row("files", "log"), None);
        assert_eq!(
            world_read_row("world", "run").map(|row| row.op),
            Some("run")
        );
        assert!(world_read_row("world", "run").is_some_and(|row| row.is_run));
    }

    #[test]
    fn artifact_rows_cover_both_families() {
        assert_eq!(
            artifact_row("markdown").map(|row| row.op),
            Some(ArtifactOp::Markdown)
        );
        assert_eq!(
            artifact_row("board").map(|row| row.op),
            Some(ArtifactOp::Board)
        );
        assert_eq!(artifact_row("nope"), None);
        for row in ARTIFACT_ROWS {
            assert_eq!(
                row.min_args, 2,
                "{:?}: ids and payload are both required",
                row.member
            );
        }
    }

    #[test]
    fn artifact_ids_follow_the_submitted_id_rule() {
        assert!(is_valid_artifact_id("summary"));
        assert!(is_valid_artifact_id("perf-board.v2_1"));
        assert!(!is_valid_artifact_id(""));
        assert!(!is_valid_artifact_id("has space"));
        assert!(!is_valid_artifact_id("has/slash"));
        assert!(!is_valid_artifact_id(&"x".repeat(65)));
    }
}
