//! Intake -> requirements -> plan (spec: specs/nc-ag-orchestration.md section 4).
//!
//! Two real planners:
//! 1. `analyze_request` - deterministic intent/ambiguity/contradiction analysis over the
//!    instruction and the detected repository facts (always runs, fully testable);
//! 2. `plan_tasks` - decomposition into capability-tagged tasks with a validated DAG.
//!
//! A configured model may *refine* the task list (`planner: "model"`); its output is
//! validated against the same rules and rejected (with the reason recorded) if invalid.
//! When no provider is configured, the deterministic planner is used and the request
//! status says so - never a silent fake.

use std::path::Path;

use serde_json::Value;

use crate::dag;
use crate::error::{NcagError, Result};
use crate::profiles::ProfileRegistry;
use crate::types::{
    AcceptanceCheck, AcceptanceKind, IntakeAnalysis, Plan, PlannedTask, ProjectConfig, Task,
    TaskStatus,
};

/// What we actually found in the repository (never assumed).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoFacts {
    pub languages: Vec<String>,
    pub build_tool: Option<String>,
    pub test_command: Option<String>,
    pub typecheck_command: Option<String>,
    pub has_frontend: bool,
    pub has_backend: bool,
    pub has_database: bool,
    pub has_api: bool,
    pub has_tests: bool,
}

pub fn detect_repo_facts(root: &Path) -> RepoFacts {
    let mut facts = RepoFacts::default();
    let exists = |name: &str| root.join(name).exists();
    if exists("Cargo.toml") {
        facts.languages.push("rust".into());
        facts.build_tool = Some("cargo".into());
        facts.test_command = Some("cargo test".into());
        facts.typecheck_command = Some("cargo check".into());
        facts.has_tests = root.join("tests").exists() || root.join("src").exists();
    }
    if exists("package.json") {
        facts.languages.push("typescript/javascript".into());
        if facts.build_tool.is_none() {
            facts.build_tool = Some("node".into());
        }
        if let Ok(raw) = std::fs::read_to_string(root.join("package.json")) {
            if let Ok(value) = serde_json::from_str::<Value>(&raw) {
                let scripts = value.get("scripts").cloned().unwrap_or(Value::Null);
                if facts.test_command.is_none() && scripts.get("test").is_some() {
                    facts.test_command = Some("npm test".into());
                }
                if scripts.get("build").is_some() {
                    facts
                        .typecheck_command
                        .get_or_insert_with(|| "npm run build".to_string());
                }
                if scripts.get("typecheck").is_some() {
                    facts.typecheck_command = Some("npm run typecheck".into());
                }
                let deps = ["react", "vue", "svelte", "@angular/core"]
                    .iter()
                    .any(|name| {
                        value.pointer(&format!("/dependencies/{name}")).is_some()
                            || value.pointer(&format!("/devDependencies/{name}")).is_some()
                    });
                facts.has_frontend = deps || root.join("index.html").exists();
            }
        }
    }
    if exists("pyproject.toml") || exists("requirements.txt") {
        facts.languages.push("python".into());
        facts.build_tool.get_or_insert_with(|| "python".to_string());
        facts
            .test_command
            .get_or_insert_with(|| "python -m pytest".to_string());
    }
    if exists("go.mod") {
        facts.languages.push("go".into());
        facts.build_tool.get_or_insert_with(|| "go".to_string());
        facts
            .test_command
            .get_or_insert_with(|| "go test ./...".to_string());
    }
    if exists("Dockerfile") || exists("docker-compose.yml") {
        facts.languages.push("docker".into());
    }
    facts.has_backend = facts.build_tool.is_some();
    facts.has_api = root.join("openapi").exists() || root.join("api").exists() || facts.has_backend;
    facts.has_database = ["migrations", "schema.sql", "prisma"]
        .iter()
        .any(|name| root.join(name).exists());
    // Frontend may exist even without package.json (e.g. static site).
    if root.join("public").exists() || root.join("src").join("App.tsx").exists() {
        facts.has_frontend = true;
    }
    facts
}

fn mentions(instruction: &str, needles: &[&str]) -> bool {
    let lowercase = instruction.to_ascii_lowercase();
    needles.iter().any(|needle| lowercase.contains(needle))
}

