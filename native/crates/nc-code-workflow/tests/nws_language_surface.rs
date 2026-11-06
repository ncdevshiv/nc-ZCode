//! The language surface `specs/nws-language-reference.md` documents, pinned by compile tests.
//!
//! The reference is what an author (or a model) reads before writing a script, so every
//! construct it shows must actually compile — and every rule it states must actually be a
//! diagnostic. These tests take the document's own examples verbatim and assert the shape the
//! document promises: the site table, which asks are typed and from what, and which statements
//! the await rules reject.

use nc_code_workflow::script::diagnostic;
use nc_code_workflow::script::Plan;
use serde_json::json;

fn compile(source: &str) -> Plan {
    Plan::compile(source).unwrap_or_else(|diagnostics| {
        panic!(
            "the documented example must compile, got:\n{}",
            diagnostic::render(&diagnostics)
        )
    })
}

fn codes(source: &str) -> Vec<&'static str> {
    match Plan::compile(source) {
        Ok(_) => Vec::new(),
        Err(errors) => errors.iter().map(|error| error.code).collect(),
    }
}

/// The reference's first worked example, verbatim.
const LINEAR_EXAMPLE: &str = r###"schema Plan {
  steps: string[]
  rationale: string
}

workflow "readme-plan" {
  args { repo: string }

  phase "plan";
  let planner = agent("planner", "You produce short, concrete plans.");
  let plan: Plan = await planner.ask("Draft a 3-step plan for " + args.repo);

  phase "execute";
  let readme = await files.read("README.md");
  report({ repo: args.repo, steps: plan.steps, readmeLength: readme.length });
  await artifact.markdown("summary", "## Plan\n\n" + plan.rationale);

  return { steps: plan.steps, rationale: plan.rationale };
}
"###;

const FANOUT_EXAMPLE: &str = r#"schema Verdict {
  file: string
  findings: string[]
}

workflow "review" {
  args { files: string[] }

  artifact.table("progress", { columns: [{ field: "name", label: "File" }] });

  phase "review";
  let first = agent("first-reviewer");
  let second = agent("second-reviewer");
  let verdicts = await Promise.all([
    first.ask("Review the first file", Verdict),
    second.ask("Review the second file", Verdict)
  ]);
  report({ name: "reviewed", note: verdicts.length }, "progress");

  phase "collect";
  let worker = agent("worker");
  for (const name of args.files) {
    let note = await worker.ask("Summarize " + name);
    report({ name: name, note: note }, "progress");
  }

  phase "close";
  return { reviewed: verdicts.length, collected: args.files.length };
}
"#;

#[test]
fn the_documented_linear_example_compiles_to_the_documented_site_table() {
    let plan = compile(LINEAR_EXAMPLE);
    assert_eq!(
        plan.site_ids(),
        ["actor#1", "ask#1", "world-read#1", "report#1", "artifact#1"],
        "per-kind counters in source order; the two phase markers consume nothing"
    );
    assert_eq!(plan.phase_names, ["plan", "execute"]);
    assert!(
        plan.phase_alongside.is_empty(),
        "no parallel block, so the alongside table is absent"
    );

    let typed = plan.ask_specs.get("ask#1").expect("ask#1");
    assert!(typed.typed, "the `let plan: Plan` annotation types the ask");
    let schema = typed.schema.clone().expect("schema");
    assert_eq!(
        schema["properties"]["steps"],
        json!({"type":"array","items":{"type":"string"}})
    );
    assert_eq!(schema["required"], json!(["steps", "rationale"]));
    assert_eq!(schema["additionalProperties"], json!(false));

    assert_eq!(
        plan.args_schema.clone().expect("args schema")["properties"]["repo"],
        json!({"type": "string"})
    );
    assert_eq!(
        plan.schemas["Plan"], schema,
        "the declared schema is exposed by name"
    );
}

