//! The walk: the lowered tree read into steps, lanes, regions, edges and a marker trace.
//!
//! Everything here recovers a fact - what ran before what, under which phase, in which region,
//! with what certainty. Turning facts into JSON is `super::display`'s job, and the split matters
//! because the two change for different reasons: a new statement shape touches the walk, a new
//! card field touches the renderers.

use std::collections::BTreeSet;

use serde_json::Value;

use super::*;
use crate::script::ast::Span;
use crate::script::ir::{Expr, ExprNode, Instr, SiteIndex, SiteKind, TemplateChunk};
use crate::script::plan::Plan;

impl<'a> Projection<'a> {
    pub(super) fn new(plan: &'a Plan) -> Self {
        let mut lane_ids = BTreeSet::new();
        lane_ids.insert(WORKSPACE_LANE.to_string());
        Self {
            plan,
            steps: Vec::new(),
            edges: Vec::new(),
            regions: vec![Region {
                id: "root".to_string(),
                kind: RegionKind::Seq,
                parent: None,
                span: None,
            }],
            lanes: Vec::new(),
            lane_ids,
            counters: BTreeMap::new(),
            producers: HashMap::new(),
            site_inputs: BTreeMap::new(),
            actor_of: BTreeMap::new(),
            trace: Vec::new(),
            sink: BTreeSet::new(),
            phases_that_can_reject: BTreeSet::new(),
        }
    }

    fn open_region(&mut self, counter: &str, kind: RegionKind, span: Span, parent: &str) -> String {
        let count = self.counters.entry(counter.to_string()).or_insert(0);
        *count += 1;
        let id = format!("{counter}#{count}");
        self.regions.push(Region {
            id: id.clone(),
            kind,
            parent: Some(parent.to_string()),
            span: Some(span),
        });
        id
    }

    pub(super) fn is_step(&self, site: SiteIndex) -> bool {
        matches!(
            self.plan.sites[site].kind,
            SiteKind::Ask | SiteKind::WorldRead
        )
    }

    fn push_edge(
        &mut self,
        from: SiteIndex,
        to: SiteIndex,
        kind: &'static str,
        certainty: &'static str,
    ) {
        if from == to || !self.is_step(from) || !self.is_step(to) {
            return;
        }
        let candidate = Edge {
            from,
            to,
            kind,
            certainty,
        };
        if !self.edges.contains(&candidate) {
            self.edges.push(candidate);
        }
    }

    /// Record a lane when its `agent()` site is first seen. A second sighting of the same site in
    /// another scope is the same lane, so nothing is duplicated.
    pub(super) fn register_actor_lane(
        &mut self,
        site: SiteIndex,
        state: &State,
        name: Option<String>,
        pattern: Option<Value>,
    ) {
        let id = self.plan.sites[site].id.clone();
        self.actor_of.insert(site, id.clone());
        if self.lane_ids.insert(id.clone()) {
            self.lanes.push(Lane {
                families: state
                    .family
                    .as_ref()
                    .map(|(regions, _)| regions.clone())
                    .unwrap_or_default(),
                cardinality: state.family.as_ref().and_then(|(_, count)| *count),
                id,
                name,
                name_pattern: pattern,
                span: self.plan.sites[site].span,
            });
        }
    }

    pub(super) fn run(&mut self, body: &[Instr], mut state: State) -> State {
        for instruction in body {
            state = self.statement(instruction, state);
        }
        state
    }

