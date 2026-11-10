//! Git worktree manager: one isolated worktree per task, commits, changed-file
//! manifests and integration merges. All git invocations are real `git` processes.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;

use crate::error::{NcagError, Result};

#[derive(Debug, Clone)]
pub struct GitRepo {
    pub root: PathBuf,
    pub worktrees_dir: PathBuf,
}

pub struct GitOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl GitOutput {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
}

pub fn run_git(cwd: &Path, args: &[&str]) -> Result<GitOutput> {
    let mut command = Command::new("git");
    // Worktrees live under `.git/ncag-worktrees/<task>`; a checked-out tree can hold
    // paths deeper than the platform's legacy 260-char limit (a committed `target/`
    // tree did on Windows: "Filename too long"). Long paths are a per-invocation
    // config, so this never mutates the repository.
    command.args(["-c", "core.longpaths=true"]).args(args);
    command.current_dir(cwd);
    #[cfg(windows)]
    {
        #[allow(unused_imports)]
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let output = command.output().map_err(|error| {
        NcagError::new("git_unavailable", format!("git could not run: {error}"))
    })?;
    Ok(GitOutput {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).replace('\r', ""),
        stderr: String::from_utf8_lossy(&output.stderr).replace('\r', ""),
    })
}

/// Build-output directories that must never enter a task commit or the change
/// manifest: these are produced by the build/test commands the agents are required to
/// run, never by hand.
///
/// WHY: agents legitimately run `cargo check` / `npm test` inside their worktree, and
/// `git add -A` then committed the output. The integration merge checked that tree out
/// again and failed with "Filename too long" on Windows (observed live), and the
/// changed-file manifest reported hundreds of artifact files as "the change".
pub const ARTIFACT_DIRS: &[&str] = &[
    "target",
    "node_modules",
    "__pycache__",
    ".pytest_cache",
    ".venv",
    "venv",
    ".next",
    ".turbo",
];

/// `[".", ":(exclude)target", …]` for `git add` / `git status` pathspecs.
fn artifact_pathspecs() -> Vec<String> {
    let mut specs = vec![".".to_string()];
    for dir in ARTIFACT_DIRS {
        specs.push(format!(":(exclude){dir}"));
        specs.push(format!(":(exclude){dir}/**"));
    }
    specs
}

/// Artifact-excluding `git <args> -- <pathspecs>` argv. git treats a comma-joined
/// pathspec as one literal path (`Invalid path '.:/(exclude)target…'`), so every
/// pathspec must be its own argument.
fn scoped_argv<'a>(base: &[&'a str], specs: &'a [String]) -> Vec<&'a str> {
    let mut argv: Vec<&str> = base.to_vec();
    argv.push("--");
    argv.extend(specs.iter().map(String::as_str));
    argv
}

/// `git status --porcelain -- <pathspecs>` with the artifact exclusions applied.
fn status_scoped(worktree: &Path) -> Result<GitOutput> {
    let specs = artifact_pathspecs();
    run_git(worktree, &scoped_argv(&["status", "--porcelain"], &specs))
}

/// True when a repository-relative path is build output the change manifest must omit.
pub fn is_artifact_path(path: &str) -> bool {
    let trimmed = path.trim().replace('\\', "/");
    let trimmed = trimmed.trim_start_matches("./");
    ARTIFACT_DIRS.iter().any(|dir| {
        trimmed == *dir
            || trimmed.starts_with(&format!("{dir}/"))
            || trimmed.starts_with(&format!("{dir}\\"))
    })
}

impl GitRepo {
    pub fn open(root: &Path) -> Result<Self> {
        let check = run_git(root, &["rev-parse", "--is-inside-work-tree"])?;
        if !check.ok() || check.stdout.trim() != "true" {
            return Err(NcagError::invalid(format!(
                "{} is not a git worktree: {}",
                root.display(),
                check.stderr.trim()
            )));
        }
        let common = run_git(root, &["rev-parse", "--git-common-dir"])?;
        let common_dir = PathBuf::from(common.stdout.trim());
        let common_dir = if common_dir.is_absolute() {
            common_dir
        } else {
            root.join(common_dir)
        };
        let worktrees_dir = common_dir.join("ncag-worktrees");
        Ok(Self {
            root: root.to_path_buf(),
            worktrees_dir,
        })
    }

    pub fn head(&self) -> Result<String> {
        let output = run_git(&self.root, &["rev-parse", "HEAD"])?;
        if !output.ok() {
            return Err(NcagError::new(
                "git_failed",
                format!("git rev-parse HEAD failed: {}", output.stderr.trim()),
            ));
        }
        Ok(output.stdout.trim().to_string())
    }