#[test]
fn the_documented_fanout_example_types_its_join_items_by_schema_argument() {
    let plan = compile(FANOUT_EXAMPLE);
    assert_eq!(
        plan.site_ids(),
        [
            "artifact#1",
            "actor#1",
            "actor#2",
            "ask#1",
            "ask#2",
            "report#1",
            "actor#3",
            "ask#3",
            "report#2",
        ],
        "a join takes a `join#N` id for the analysis surface but no journal row, so it is \
         absent from the journaled list"
    );
    assert_eq!(
        plan.sites
            .iter()
            .filter(|site| site.id.starts_with("join#"))
            .count(),
        1,
        "the Promise.all is sited even though nothing is journaled for it"
    );

    // The second argument of ask is what types a join item: an annotation could not reach it.
    assert!(plan.ask_specs.get("ask#1").expect("ask#1").typed);
    assert!(plan.ask_specs.get("ask#2").expect("ask#2").typed);
    let loop_ask = plan.ask_specs.get("ask#3").expect("ask#3");
    assert!(!loop_ask.typed, "an unannotated ask is untyped, not absent");

    // Two workers with one typed schema each, one worker with only untyped asks.
    assert_eq!(
        plan.actor_submit_profiles.get("actor#1"),
        Some(&json!({"kind": "mono", "schema": plan.schemas["Verdict"]}))
    );
    assert_eq!(
        plan.actor_submit_profiles.get("actor#3"),
        Some(&json!({"kind": "untyped"}))
    );

    assert_eq!(plan.phase_names, ["review", "collect", "close"]);
    assert_eq!(
        plan.site_summaries()
            .iter()
            .filter(|site| site.member == "table")
            .count(),
        1,
        "the preset declaration is a real site"
    );
}

#[test]
fn every_facade_the_reference_lists_compiles() {
    let source = r#"workflow "surface" {
  artifact.board("board", { key: "k", status: "s", columns: ["a", "b"] });
  artifact.metrics("meters", { metrics: [{ field: "found" }] });
  artifact.chart("trend", { x: { field: "round" }, y: [{ field: "found" }], type: "line" });
  artifact.table("rows", { columns: [{ field: "name" }] });

  phase "work";
  let a = agent("a", { system: "persona object form" });
  let named = agent();
  let typed = await a.ask("instruction only");
  let text = await files.read("README.md");
  let paths = await files.glob("src/**");
  let hits = await files.grep("TODO", "src/**");
  let status = await git.status();
  let changed = await git.changedFiles("HEAD");
  let diff = await git.diff("HEAD", "src/app.ts");
  let recent = await git.log(10);
  let outcome = await world.run("cargo test", ["-q"], { timeoutMs: 120000 });
  let ref = await artifact.file("report", "out/report.pdf", { title: "Report", primary: true });
  let md = await artifact.markdown("summary", "body");
  log("progress line");
  report({ one: 1 }, "board");
  report({ two: 2 });
  return [typed, text, paths, hits, status, changed, diff, recent, outcome, ref, md, named];
}"#;
    let plan = compile(source);
    assert_eq!(
        plan.declared_run_commands,
        ["cargo test"],
        "the literal command is collected for the driver's authorization set"
    );
    assert_eq!(
        plan.site_ids(),
        [
            "artifact#1",
            "artifact#2",
            "artifact#3",
            "artifact#4",
            "actor#1",
            "actor#2",
            "ask#1",
            "world-read#1",
            "world-read#2",
            "world-read#3",
            "world-read#4",
            "world-read#5",
            "world-read#6",
            "world-read#7",
            "world-read#8",
            "artifact#5",
            "artifact#6",
            "report#1",
            "report#2",
        ],
        "four preset declarations, two actors, one ask, eight world reads (including the run), \
         two content publishes and two reports"
    );
    assert_eq!(
        plan.actor_submit_profiles.get("actor#1"),
        Some(&json!({"kind": "untyped"}))
    );
}

