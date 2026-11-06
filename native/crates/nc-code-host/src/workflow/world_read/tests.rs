//! Unit tests for `world_read`: real files, a real git repository, a real child process.
//!
//! Every test works inside its own temporary directory; the git tests print a skip when git is unavailable (a CI environment may still lack git).

use std::path::{Path, PathBuf};

use nc_code_workflow::types::{WorkflowErrorCode, WORLD_READ_CAPS};
use serde_json::json;

use super::{execute, WorldReadScope, READ_MAX_BYTES};

fn temp_workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zcode-workflow-world-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&dir).expect("temp workspace");
    dir
}

fn scope(workspace: &Path) -> WorldReadScope {
    WorldReadScope::new(workspace.to_path_buf(), None)
}

fn write(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent dirs");
    }
    std::fs::write(path, content).expect("write fixture");
}

async fn call(
    scope: &WorldReadScope,
    op: &str,
    args: Vec<serde_json::Value>,
) -> Result<serde_json::Value, nc_code_workflow::types::WorkflowError> {
    execute(scope, op, &args).await
}

#[tokio::test]
async fn read_rejects_paths_outside_the_workspace() {
    let root = temp_workspace("escape");
    write(&root, "inside.txt", "ok");
    std::fs::write(root.parent().unwrap().join("outside.txt"), "secret").ok();
    let scope = scope(&root);

    let error = call(&scope, "read", vec![json!("../outside.txt")])
        .await
        .expect_err("escape must be rejected");
    assert_eq!(error.code, WorkflowErrorCode::DriverError);
    assert!(
        error.message.contains("outside the workspace"),
        "{}",
        error.message
    );

    let absolute = root.join("inside.txt").to_string_lossy().to_string();
    assert_eq!(
        call(&scope, "read", vec![json!(absolute)]).await.unwrap(),
        json!("ok"),
        "an absolute path inside the workspace resolves like the TS driver (resolve(cwd, arg))"
    );
    assert!(
        call(&scope, "glob", vec![json!("/etc/*")]).await.is_err(),
        "absolute glob patterns are rejected"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn glob_grep_and_read_work_on_real_files() {
    let root = temp_workspace("files");
    write(&root, "a.rs", "fn a() {} // needle\n");
    write(&root, "nested/b.rs", "fn b() {}\n// needle here\n");
    write(&root, "notes.txt", "needle in text\n");
    let scope = scope(&root);

    let globbed = call(&scope, "glob", vec![json!("**/*.rs")]).await.unwrap();
    assert_eq!(globbed, json!(["a.rs", "nested/b.rs"]));

    let read = call(&scope, "read", vec![json!("nested/b.rs")])
        .await
        .unwrap();
    assert_eq!(read, json!("fn b() {}\n// needle here\n"));

    let matches = call(&scope, "grep", vec![json!("needle"), json!("**/*.rs")])
        .await
        .unwrap();
    let matches = matches.as_array().expect("array");
    assert_eq!(matches.len(), 2, "{matches:?}");
    assert_eq!(matches[0]["path"], json!("a.rs"));
    assert_eq!(matches[0]["line"], json!(1));
    assert_eq!(matches[1]["path"], json!("nested/b.rs"));
    assert_eq!(matches[1]["line"], json!(2));
    assert!(
        matches[0]["text"].as_str().unwrap().contains("needle"),
        "line text is preserved"
    );

    // The combination of glob's `?` with a directory-level `**`.
    let single = call(&scope, "glob", vec![json!("?.rs")]).await.unwrap();
    assert_eq!(single, json!(["a.rs"]));
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn caps_reject_instead_of_truncating() {
    let root = temp_workspace("caps");
    // files.read：4 MiB + 1。
    let big = "x".repeat(READ_MAX_BYTES + 1);
    write(&root, "big.txt", &big);
    let scope = scope(&root);
    let error = call(&scope, "read", vec![json!("big.txt")])
        .await
        .expect_err("over-cap read must fail");
    assert_eq!(error.code, WorkflowErrorCode::WorldReadCapExceeded);
    assert!(error.message.contains("cap"), "{}", error.message);

    // files.grep: cap+1 hits are rejected outright (not truncated down to cap).
    let cap = WORLD_READ_CAPS.grep_max_matches;
    let lines: String = (0..=cap).map(|_| "needle\n").collect();
    write(&root, "many.txt", &lines);
    let error = call(&scope, "grep", vec![json!("needle")])
        .await
        .expect_err("over-cap grep must fail");
    assert_eq!(error.code, WorkflowErrorCode::WorldReadCapExceeded);
    assert!(error.message.contains("over"), "{}", error.message);

    // files.grep: the hit count is within the cap, but a serialized size over 256KB is still rejected.
    let wide = format!("needle {}\n", "y".repeat(500));
    let lines: String = (0..1000).map(|_| wide.clone()).collect();
    write(&root, "wide.txt", &lines);
    let error = call(&scope, "grep", vec![json!("needle")])
        .await
        .expect_err("serialized cap must fail");
    assert_eq!(error.code, WorkflowErrorCode::WorldReadCapExceeded);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn glob_rejects_more_than_the_cap() {
    let root = temp_workspace("glob-cap");
    for index in 0..=WORLD_READ_CAPS.glob_max_files {
        write(&root, &format!("files/f{index}.txt"), "x");
    }
    let scope = scope(&root);
    let error = call(&scope, "glob", vec![json!("files/*.txt")])
        .await
        .expect_err("over-cap glob must fail");
    assert_eq!(error.code, WorkflowErrorCode::WorldReadCapExceeded);
    let _ = std::fs::remove_dir_all(&root);
}

fn git(workspace: &Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .args(args)
        .current_dir(workspace)
        .env("GIT_AUTHOR_NAME", "tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.invalid")
        .env("GIT_COMMITTER_NAME", "tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.invalid")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn git_ops_read_a_real_repository() {
    if !git_available() {
        eprintln!("skipping git_ops_read_a_real_repository: git is not available");
        return;
    }
    let root = temp_workspace("git");
    assert!(git(&root, &["init", "-q"]), "git init");
    write(&root, "tracked.txt", "first\n");
    assert!(git(&root, &["add", "tracked.txt"]), "git add");
    assert!(
        git(&root, &["commit", "-q", "-m", "first commit"]),
        "git commit"
    );
    write(&root, "tracked.txt", "first\nsecond\n");
    write(&root, "untracked.txt", "new\n");

    let scope = scope(&root);

    let status = call(&scope, "git-status", vec![]).await.unwrap();
    assert_eq!(status["clean"], json!(false), "{status}");
    assert_eq!(status["unstaged"], json!(["tracked.txt"]), "{status}");
    assert_eq!(status["untracked"], json!(["untracked.txt"]), "{status}");
    assert!(
        status["branch"].as_str().is_some(),
        "the default branch must be reported: {status}"
    );

    let changed = call(&scope, "git-changed-files", vec![]).await.unwrap();
    assert_eq!(changed, json!(["tracked.txt", "untracked.txt"]));

    let diff = call(&scope, "git-diff", vec![json!(null), json!("tracked.txt")])
        .await
        .unwrap();
    let diff = diff.as_str().expect("patch text");
    assert!(diff.contains("+second"), "{diff}");

    let log = call(&scope, "git-log", vec![json!(5)]).await.unwrap();
    let log = log.as_array().expect("commits");
    assert_eq!(log.len(), 1);
    assert_eq!(log[0]["subject"], json!("first commit"));
    assert!(
        !log[0]["hash"].as_str().unwrap_or_default().is_empty(),
        "{log:?}"
    );

    // base validation happens before git is run.
    let error = call(&scope, "git-diff", vec![json!("-pwn")])
        .await
        .expect_err("option-shaped ref must be rejected");
    assert_eq!(error.code, WorkflowErrorCode::DriverError);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn world_run_is_fail_closed_and_zero_exit_is_a_value() {
    let root = temp_workspace("run");
    let undeclared = scope(&root);
    let error = call(&undeclared, "run", vec![json!("git"), json!(["--version"])])
        .await
        .expect_err("no declared commands -> everything is rejected");
    assert_eq!(error.code, WorkflowErrorCode::DriverError);
    assert!(error.message.contains("not in the declared set"));

    let declared = WorldReadScope::new(
        root.clone(),
        Some(["git".to_string()].into_iter().collect()),
    );
    let value = call(&declared, "run", vec![json!("git"), json!(["--version"])])
        .await
        .expect("declared command runs");
    assert_eq!(value["exitCode"], json!(0));
    assert!(
        value["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("git version"),
        "{value}"
    );

    // A non-zero exit is a value rather than an error (the normal path of a gated loop).
    let value = call(
        &declared,
        "run",
        vec![
            json!("git"),
            json!(["rev-parse", "--verify", "no-such-ref"]),
        ],
    )
    .await
    .expect("non-zero exit is an observation");
    assert_ne!(value["exitCode"], json!(0), "{value}");
    assert!(!value["stderr"].as_str().unwrap_or_default().is_empty());

    // A spawn failure is an error.
    let nope = WorldReadScope::new(
        root.clone(),
        Some(
            ["definitely-not-a-real-command".to_string()]
                .into_iter()
                .collect(),
        ),
    );
    let error = call(&nope, "run", vec![json!("definitely-not-a-real-command")])
        .await
        .expect_err("spawn failure is an error");
    assert_eq!(error.code, WorkflowErrorCode::DriverError);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn world_run_rejects_stdout_over_the_cap() {
    let root = temp_workspace("run-cap");
    // Produce >256KB of output platform-independently: Windows uses cmd's for loop, Unix uses sh + yes/head.
    #[cfg(windows)]
    let (cmd, args) = (
        "cmd",
        vec![
            "/D".to_string(),
            "/C".to_string(),
            "for /L %i in (1,1,60000) do @echo xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
                .to_string(),
        ],
    );
    #[cfg(not(windows))]
    let (cmd, args) = (
        "sh",
        vec![
            "-c".to_string(),
            "yes xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx | head -n 60000".to_string(),
        ],
    );
    let scope = WorldReadScope::new(root.clone(), Some([cmd.to_string()].into_iter().collect()));
    let result = call(&scope, "run", vec![json!(cmd), json!(args)]).await;
    match result {
        Ok(value) => panic!("stdout over the cap must be rejected, got {value}"),
        Err(error) => {
            assert_eq!(
                error.code,
                WorkflowErrorCode::WorldReadCapExceeded,
                "{}",
                error.message
            );
            assert!(error.message.contains("stdout"), "{}", error.message);
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn arg_shapes_fail_loudly_instead_of_coercing() {
    let root = temp_workspace("args");
    let scope = scope(&root);
    // arity mismatch
    assert!(call(&scope, "read", vec![]).await.is_err());
    assert!(call(&scope, "read", vec![json!("a"), json!("b")])
        .await
        .is_err());
    // type mismatch (never coerced with String(...))
    assert!(call(&scope, "read", vec![json!(42)]).await.is_err());
    assert!(call(&scope, "grep", vec![json!("x"), json!(5)])
        .await
        .is_err());
    assert!(call(&scope, "git-log", vec![json!(0)]).await.is_err());
    assert!(call(
        &scope,
        "git-log",
        vec![json!(WORLD_READ_CAPS.git_log_max_count + 1)]
    )
    .await
    .is_err());
    assert!(
        call(&scope, "git-status", vec![json!("x")]).await.is_err(),
        "no-arg op rejects extra args"
    );
    assert!(call(&scope, "nope", vec![]).await.is_err());
    let _ = std::fs::remove_dir_all(&root);
}