    pub fn current_branch(&self) -> Result<String> {
        let output = run_git(&self.root, &["rev-parse", "--abbrev-ref", "HEAD"])?;
        Ok(output.stdout.trim().to_string())
    }

    /// Create (or attach) an isolated worktree for a task branch at `base`.
    pub fn create_worktree(&self, name: &str, base: &str) -> Result<(PathBuf, String)> {
        std::fs::create_dir_all(&self.worktrees_dir)?;
        let sanitized: String = name
            .chars()
            .map(|ch| {
                if ch.is_alphanumeric() || ch == '-' {
                    ch
                } else {
                    '-'
                }
            })
            .collect();
        let branch = format!("ncag/{}", sanitized);
        let path = self.worktrees_dir.join(&sanitized);
        if path.exists() {
            // Reuse an existing worktree only when it belongs to the same branch; otherwise
            // prune it first so a stale directory never silently hosts a different task.
            let probe = run_git(&path, &["rev-parse", "--abbrev-ref", "HEAD"])?;
            if probe.ok() && probe.stdout.trim() == branch {
                return Ok((path, branch));
            }
            let _ = run_git(
                &self.root,
                &["worktree", "remove", "--force", &path.to_string_lossy()],
            );
            let _ = std::fs::remove_dir_all(&path);
        }
        let branch_exists = run_git(
            &self.root,
            &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
        )?
        .ok();
        let path_text = path.to_string_lossy().to_string();
        let output = if branch_exists {
            run_git(&self.root, &["worktree", "add", &path_text, &branch])?
        } else {
            run_git(
                &self.root,
                &["worktree", "add", "-b", &branch, &path_text, base],
            )?
        };
        if !output.ok() {
            return Err(NcagError::new(
                "git_failed",
                format!("git worktree add failed: {}", output.stderr.trim()),
            ));
        }
        Ok((path, branch))
    }

    pub fn remove_worktree(&self, path: &Path) -> Result<()> {
        let _ = run_git(
            &self.root,
            &["worktree", "remove", "--force", &path.to_string_lossy()],
        )?;
        let _ = run_git(&self.root, &["worktree", "prune"]);
        Ok(())
    }

    /// Changed files relative to `base` (staged + unstaged + untracked), excluding build
    /// output (`ARTIFACT_DIRS`) so the manifest and the commit only describe real work.
    pub fn changed_files(&self, worktree: &Path, base: &str) -> Result<Vec<String>> {
        let diff = status_scoped(worktree)?;
        if !diff.ok() {
            return Err(NcagError::new(
                "git_failed",
                format!("git status failed: {}", diff.stderr.trim()),
            ));
        }
        let mut files: Vec<String> = Vec::new();
        for line in diff.stdout.lines() {
            if line.len() < 4 {
                continue;
            }
            let path = line[3..].trim();
            let path = path.split(" -> ").last().unwrap_or(path).trim();
            let normalized = path.trim_matches('"').replace('\\', "/");
            if is_artifact_path(&normalized) {
                continue;
            }
            if !files.contains(&normalized) {
                files.push(normalized);
            }
        }
        // Also compare against the base commit so a commit made by the agent is visible.
        let committed = run_git(worktree, &["diff", "--name-only", base, "HEAD"])?;
        if committed.ok() {
            for line in committed.stdout.lines() {
                let normalized = line.trim().replace('\\', "/");
                if normalized.is_empty() || is_artifact_path(&normalized) {
                    continue;
                }
                if !files.contains(&normalized) {
                    files.push(normalized);
                }
            }
        }
        files.sort();
        Ok(files)
    }