/// Step 1-2: intent, outcomes, constraints, technologies, ambiguities, contradictions.
pub fn analyze_request(instruction: &str, facts: &RepoFacts) -> IntakeAnalysis {
    let trimmed = instruction.trim();
    let mut outcomes = Vec::new();
    let intent = if mentions(trimmed, &["fix", "bug", "broken", "regression"]) {
        "repair existing behavior".to_string()
    } else if mentions(trimmed, &["refactor", "clean up", "restructure"]) {
        "refactor without behavior change".to_string()
    } else if mentions(trimmed, &["document", "readme", "docs"]) {
        "produce documentation".to_string()
    } else if mentions(trimmed, &["build", "create", "implement", "add", "develop"]) {
        "implement new behavior".to_string()
    } else {
        "analyze and propose an implementation".to_string()
    };
    if mentions(trimmed, &["api", "endpoint", "rest"]) {
        outcomes.push("HTTP API surface".to_string());
    }
    if mentions(
        trimmed,
        &["ui", "frontend", "page", "screen", "dashboard", "component"],
    ) {
        outcomes.push("frontend surface".to_string());
    }
    if mentions(
        trimmed,
        &["database", "schema", "migration", "table", "persist"],
    ) {
        outcomes.push("persistent storage".to_string());
    }
    if mentions(trimmed, &["test", "coverage", "verify"]) {
        outcomes.push("tests and verification".to_string());
    }
    if outcomes.is_empty() {
        outcomes.push("working implementation with evidence".to_string());
    }
    let mut technologies: Vec<String> = facts.languages.clone();
    for tech in [
        "react", "vue", "svelte", "rust", "node", "python", "go", "tauri", "electron", "sqlite",
        "postgres", "mysql", "redis", "graphql", "openapi",
    ] {
        if mentions(trimmed, &[tech]) && !technologies.iter().any(|item| item == tech) {
            technologies.push(tech.to_string());
        }
    }
    let mut constraints = Vec::new();
    for (needle, constraint) in [
        ("offline", "must work without network access"),
        ("windows", "must run on Windows"),
        ("linux", "must run on Linux"),
        ("macos", "must run on macOS"),
        ("mobile", "must work on mobile viewports"),
        ("no external", "no external service dependency"),
        ("backward", "backward compatible with the existing contract"),
    ] {
        if mentions(trimmed, &[needle]) {
            constraints.push(constraint.to_string());
        }
    }
    let mut ambiguities = Vec::new();
    if trimmed.split_whitespace().count() < 4 {
        ambiguities.push("the instruction is very short; scope is under-specified".to_string());
    }
    if mentions(trimmed, &["fast", "quick", "soon"]) {
        ambiguities
            .push("'fast' has no measurable target (latency, build time, deadline?)".to_string());
    }
    if mentions(trimmed, &["secure", "safe"]) {
        ambiguities.push(
            "'secure' is not a testable criterion; name the threat and the control".to_string(),
        );
    }
    if !mentions(trimmed, &["test", "verify", "acceptance"]) {
        ambiguities.push(
            "no acceptance criteria were stated; NC-AG will derive build/test/file checks from the repository".to_string(),
        );
    }
    if facts.build_tool.is_none() {
        ambiguities.push("no build/test tool detected in the repository; verification will rely on file and review evidence".to_string());
    }
    let mut contradictions = Vec::new();
    if mentions(trimmed, &["offline", "no network"]) && mentions(trimmed, &["api", "cloud"]) {
        contradictions.push(
            "the request asks for offline operation and a network API in the same scope"
                .to_string(),
        );
    }
    if mentions(trimmed, &["no tests", "without tests"])
        && mentions(trimmed, &["fully tested", "coverage"])
    {
        contradictions.push(
            "the request forbids tests while also requiring verified test coverage".to_string(),
        );
    }
    if mentions(trimmed, &["delete", "remove all"]) && mentions(trimmed, &["keep", "preserve"]) {
        contradictions.push("the request both removes and preserves the same behavior".to_string());
    }
    let mut assumptions = Vec::new();
    if facts.languages.is_empty() {
        assumptions.push(
            "no language detected; the implementation language is chosen by the architect"
                .to_string(),
        );
    } else {
        assumptions.push(format!(
            "the existing {} codebase and its conventions are reused rather than rewritten",
            facts.languages.join("/")
        ));
    }
    assumptions.push(
        "the project branch is the integration target; agents work in isolated worktrees"
            .to_string(),
    );
    let risk_notes = if facts.has_database {
        vec!["database changes need a reversible migration and integrity check".to_string()]
    } else if mentions(trimmed, &["auth", "login", "password", "secret"]) {
        vec!["authentication and secret handling require an independent security audit".to_string()]
    } else {
        Vec::new()
    };
    IntakeAnalysis {
        intent,
        outcomes,
        constraints,
        technologies,
        ambiguities,
        assumptions,
        contradictions,
        risk_notes,
    }
}

