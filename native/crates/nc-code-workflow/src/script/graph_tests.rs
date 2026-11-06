//! Tests for the NWS causality projection: what the preview card is built from.
//!
//! Each test pins one rule of `specs/nws-causality-projection.md` — the step vocabulary, the
//! certainty and region assignment, what orders and what merely coexists, the family expansion, the
//! phase quotient and the sink — rather than a whole-script snapshot, so a regression names the rule
//! it broke.

use serde_json::{json, Value};

use super::graph::project_analysis;
use super::plan::Plan;

fn compile(source: &str) -> Plan {
    Plan::compile(source).unwrap_or_else(|diagnostics| {
        panic!(
            "fixture must compile: {}",
            diagnostics
                .iter()
                .map(|diagnostic| format!("{diagnostic:?}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    })
}

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/../nc-code-workflow/tests/fixtures/{name}.nws",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
}

fn project(source: &str) -> (Plan, Value) {
    let plan = compile(source);
    let value = project_analysis(&plan);
    (plan, value)
}

fn steps(analysis: &Value) -> Vec<(String, String, String)> {
    analysis["causality"]["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .map(|step| {
            (
                step["id"].as_str().unwrap_or_default().to_string(),
                step["kind"].as_str().unwrap_or_default().to_string(),
                step["lane"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

fn edges(analysis: &Value) -> Vec<(String, String, String)> {
    analysis["causality"]["edges"]
        .as_array()
        .expect("edges")
        .iter()
        .map(|edge| {
            (
                edge["from"].as_str().unwrap_or_default().to_string(),
                edge["to"].as_str().unwrap_or_default().to_string(),
                edge["kind"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

fn has_edge(analysis: &Value, from: &str, to: &str, kind: &str) -> bool {
    edges(analysis)
        .iter()
        .any(|edge| edge.0 == from && edge.1 == to && edge.2 == kind)
}

#[test]
fn a_linear_script_names_its_steps_lanes_and_the_one_handoff() {
    let (plan, analysis) = project(&fixture("nws_linear"));
    // The contract's step kinds are ask and world-read: the actor, the report and the artifact are
    // lanes and cards, not nodes.
    assert_eq!(
        steps(&analysis),
        vec![
            (
                "ask#1".to_string(),
                "ask".to_string(),
                "actor#1".to_string()
            ),
            (
                "world-read#1".to_string(),
                "world-read".to_string(),
                "workspace".to_string()
            ),
        ],
        "steps come from the site table in source order"
    );
    assert_eq!(
        analysis["causality"]["steps"][0]["label"], "Draft a plan for ",
        "an ask's card says what it asked, and a template's holes are not invented text"
    );
    assert_eq!(analysis["causality"]["lanes"][0]["id"], "workspace");
    assert_eq!(analysis["causality"]["lanes"][1]["id"], "actor#1");
    assert_eq!(analysis["causality"]["lanes"][1]["name"], "planner");
    assert_eq!(
        analysis["causality"]["steps"][0]["loc"]["line"], 18,
        "the ask site's own line, so the card can point at the source"
    );
    assert!(
        has_edge(&analysis, "ask#1", "world-read#1", "seq"),
        "the await before the read orders them"
    );
    let participants = analysis["handoff"]["participants"]
        .as_array()
        .expect("cards");
    assert_eq!(participants.len(), 2, "one card per phase and lane");
    assert_eq!(participants[0]["id"], "phase#1:actor#1");
    assert_eq!(participants[1]["id"], "phase#2:workspace");
    let handoffs = analysis["handoff"]["handoffs"]
        .as_array()
        .expect("handoffs");
    assert!(
        handoffs
            .iter()
            .any(|edge| edge["from"] == "phase#1:actor#1" && edge["to"] == "phase#2:workspace"),
        "the actor hands its plan to the workspace step that follows: {handoffs:?}"
    );
    assert_eq!(analysis["causality"]["sink"]["fedBy"][0], "ask#1");
    assert_eq!(plan.phase_names, vec!["plan", "execute"]);
}

#[test]
fn the_phase_quotient_walks_markers_in_order_and_returns_to_the_sink() {
    let (_plan, analysis) = project(&fixture("nws_linear"));
    let edges: Vec<(String, String, String)> = analysis["flow"]["phaseEdges"]
        .as_array()
        .expect("phase edges")
        .iter()
        .map(|edge| {
            (
                edge["from"].as_str().unwrap_or_default().to_string(),
                edge["to"].as_str().unwrap_or_default().to_string(),
                edge["kind"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    assert!(
        edges.contains(&(
            "entry".to_string(),
            "phase#1".to_string(),
            "next".to_string()
        )),
        "{edges:?}"
    );
    assert!(edges.contains(&(
        "phase#1".to_string(),
        "phase#2".to_string(),
        "next".to_string()
    )));
    assert!(edges.contains(&(
        "phase#2".to_string(),
        "sink".to_string(),
        "jump".to_string()
    )));
    assert!(edges.contains(&(
        "phase#1".to_string(),
        "abort".to_string(),
        "throw".to_string()
    )));
    let phases = analysis["flow"]["phases"].as_array().expect("phases");
    assert_eq!(phases[0]["id"], "phase#1");
    assert_eq!(phases[0]["name"], "plan");
}

#[test]
fn a_branch_makes_its_steps_maybe_and_its_guard_a_control_edge() {
    let (_plan, analysis) = project(
        r#"workflow "branchy" {
  let planner = agent("planner");
  let plan = await planner.ask("plan it");
  if (plan.ready) {
    let readme = await files.read("README.md");
  }
  return plan;
}
"#,
    );
    let steps = analysis["causality"]["steps"].as_array().expect("steps");
    assert_eq!(steps[0]["certainty"], "always");
    assert_eq!(steps[1]["certainty"], "maybe");
    assert_eq!(steps[1]["region"], "branch#1");
    assert!(
        steps[1].get("phase").is_none(),
        "no marker was declared, so no phase field"
    );
    assert!(
        has_edge(&analysis, "ask#1", "world-read#1", "control"),
        "the guard's value decides whether the read runs at all"
    );
    assert!(
        !has_edge(&analysis, "ask#1", "world-read#1", "data"),
        "the read's argument is a literal, so no value crosses — control is the whole fact"
    );
    let regions = analysis["causality"]["regions"]
        .as_array()
        .expect("regions");
    assert_eq!(regions[1]["kind"], "branch");
    assert_eq!(regions[1]["parent"], "root");
    // A script with no phase vocabulary gets none of the three phase fields, exactly as the
    // TypeScript path leaves them out.
    assert!(analysis["causality"].get("phases").is_none());
    assert!(analysis["flow"].as_object().expect("flow map").is_empty());
}

#[test]
fn a_loop_marks_its_steps_serial_and_loops_the_phase_back() {
    let (_plan, analysis) = project(
        r#"workflow "loopy" {
  phase "rounds";
  let a = agent("a");
  while (true) {
    phase "work";
    let x = await a.ask("one round");
  }
}
"#,
    );
    let steps = analysis["causality"]["steps"].as_array().expect("steps");
    assert_eq!(
        steps[0]["repeat"], "serial",
        "one actor's mailbox orders the rounds"
    );
    assert_eq!(steps[0]["certainty"], "maybe");
    assert_eq!(steps[0]["phase"], "phase#2");
    let edges: Vec<String> = analysis["flow"]["phaseEdges"]
        .as_array()
        .expect("phase edges")
        .iter()
        .map(|edge| {
            format!(
                "{}-{}>{}",
                edge["from"].as_str().unwrap_or_default(),
                edge["kind"].as_str().unwrap_or_default(),
                edge["to"].as_str().unwrap_or_default()
            )
        })
        .collect();
    assert!(
        edges.contains(&"phase#2-loop>phase#1".to_string()),
        "the inner marker loops back to the marker that opened the body: {edges:?}"
    );
}

#[test]
fn parallel_strands_do_not_order_each_other() {
    let (_plan, analysis) = project(&fixture("nws_fanout"));
    assert!(
        !has_edge(&analysis, "ask#5", "ask#6", "seq")
            && !has_edge(&analysis, "ask#6", "ask#5", "seq"),
        "two strands of one parallel block coexist; drawing an order between them is a lie: {:?}",
        edges(&analysis)
    );
    assert!(
        !has_edge(&analysis, "ask#5", "ask#6", "data"),
        "and they share no value either"
    );
}

#[test]
fn a_join_orders_its_items_under_one_step_and_keeps_their_products() {
    let (_plan, analysis) = project(&fixture("nws_fanout"));
    for asked in ["ask#1", "ask#2", "ask#3"] {
        let found = analysis["handoff"]["participants"]
            .as_array()
            .expect("cards")
            .iter()
            .any(|card| {
                card["steps"]
                    .as_array()
                    .expect("steps")
                    .iter()
                    .any(|step| step.as_str() == Some(asked))
            });
        assert!(found, "{asked} belongs to the reviewer's card");
    }
    for first in ["ask#1", "ask#2", "ask#3"] {
        for second in ["ask#1", "ask#2", "ask#3"] {
            if first == second {
                continue;
            }
            assert!(
                !has_edge(&analysis, first, second, "seq"),
                "{first} and {second} are issued together; an order between them is invented"
            );
        }
    }
    assert!(
        has_edge(&analysis, "ask#1", "ask#5", "seq")
            && has_edge(&analysis, "ask#3", "ask#5", "seq"),
        "every item of the barrier precedes what runs after it"
    );
}

#[test]
fn a_literal_fanout_expands_into_member_cards() {
    let (_plan, analysis) = project(
        r#"workflow "family" {
  for (const name of ["a.ts", "b.ts", "c.ts"]) {
    let worker = agent("worker-" + name);
    let note = await worker.ask("review it");
  }
}
"#,
    );
    let lanes = analysis["causality"]["lanes"].as_array().expect("lanes");
    assert_eq!(lanes[0]["families"].as_array().expect("families").len(), 1);
    let cards = analysis["handoff"]["participants"]
        .as_array()
        .expect("cards");
    let members: Vec<String> = cards
        .iter()
        .filter(|card| card["member"].is_object())
        .map(|card| card["id"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        members,
        vec![
            "unphased:actor#1[0/3]",
            "unphased:actor#1[1/3]",
            "unphased:actor#1[2/3]"
        ],
        "three literal elements are three cards, and the card id says which one"
    );
    let regions = analysis["causality"]["regions"]
        .as_array()
        .expect("regions");
    assert_eq!(
        regions[1]["kind"], "fanout",
        "an iteration that mints an actor is a family, not a serial loop"
    );
}

#[test]
fn an_unbounded_fanout_is_one_many_card() {
    let (_plan, analysis) = project(
        r#"workflow "many" {
  args { files: string[] }
  for (const name of args.files) {
    let worker = agent("worker-" + name);
    let note = await worker.ask("review " + name);
  }
}
"#,
    );
    let cards = analysis["handoff"]["participants"]
        .as_array()
        .expect("cards");
    assert_eq!(cards.len(), 1);
    assert_eq!(
        cards[0]["many"], true,
        "the cardinality is the model's, not ours"
    );
    assert_eq!(cards[0]["id"], "unphased:actor#1");
}

#[test]
fn a_multi_phase_step_is_emitted_once_per_phase() {
    // One marker in each arm, both before a step that follows the block: the partition has to stay
    // total, so the later step belongs to both phases.
    let (_plan, analysis) = project(
        r#"workflow "split" {
  let planner = agent("planner");
  if (planner.name) {
    phase "left";
  } else {
    phase "right";
  }
  let readme = await files.read("README.md");
}
"#,
    );
    let reads: Vec<&Value> = analysis["causality"]["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .filter(|step| step["id"] == "world-read#1")
        .collect();
    assert_eq!(reads.len(), 2, "one copy per reachable phase");
    let phases: Vec<String> = reads
        .iter()
        .map(|step| step["phase"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(phases, vec!["phase#1", "phase#2"]);
    assert_eq!(
        reads[0]["id"], reads[1]["id"],
        "the copies share the site id"
    );
}

#[test]
fn every_identifier_the_projection_emits_exists_upstream() {
    for name in ["nws_linear", "nws_fanout"] {
        let (plan, analysis) = project(&fixture(name));
        let site_ids: Vec<String> = plan.sites.iter().map(|site| site.id.clone()).collect();
        let step_ids: Vec<String> = analysis["causality"]["steps"]
            .as_array()
            .expect("steps")
            .iter()
            .map(|step| step["id"].as_str().unwrap_or_default().to_string())
            .collect();
        for id in &step_ids {
            assert!(site_ids.contains(id), "{name}: step {id} is not a site");
        }
        let actor_ids: Vec<String> = plan
            .sites
            .iter()
            .filter(|site| site.kind == crate::script::ir::SiteKind::Actor)
            .map(|site| site.id.clone())
            .collect();
        let lane_ids: Vec<String> = analysis["causality"]["lanes"]
            .as_array()
            .expect("lanes")
            .iter()
            .map(|lane| lane["id"].as_str().unwrap_or_default().to_string())
            .collect();
        for lane in &lane_ids {
            assert!(
                lane == "workspace" || lane == "unknown" || actor_ids.contains(lane),
                "{name}: lane {lane} is neither a reserved lane nor an actor site"
            );
        }
        let card_ids: Vec<String> = analysis["handoff"]["participants"]
            .as_array()
            .expect("cards")
            .iter()
            .map(|card| card["id"].as_str().unwrap_or_default().to_string())
            .collect();
        for edge in analysis["handoff"]["handoffs"]
            .as_array()
            .expect("handoffs")
        {
            for key in ["from", "to"] {
                let id = edge[key].as_str().unwrap_or_default();
                assert!(
                    card_ids.iter().any(|card| card == id),
                    "{name}: handoff {key} {id} names no card"
                );
            }
        }
        for edge in analysis["causality"]["edges"].as_array().expect("edges") {
            for key in ["from", "to"] {
                let id = edge[key].as_str().unwrap_or_default();
                assert!(step_ids.contains(&id.to_string()), "{name}: edge {key} {id} is not a step (it should be — non-step producers are flattened)");
            }
        }
    }
}

#[test]
fn the_phase_table_row_order_matches_the_run_launched_table() {
    let (plan, analysis) = project(&fixture("nws_fanout"));
    let phases = analysis["causality"]["phases"].as_array().expect("phases");
    let declared: Vec<String> = phases
        .iter()
        .filter(|phase| phase["id"] != "unphased")
        .map(|phase| {
            format!(
                "{}|{}",
                phase["id"].as_str().unwrap_or_default(),
                phase["name"].as_str().unwrap_or_default()
            )
        })
        .collect();
    let expected: Vec<String> = plan
        .phase_names
        .iter()
        .enumerate()
        .map(|(index, name)| format!("phase#{}|{}", index + 1, name))
        .collect();
    assert_eq!(
        declared, expected,
        "the graph and the run payload must number phases alike"
    );
    for (index, phase) in phases.iter().enumerate() {
        if phase["id"] == "unphased" {
            continue;
        }
        let graph_row: Vec<String> = phase["alongside"]
            .as_array()
            .map(|row| {
                row.iter()
                    .map(|other| other.as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let launch_row: Vec<String> = plan
            .phase_alongside
            .get(index)
            .map(|row| {
                row.iter()
                    .map(|other| format!("phase#{}", other + 1))
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(
            graph_row,
            launch_row,
            "phase {} alongside must be the same fact in both surfaces",
            index + 1
        );
    }
}

#[test]
fn a_data_edge_names_the_product_that_crosses_the_handoff() {
    let (_plan, analysis) = project(
        r#"schema Plan {
  entry: string
}

workflow "crossing" {
  let planner = agent("planner");
  let plan: Plan = await planner.ask("pick an entry point");
  let readme = await files.read(plan.entry);
  return readme;
}
"#,
    );
    assert!(
        has_edge(&analysis, "ask#1", "world-read#1", "data"),
        "the read's argument came from the ask"
    );
    let handoffs = analysis["handoff"]["handoffs"]
        .as_array()
        .expect("handoffs");
    let crossing = handoffs
        .iter()
        .find(|edge| edge["from"] == "unphased:actor#1" && edge["to"] == "unphased:workspace")
        .expect("the planner hands work to the workspace lane");
    assert_eq!(
        crossing["types"],
        json!(["Plan"]),
        "NWS knows the schema name statically, so the card says what crosses: {crossing}"
    );
    assert_eq!(analysis["causality"]["sink"]["fedBy"][0], "world-read#1");
}

/// The projection deliberately does not cap: it describes the script. Bounding is the host's shared
/// display pass, and a cap inside the projection would leave that pass unable to prove the
/// `truncated` flag came from the display contract rather than from here.
#[test]
fn the_projection_describes_every_step_without_capping() {
    let mut script = String::from(
        "workflow \"wide\" {
  let a = agent(\"a\");
",
    );
    for round in 1..=70 {
        script.push_str(&format!(
            "  let x{round} = await a.ask(\"question {round}\");
"
        ));
    }
    script.push_str(
        "}
",
    );
    let (_plan, analysis) = project(&script);
    assert_eq!(
        analysis["causality"]["steps"]
            .as_array()
            .expect("steps")
            .len(),
        70,
        "each ask site is one step, however many there are"
    );
    assert!(
        analysis["causality"].get("truncated").is_none(),
        "truncation is a display decision, not a projection one"
    );
    let edges = edges(&analysis);
    assert!(
        edges
            .iter()
            .filter(|edge| edge.2 == "seq" && edge.1 == "ask#70")
            .count()
            <= 1,
        "a straight line of awaits chains one predecessor at a time, not all sixty-nine"
    );
}
