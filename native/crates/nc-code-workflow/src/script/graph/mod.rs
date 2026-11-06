//! The NWS causality projection: the analysis triple behind the `CreateWorkflow` preview card.
//!
//! Spec: `specs/nws-causality-projection.md`. The TypeScript path recovers what-runs-before-what
//! from JavaScript data flow because JavaScript hides it: assignment, indirect calls, combinator
//! nesting. NWS states the same facts lexically - no assignment, no user function, concurrency as
//! a block - and every facade call already carries a site id and a span from the resolver. So the
//! projection derives from the lowered tree instead of transcribing the taint machinery, and it
//! emits the same three projections the TypeScript analyzer emits (`causality` / `flow` /
//! `handoff`) so the host's shared bounding pass remains the only display contract.
//!
//! Pure: no I/O, no clock, no invented identity. A step's id is its site id, which is what the
//! journal reports and what the live overlay joins on.

mod display;
mod walk;
mod walk_expr;

pub use display::project_analysis;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::Value;

use super::ast::Span;
use super::ir::{SiteIndex, SiteKind, Slot};
use super::plan::Plan;

/// Reserved lane ids, in the words the TypeScript projection uses so the GUI needs no special case.
pub(super) const WORKSPACE_LANE: &str = "workspace";
pub(super) const UNKNOWN_LANE: &str = "unknown";
pub(super) const UNPHASED: &str = "unphased";
/// Pseudo-nodes of the phase quotient.
pub(super) const FLOW_ENTRY: &str = "entry";
pub(super) const FLOW_SINK: &str = "sink";
pub(super) const FLOW_ABORT: &str = "abort";
/// The step kinds the display contract allows; everything else shapes lanes and cards instead.
pub(super) const KIND_ASK: &str = "ask";
pub(super) const KIND_WORLD_READ: &str = "world-read";
/// Certainty words, as the contract spells them.
pub(super) const ALWAYS: &str = "always";
pub(super) const MAYBE: &str = "maybe";
/// A family with a literal cardinality expands into that many cards; past the limit one `many`
/// card tells the truth without drawing sixty boxes. Same shape as the TypeScript expansion.
pub(super) const FAMILY_EXPANSION_LIMIT: usize = 8;

/// What a producer contributes when its value crosses a handoff. NWS knows this statically, which
/// is more than the JavaScript taint labels could promise.
fn product_of(plan: &Plan, site: SiteIndex) -> String {
    let site_ref = &plan.sites[site];
    match site_ref.kind {
        SiteKind::Ask => match plan.ask_specs.get(&site_ref.id) {
            Some(spec) if spec.typed => typed_schema_name(plan, spec.schema.as_ref())
                .unwrap_or_else(|| "object".to_string()),
            _ => "text".to_string(),
        },
        SiteKind::WorldRead => match site_ref.member.as_str() {
            "glob" => "paths",
            "grep" => "hits",
            "changedFiles" => "changed",
            "diff" => "diff",
            "status" => "status",
            "log" => "log",
            "run" => "outcome",
            _ => "text",
        }
        .to_string(),
        SiteKind::Artifact => "ref".to_string(),
        SiteKind::Report => "report".to_string(),
        SiteKind::Join => "array".to_string(),
        // An actor handle is never an argument value that crosses a handoff.
        SiteKind::Actor => "actor".to_string(),
    }
}

/// The declared `schema` name a typed ask resolves to, matched by canonical shape: the plan keeps
/// the compiled schema, not the annotation's identifier. Two identical schemas are the same
/// contract to a reader, so equality is the right lookup.
fn typed_schema_name(plan: &Plan, schema: Option<&Value>) -> Option<String> {
    let wanted = schema?;
    let canonical = crate::types::canonical_json(wanted);
    plan.schemas
        .iter()
        .find(|(_, value)| crate::types::canonical_json(value) == canonical)
        .map(|(name, _)| name.clone())
}

/// What one facade call became, before the walk's scope facts are attached to it.
struct NewStep {
    pub(super) site: SiteIndex,
    pub(super) kind: &'static str,
    pub(super) label: String,
    pub(super) label_pattern: Option<Value>,
    pub(super) lane: String,
    pub(super) lanes: Vec<String>,
}

/// A step of the display graph, before it is expanded into its per-phase copies.
struct Step {
    pub(super) site: SiteIndex,
    pub(super) kind: &'static str,
    pub(super) label: String,
    pub(super) label_pattern: Option<Value>,
    pub(super) span: Span,
    pub(super) lane: String,
    pub(super) lanes: Vec<String>,
    pub(super) region: String,
    pub(super) certainty: &'static str,
    /// Phase indices into [`Plan::phase_names`]. Empty is `unphased`; more than one is a step
    /// reachable under several markers, and it is emitted once per phase.
    pub(super) phases: Vec<usize>,
    pub(super) repeat: Option<&'static str>,
}