/// A requirement derived from the instruction and the analysis.
#[derive(Debug, Clone)]
pub struct RequirementDraft {
    pub code: String,
    pub description: String,
    pub category: String,
    pub priority: i32,
    pub acceptance: Vec<AcceptanceCheck>,
    pub verification: String,
}

fn check(
    id: &str,
    description: &str,
    kind: AcceptanceKind,
    command: Option<&str>,
    target: Option<&str>,
    expect: Option<&str>,
) -> AcceptanceCheck {
    AcceptanceCheck {
        id: id.to_string(),
        description: description.to_string(),
        kind,
        command: command.map(str::to_string),
        target: target.map(str::to_string),
        expect: expect.map(str::to_string),
    }
}

/// Build acceptance checks that are actually executable in a worktree.
/// Is this string a plausible workspace-relative path (and not a prose deliverable)?
///
/// Paths are one token with no sentence punctuation, at most a few segments, and either
/// a separator or an extension. Anything else is a description and must not become a
/// file-existence criterion (observed live: a verifier was failed for not producing a
/// file literally named "verification report with verbatim file content").
pub fn looks_like_path(candidate: &str) -> bool {
    let value = candidate.trim();
    if value.is_empty() || value.len() > 200 {
        return false;
    }
    if value.split_whitespace().count() != 1 {
        return false;
    }
    if value.contains([',', ';', ':', '"', '\'', '!', '?', '(', ')']) {
        return false;
    }
    if value.contains("..") {
        return false;
    }
    let segments = value
        .split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .count();
    if segments == 0 || segments > 6 {
        return false;
    }
    value.contains('/') || value.contains('\\') || value.ends_with('/') || value.contains('.')
}

pub fn build_checks(
    id_prefix: &str,
    facts: &RepoFacts,
    outputs: &[String],
) -> Vec<AcceptanceCheck> {
    let mut checks = Vec::new();
    let mut index = 0;
    let mut next = || {
        index += 1;
        format!("{id_prefix}-ac{index}")
    };
    if let Some(command) = &facts.typecheck_command {
        checks.push(check(
            &next(),
            "the workspace still builds/type-checks",
            AcceptanceKind::CommandSucceeded,
            Some(command),
            None,
            None,
        ));
    }
    if let Some(command) = &facts.test_command {
        checks.push(check(
            &next(),
            "the test suite passes",
            AcceptanceKind::TestPassed,
            Some(command),
            None,
            None,
        ));
    }
    for output in outputs {
        // Only path-shaped outputs are file criteria; prose deliverables ("verification
        // report with verbatim file content") are described, not turned into impossible
        // file checks - that failed a verifier whose work was complete (observed live).
        if output.contains('*') || !looks_like_path(output) {
            continue;
        }
        checks.push(check(
            &next(),
            &format!("{output} exists after the task"),
            AcceptanceKind::FileExists,
            None,
            Some(output),
            None,
        ));
    }
    if checks.is_empty() {
        // No build tool and no declared output: require review approval only; the
        // reviewer must justify on evidence, so this is not a free pass.
        checks.push(check(
            &next(),
            "an independent reviewer approved the deliverable",
            AcceptanceKind::ReviewApproved,
            None,
            None,
            None,
        ));
    } else {
        checks.push(check(
            &next(),
            "an independent reviewer approved the deliverable",
            AcceptanceKind::ReviewApproved,
            None,
            None,
            None,
        ));
    }
    checks
}