    pub fn commit_all(&self, worktree: &Path, message: &str) -> Result<Option<String>> {
        let specs = artifact_pathspecs();
        let add = run_git(worktree, &scoped_argv(&["add", "-A"], &specs))?;
        if !add.ok() {
            return Err(NcagError::new(
                "git_failed",
                format!("git add failed: {}", add.stderr.trim()),
            ));
        }
        let status = status_scoped(worktree)?;
        if status.stdout.trim().is_empty() {
            let head = run_git(worktree, &["rev-parse", "HEAD"])?;
            return Ok(Some(head.stdout.trim().to_string()));
        }
        let commit = run_git(worktree, &["commit", "-m", message])?;
        if !commit.ok() {
            // Machines without a global git identity still need task commits: retry with an
            // explicit NC-AG identity only when git asked for one (never silently elsewhere).
            if commit.stderr.contains("Author identity unknown")
                || commit.stderr.contains("Please tell me who you are")
            {
                let retried = run_git(
                    worktree,
                    &[
                        "-c",
                        "user.name=NC-AG",
                        "-c",
                        "user.email=ncag@localhost",
                        "commit",
                        "-m",
                        message,
                    ],
                )?;
                if !retried.ok() {
                    return Err(NcagError::new(
                        "git_failed",
                        format!("git commit failed: {}", retried.stderr.trim()),
                    ));
                }
            } else {
                return Err(NcagError::new(
                    "git_failed",
                    format!("git commit failed: {}", commit.stderr.trim()),
                ));
            }
        }
        let head = run_git(worktree, &["rev-parse", "HEAD"])?;
        Ok(Some(head.stdout.trim().to_string()))
    }

    /// Merge a task branch into the project branch in a scratch worktree. Returns the
    /// resulting commit, the conflict list and the scratch path used (caller removes it).
    pub fn try_merge(
        &self,
        project_branch: &str,
        task_branch: &str,
        scratch_name: &str,
    ) -> Result<MergeAttempt> {
        std::fs::create_dir_all(&self.worktrees_dir)?;
        let scratch = self.worktrees_dir.join(scratch_name);
        if scratch.exists() {
            let _ = run_git(
                &self.root,
                &["worktree", "remove", "--force", &scratch.to_string_lossy()],
            );
            let _ = std::fs::remove_dir_all(&scratch);
        }
        let scratch_text = scratch.to_string_lossy().to_string();
        let add = run_git(
            &self.root,
            &["worktree", "add", "--detach", &scratch_text, project_branch],
        )?;
        if !add.ok() {
            return Err(NcagError::new(
                "git_failed",
                format!("scratch worktree add failed: {}", add.stderr.trim()),
            ));
        }
        let merge = run_git(
            &scratch,
            &[
                "-c",
                "user.name=NC-AG",
                "-c",
                "user.email=ncag@localhost",
                "merge",
                "--no-ff",
                "--no-commit",
                "--no-edit",
                task_branch,
            ],
        )?;
        if !merge.ok() {
            let conflicts = run_git(&scratch, &["diff", "--name-only", "--diff-filter=U"])?;
            let conflicted: Vec<String> = conflicts
                .stdout
                .lines()
                .map(|line| line.trim().replace('\\', "/"))
                .filter(|line| !line.is_empty())
                .collect();
            let abort = run_git(&scratch, &["merge", "--abort"])?;
            let _ = run_git(
                &self.root,
                &["worktree", "remove", "--force", &scratch_text],
            );
            let _ = run_git(&self.root, &["worktree", "prune"]);
            return Ok(MergeAttempt {
                merged: false,
                conflicts: if conflicted.is_empty() {
                    vec!["merge failed without textual conflict markers".to_string()]
                } else {
                    conflicted
                },
                commit: None,
                detail: format!("{}{}", merge.stdout.trim(), abort.stderr.trim()),
            });
        }
        let commit = run_git(&scratch, &["rev-parse", "HEAD"])?;
        Ok(MergeAttempt {
            merged: true,
            conflicts: Vec::new(),
            commit: Some(commit.stdout.trim().to_string()),
            detail: format!(
                "merge prepared in {scratch_name}; commit {} pending",
                commit.stdout.trim()
            ),
        })
    }