/// An ordering or data fact between two step sites.
#[derive(Clone, PartialEq, Eq)]
struct Edge {
    pub(super) from: SiteIndex,
    pub(super) to: SiteIndex,
    pub(super) kind: &'static str,
    pub(super) certainty: &'static str,
}

#[derive(Clone, Copy)]
enum RegionKind {
    Seq,
    Parallel,
    Loop,
    Fanout,
    Branch,
}

impl RegionKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Seq => "seq",
            Self::Parallel => "parallel",
            Self::Loop => "loop",
            Self::Fanout => "fanout",
            Self::Branch => "branch",
        }
    }
}

struct Region {
    pub(super) id: String,
    pub(super) kind: RegionKind,
    pub(super) parent: Option<String>,
    pub(super) span: Option<Span>,
}

/// A lane: one actor site, with the iteration family it belongs to when it was minted inside one.
struct Lane {
    pub(super) id: String,
    pub(super) name: Option<String>,
    pub(super) name_pattern: Option<Value>,
    pub(super) span: Span,
    /// Enclosing iteration regions, outermost first. Non-empty makes this lane a family.
    pub(super) families: Vec<String>,
    /// Literal member count when the iterable was a literal array inside the expansion limit;
    /// `Some(None)` means a family of unknown size, which becomes one `many` card.
    pub(super) cardinality: Option<usize>,
}

/// Everything a construct leaves for the statement after it. Passing the walk state by value and
/// returning it is what keeps an arm or a strand from leaking its ordering and phase into a sibling.
#[derive(Clone)]
struct State {
    pub(super) region: String,
    pub(super) certainty: &'static str,
    pub(super) phases: Vec<usize>,
    /// Steps that immediately precede the next one in this strand, each with the certainty it was
    /// issued under: a body step that may not have run orders what follows it only `maybe`.
    pub(super) prev: Vec<(SiteIndex, &'static str)>,
    pub(super) repeat: Option<&'static str>,
    pub(super) in_parallel: bool,
    /// Set while inside an iteration body: the region ids of the enclosing iterations, with the
    /// literal cardinality of the innermost one. `None` outside a body.
    pub(super) family: Option<(Vec<String>, Option<usize>)>,
}

impl State {
    pub(super) fn root() -> Self {
        Self {
            region: "root".to_string(),
            certainty: ALWAYS,
            phases: Vec::new(),
            prev: Vec::new(),
            repeat: None,
            in_parallel: false,
            family: None,
        }
    }
}

/// One card of the handoff projection: a phase and a lane, or one member of a family.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CardKey {
    pub(super) phase: String,
    pub(super) lane: String,
    pub(super) member: Option<(usize, usize)>,
}

impl CardKey {
    pub(super) fn id(&self) -> String {
        match self.member {
            Some((index, of)) => format!("{}:{}[{}/{}]", self.phase, self.lane, index, of),
            None => format!("{}:{}", self.phase, self.lane),
        }
    }
}

struct Projection<'a> {
    pub(super) plan: &'a Plan,
    pub(super) steps: Vec<Step>,
    pub(super) edges: Vec<Edge>,
    pub(super) regions: Vec<Region>,
    pub(super) lanes: Vec<Lane>,
    pub(super) lane_ids: BTreeSet<String>,
    pub(super) counters: BTreeMap<String, usize>,
    /// Which site produced a slot's value (several for a `Promise.all`).
    pub(super) producers: HashMap<Slot, Vec<SiteIndex>>,
    /// What each facade site read, so a non-step producer (a publish, a join) resolves to the
    /// steps that fed it.
    pub(super) site_inputs: BTreeMap<SiteIndex, BTreeSet<SiteIndex>>,
    pub(super) actor_of: BTreeMap<SiteIndex, String>,
    /// The marker trace of the whole walk, in program order: the input to the phase quotient.
    pub(super) trace: Vec<Trace>,
    pub(super) sink: BTreeSet<SiteIndex>,
    /// Any phase that contains an awaitable call has a rejection path.
    pub(super) phases_that_can_reject: BTreeSet<usize>,
}

/// One entry of the marker trace.
#[derive(Clone, Copy)]
enum Trace {
    /// A phase marker, and whether it sat inside a repeating region.
    Mark(usize, bool),
    /// A `return`: control leaves through the sink.
    Exit,
}