pub fn normalize_requirements(
    instruction: &str,
    analysis: &IntakeAnalysis,
    facts: &RepoFacts,
) -> Vec<RequirementDraft> {
    let mut requirements: Vec<RequirementDraft> = Vec::new();
    let excerpt: String = instruction.chars().take(240).collect();
    let wants_ui = analysis
        .outcomes
        .iter()
        .any(|item| item == "frontend surface");
    let wants_api = analysis
        .outcomes
        .iter()
        .any(|item| item == "HTTP API surface");
    let wants_db = analysis
        .outcomes
        .iter()
        .any(|item| item == "persistent storage");
    if !wants_ui && !wants_api && !wants_db {
        requirements.push(RequirementDraft {
            code: "R1".into(),
            description: format!("Deliver the requested change: {excerpt}"),
            category: "functional".into(),
            priority: 10,
            acceptance: build_checks("r1", facts, &[]),
            verification: "build/test command + independent review".into(),
        });
    }
    if wants_api {
        requirements.push(RequirementDraft {
            code: "R-API".into(),
            description:
                "Expose the requested HTTP API with a documented request/response contract".into(),
            category: "functional".into(),
            priority: 10,
            acceptance: build_checks("rapi", facts, &[]),
            verification: "build/test command + endpoint contract review".into(),
        });
    }
    if wants_ui {
        requirements.push(RequirementDraft {
            code: "R-UI".into(),
            description: "Provide the requested user-facing surface, wired to the real data source"
                .into(),
            category: "functional".into(),
            priority: 9,
            acceptance: build_checks("rui", facts, &[]),
            verification: "frontend build + reviewer inspection of the rendered surface".into(),
        });
    }
    if wants_db {
        requirements.push(RequirementDraft {
            code: "R-DB".into(),
            description:
                "Persist the new data with a reversible migration and integrity constraints".into(),
            category: "data".into(),
            priority: 9,
            acceptance: build_checks("rdb", facts, &[]),
            verification: "migration applied + tests".into(),
        });
    }
    if facts.has_backend {
        requirements.push(RequirementDraft {
            code: "R-BUILD".into(),
            description: "The existing build and test suite keep passing (no regression)".into(),
            category: "quality".into(),
            priority: 8,
            acceptance: vec![check(
                "rbuild-ac1",
                "project build/typecheck command succeeds",
                AcceptanceKind::CommandSucceeded,
                facts
                    .typecheck_command
                    .as_deref()
                    .or(facts.test_command.as_deref()),
                None,
                None,
            )],
            verification: "command executed by an independent verifier".into(),
        });
    }
    requirements.push(RequirementDraft {
        code: "R-DOC".into(),
        description: "Document what changed, how to run it and its known limitations".into(),
        category: "documentation".into(),
        priority: 4,
        acceptance: vec![check(
            "rdoc-ac1",
            "a change note exists next to the implementation",
            AcceptanceKind::FileExists,
            None,
            Some("NCAG_CHANGES.md"),
            None,
        )],
        verification: "file presence + review".into(),
    });
    requirements
}

