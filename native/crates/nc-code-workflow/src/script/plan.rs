//! The compiled NWS plan: what the run service hands the engine and the interpreter.
//!
//! `Plan` is the Rust counterpart of the TypeScript path's `CompiledScript` plus the site
//! table projection, and it feeds the same engine fields:
//!
//! | plan field | engine / host use |
//! | --- | --- |
//! | `script_hash` | resume's byte-identity check (`ScriptHashMismatch`) |
//! | `site_ids` | the facade sites a run may journal, in source order |
//! | `ask_specs` | `EngineConfig::ask_specs`; a gap is the engine's `MissingAskSpec` |
//! | `phase_names` / `phase_alongside` | the `run-launched` sidebar track |
//! | `declared_run_commands` | the driver's `world.run` authorization set |
//! | `actor_submit_profiles` | each actor session's `submit_result` tool shape |
//! | `body` | the tree the interpreter executes |

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

use crate::compile::AskSpecWire;
use crate::types::AskSpec;

use super::ast::Span;
use super::diagnostic::{self, Diagnostic};
use super::ir::{Instr, Site, SiteIndex, SiteKind};
use super::lower::{self, Lowered};
use super::parser::parse;

/// A compiled workflow script, ready to run.
#[derive(Clone, Debug)]
pub struct Plan {
    /// The author's text, hashed for resume and stored in `dwf_run.script_text`.
    pub source: String,
    /// sha256 of the source, the same value the TypeScript path recorded.
    pub script_hash: String,
    pub workflow_name: String,
    pub body: Vec<Instr>,
    pub sites: Vec<Site>,
    /// Local slots one frame needs; the interpreter allocates this many `Value`s.
    pub slot_count: usize,
    pub ask_specs: BTreeMap<String, AskSpecWire>,
    pub schemas: BTreeMap<String, Value>,
    pub args_schema: Option<Value>,
    pub phase_names: Vec<String>,
    pub phase_alongside: Vec<Vec<u32>>,
    pub declared_run_commands: Vec<String>,
    pub actor_submit_profiles: BTreeMap<String, Value>,
}

impl Plan {
    /// Parse, resolve and lower a script. Any diagnostic fails the compile: the run
    /// service refuses to create a run for a script that is not clean.
    pub fn compile(source: &str) -> Result<Self, Vec<Diagnostic>> {
        let program = match parse(source) {
            Ok(program) => program,
            Err(error) => return Err(vec![error.to_diagnostic()]),
        };
        let lowered = lower::lower(&program)?;
        Ok(Self::from_lowered(
            source,
            program.workflow.name.clone(),
            lowered,
        ))
    }

    fn from_lowered(source: &str, workflow_name: String, lowered: Lowered) -> Self {
        Self {
            source: source.to_string(),
            script_hash: script_hash(source),
            workflow_name,
            body: lowered.body,
            sites: lowered.sites,
            slot_count: lowered.slot_count,
            ask_specs: lowered.ask_specs,
            schemas: lowered.schemas,
            args_schema: lowered.args_schema,
            phase_names: lowered.phase_names,
            phase_alongside: lowered.phase_alongside,
            declared_run_commands: lowered.declared_run_commands,
            actor_submit_profiles: lowered.actor_submit_profiles,
        }
    }

    /// Facade site ids that carry a journal row, in source order. `join` sites are
    /// analysed but not journaled — the nodes a join produces are keyed by their own
    /// sites, exactly as the TypeScript path instrumented only the facade calls.
    pub fn site_ids(&self) -> Vec<String> {
        self.sites
            .iter()
            .filter(|site| site.kind.is_journaled())
            .map(|site| site.id.clone())
            .collect()
    }

    /// Every ask site's static spec, in the shape `EngineConfig` takes.
    pub fn engine_ask_specs(&self) -> HashMap<String, AskSpec> {
        self.ask_specs
            .iter()
            .map(|(id, spec)| (id.clone(), spec.clone().into_engine()))
            .collect()
    }

    pub fn site_id(&self, index: SiteIndex) -> &str {
        &self.sites[index].id
    }

    pub fn ask_spec(&self, index: SiteIndex) -> Option<&AskSpecWire> {
        self.ask_specs.get(&self.sites[index].id)
    }