#[test]
fn the_documented_rules_are_actually_diagnostics() {
    // Each row here is a sentence in the reference; the sentence and the compiler must agree.
    let cases: Vec<(&str, &str)> =
        vec![
        (
            "workflow \"w\" { let a = agent(\"x\"); let n = a.ask(\"i\"); return n; }",
            "an ask must be awaited at the call",
        ),
        (
            "workflow \"w\" { return await 1; }",
            "everything else is already a value",
        ),
        ("workflow \"w\" { return unknownName; }", "no globals"),
        ("workflow \"w\" { return args.nope; }", "declared args fields only"),
        ("workflow \"w\" { let x: Nope = 1; return x; }", "schema must be declared"),
        ("workflow \"w\" { return files.nothere(\"x\"); }", "unknown member"),
        ("workflow \"w\" { let f = files.read; return f; }", "no member aliases"),
        ("workflow \"w\" { return helper(1); }", "no user-defined functions"),
        (
            "workflow \"w\" { await artifact.markdown(\"bad id\", \"body\"); return 1; }",
            "artifact id charset",
        ),
        (
            "workflow \"w\" { return await world.run(computed); }",
            "the run command must be a literal",
        ),
        (
            "workflow \"w\" { report(1, \"undeclared\"); return 1; }",
            "a report tag needs its preset",
        ),
        (
            "workflow \"w\" { let a = agent(\"x\"); return await Promise.all([1]); }",
            "join items must be facade calls",
        ),
        (
            "workflow \"w\" { let a = agent(\"x\"); return await Promise.all(computed); }",
            "join needs an array literal",
        ),
        (
            "workflow \"w\" { let a = agent(\"x\"); return await Promise.all([]); }",
            "an empty join waits for nothing",
        ),
        (
            "workflow \"w\" { let x = 1; let x = 2; return x; }",
            "no redeclaration in one block",
        ),
        (
            "workflow \"w\" { let s = \"text\"; return await s.ask(\"x\"); }",
            "only an actor can be asked",
        ),
        (
            "workflow \"w\" { let x = log(\"hi\"); return x; }",
            "log returns nothing",
        ),
        (
            "workflow \"w\" { phase \"\"; return 1; }",
            "an empty phase names nothing",
        ),
        (
            "workflow \"w\" { let a = agent(\"x\"); parallel { { return await a.ask(\"i\"); } } \
             return 1; }",
            "no return inside a strand",
        ),
        (
            "workflow \"w\" { 1 + 2; return 3; }",
            "a statement with no effect",
        ),
        (
            "schema Loop { self: Loop }\nworkflow \"w\" { return 1; }",
            "a schema must be a finite tree",
        ),
        ("schema Empty {}\nworkflow \"w\" { return 1; }", "an empty schema types nothing"),
        (
            "schema Dup { x: string\n  x: string }\nworkflow \"w\" { return 1; }",
            "one field per name",
        ),
        (
            "workflow \"w\" { await artifact.markdown(\"a\", \"x\"); await artifact.file(\"a\", \
             \"y\"); return 1; }",
            "one id belongs to one member",
        ),
    ];
    for (source, reason) in cases {
        let found = codes(source);
        assert!(
            found.iter().any(|code| code.starts_with("nws_")),
            "expected a diagnostic ({reason}) from:\n{source}"
        );
    }
}