    /// Finalize a prepared merge: run the commit in the scratch worktree, then move the
    /// project branch to it and fast-forward the root checkout (if it is on that branch).
    pub fn finalize_merge(
        &self,
        scratch_name: &str,
        project_branch: &str,
        message: &str,
    ) -> Result<String> {
        let scratch = self.worktrees_dir.join(scratch_name);
        // A merge whose tree equals the project branch (both tasks made the same change)
        // has nothing to commit: integrate the existing head instead of failing.
        let staged = run_git(&scratch, &["diff", "--cached", "--quiet"])?;
        if staged.code == 0 {
            let head = run_git(&scratch, &["rev-parse", "HEAD"])?;
            if !head.ok() {
                return Err(NcagError::new(
                    "git_failed",
                    "rev-parse after no-op merge failed",
                ));
            }
            let sha = head.stdout.trim().to_string();
            let _ = run_git(
                &self.root,
                &["worktree", "remove", "--force", &scratch.to_string_lossy()],
            );
            let _ = run_git(&self.root, &["worktree", "prune"]);
            return Ok(sha);
        }
        let commit = run_git(
            &scratch,
            &[
                "-c",
                "user.name=NC-AG",
                "-c",
                "user.email=ncag@localhost",
                "commit",
                "-m",
                message,
            ],
        )?;
        if !commit.ok() {
            return Err(NcagError::new(
                "git_failed",
                format!("merge commit failed: {}", commit.stderr.trim()),
            ));
        }
        let head = run_git(&scratch, &["rev-parse", "HEAD"])?;
        if !head.ok() {
            return Err(NcagError::new("git_failed", "rev-parse after merge failed"));
        }
        let sha = head.stdout.trim().to_string();
        let update = run_git(
            &self.root,
            &["update-ref", &format!("refs/heads/{project_branch}"), &sha],
        )?;
        if !update.ok() {
            return Err(NcagError::new(
                "git_failed",
                format!("update-ref failed: {}", update.stderr.trim()),
            ));
        }
        // Keep the operator's checkout usable when it is on the project branch, but never
        // destroy local work: only a clean checkout is fast-forwarded; a dirty one is left
        // untouched (the integration row carries the resulting commit either way).
        if self.current_branch()? == project_branch {
            let status = run_git(&self.root, &["status", "--porcelain"])?;
            if status.ok() && status.stdout.trim().is_empty() {
                // `--ff-only` refuses to rewrite a diverged checkout instead of resetting it.
                let _ = run_git(&self.root, &["merge", "--ff-only", &sha])?;
            }
        }
        let _ = run_git(
            &self.root,
            &["worktree", "remove", "--force", &scratch.to_string_lossy()],
        );
        let _ = run_git(&self.root, &["worktree", "prune"]);
        Ok(sha)
    }

    /// Abort a prepared merge and drop the scratch worktree.
    pub fn abort_merge(&self, scratch_name: &str) -> Result<()> {
        let scratch = self.worktrees_dir.join(scratch_name);
        let _ = run_git(&scratch, &["merge", "--abort"]);
        let _ = run_git(
            &self.root,
            &["worktree", "remove", "--force", &scratch.to_string_lossy()],
        );
        let _ = run_git(&self.root, &["worktree", "prune"]);
        Ok(())
    }

    /// A new base commit for the project after an integration.
    pub fn advance_base(&self, project_branch: &str) -> Result<String> {
        let output = run_git(
            &self.root,
            &["rev-parse", &format!("refs/heads/{project_branch}")],
        )?;
        if !output.ok() {
            return Err(NcagError::new(
                "git_failed",
                format!(
                    "cannot resolve obj {project_branch}: {}",
                    output.stderr.trim()
                ),
            ));
        }
        Ok(output.stdout.trim().to_string())
    }

    /// Ensure the project branch exists; returns that branch name and its base commit.
    pub fn ensure_project_branch(&self, preferred: &str) -> Result<(String, String)> {
        let branch = preferred.trim();
        let branch = if branch.is_empty() {
            let current = self.current_branch()?;
            if current == "HEAD" {
                "ncag/main".to_string()
            } else {
                current
            }
        } else {
            branch.to_string()
        };
        if branch != "HEAD" {
            let exists = run_git(
                &self.root,
                &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
            )?
            .ok();
            if !exists {
                let head = self.head()?;
                let create = run_git(&self.root, &["branch", &branch, &head])?;
                if !create.ok() {
                    return Err(NcagError::new(
                        "git_failed",
                        format!(
                            "cannot create project branch {branch}: {}",
                            create.stderr.trim()
                        ),
                    ));
                }
            }
        }
        let base = run_git(&self.root, &["rev-parse", &format!("refs/heads/{branch}")])?
            .stdout
            .trim()
            .to_string();
        let base = if base.is_empty() { self.head()? } else { base };
        Ok((branch, base))
    }
}

#[derive(Debug, Clone)]
pub struct MergeAttempt {
    pub merged: bool,
    pub conflicts: Vec<String>,
    pub commit: Option<String>,
    pub detail: String,
}

/// Detect files claimed by more than one live task (semantic overlap pre-check).
pub fn overlapping(a: &[String], b: &[String]) -> Vec<String> {
    let mut overlaps: Vec<String> = a
        .iter()
        .filter(|file| b.iter().any(|other| *other == **file))
        .cloned()
        .collect();
    overlaps.sort();
    overlaps.dedup();
    overlaps
}