    /// The sites the sidebar graph and the analysis surface read.
    pub fn site_summaries(&self) -> Vec<SiteSummary> {
        self.sites
            .iter()
            .map(|site| SiteSummary {
                id: site.id.clone(),
                kind: site.kind,
                member: site.member.clone(),
                span: site.span,
            })
            .collect()
    }

    /// Diagnostics for the authoring tools, in the wire shape the host renders.
    pub fn diagnostics_as_wire(
        diagnostics: &[Diagnostic],
    ) -> Vec<crate::compile::CompileDiagnostic> {
        diagnostic::to_wire(diagnostics)
    }
}

/// A site as the analysis/read surface sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SiteSummary {
    pub id: String,
    pub kind: SiteKind,
    pub member: String,
    pub span: Span,
}

/// sha256 of the author text, hex — the value `dwf_run.script_hash` records and the one
/// resume compares. Computed on the source, never on a lowered form: a hash of a derived
/// text would make the resume check silently compare the wrong thing.
fn script_hash(source: &str) -> String {
    use std::fmt::Write as _;

    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    const README_PLAN: &str = r#"
schema Plan {
  steps: string[]
  rationale: string
}

workflow "readme-plan" {
  args { repo: string }

  artifact.board("summary", { title: "Plan" });

  phase "plan";
  let planner = agent("planner", "You produce short plans.");
  let plan: Plan = await planner.ask("Draft a plan for " + args.repo);

  phase "execute";
  let readme = await files.read("README.md");
  report({ repo: args.repo, steps: plan.steps }, "summary");
  await artifact.markdown("plan", plan.rationale);

  return { steps: plan.steps, bytes: readme.length };
}
"#;

    fn compiled() -> Plan {
        Plan::compile(README_PLAN).unwrap_or_else(|diagnostics| {
            panic!(
                "fixture plan must compile cleanly, got:\n{}",
                diagnostic::render(&diagnostics)
            )
        })
    }

    #[test]
    fn compiles_the_site_table_in_source_order() {
        let plan = compiled();
        let ids = plan.site_ids();
        assert_eq!(
            ids,
            [
                "artifact#1",
                "actor#1",
                "ask#1",
                "world-read#1",
                "report#1",
                "artifact#2"
            ],
            "site ids are per-kind source-order counters, as the journal keys depend on them"
        );
        assert_eq!(plan.script_hash, script_hash(README_PLAN));
        assert_eq!(plan.script_hash.len(), 64);
        assert_eq!(plan.workflow_name, "readme-plan");
    }

    #[test]
    fn a_typed_ask_carries_its_schema_and_the_engine_shape() {
        let plan = compiled();
        let spec = plan.ask_specs.get("ask#1").expect("ask#1 spec");
        assert!(spec.typed);
        let schema = spec.schema.clone().expect("typed schema");
        assert_eq!(schema["type"], serde_json::json!("object"));
        assert_eq!(
            schema["properties"]["steps"]["type"],
            serde_json::json!("array")
        );
        assert_eq!(
            schema["properties"]["steps"]["items"]["type"],
            serde_json::json!("string")
        );
        assert_eq!(
            schema["required"],
            serde_json::json!(["steps", "rationale"]),
            "non-optional fields are required, and the object is closed"
        );
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));

        let engine_specs = plan.engine_ask_specs();
        assert!(engine_specs.get("ask#1").expect("engine spec").typed);
    }

    #[test]
    fn an_untyped_ask_is_recorded_as_untyped() {
        // Not a gap: the engine reads a missing spec as a wiring error, so every ask site
        // must appear with `typed: false`.
        let plan = Plan::compile(
            r#"workflow "w" {
  let a = agent("a");
  let x = await a.ask("do it");
  return x;
}"#,
        )
        .expect("compiles");
        let spec = plan.ask_specs.get("ask#1").expect("ask#1 spec");
        assert!(!spec.typed);
        assert_eq!(spec.schema, None);
    }

    #[test]
    fn phase_table_and_strands_come_from_the_source_shape() {
        let plan = compiled();
        assert_eq!(plan.phase_names, ["plan", "execute"]);
        assert!(
            plan.phase_alongside.is_empty(),
            "a straight-line script leaves the alongside table absent, not full of empties"
        );

        let branched = Plan::compile(
            r#"workflow "w" {
  let a = agent("left");
  let b = agent("right");
  parallel {
    { phase "review"; let l = await a.ask("l"); }
    { phase "test"; let r = await b.ask("r"); }
  }
  return [1, 2];
}"#,
        )
        .expect("parallel compiles");
        assert_eq!(branched.phase_names, ["review", "test"]);
        assert_eq!(
            branched.phase_alongside,
            vec![vec![1], vec![0]],
            "each strand's phase is alongside the other strand's phase"
        );
    }

    #[test]
    fn args_schema_is_the_declared_argument_object() {
        let plan = compiled();
        let schema = plan.args_schema.clone().expect("args schema");
        assert_eq!(
            schema["properties"]["repo"]["type"],
            serde_json::json!("string")
        );
        assert_eq!(schema["required"], serde_json::json!(["repo"]));

        let none = Plan::compile("workflow \"w\" { return 1; }")
            .expect("compiles")
            .args_schema;
        assert_eq!(none, None, "a workflow with no args declares no schema");
    }

    #[test]
    fn world_run_commands_are_collected_as_a_sorted_set() {
        let plan = Plan::compile(
            r#"workflow "w" {
  let a = agent("a");
  phase "build";
  let first = await world.run("cargo test", ["-q"]);
  let again = await world.run("cargo test", []);
  let other = await world.run("git status");
  return [first, again, other];
}"#,
        )
        .expect("compiles");
        assert_eq!(plan.declared_run_commands, ["cargo test", "git status"]);
        assert_eq!(
            plan.site_ids(),
            ["actor#1", "world-read#1", "world-read#2", "world-read#3"],
            "a world.run is a world-read site with its own ordinal key"
        );
    }

    #[test]
    fn submit_profiles_follow_the_typed_ask_shape() {
        // One actor, one typed ask whose schema is fixed: `mono` is what makes the actor's
        // submit tool cache-neutral.
        let mono = Plan::compile(
            r#"schema Plan { steps: string[] }
workflow "w" {
  let a = agent("planner");
  phase "plan";
  let p: Plan = await a.ask("plan");
  return p;
}"#,
        )
        .expect("compiles");
        assert_eq!(
            mono.actor_submit_profiles.get("actor#1"),
            Some(&serde_json::json!({"kind": "mono", "schema": mono.schemas["Plan"]}))
        );

        // Two different schemas reaching one actor: `generic`, because the tool schema
        // would change mid-session.
        let mixed = Plan::compile(
            r#"schema A { x: string }
schema B { y: string }
workflow "w" {
  let a = agent("planner");
  phase "plan";
  let one: A = await a.ask("x");
  let two: B = await a.ask("y");
  return [one, two];
}"#,
        )
        .expect("compiles");
        assert_eq!(
            mixed.actor_submit_profiles.get("actor#1"),
            Some(&serde_json::json!({"kind": "generic"}))
        );

        // Only untyped asks: no submit tool at all.
        let untyped = Plan::compile(
            r#"workflow "w" {
  let a = agent("planner");
  phase "plan";
  let one = await a.ask("x");
  return one;
}"#,
        )
        .expect("compiles");
        assert_eq!(
            untyped.actor_submit_profiles.get("actor#1"),
            Some(&serde_json::json!({"kind": "untyped"}))
        );
    }

    #[test]
    fn unresolved_actor_flow_makes_every_profile_generic() {
        // The receiver is a loop element: nothing static says which actor it holds, so the
        // analysis says nothing about any of them.
        let plan = Plan::compile(
            r#"schema A { x: string }
workflow "w" {
  let a = agent("first");
  let b = agent("second");
  phase "plan";
  for (const worker of [a, b]) {
    let one: A = await worker.ask("x");
    report(one);
  }
  return 1;
}"#,
        )
        .expect("compiles");
        assert_eq!(
            plan.actor_submit_profiles.get("actor#1"),
            Some(&serde_json::json!({"kind": "generic"}))
        );
        assert_eq!(
            plan.actor_submit_profiles.get("actor#2"),
            Some(&serde_json::json!({"kind": "generic"}))
        );
    }

    #[test]
    fn a_computed_actor_name_in_a_loop_is_a_compile_error() {
        let diagnostics = Plan::compile(
            r#"workflow "w" {
  phase "plan";
  for (const file of ["a", "b"]) {
    let reviewer = agent("reviewer");
    let verdict = await reviewer.ask("check " + file);
    report(verdict);
  }
  return 1;
}"#,
        )
        .expect_err("a fixed name inside a per-element body cannot be legal");
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "nws_fanout_actor_name"));
    }

    #[test]
    fn two_sites_with_the_same_literal_name_are_reported_on_the_later_one() {
        let diagnostics = Plan::compile(
            r#"workflow "w" {
  let first = agent("planner");
  let second = agent("planner");
  phase "plan";
  let x = await first.ask("a");
  let y = await second.ask("b");
  return [x, y];
}"#,
        )
        .expect_err("duplicate literal actor names");
        let reported = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code == "nws_duplicate_actor_name")
            .expect("duplicate name diagnostic");
        assert_eq!(
            reported.span.line, 3,
            "the second agent(…) call is the one to fix"
        );
    }

    #[test]
    fn facade_misuse_is_rejected_before_the_run_starts() {
        let cases = [
            ("workflow \"w\" { let f = files.read; return f; }", "nws_facade_member_not_called"),
            ("workflow \"w\" { return files.nope(\"x\"); }", "nws_unknown_facade_member"),
            ("workflow \"w\" { let x = unknownThing; return x; }", "nws_unknown_identifier"),
            ("workflow \"w\" { let a = agent(); let x = a.ask(\"i\"); return x; }", "nws_await_required"),
            ("workflow \"w\" { let n = 1; let x = await n; return x; }", "nws_await_misplaced"),
            ("workflow \"w\" { return args.missing; }", "nws_unknown_args_field"),
            ("workflow \"w\" { let p: Nope = 1; return p; }", "nws_unknown_schema"),
            ("workflow \"w\" { return myHelper(1); }", "nws_not_callable"),
            (
                "workflow \"w\" { let a = agent(\"x\"); phase \"p\"; let v = await \"planner\".ask(\"i\"); return v; }",
                "nws_ask_receiver_not_an_actor",
            ),
            (
                "workflow \"w\" { await artifact.markdown(\"bad id\", \"body\"); return 1; }",
                "nws_artifact_id_invalid",
            ),
            (
                "workflow \"w\" { await artifact.markdown(computed, \"body\"); return 1; }",
                "nws_literal_required",
            ),
            (
                "workflow \"w\" { report(1, \"undeclared\"); return 1; }",
                "nws_undeclared_artifact_tag",
            ),
            (
                "workflow \"w\" { let r = log(\"hi\"); return r; }",
                "nws_facade_returns_nothing",
            ),
            ("workflow \"w\" { return await world.run(computed); }", "nws_literal_required"),
            ("workflow \"w\" { return await Promise.all([1, 2]); }", "nws_join_item_not_async"),
            ("workflow \"w\" { let a = agent(\"n\", \"s\", 3); return a; }", "nws_arity_mismatch"),
        ];
        for (source, code) in cases {
            let diagnostics = Plan::compile(source)
                .err()
                .unwrap_or_else(|| panic!("{source} compiled cleanly; expected {code}"));
            assert!(
                diagnostics.iter().any(|d| d.code == code),
                "expected {code} in:
{}",
                diagnostic::render(&diagnostics)
            );
        }
    }

    #[test]
    fn a_single_artifact_id_belongs_to_one_member() {
        let diagnostics = Plan::compile(
            r#"workflow "w" {
  phase "plan";
  let a = agent("a");
  let v = await a.ask("x");
  await artifact.markdown("board", v);
  await artifact.file("board", "out.pdf");
  return v;
}"#,
        )
        .expect_err("one id, two members");
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "nws_artifact_kind_mismatch"));
    }

    #[test]
    fn a_syntax_error_becomes_one_diagnostic() {
        let errors = Plan::compile("workflow \"w\" { phase }").expect_err("missing name");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].code, "nws_parse_error");
        assert!(errors[0].span.line >= 1);
    }
}
