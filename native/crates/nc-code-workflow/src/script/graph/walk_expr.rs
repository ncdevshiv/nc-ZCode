//! The expression half of the walk: what an expression contributes to the graph.
//!
//! An NWS expression is either a value computation or one facade call, and facade calls are the
//! only things the display graph has nodes for. So this file answers three questions and nothing
//! else: which slots an expression reads (`slots_read`, `step_inputs`), which lane an actor
//! receiver names (`receiver_lane`), and what the call becomes as a step (`expression`).

use std::collections::BTreeSet;

use serde_json::{json, Map, Value};

use crate::script::ir::{Expr, ExprNode, SiteIndex, Slot, TemplateChunk};

use super::*;

impl<'a> Projection<'a> {
    fn slots_read(expr: &Expr, out: &mut BTreeSet<Slot>) {
        match &expr.node {
            ExprNode::Local(slot) => {
                out.insert(*slot);
            }
            ExprNode::Template(chunks) => {
                for chunk in chunks {
                    if let TemplateChunk::Expr(inner) = chunk {
                        Self::slots_read(inner, out);
                    }
                }
            }
            ExprNode::Array(items) | ExprNode::Join { items, .. } => {
                for item in items {
                    Self::slots_read(item, out);
                }
            }
            ExprNode::Object(fields) => {
                for field in fields {
                    Self::slots_read(&field.value, out);
                }
            }
            ExprNode::Unary { value, .. } => Self::slots_read(value, out),
            ExprNode::Binary { left, right, .. } => {
                Self::slots_read(left, out);
                Self::slots_read(right, out);
            }
            ExprNode::Member { object, .. } => Self::slots_read(object, out),
            ExprNode::Index { object, index } => {
                Self::slots_read(object, out);
                Self::slots_read(index, out);
            }
            ExprNode::Ask {
                actor,
                instructions,
                ..
            } => {
                Self::slots_read(actor, out);
                Self::slots_read(instructions, out);
            }
            ExprNode::WorldRead { args, .. }
            | ExprNode::PublishArtifact { args, .. }
            | ExprNode::DeclareArtifact { args, .. } => {
                for arg in args {
                    Self::slots_read(arg, out);
                }
            }
            ExprNode::Report { item, .. } => Self::slots_read(item, out),
            ExprNode::Log { message } => Self::slots_read(message, out),
            ExprNode::CreateActor { name, persona, .. } => {
                for arg in name.iter().chain(persona.iter()) {
                    Self::slots_read(arg, out);
                }
            }
            ExprNode::Literal(_) | ExprNode::ArgsObject | ExprNode::ArgField { .. } => {}
        }
    }

    /// The step sites whose value reaches `expr`, flattened through non-step producers.
    pub(super) fn step_inputs(&self, expr: &Expr, exclude: SiteIndex) -> BTreeSet<SiteIndex> {
        let mut slots = BTreeSet::new();
        Self::slots_read(expr, &mut slots);
        let mut out = BTreeSet::new();
        let mut visited = BTreeSet::new();
        let mut pending: Vec<SiteIndex> = Vec::new();
        for slot in slots {
            if let Some(sites) = self.producers.get(&slot) {
                pending.extend(sites.iter().copied());
            }
        }
        while let Some(site) = pending.pop() {
            if site == exclude || !visited.insert(site) {
                continue;
            }
            if self.is_step(site) {
                out.insert(site);
            } else if let Some(inputs) = self.site_inputs.get(&site) {
                pending.extend(inputs.iter().copied());
            }
        }
        out
    }

    /// The actor lane a `.ask` receiver names: exactly one when the slot is pinned to one actor
    /// site, a may-set when several, `unknown` when none.
    fn receiver_lane(&self, receiver: &Expr) -> (String, Vec<String>) {
        let mut slots = BTreeSet::new();
        Self::slots_read(receiver, &mut slots);
        let mut lanes: Vec<String> = Vec::new();
        for slot in slots {
            for site in self.producers.get(&slot).into_iter().flatten() {
                if let Some(lane) = self.actor_of.get(site) {
                    if !lanes.contains(lane) {
                        lanes.push(lane.clone());
                    }
                }
            }
        }
        match lanes.len() {
            1 => (lanes.remove(0), Vec::new()),
            0 => (UNKNOWN_LANE.to_string(), Vec::new()),
            _ => (lanes[0].clone(), lanes),
        }
    }