    fn statement(&mut self, instruction: &Instr, state: State) -> State {
        match instruction {
            Instr::EnterPhase { name, .. } => {
                let mut next = state;
                if let Some(index) = self.plan.phase_names.iter().position(|phase| phase == name) {
                    next.phases = vec![index];
                    let inside_loop = next.repeat.is_some();
                    self.trace.push(Trace::Mark(index, inside_loop));
                }
                next
            }
            Instr::Let { slot, value, .. } => {
                let emitted = self.expression(value, Some(*slot), &state);
                self.thread(state, emitted)
            }
            Instr::Effect { expr } => {
                let emitted = self.expression(expr, None, &state);
                self.thread(state, emitted)
            }
            Instr::Block { body, .. } => self.run(body, state),
            Instr::Return { value, .. } => {
                if let Some(expr) = value {
                    let _ = self.expression(expr, None, &state);
                    self.sink.extend(self.step_inputs(expr, usize::MAX));
                }
                self.trace.push(Trace::Exit);
                State {
                    prev: Vec::new(),
                    ..state
                }
            }
            Instr::If {
                condition,
                then_branch,
                else_branch,
                span,
            } => {
                let guard = self.step_inputs(condition, usize::MAX);
                let _ = self.expression(condition, None, &state);
                let region = self.open_region("branch", RegionKind::Branch, *span, &state.region);
                let mut phases: Vec<usize> = Vec::new();
                let mut prev: Vec<(SiteIndex, &'static str)> = Vec::new();
                for branch in [then_branch.as_slice(), else_branch.as_slice()] {
                    if branch.is_empty() {
                        continue;
                    }
                    let arm = State {
                        region: region.clone(),
                        certainty: MAYBE,
                        phases: state.phases.clone(),
                        prev: state.prev.clone(),
                        repeat: state.repeat,
                        in_parallel: state.in_parallel,
                        family: state.family.clone(),
                    };
                    let exit = self.run(branch, arm);
                    self.tag_region(&guard, &region);
                    for phase in exit.phases {
                        if !phases.contains(&phase) {
                            phases.push(phase);
                        }
                    }
                    for entry in exit.prev {
                        if !prev.contains(&entry) {
                            prev.push(entry);
                        }
                    }
                }
                State {
                    region: state.region,
                    certainty: state.certainty,
                    phases: if phases.is_empty() {
                        state.phases
                    } else {
                        phases
                    },
                    prev: if prev.is_empty() { state.prev } else { prev },
                    repeat: state.repeat,
                    in_parallel: state.in_parallel,
                    family: state.family,
                }
            }
            Instr::While {
                condition,
                body,
                span,
            } => {
                let feeds = self.step_inputs(condition, usize::MAX);
                let _ = self.expression(condition, None, &state);
                self.repeating("loop", RegionKind::Loop, *span, body, &state, &feeds, None)
            }
            Instr::ForOf {
                iterable,
                body,
                span,
                ..
            } => {
                let feeds = self.step_inputs(iterable, usize::MAX);
                let _ = self.expression(iterable, None, &state);
                let cardinality = literal_cardinality(iterable);
                let family = body_mints_actors(body).then_some(cardinality);
                let (counter, kind) = if family.is_some() {
                    ("fanout", RegionKind::Fanout)
                } else {
                    ("loop", RegionKind::Loop)
                };
                self.repeating(counter, kind, *span, body, &state, &feeds, family)
            }
            Instr::Parallel { strands, span } => {
                let region =
                    self.open_region("parallel", RegionKind::Parallel, *span, &state.region);
                let mut prev: Vec<(SiteIndex, &'static str)> = Vec::new();
                let mut phases: Vec<usize> = Vec::new();
                for strand in strands {
                    let arm = State {
                        region: region.clone(),
                        certainty: state.certainty,
                        phases: state.phases.clone(),
                        prev: state.prev.clone(),
                        repeat: state.repeat,
                        in_parallel: true,
                        family: state.family.clone(),
                    };
                    let exit = self.run(strand, arm);
                    for entry in exit.prev {
                        if !prev.contains(&entry) {
                            prev.push(entry);
                        }
                    }
                    for phase in exit.phases {
                        if !phases.contains(&phase) {
                            phases.push(phase);
                        }
                    }
                }
                State {
                    region: state.region,
                    certainty: state.certainty,
                    // The block is a barrier: what follows runs after every strand, so it inherits
                    // all of them as predecessors and none of their phases as its own.
                    phases: if phases.is_empty() {
                        state.phases
                    } else {
                        phases
                    },
                    prev: if prev.is_empty() { state.prev } else { prev },
                    repeat: state.repeat,
                    in_parallel: state.in_parallel,
                    family: state.family,
                }
            }
            Instr::Try {
                body,
                error_slot,
                catch_body,
                span,
            } => {
                let region = self.open_region("branch", RegionKind::Branch, *span, &state.region);
                let body_scope = State {
                    region: region.clone(),
                    certainty: MAYBE,
                    ..state.clone()
                };
                let first = self.run(body, body_scope);
                // The catch binding is the engine's structured error, not a facade product, so it
                // has no producer site: reads of it contribute no edge.
                self.producers.insert(*error_slot, Vec::new());
                let catch_scope = State {
                    region: region.clone(),
                    certainty: MAYBE,
                    prev: Vec::new(),
                    ..state.clone()
                };
                let second = self.run(catch_body, catch_scope);
                let mut phases = first.phases;
                for phase in second.phases {
                    if !phases.contains(&phase) {
                        phases.push(phase);
                    }
                }
                let mut prev = first.prev;
                for entry in second.prev {
                    if !prev.contains(&entry) {
                        prev.push(entry);
                    }
                }
                State {
                    prev: if prev.is_empty() { state.prev } else { prev },
                    phases: if phases.is_empty() {
                        state.phases
                    } else {
                        phases
                    },
                    ..state
                }
            }
        }
    }

    /// One repeating construct: its body is a fresh round, so a step at the top of the body has no
    /// in-body predecessor, and the enclosing sequence has no successor from the body.
    #[allow(clippy::too_many_arguments)]
    fn repeating(
        &mut self,
        counter: &str,
        kind: RegionKind,
        span: Span,
        body: &[Instr],
        state: &State,
        feeds: &BTreeSet<SiteIndex>,
        cardinality: Option<Option<usize>>,
    ) -> State {
        let region = self.open_region(counter, kind, span, &state.region);
        let family = cardinality.map(|count| {
            let mut regions = state
                .family
                .as_ref()
                .map(|(regions, _)| regions.clone())
                .unwrap_or_default();
            regions.push(region.clone());
            (regions, count)
        });
        let inner = State {
            region,
            certainty: MAYBE,
            phases: state.phases.clone(),
            prev: Vec::new(),
            // `stack` when instances coexist (a `parallel` block encloses them), `serial` when an
            // await barrier or one actor's mailbox orders the rounds.
            repeat: Some(if state.in_parallel { "stack" } else { "serial" }),
            in_parallel: state.in_parallel,
            family,
        };
        let region_id = inner.region.clone();
        let exit = self.run(body, inner);
        self.tag_region(feeds, &region_id);
        let mut prev = state.prev.clone();
        // Zero rounds is allowed, so the body's last steps stay `maybe` predecessors — but they are
        // predecessors: a loop that finished does precede what comes after it.
        for entry in exit.prev {
            if !prev.contains(&entry) {
                prev.push(entry);
            }
        }
        State {
            prev,
            phases: if exit.phases.is_empty() {
                state.phases.clone()
            } else {
                exit.phases
            },
            ..state.clone()
        }
    }

    /// Everything inside `region` is guarded by `guard`.
    fn tag_region(&mut self, guard: &BTreeSet<SiteIndex>, region: &str) {
        let sites: Vec<SiteIndex> = self
            .steps
            .iter()
            .filter(|step| step.region == region)
            .map(|step| step.site)
            .collect();
        for site in sites {
            for feed in guard {
                self.push_edge(*feed, site, "control", MAYBE);
            }
        }
    }

    // ———————————————————————— expressions ————————————————————————

    /// Walk an expression, emitting the facade calls it contains in evaluation order. `into` is the
    /// slot the value lands in, which is what a later read resolves back to this site.
    /// The await points this expression emitted, in evaluation order. A statement threads them into
    /// `prev`, which is what puts a seq edge between two consecutive awaits — and what keeps the
    /// items of one `Promise.all` un-ordered relative to each other, since they are handed the same
    /// predecessor set.
    pub(super) fn arguments_inputs(
        &self,
        args: &[Expr],
        exclude: SiteIndex,
    ) -> BTreeSet<SiteIndex> {
        let mut inputs = BTreeSet::new();
        for arg in args {
            inputs.extend(self.step_inputs(arg, exclude));
        }
        inputs
    }

    /// A non-step site (publish, declare, report) is not a node, but what it read still matters for
    /// the flattening in [`Projection::step_inputs`].
    pub(super) fn record_inputs(
        &mut self,
        site: SiteIndex,
        inputs: &BTreeSet<SiteIndex>,
        state: &State,
    ) {
        self.site_inputs.insert(site, inputs.clone());
        for input in inputs {
            self.push_edge(*input, site, "data", state.certainty);
        }
    }

    /// Whatever an expression awaited becomes this statement's predecessor set.
    fn thread(&self, state: State, emitted: Vec<SiteIndex>) -> State {
        if emitted.is_empty() {
            state
        } else {
            State {
                prev: emitted
                    .into_iter()
                    .map(|site| (site, state.certainty))
                    .collect(),
                ..state
            }
        }
    }

    pub(super) fn record_step(
        &mut self,
        state: &State,
        draft: NewStep,
        inputs: &BTreeSet<SiteIndex>,
    ) {
        let NewStep {
            site,
            kind,
            label,
            label_pattern,
            lane,
            lanes,
        } = draft;
        self.site_inputs.insert(site, inputs.clone());
        for input in inputs {
            self.push_edge(*input, site, "data", state.certainty);
        }
        for (previous, previous_certainty) in &state.prev {
            let certainty = if state.certainty == MAYBE || *previous_certainty == MAYBE {
                MAYBE
            } else {
                ALWAYS
            };
            self.push_edge(*previous, site, "seq", certainty);
        }
        for phase in &state.phases {
            self.phases_that_can_reject.insert(*phase);
        }
        self.steps.push(Step {
            site,
            kind,
            label,
            label_pattern,
            span: self.plan.sites[site].span,
            lane,
            lanes,
            region: state.region.clone(),
            certainty: state.certainty,
            phases: state.phases.clone(),
            repeat: state.repeat,
        });
    }
}

fn literal_cardinality(iterable: &Expr) -> Option<usize> {
    match &iterable.node {
        ExprNode::Array(items) if items.len() <= FAMILY_EXPANSION_LIMIT => Some(items.len()),
        ExprNode::Literal(Value::Array(items)) if items.len() <= FAMILY_EXPANSION_LIMIT => {
            Some(items.len())
        }
        _ => None,
    }
}

fn body_mints_actors(body: &[Instr]) -> bool {
    body.iter().any(|instruction| match instruction {
        Instr::Let { value, .. } | Instr::Effect { expr: value } => expr_mints_actors(value),
        Instr::Block { body, .. } | Instr::While { body, .. } | Instr::ForOf { body, .. } => {
            body_mints_actors(body)
        }
        Instr::If {
            then_branch,
            else_branch,
            ..
        } => body_mints_actors(then_branch) || body_mints_actors(else_branch),
        Instr::Parallel { strands, .. } => strands.iter().any(|strand| body_mints_actors(strand)),
        Instr::Try {
            body, catch_body, ..
        } => body_mints_actors(body) || body_mints_actors(catch_body),
        Instr::Return { value, .. } => value.as_ref().is_some_and(expr_mints_actors),
        Instr::EnterPhase { .. } => false,
    })
}

fn expr_mints_actors(expr: &Expr) -> bool {
    match &expr.node {
        ExprNode::CreateActor { .. } => true,
        ExprNode::Array(items) | ExprNode::Join { items, .. } => {
            items.iter().any(expr_mints_actors)
        }
        ExprNode::Template(chunks) => chunks.iter().any(|chunk| match chunk {
            TemplateChunk::Expr(inner) => expr_mints_actors(inner),
            TemplateChunk::Text(_) => false,
        }),
        ExprNode::Object(fields) => fields.iter().any(|field| expr_mints_actors(&field.value)),
        ExprNode::Unary { value, .. } => expr_mints_actors(value),
        ExprNode::Binary { left, right, .. } => expr_mints_actors(left) || expr_mints_actors(right),
        ExprNode::Member { object, .. } => expr_mints_actors(object),
        ExprNode::Index { object, index } => expr_mints_actors(object) || expr_mints_actors(index),
        _ => false,
    }
}