pub fn describe_attempt(attempt: &MergeAttempt) -> serde_json::Value {
    json!({
        "merged": attempt.merged,
        "conflicts": attempt.conflicts,
        "commit": attempt.commit,
        "detail": attempt.detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let out = run_git(dir, &["init", "-b", "main"]).unwrap();
        assert!(out.ok(), "{}", out.stderr);
        std::fs::write(dir.join("README.md"), "# repo\n").unwrap();
        assert!(run_git(dir, &["add", "-A"]).unwrap().ok());
        let commit = run_git(
            dir,
            &[
                "-c",
                "user.name=NCAG",
                "-c",
                "user.email=ncag@example.invalid",
                "commit",
                "-m",
                "init",
            ],
        )
        .unwrap();
        assert!(commit.ok(), "{}", commit.stderr);
    }

    #[test]
    fn worktrees_are_isolated_and_commits_land_on_branches() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        init_repo(&root);
        let repo = GitRepo::open(&root).unwrap();
        let head = repo.head().unwrap();
        let (path, branch) = repo.create_worktree("t1", &head).unwrap();
        std::fs::write(path.join("a.txt"), "one\n").unwrap();
        let commit = repo.commit_all(&path, "task t1").unwrap().unwrap();
        assert_eq!(repo.changed_files(&path, &head).unwrap(), vec!["a.txt"]);
        // The root checkout stays untouched.
        assert!(!root.join("a.txt").exists());
        let attempt = repo.try_merge("main", &branch, "scratch-1").unwrap();
        assert!(attempt.merged, "{:?}", attempt);
        let merged = repo
            .finalize_merge("scratch-1", "main", "merge t1")
            .unwrap();
        assert!(merged.len() >= 7);
        assert!(merged != commit);
    }

    #[test]
    fn build_output_never_enters_a_commit_or_the_change_manifest() {
        // Agents must run `cargo check` / `npm test` inside the worktree; the resulting
        // `target/` tree must not be committed (the merge would then fail to check that
        // tree out on Windows: "Filename too long"), and it is not "the change".
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        init_repo(&root);
        let repo = GitRepo::open(&root).unwrap();
        let head = repo.head().unwrap();
        let (path, _branch) = repo.create_worktree("t-artifacts", &head).unwrap();
        std::fs::write(path.join("src.rs"), "pub fn f() {}\n").unwrap();
        std::fs::create_dir_all(path.join("target").join("debug")).unwrap();
        std::fs::write(path.join("target").join("debug").join("out.rlib"), "bin\n").unwrap();
        std::fs::create_dir_all(path.join("node_modules").join("dep")).unwrap();
        std::fs::write(
            path.join("node_modules").join("dep").join("index.js"),
            "x\n",
        )
        .unwrap();
        let commit = repo.commit_all(&path, "task artifacts").unwrap().unwrap();
        assert_eq!(
            repo.changed_files(&path, &head).unwrap(),
            vec!["src.rs"],
            "only the real source change is a change"
        );
        let files = run_git(&path, &["show", "--name-only", "--pretty=format:", &commit]).unwrap();
        assert_eq!(files.stdout.trim(), "src.rs", "{}", files.stdout);
        assert!(
            run_git(&path, &["status", "--porcelain"])
                .unwrap()
                .stdout
                .contains("target"),
            "build output still exists in the worktree, just uncommitted"
        );
    }

    #[test]
    fn conflicting_changes_are_reported_as_textual_conflicts() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        init_repo(&root);
        let repo = GitRepo::open(&root).unwrap();
        let head = repo.head().unwrap();
        let (a, branch_a) = repo.create_worktree("ta", &head).unwrap();
        let (b, branch_b) = repo.create_worktree("tb", &head).unwrap();
        std::fs::write(a.join("README.md"), "# A\n").unwrap();
        repo.commit_all(&a, "A").unwrap();
        std::fs::write(b.join("README.md"), "# B\n").unwrap();
        repo.commit_all(&b, "B").unwrap();
        let attempt = repo.try_merge("main", &branch_a, "scratch-a").unwrap();
        assert!(attempt.merged);
        repo.finalize_merge("scratch-a", "main", "merge A").unwrap();
        let attempt = repo.try_merge("main", &branch_b, "scratch-b").unwrap();
        assert!(!attempt.merged);
        assert_eq!(attempt.conflicts, vec!["README.md"]);
    }

    #[test]
    fn overlap_detection_is_sorted_and_unique() {
        let a = vec!["a.rs".to_string(), "b.rs".to_string(), "b.rs".to_string()];
        let b = vec!["b.rs".to_string(), "c.rs".to_string()];
        assert_eq!(overlapping(&a, &b), vec!["b.rs"]);
    }
}