    /// A card's label: the ask's own instruction text, with the static shape (head and tail around
    /// the first hole) the contract reserves for a string that is only partly known. Concatenation
    /// is flattened exactly like a template, because `"Draft for " + args.repo` is the common shape
    /// and falling back to the site id would throw away the one thing the card could show.
    fn label_of(instructions: &Expr) -> (String, Option<Value>) {
        fn flatten(expr: &Expr, parts: &mut Vec<Option<String>>) {
            match &expr.node {
                ExprNode::Binary {
                    op: crate::script::ast::BinaryOp::Add,
                    left,
                    right,
                } => {
                    flatten(left, parts);
                    flatten(right, parts);
                }
                ExprNode::Literal(Value::String(text)) => parts.push(Some(text.clone())),
                ExprNode::Template(chunks) => {
                    for chunk in chunks {
                        match chunk {
                            TemplateChunk::Text(text) => parts.push(Some(text.clone())),
                            TemplateChunk::Expr(inner) => flatten(inner, parts),
                        }
                    }
                }
                _ => parts.push(None),
            }
        }
        let mut parts: Vec<Option<String>> = Vec::new();
        flatten(instructions, &mut parts);
        let holes = parts.iter().filter(|part| part.is_none()).count();
        if holes == 0 && parts.len() > 1 {
            // Every piece is literal: the concatenation is the text the ask will actually carry.
            let text: String = parts
                .iter()
                .map(|part| part.as_deref().unwrap_or_default())
                .collect();
            return (text, None);
        }
        if holes > 0 {
            let mut head = String::new();
            let mut tail = String::new();
            let mut seen_hole = false;
            for part in &parts {
                match part {
                    Some(text) => {
                        if seen_hole {
                            tail.push_str(text);
                        } else {
                            head.push_str(text);
                        }
                    }
                    None => seen_hole = true,
                }
            }
            let mut pattern = Map::new();
            if !head.is_empty() {
                pattern.insert("head".to_string(), json!(head));
            }
            if !tail.is_empty() {
                pattern.insert("tail".to_string(), json!(tail));
            }
            let mut label = head;
            label.push_str(&tail);
            return (label, (!pattern.is_empty()).then(|| Value::Object(pattern)));
        }
        let template_shape = |chunks: &[TemplateChunk]| -> (String, Option<Value>) {
            let mut head = String::new();
            let mut tail = String::new();
            let mut hole = false;
            for chunk in chunks {
                match chunk {
                    TemplateChunk::Text(text) => {
                        if hole {
                            tail.push_str(text);
                        } else {
                            head.push_str(text);
                        }
                    }
                    TemplateChunk::Expr(_) => hole = true,
                }
            }
            let mut pattern = Map::new();
            if !head.is_empty() {
                pattern.insert("head".to_string(), json!(head));
            }
            if !tail.is_empty() {
                pattern.insert("tail".to_string(), json!(tail));
            }
            let mut label = head;
            label.push_str(&tail);
            (label, (!pattern.is_empty()).then(|| Value::Object(pattern)))
        };
        match &instructions.node {
            ExprNode::Literal(Value::String(text)) => (text.clone(), None),
            ExprNode::Template(chunks) => template_shape(chunks),
            _ => (String::new(), None),
        }
    }

    fn literal_text(expr: &Expr) -> Option<String> {
        match &expr.node {
            ExprNode::Literal(Value::String(text)) => Some(text.clone()),
            ExprNode::Template(chunks) => {
                let text: String = chunks
                    .iter()
                    .filter_map(|chunk| match chunk {
                        TemplateChunk::Text(part) => Some(part.as_str()),
                        TemplateChunk::Expr(_) => None,
                    })
                    .collect();
                (!text.is_empty()).then_some(text)
            }
            _ => None,
        }
    }

    // ———————————————————————— statements ————————————————————————