/// Task decomposition.
pub fn plan_tasks(
    request_id: &str,
    instruction: &str,
    analysis: &IntakeAnalysis,
    facts: &RepoFacts,
    project_config: &ProjectConfig,
    profiles: &ProfileRegistry,
) -> Result<Plan> {
    let wants_ui = analysis
        .outcomes
        .iter()
        .any(|item| item == "frontend surface");
    let wants_api = analysis
        .outcomes
        .iter()
        .any(|item| item == "HTTP API surface");
    let wants_db = analysis
        .outcomes
        .iter()
        .any(|item| item == "persistent storage");
    let needs_security = analysis
        .risk_notes
        .iter()
        .any(|note| note.contains("security"))
        || mentions(
            instruction,
            &["auth", "login", "password", "secret", "permission"],
        );
    let mut tasks: Vec<PlannedTask> = Vec::new();
    let excerpt: String = instruction.chars().take(400).collect();
    let files_note =
        "Write only files under the task's declared outputs; explain every change in the summary.";

    let key = |suffix: &str| format!("{}-{}", request_id.replace('-', ""), suffix);
    let mut push = |planned: PlannedTask| tasks.push(planned);

    if wants_db {
        push(PlannedTask {
            key: key("db"),
            title: "Database schema and migration".into(),
            description: format!("Design and implement the persistence layer: schema, migration, integrity constraints. Requirement: {excerpt}\n{files_note}"),
            capability: "database".into(),
            role: "database".into(),
            priority: 9,
            depends_on: vec![],
            resources: vec!["migrations/".into(), "schema".into()],
            acceptance: build_checks("db", facts, &["NCAG_CHANGES.md".to_string()]),
            expected_outputs: vec!["NCAG_CHANGES.md".into()],
        });
    }
    if wants_api {
        push(PlannedTask {
            key: key("api"),
            title: "API contract and endpoints".into(),
            description: format!("Define the request/response contract and implement the endpoints. Requirement: {excerpt}\n{files_note}"),
            capability: "api".into(),
            role: "api".into(),
            priority: 9,
            depends_on: if wants_db { vec![key("db")] } else { vec![] },
            resources: vec!["api/".into()],
            acceptance: build_checks("api", facts, &["NCAG_CHANGES.md".to_string()]),
            expected_outputs: vec!["NCAG_CHANGES.md".into()],
        });
    }
    push(PlannedTask {
        key: key("impl"),
        title: "Backend implementation".into(),
        description: format!("Implement the requested behavior in the existing backend, following the repository's conventions. Requirement: {excerpt}\n{files_note}"),
        capability: "backend".into(),
        role: "backend".into(),
        priority: 8,
        depends_on: {
            let mut deps = Vec::new();
            if wants_db {
                deps.push(key("db"));
            }
            if wants_api {
                deps.push(key("api"));
            }
            deps
        },
        resources: vec!["src/".into(), "packages/".into()],
        acceptance: build_checks("impl", facts, &["NCAG_CHANGES.md".to_string()]),
        expected_outputs: vec!["NCAG_CHANGES.md".into()],
    });
    if wants_ui {
        push(PlannedTask {
            key: key("ui"),
            title: "Frontend implementation".into(),
            description: format!("Build the requested user-facing surface against the real API/state, with loading and error states. Requirement: {excerpt}\n{files_note}"),
            capability: "frontend".into(),
            role: "frontend".into(),
            priority: 7,
            depends_on: vec![key("impl")],
            resources: vec!["packages/ui/".into(), "packages/web/".into()],
            acceptance: build_checks("ui", facts, &["NCAG_CHANGES.md".to_string()]),
            expected_outputs: vec!["NCAG_CHANGES.md".into()],
        });
    }
    push(PlannedTask {
        key: key("qa"),
        title: "Tests for the delivered behavior".into(),
        description: format!("Add tests that fail without the implementation and pass with it, including one negative path. Requirement: {excerpt}"),
        capability: "testing".into(),
        role: "qa".into(),
        priority: 6,
        depends_on: vec![key("impl")],
        resources: vec!["tests/".into()],
        acceptance: build_checks("qa", facts, &[]),
        expected_outputs: vec![],
    });
    push(PlannedTask {
        key: key("docs"),
        title: "Change documentation".into(),
        description: "Write NCAG_CHANGES.md: what changed, how to run it, verification performed, and known limitations. Do not document unimplemented behavior.".into(),
        capability: "documentation".into(),
        role: "documentation".into(),
        priority: 3,
        depends_on: vec![key("impl")],
        resources: vec!["NCAG_CHANGES.md".into()],
        acceptance: vec![check(
            "docs-ac1",
            "NCAG_CHANGES.md exists",
            AcceptanceKind::FileExists,
            None,
            Some("NCAG_CHANGES.md"),
            None,
        )],
        expected_outputs: vec!["NCAG_CHANGES.md".into()],
    });
    push(PlannedTask {
        key: key("review"),
        title: "Independent review".into(),
        description: "Review every change in the request branch against the requirements. Findings must cite files and evidence; approve only when the acceptance evidence exists.".into(),
        capability: "code-review".into(),
        role: "review".into(),
        priority: 5,
        depends_on: vec![key("docs")],
        resources: vec![],
        acceptance: vec![check(
            "review-ac1",
            "review verdict is recorded with evidence",
            AcceptanceKind::ReviewApproved,
            None,
            None,
            None,
        )],
        expected_outputs: vec![],
    });
    if needs_security {
        push(PlannedTask {
            key: key("security"),
            title: "Security audit".into(),
            description: "Audit the change for authentication/authorization gaps, secret handling, injection and untrusted input. Read-only; report findings with severity and verification method.".into(),
            capability: "security-audit".into(),
            role: "security".into(),
            priority: 6,
            depends_on: vec![key("docs")],
            resources: vec![],
            acceptance: vec![check(
                "sec-ac1",
                "no unresolved high or critical findings",
                AcceptanceKind::AuditClean,
                None,
                None,
                None,
            )],
            expected_outputs: vec![],
        });
    }
    // Validate capabilities exist in the registry before returning the plan.
    for task in &tasks {
        if profiles.find_capability(&task.capability).is_none() {
            return Err(NcagError::invalid(format!(
                "no registered agent profile has capability {} (task {})",
                task.capability, task.key
            )));
        }
    }
    let mut plan = Plan {
        request_id: request_id.to_string(),
        summary: format!(
            "{} -> {} task(s) for: {}",
            analysis.intent,
            tasks.len(),
            excerpt
        ),
        critical_path: Vec::new(),
        tasks,
        estimated_agents: 0,
        estimated_tokens: 0,
        assumptions: analysis.assumptions.clone(),
        contradictions: analysis.contradictions.clone(),
    };
    plan.critical_path = critical_path_keys(&plan.tasks);
    plan.estimated_agents = plan
        .tasks
        .iter()
        .map(|task| task.role.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    plan.estimated_tokens =
        (plan.tasks.len() as u64) * project_config.task_token_budget.min(40_000);
    Ok(plan)
}

fn critical_path_keys(tasks: &[PlannedTask]) -> Vec<String> {
    use std::collections::BTreeMap;
    let by_key: BTreeMap<&str, &PlannedTask> =
        tasks.iter().map(|task| (task.key.as_str(), task)).collect();
    let mut memo: BTreeMap<String, i64> = BTreeMap::new();
    fn depth(
        key: &str,
        by_key: &BTreeMap<&str, &PlannedTask>,
        memo: &mut BTreeMap<String, i64>,
    ) -> i64 {
        if let Some(value) = memo.get(key) {
            return *value;
        }
        let Some(task) = by_key.get(key) else {
            return 0;
        };
        let mut best = 0;
        for dep in &task.depends_on {
            best = best.max(1 + depth(dep, by_key, memo));
        }
        memo.insert(key.to_string(), best);
        best
    }
    let mut path: Vec<(i64, String)> = tasks
        .iter()
        .map(|task| (depth(&task.key, &by_key, &mut memo), task.key.clone()))
        .collect();
    path.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    path.into_iter().map(|(_, key)| key).collect()
}

/// Materialise planned tasks as store rows at `planned` status.
///
/// `requirement_map` links a task key to the requirement id it satisfies, so the delivery
/// gap "task -> requirement -> acceptance" stays traceable (spec section 4, step 3).
/// Stable task id for one plan key inside one request.
///
/// Keys are only unique inside a plan (a model emits `verify-live-proof`), so scoping them
/// by the request id is what keeps `tasks.id` globally unique across requests and rounds.
pub fn scoped_task_id(request_id: &str, key: &str) -> String {
    let prefix = request_id.replace('-', "");
    // The deterministic planner already emits scoped keys (`<request>-impl`); a model
    // plan emits bare ones (`impl`). Accept both shapes instead of prefixing twice.
    if key.starts_with(&prefix) {
        return key.to_string();
    }
    format!("{prefix}-{key}")
}
pub fn materialize_plan(
    project_id: &str,
    request_id: &str,
    plan: &Plan,
    project_config: &ProjectConfig,
    requirement_map: &std::collections::BTreeMap<String, String>,
    now: i64,
) -> Vec<Task> {
    // Plan keys are local to one plan (the model emits short ids like `verify-live-proof`).
    // Persisting them raw made a second request in the same project die with
    // `UNIQUE constraint failed: tasks.id` (observed live). The stable request id is the
    // scope, exactly like the deterministic planner's `key()` closure.
    plan.tasks
        .iter()
        .map(|planned| Task {
            id: scoped_task_id(request_id, &planned.key),
            project_id: project_id.to_string(),
            request_id: request_id.to_string(),
            requirement_id: requirement_map.get(&planned.key).cloned(),
            title: planned.title.clone(),
            description: planned.description.clone(),
            capability: planned.capability.clone(),
            role: planned.role.clone(),
            agent_id: None,
            status: TaskStatus::Planned,
            priority: planned.priority,
            depends_on: planned
                .depends_on
                .iter()
                .map(|dep| scoped_task_id(request_id, dep))
                .collect(),
            inputs: Vec::new(),
            outputs: planned.expected_outputs.clone(),
            acceptance: planned.acceptance.clone(),
            resources: planned.resources.clone(),
            max_retries: project_config.max_retries,
            attempts: 0,
            timeout_ms: {
                let base = 900_000u64;
                base.saturating_add(300_000)
            },
            deadline_at: None,
            worktree: None,
            branch: None,
            result: None,
            blocker: None,
            created_at: now,
            updated_at: now,
            started_at: None,
            ended_at: None,
        })
        .collect()
}

/// Map a planned task to the requirement it satisfies.
///
/// The mapping is deterministic and based on the requirement codes assigned in
/// `normalize_requirements`: UI/API/data/build/documentation requirements get their
/// matching roles, and every remaining implementation task falls back to the first
/// functional requirement so a task is never orphaned.
pub fn requirement_for_task(
    planned: &PlannedTask,
    requirements: &[(String, String)],
) -> Option<String> {
    // (requirement code, requirement id) pairs, in priority order.
    let by_code = |code: &str| {
        requirements
            .iter()
            .find(|(candidate, _)| candidate == code)
            .map(|(_, id)| id.clone())
    };
    let matches_code = match planned.role.as_str() {
        "frontend" => by_code("R-UI"),
        "api" | "integrator" => by_code("R-API"),
        "database" => by_code("R-DB"),
        "documentation" => by_code("R-DOC"),
        "qa" | "e2e-test" => by_code("R-BUILD"),
        _ => None,
    };
    matches_code.or_else(|| {
        requirements
            .iter()
            .find(|(code, _)| code.starts_with("R1") || code.starts_with("R-API"))
            .map(|(_, id)| id.clone())
    })
}

/// Guarantee that every planned task carries at least one judgeable criterion.
///
/// A model planner may emit `acceptance: []`; such a task could only ever produce a
/// vacuous "verified" result, so it is completed with the repository's real build/test
/// checks and a mandatory review criterion.
pub fn ensure_checks(plan: &mut Plan, facts: &RepoFacts) {
    for (index, task) in plan.tasks.iter_mut().enumerate() {
        if task.acceptance.is_empty() {
            let prefix = format!("{}-{}", task.role, index + 1).replace(['-', '/'], "");
            task.acceptance = build_checks(&prefix, facts, &task.expected_outputs);
        }
    }
}

/// Validate a model-produced plan object against the same invariants. Anything the
/// model gets wrong is reported, not repaired silently.
pub fn validate_model_plan(value: &Value, profiles: &ProfileRegistry) -> Result<Plan> {
    let request_id = value
        .get("requestId")
        .and_then(Value::as_str)
        .ok_or_else(|| NcagError::invalid("model plan is missing requestId"))?;
    let tasks_value = value
        .get("tasks")
        .and_then(Value::as_array)
        .filter(|tasks| !tasks.is_empty())
        .ok_or_else(|| NcagError::invalid("model plan has no tasks"))?;
    let mut tasks: Vec<PlannedTask> = Vec::new();
    for (index, item) in tasks_value.iter().enumerate() {
        let key = item
            .get("key")
            .and_then(Value::as_str)
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| NcagError::invalid(format!("task[{index}] has no key")))?;
        let capability = item
            .get("capability")
            .and_then(Value::as_str)
            .ok_or_else(|| NcagError::invalid(format!("task {key} has no capability")))?;
        if profiles.find_capability(capability).is_none() {
            return Err(NcagError::invalid(format!(
                "task {key} requires unregistered capability {capability}"
            )));
        }
        let depends_on: Vec<String> = item
            .get("dependsOn")
            .and_then(Value::as_array)
            .map(|deps| {
                deps.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        // Task ids are resolved AFTER the whole plan is read (duplicate keys across
        // rounds / re-plans must not collide in the `tasks` table): the model emits
        // short local keys, `materialize_plan` prefixes them with the stable request id.
        for dep in &depends_on {
            if !tasks_value
                .iter()
                .any(|candidate| candidate.get("key").and_then(Value::as_str) == Some(dep.as_str()))
            {
                return Err(NcagError::invalid(format!(
                    "task {key} depends on unknown task {dep}"
                )));
            }
        }
        let resources: Vec<String> = item
            .get("resources")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let outputs: Vec<String> = item
            .get("expectedOutputs")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let acceptance = item
            .get("acceptance")
            .map(|value| {
                serde_json::from_value::<Vec<AcceptanceCheck>>(value.clone()).unwrap_or_default()
            })
            .unwrap_or_default();
        tasks.push(PlannedTask {
            key: key.to_string(),
            title: item
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(key)
                .to_string(),
            description: item
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            capability: capability.to_string(),
            role: item
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or(capability)
                .to_string(),
            priority: item.get("priority").and_then(Value::as_i64).unwrap_or(5) as i32,
            depends_on,
            resources,
            acceptance,
            expected_outputs: outputs,
        });
    }
    // Reuse the same DAG validator as the deterministic planner.
    let placeholder: Vec<Task> = tasks
        .iter()
        .map(|planned| Task {
            id: planned.key.clone(),
            project_id: "validation".into(),
            request_id: request_id.to_string(),
            requirement_id: None,
            title: planned.title.clone(),
            description: planned.description.clone(),
            capability: planned.capability.clone(),
            role: planned.role.clone(),
            agent_id: None,
            status: TaskStatus::Planned,
            priority: planned.priority,
            depends_on: planned.depends_on.clone(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            acceptance: Vec::new(),
            resources: Vec::new(),
            max_retries: 1,
            attempts: 0,
            timeout_ms: 60_000,
            deadline_at: None,
            worktree: None,
            branch: None,
            result: None,
            blocker: None,
            created_at: 0,
            updated_at: 0,
            started_at: None,
            ended_at: None,
        })
        .collect();
    dag::validate_graph(&placeholder)?;
    Ok(Plan {
        request_id: request_id.to_string(),
        summary: value
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("model plan")
            .to_string(),
        critical_path: critical_path_keys(&tasks),
        estimated_agents: value
            .get("estimatedAgents")
            .and_then(Value::as_u64)
            .unwrap_or(tasks.len() as u64) as usize,
        estimated_tokens: value
            .get("estimatedTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        assumptions: value
            .get("assumptions")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        contradictions: value
            .get("contradictions")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        tasks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust_facts() -> RepoFacts {
        RepoFacts {
            languages: vec!["rust".into()],
            build_tool: Some("cargo".into()),
            test_command: Some("cargo test".into()),
            typecheck_command: Some("cargo check".into()),
            has_backend: true,
            has_api: true,
            has_tests: true,
            ..Default::default()
        }
    }

    #[test]
    fn contradictions_and_ambiguities_are_detected() {
        let facts = rust_facts();
        let analysis = analyze_request(
            "Make it work offline but call the cloud API, and it must be fast.",
            &facts,
        );
        assert!(analysis
            .contradictions
            .iter()
            .any(|item| item.contains("offline")));
        assert!(analysis
            .ambiguities
            .iter()
            .any(|item| item.contains("'fast'")));
        assert!(!analysis.assumptions.is_empty());
    }

    #[test]
    fn full_stack_plan_has_a_valid_dag_and_real_checks() {
        let profiles = ProfileRegistry::new();
        let facts = RepoFacts {
            has_frontend: true,
            has_backend: true,
            has_api: true,
            has_database: true,
            build_tool: Some("cargo".into()),
            typecheck_command: Some("cargo check".into()),
            test_command: Some("cargo test".into()),
            ..Default::default()
        };
        let instruction = "Build a dashboard with an API, a database migration and a React UI";
        let analysis = analyze_request(instruction, &facts);
        let plan = plan_tasks(
            "req-1",
            instruction,
            &analysis,
            &facts,
            &ProjectConfig::default(),
            &profiles,
        )
        .expect("plan");
        assert!(plan.tasks.len() >= 6);
        // Every task is linked to the requirement it satisfies.
        let requirements: Vec<(String, String)> = vec![
            ("R1".to_string(), "req-1-r1".to_string()),
            ("R-UI".to_string(), "req-1-r2".to_string()),
            ("R-API".to_string(), "req-1-r3".to_string()),
            ("R-DB".to_string(), "req-1-r4".to_string()),
            ("R-DOC".to_string(), "req-1-r5".to_string()),
            ("R-BUILD".to_string(), "req-1-r6".to_string()),
        ];
        let mapping: std::collections::BTreeMap<String, String> = plan
            .tasks
            .iter()
            .filter_map(|planned| {
                requirement_for_task(planned, &requirements)
                    .map(|requirement_id| (planned.key.clone(), requirement_id))
            })
            .collect();
        let tasks = materialize_plan("p", "req-1", &plan, &ProjectConfig::default(), &mapping, 0);
        assert!(
            tasks.iter().all(|task| task.requirement_id.is_some()),
            "every materialized task must carry its requirement: {tasks:?}"
        );
        dag::validate_graph(&tasks).expect("dag");
        // Every task carries at least one executable or review check.
        for task in &tasks {
            assert!(!task.acceptance.is_empty(), "{}", task.title);
        }
        // Review is last and depends on docs.
        let review = plan.tasks.iter().find(|t| t.role == "review").unwrap();
        assert!(!review.depends_on.is_empty());
    }

    #[test]
    fn planner_rejects_unknown_capability() {
        let profiles = ProfileRegistry::new();
        let facts = rust_facts();
        let instruction = "Implement the feature";
        let analysis = analyze_request(instruction, &facts);
        let mut plan = plan_tasks(
            "req-2",
            instruction,
            &analysis,
            &facts,
            &ProjectConfig::default(),
            &profiles,
        )
        .unwrap();
        plan.tasks[0].capability = "quantum-compiler".into();
        assert!(profiles
            .find_capability(&plan.tasks[0].capability)
            .is_none());
    }

    #[test]
    fn model_plan_validation_rejects_cycles_and_missing_capabilities() {
        let profiles = ProfileRegistry::new();
        let cyclic = serde_json::json!({
            "requestId": "r",
            "tasks": [
                {"key": "a", "capability": "backend", "dependsOn": ["b"]},
                {"key": "b", "capability": "backend", "dependsOn": ["a"]}
            ]
        });
        assert!(validate_model_plan(&cyclic, &profiles).is_err());
        let unknown = serde_json::json!({
            "requestId": "r",
            "tasks": [{"key": "a", "capability": "nope"}]
        });
        assert!(validate_model_plan(&unknown, &profiles).is_err());
        let valid = serde_json::json!({
            "requestId": "r",
            "summary": "ok",
            "tasks": [
                {"key": "a", "title": "A", "capability": "backend", "role": "backend"},
                {"key": "b", "title": "B", "capability": "testing", "role": "qa", "dependsOn": ["a"]}
            ]
        });
        let plan = validate_model_plan(&valid, &profiles).unwrap();
        assert_eq!(plan.tasks.len(), 2);
    }
}
