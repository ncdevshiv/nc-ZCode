# -*- coding: utf-8 -*-
# git-checkpoint first compile fix: 8 fixes for gitcmd (never involved in compilation) + 6 fixes for repo/helpers/service
# + Cargo two missing dependencies.
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


base = "native/crates/zcode-services-git-checkpoint/"

# ── Cargo: serde (gitcmd’s derive) + zcode-diff (line_stats uses shared diff engine) ──
patch(
    base + "Cargo.toml",
    [
        (
            "serde_json.workspace = true",
            "serde.workspace = true\nserde_json.workspace = true",
            "serde dep",
        ),
        (
            'zcode-service-core = { path = "../zcode-service-core" }',
            'zcode-service-core = { path = "../zcode-service-core" }\n# line_stats_from_contents uses the shared diff engine (jsdiff@9 equivalent implementation, no need to write the second set by yourself). \nzcode-diff = { path = "../zcode-diff" }',
            "zcode-diff dep",
        ),
    ],
)

# ── gitcmd ──
patch(
    base + "src/gitcmd.rs",
    [
        # pinned is Option<OsString>: Some(path)=force this binary, None=use detection
        # (Equivalent to "not installed" when resolved has been initialized to None).
        (
            """ Self {
            pinned: Some(Some(binary.clone())),
            resolved: tokio::sync::OnceCell::new(),
        }""",
            """ Self {
            // pinned: Some(path)=forced binary; None=not forced (with_availability will resolve
            // Initialization, probe is therefore skipped, equivalent to "not installed"). Original writing Some(Some(..)) Yes
            // Option<OsString> always has two layers, and the types do not match.
            pinned: binary.clone(),resolved: tokio::sync::OnceCell::new(),
        }""",
            "with_pinned pinned type",
        ),
        (
            " error.total_message(),",
            " error.to_string(),",
            "io::Error message",
        ),
        (
            "Ok(result) => result.ok().flatten(),",
            "Ok(result) => result,",
            "wait result Option",
        ),
        (
            " child.wait().await.ok().flatten()",
            "child.wait().await.ok()",
            "timeout wait Option",
        ),
        (
            "raw_elapsed_ms: elapsed_ms,\n",
            "",
            "drop unknown field",
        ),
        (
            " let elapsed_ms = started.elapsed().as_millis();\n",
            " let elapsed_ms = started.elapsed().as_millis();\n let _ = elapsed_ms;\n",
            "keep binding used",
        ),
        (
            " .filter(|key| local.contains(&key.to_lowercase()))",
            " .filter(|key| local.contains_key(&key.to_lowercase()))",
            "contains_key",
        ),
    ],
)

# ── repo ──
patch(
    base + "src/repo.rs",
    [
        (
            "use std::path::{Path, PathBuf};",
            "use std::path::Path;",
            "unused PathBuf",
        ),
        (
            " git: GitRepo::new(std::sync::Arc::new(GitCommandProvider::new())),",
            " // GitCommandProvider::new() itself returns Arc<..>.\n git: GitRepo::new(GitCommandProvider::new()),",
            "GitRepo::new arg",
        ),
        (
            """ string_of(from, "checkpointId")?,
            string_of(to, "checkpointId")?,""",""" &string_of(from, "checkpointId")?,
            &string_of(to, "checkpointId")?,""",
            "borrow string_of results",
        ),
        (
            """ let to_oid = oid_of(to)?;
            let mut args: Vec<&str> = vec!["restore", &format!("--source={to_oid}"), "--worktree", "--"];""",
            """ let to_oid = oid_of(to)?;
            let source = format!("--source={to_oid}");
            let mut args: Vec<&str> = vec!["restore", &source, "--worktree", "--"];""",
            "bind source arg",
        ),
    ],
)

# ── helpers: The structure fields returned/read by pub fn must be visible ──
patch(
    base + "src/helpers.rs",
    [
        (
            """struct NameStatusEntry {
    kind: &'static str,
    path: String,
    original_path: Option<String>,
}""",
            """pub struct NameStatusEntry {
    pub kind: &'static str,
    pub path: String,
    pub original_path: Option<String>,
}""",
            "NameStatusEntry pub fields",
        ),
        (
            """struct TreeEntry {
    mode: String,
    object_id: String,
}""",
            """pub struct TreeEntry {
    pub mode: String,
    pub object_id: String,
}""",
            "TreeEntry pub fields",
        ),
    ],
)

# ── service: json! Only used for testing (the parent module leaves Value) ──
patch(
    base + "src/service.rs",
    [
        (
            "use serde_json::{json, Value};",
            "use serde_json::Value;",
            "json import scope",
        ),
        (
            "#[cfg(test)]\nmod tests {\n use super::*;",
            "#[cfg(test)]\nmod tests {\n use super::*;\n use serde_json::json;",
            "tests import json",
        ),
    ],
)

if failures:
    raise SystemExit("FAILED: %d" % failures)
print("all git-checkpoint compile fixes landed")