    pub(super) fn expression(
        &mut self,
        expr: &Expr,
        into: Option<Slot>,
        state: &State,
    ) -> Vec<SiteIndex> {
        match &expr.node {
            ExprNode::Ask {
                site,
                actor,
                instructions,
            } => {
                let mut inputs = self.step_inputs(actor, *site);
                inputs.extend(self.step_inputs(instructions, *site));
                let (lane, lanes) = self.receiver_lane(actor);
                let (label, label_pattern) = Self::label_of(instructions);
                let label = if label.is_empty() {
                    self.plan.sites[*site].id.clone()
                } else {
                    label
                };
                self.record_step(
                    state,
                    NewStep {
                        site: *site,
                        kind: KIND_ASK,
                        label,
                        label_pattern,
                        lane,
                        lanes,
                    },
                    &inputs,
                );
                if let Some(slot) = into {
                    self.producers.insert(slot, vec![*site]);
                }
                vec![*site]
            }
            ExprNode::WorldRead { site, op, args } => {
                let inputs = self.arguments_inputs(args, *site);
                let first = args
                    .first()
                    .and_then(Self::literal_text)
                    .unwrap_or_default();
                let label = if first.is_empty() {
                    op.clone()
                } else {
                    format!("{op} {first}")
                };
                self.record_step(
                    state,
                    NewStep {
                        site: *site,
                        kind: KIND_WORLD_READ,
                        label,
                        label_pattern: None,
                        lane: WORKSPACE_LANE.to_string(),
                        lanes: Vec::new(),
                    },
                    &inputs,
                );
                if let Some(slot) = into {
                    self.producers.insert(slot, vec![*site]);
                }
                vec![*site]
            }
            ExprNode::PublishArtifact { site, args, .. }
            | ExprNode::DeclareArtifact { site, args, .. } => {
                let inputs = self.arguments_inputs(args, *site);
                self.record_inputs(*site, &inputs, state);
                if let Some(slot) = into {
                    self.producers.insert(slot, vec![*site]);
                }
                // A publish settles before the next step does, but it is not a node of the graph, so
                // it cannot be an edge endpoint.
                Vec::new()
            }
            ExprNode::Report { site, item, .. } => {
                let inputs = self.step_inputs(item, *site);
                self.record_inputs(*site, &inputs, state);
                Vec::new()
            }
            ExprNode::CreateActor { site, name, .. } => {
                // `name` is the author's verbatim word and never a reconstruction: a template
                // carries its static shape in `namePattern` instead, which is what tells the GUI
                // the lane is one member of a family rather than a named actor.
                let literal_name = match name.as_deref() {
                    Some(Expr {
                        node: ExprNode::Literal(Value::String(text)),
                        ..
                    }) if !text.is_empty() => Some(text.clone()),
                    _ => None,
                };
                let pattern = name.as_deref().and_then(|expr| Self::label_of(expr).1);
                self.register_actor_lane(*site, state, literal_name, pattern);
                if let Some(slot) = into {
                    self.producers.insert(slot, vec![*site]);
                }
                Vec::new()
            }
            ExprNode::Join { site, items } => {
                // The items are issued together under one barrier: each is walked with the join's
                // predecessor set, so none of them becomes another's predecessor either.
                let mut members: Vec<SiteIndex> = Vec::new();
                let mut inputs: BTreeSet<SiteIndex> = BTreeSet::new();
                for item in items {
                    inputs.extend(self.step_inputs(item, *site));
                    members.extend(self.expression(item, None, state));
                }
                self.site_inputs.insert(*site, inputs.clone());
                if let Some(slot) = into {
                    self.producers.insert(slot, members.clone());
                }
                members
            }
            ExprNode::Template(chunks) => {
                let mut out = Vec::new();
                for chunk in chunks {
                    if let TemplateChunk::Expr(inner) = chunk {
                        out.extend(self.expression(inner, None, state));
                    }
                }
                out
            }
            ExprNode::Array(items) => {
                let mut out = Vec::new();
                for item in items {
                    out.extend(self.expression(item, None, state));
                }
                out
            }
            ExprNode::Object(fields) => {
                let mut out = Vec::new();
                for field in fields {
                    out.extend(self.expression(&field.value, None, state));
                }
                out
            }
            ExprNode::Unary { value, .. } => self.expression(value, None, state),
            ExprNode::Binary { left, right, .. } => {
                let mut out = self.expression(left, None, state);
                out.extend(self.expression(right, None, state));
                out
            }
            ExprNode::Member { object, .. } => self.expression(object, None, state),
            ExprNode::Index { object, index } => {
                let mut out = self.expression(object, None, state);
                out.extend(self.expression(index, None, state));
                out
            }
            ExprNode::Log { message } => self.expression(message, None, state),
            ExprNode::Literal(_) | ExprNode::ArgsObject | ExprNode::ArgField { .. } => Vec::new(),
            // `let b = a` is an alias, not a new value: b's producers are a's. Resolved here, in
            // declaration order, so no fixpoint is needed later.
            ExprNode::Local(slot) => {
                if let (Some(sites), Some(target)) = (self.producers.get(slot).cloned(), into) {
                    self.producers.insert(target, sites);
                }
                Vec::new()
            }
        }
    }
}