#[test]
fn a_try_block_keeps_the_site_table_and_scopes_its_catch_binding() {
    // The calls inside a try body are ordinary sites: recovery does not hide them from the
    // journal, which is what lets a resumed run replay the work that already succeeded.
    let plan = compile(
        r#"workflow "try-sites" {
  phase "work";
  let worker = agent("worker");
  try {
    let answer = await worker.ask("answer");
    report({ answer: answer });
  } catch (err) {
    report({ failed: err.code });
  }
  return 1;
}"#,
    );
    assert_eq!(
        plan.site_ids(),
        ["actor#1", "ask#1", "report#1", "report#2"],
        "the report in each arm is its own site"
    );

    // The binding exists only inside the catch body.
    let escaped = codes(
        r#"workflow "w" {
  try {
    return 1;
  } catch (err) {
    return 2;
  }
  return err;
}"#,
    );
    assert!(
        escaped.contains(&"nws_unknown_identifier"),
        "the catch binding must not leak out: {escaped:?}"
    );

    // Shadowing an outer name is legal, exactly as with any block scope.
    compile(
        r#"workflow "shadow" {
  let err = "outer";
  try {
    return await files.read("README.md");
  } catch (err) {
    report(err.code);
  }
  return err;
}"#,
    );
}

#[test]
fn a_preset_declared_in_a_try_body_feeds_a_report_tag_in_the_catch_body() {
    // The declaration pre-pass walks both blocks, so "declared anywhere in the script" means
    // what it says even across a recovery boundary.
    compile(
        r#"workflow "try-preset" {
  phase "work";
  let worker = agent("worker");
  try {
    artifact.table("rows", { columns: [{ field: "code" }] });
    let answer = await worker.ask("answer");
    report({ code: answer }, "rows");
  } catch (err) {
    report({ code: err.code }, "rows");
  }
  return 1;
}"#,
    );
    let undeclared = codes(
        r#"workflow "w" {
  let worker = agent("worker");
  try {
    return await worker.ask("x");
  } catch (err) {
    report({ code: err.code }, "missing");
  }
}"#,
    );
    assert!(
        undeclared.contains(&"nws_undeclared_artifact_tag"),
        "a tag with no preset is still refused: {undeclared:?}"
    );
}

#[test]
fn a_try_without_a_catch_is_a_syntax_error() {
    let errors = codes(
        r#"workflow "w" {
  try {
    return 1;
  }
  return 2;
}"#,
    );
    assert!(
        errors.contains(&"nws_parse_error"),
        "there is no `finally` and no bare `try` in NWS: {errors:?}"
    );
}

#[test]
fn nws_has_no_throw_and_no_catch_of_anything_else() {
    // Only a facade rejection is catchable. `throw` is not in the grammar, and awaiting a
    // non-call inside a try is still the same misuse it was outside one.
    let errors = codes(
        r#"workflow "w" {
  try {
    throw "nope";
  } catch (err) {
    return err.code;
  }
}"#,
    );
    assert!(
        errors.iter().any(|code| matches!(
            *code,
            "nws_parse_error" | "nws_unknown_identifier" | "nws_not_callable"
        )),
        "`throw` is not NWS: {errors:?}"
    );
}

#[test]
fn the_documented_operators_and_member_access_compile() {
    // The reference's operator table is part of the contract; every row must at least parse
    // and resolve. The value-level semantics of each row are pinned in `script::value` tests
    // and re-run through the real engine in `nws_interpret_e2e`.
    let plan = compile(
        r#"workflow "ops" {
  let base = 1 + 2;
  let text = "a" + 1;
  let same = 1 == 1.0;
  let either = "" || "fallback";
  let both = "kept" && 7;
  let notFlag = !0;
  let list = ["x", "y"];
  let count = list.length;
  let pick = list[1];
  let absent = list[9];
  let interp = `n=${base} t=${text} c=${count}`;
  let negated = -base;
  let divided = base / 2;
  let compare = count > 1;
  let object = { key: "value", nested: { deep: 1 } };
  let member = object.key;
  let deep = object.nested;
  return [base, text, same, either, both, notFlag, pick, absent, interp, negated, divided,
          compare, member, deep, object];
}"#,
    );
    assert_eq!(
        plan.site_ids(),
        Vec::<String>::new(),
        "pure logic creates no sites, so a script of only operators journals nothing"
    );
    assert!(plan.phase_names.is_empty());
}
