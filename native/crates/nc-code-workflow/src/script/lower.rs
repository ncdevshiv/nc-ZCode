//! Resolution and lowering of an NWS program into a plan: statements, scopes and the
//! site/profile/phase derivations. Expression lowering is in `lower_expr.rs`.
//!
//! One pass over the AST does the work the TypeScript front-end split across a
//! typechecker, a site table, a schema synthesizer and a lowering step:
//!
//! - names resolve scope-first, so the facade functions (`agent`, `log`) and the facade
//!   containers (`files`, `git`, `world`, `artifact`, `Promise`) are reachable exactly
//!   where the author has not shadowed them;
//! - facade misuse is a compile error, not a runtime surprise: a container member that
//!   is never called, an async call without `await`, an `.ask` on something that is
//!   provably not an actor;
//! - site ids are assigned by per-kind source-order counters (`actor#1`, `ask#2`,
//!   `world-read#1`, `report#1`, `artifact#1`, `join#1`), which is what keeps the
//!   engine's `(siteId, ordinal)` journal identity stable across a resume;
//! - typed asks come from the receiving `let` annotation (or the schema argument of
//!   `.ask`) and compile to the same JSON Schema shape the TypeScript synthesizer
//!   emitted: closed objects with `additionalProperties: false` and `required` for the
//!   non-optional fields.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::{json, Value};

use crate::compile::AskSpecWire;
use crate::types::ArtifactOp;

use super::ast::{
    Block, ElseBranch, Expr as AstExpr, Field, Program, SchemaDecl, Span, Stmt, TypeExpr,
    WorkflowDecl,
};
use super::diagnostic::Diagnostic;
use super::facade;
use super::ir::{Expr, ExprNode, Instr, Site, SiteIndex, SiteKind, Slot};
use super::lower_expr::Position;

/// Everything a clean lowering produced, before the plan attaches identity fields.
pub struct Lowered {
    pub body: Vec<Instr>,
    pub sites: Vec<Site>,
    /// Number of local slots the interpreter allocates per frame; one per declaration.
    pub slot_count: usize,
    /// ask site id → static spec; every ask site has one (the engine hard-fails on a gap).
    pub ask_specs: BTreeMap<String, AskSpecWire>,
    /// Declared schemas compiled to JSON Schema, by name.
    pub schemas: BTreeMap<String, Value>,
    /// The workflow's own `args` compiled to JSON Schema, when it declared any.
    pub args_schema: Option<Value>,
    pub phase_names: Vec<String>,
    pub phase_alongside: Vec<Vec<u32>>,
    pub declared_run_commands: Vec<String>,
    pub actor_submit_profiles: BTreeMap<String, Value>,
}

/// Compile-time identity of a local binding's value, for the actor-receiver rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Not an actor handle.
    None,
    /// An actor handle created at this actor site.
    Actor(SiteIndex),
    /// A value that may hold an actor handle but cannot be traced to one site (a
    /// `for`-of element, an array or object member). An `.ask` on one of these leaves the
    /// actor set empty, which is the same signal the TypeScript derivation reads when it
    /// falls back to `generic` for every actor.
    Unknown,
}

#[derive(Clone, Debug)]
pub struct Binding {
    pub name: String,
    pub name_span: Span,
    pub slot: Slot,
    pub ty: Option<TypeExpr>,
    pub origin: Origin,
}

/// Lower one program: `Err(diagnostics)` when the script is not clean.
pub fn lower(program: &Program) -> Result<Lowered, Vec<Diagnostic>> {
    Lowerer::new(program).run(program)
}

pub struct Lowerer<'a> {
    schemas: BTreeMap<String, &'a SchemaDecl>,
    schema_order: Vec<String>,
    pub arg_fields: Vec<&'a Field>,
    scopes: Vec<Vec<Binding>>,
    next_slot: Slot,
    sites: Vec<Site>,
    counters: BTreeMap<SiteKind, u32>,
    ask_specs: BTreeMap<String, AskSpecWire>,
    /// ask site index → the actor sites its receiver may resolve to (empty = unresolved).
    ask_actors: BTreeMap<SiteIndex, Vec<SiteIndex>>,
    /// ask site index → the schema it was typed with (`None` = untyped).
    ask_schemas: BTreeMap<SiteIndex, Option<Value>>,
    diagnostics: Vec<Diagnostic>,
    /// Preset artifact ids declared anywhere in the script (pre-pass), by id.
    declared_presets: BTreeMap<String, ArtifactOp>,
    /// Artifact ids used by any member, to enforce one member per id.
    used_artifact_ids: BTreeMap<String, ArtifactOp>,
    run_commands: BTreeSet<String>,
    phase_names: Vec<String>,
    phase_index: HashMap<String, u32>,
    /// Literal actor names already claimed, by name → first site index.
    claimed_actor_names: BTreeMap<String, SiteIndex>,
    /// Statements that run once per element (`for`-of, `while`).
    fanout_depth: u32,
    /// Nesting depth of `parallel` blocks: a `return` inside one has no body to unwind to.
    parallel_depth: u32,
    /// The async call currently being lowered, for the `await` rule's message.
    in_join_items: bool,
}

impl<'a> Lowerer<'a> {
    fn new(program: &'a Program) -> Self {
        let mut schemas = BTreeMap::new();
        let mut schema_order = Vec::new();
        for declaration in &program.schemas {
            schemas.insert(declaration.name.clone(), declaration);
            schema_order.push(declaration.name.clone());
        }
        Self {
            arg_fields: program.workflow.args.iter().collect(),
            schemas,
            schema_order,
            scopes: Vec::new(),
            next_slot: 0,
            sites: Vec::new(),
            counters: BTreeMap::new(),
            ask_specs: BTreeMap::new(),
            ask_actors: BTreeMap::new(),
            ask_schemas: BTreeMap::new(),
            diagnostics: Vec::new(),
            declared_presets: BTreeMap::new(),
            used_artifact_ids: BTreeMap::new(),
            run_commands: BTreeSet::new(),
            phase_names: Vec::new(),
            phase_index: HashMap::new(),
            claimed_actor_names: BTreeMap::new(),
            fanout_depth: 0,
            parallel_depth: 0,
            in_join_items: false,
        }
    }

    fn run(mut self, program: &'a Program) -> Result<Lowered, Vec<Diagnostic>> {
        self.check_schemas();
        self.collect_preset_declarations(&program.workflow);
        self.enter_scope();
        let body = self.lower_statements(&program.workflow.body);
        self.leave_scope();
        let actor_sites = self
            .sites
            .iter()
            .enumerate()
            .filter(|(_, site)| site.kind == SiteKind::Actor)
            .map(|(index, _)| index)
            .collect::<Vec<SiteIndex>>();
        let actor_submit_profiles = self.derive_submit_profiles(&actor_sites);
        let phase_alongside = derive_phase_alongside(&body, &self.phase_index);
        if !self.diagnostics.is_empty() {
            return Err(self.diagnostics);
        }
        // Computed before the struct literal: the schema and args projections borrow
        // `self`, and `ask_specs`/`sites` are moved out of it.
        let schemas = self.compile_schemas();
        let args_schema = self.args_object_schema();
        let slot_count = self.next_slot;
        let declared_run_commands = std::mem::take(&mut self.run_commands);
        let phase_names = std::mem::take(&mut self.phase_names);
        Ok(Lowered {
            body,
            sites: self.sites,
            slot_count,
            ask_specs: self.ask_specs,
            schemas,
            args_schema,
            phase_names,
            phase_alongside,
            declared_run_commands: declared_run_commands.into_iter().collect(),
            actor_submit_profiles,
        })
    }

    // —————————————————————— diagnostics ——————————————————————

    pub fn push(&mut self, span: Span, code: &'static str, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic::new(span, code, message));
    }

    // —————————————————————— scopes and slots ——————————————————————

    pub fn enter_scope(&mut self) {
        self.scopes.push(Vec::new());
    }

    pub fn leave_scope(&mut self) {
        self.scopes.pop();
    }

    pub fn lookup(&self, name: &str) -> Option<Binding> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.iter().rev().find(|binding| binding.name == name))
            .cloned()
    }

    /// Declare a local and return its slot. Redeclaring in the same block is an error;
    /// shadowing an outer block's binding is legal.
    pub fn declare(
        &mut self,
        name: &str,
        name_span: Span,
        ty: Option<TypeExpr>,
        origin: Origin,
    ) -> Slot {
        let slot = self.next_slot;
        self.next_slot += 1;
        if let Some(existing) = self
            .scopes
            .last()
            .and_then(|scope| scope.iter().rev().find(|binding| binding.name == name))
        {
            self.push(
                name_span,
                "nws_duplicate_declaration",
                format!(
                    "`{name}` is already declared in this block (line {}): `let` has no \
                     redeclaration. Rename one of them.",
                    existing.name_span.line
                ),
            );
        }
        if let Some(scope) = self.scopes.last_mut() {
            scope.push(Binding {
                name: name.to_string(),
                name_span,
                slot,
                ty,
                origin,
            });
        }
        slot
    }

    // —————————————————————— statements ——————————————————————

    pub fn lower_statements(&mut self, statements: &'a [Stmt]) -> Vec<Instr> {
        statements
            .iter()
            .map(|statement| self.lower_statement(statement))
            .collect()
    }

    fn lower_block(&mut self, block: &'a Block) -> Vec<Instr> {
        self.enter_scope();
        let instrs = self.lower_statements(&block.statements);
        self.leave_scope();
        instrs
    }

    fn lower_statement(&mut self, statement: &'a Stmt) -> Instr {
        match statement {
            Stmt::Phase { name, span } => {
                let trimmed = name.trim();
                if trimmed.is_empty() {
                    self.push(
                        *span,
                        "nws_phase_name_empty",
                        "a phase name must not be empty: `phase \"plan\"` names a stage the \
                         user watches, and an empty string names nothing.",
                    );
                }
                self.record_phase(trimmed);
                Instr::EnterPhase {
                    name: trimmed.to_string(),
                    span: *span,
                }
            }
            Stmt::Let {
                name,
                ty,
                value,
                span,
            } => {
                if let Some(ty) = ty {
                    self.check_type_resolvable(ty, value.span());
                }
                let expr = self.lower_expression(value, ty.as_ref(), Position::Value);
                let origin = self.origin_of(&expr);
                let slot = self.declare(name, value.span(), ty.clone(), origin);
                Instr::Let {
                    slot,
                    value: expr,
                    span: *span,
                }
            }
            Stmt::Return { value, span } => {
                if self.parallel_depth > 0 {
                    self.push(
                        *span,
                        "nws_return_in_parallel",
                        "`return` inside a `parallel` block has no body to unwind to: a strand                          ends when its statements end, so return from the workflow after the                          `parallel` block instead.",
                    );
                }
                Instr::Return {
                    value: value
                        .as_ref()
                        .map(|value| self.lower_expression(value, None, Position::Value)),
                    span: *span,
                }
            }
            Stmt::Report {
                item,
                artifact_id,
                span,
            } => {
                let site = self.add_site(SiteKind::Report, "report", *span);
                let tag = self.lower_report_tag(artifact_id.as_ref(), *span);
                let lowered_item = self.lower_expression(item, None, Position::Value);
                Instr::Effect {
                    expr: Expr::new(
                        *span,
                        ExprNode::Report {
                            site,
                            item: Box::new(lowered_item),
                            artifact_id: tag,
                        },
                    ),
                }
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
                span,
            } => {
                let condition = self.lower_expression(condition, None, Position::Value);
                let then_branch = self.lower_block(then_branch);
                let else_branch = match else_branch.as_ref() {
                    None => Vec::new(),
                    Some(ElseBranch::Block(block)) => self.lower_block(block),
                    Some(ElseBranch::If(nested)) => vec![self.lower_statement(nested.as_ref())],
                };
                Instr::If {
                    condition,
                    then_branch,
                    else_branch,
                    span: *span,
                }
            }
            Stmt::While {
                condition,
                body,
                span,
            } => {
                let condition = self.lower_expression(condition, None, Position::Value);
                self.fanout_depth += 1;
                let body = self.lower_block(body);
                self.fanout_depth -= 1;
                Instr::While {
                    condition,
                    body,
                    span: *span,
                }
            }
            Stmt::ForOf {
                binding,
                iterable,
                body,
                span,
            } => {
                let lowered_iterable = self.lower_expression(iterable, None, Position::Value);
                self.fanout_depth += 1;
                self.enter_scope();
                // A loop element can hold a handle (`for (const a of actors)`) and nothing
                // static says which actor it came from, so its origin is unknown.
                let slot = self.declare(binding, *span, None, Origin::Unknown);
                let body_instrs = self.lower_statements(&body.statements);
                self.leave_scope();
                self.fanout_depth -= 1;
                Instr::ForOf {
                    slot,
                    iterable: lowered_iterable,
                    body: body_instrs,
                    span: *span,
                }
            }
            Stmt::Parallel { body, span } => {
                // Each statement of the block is one strand; a `{ … }` group is one
                // multi-statement strand. A strand runs exactly once per block, so a
                // literal actor name inside it is not a fan-out — two strands with the same
                // literal name are two sites, and the duplicate-name clause catches those.
                let mut strands = Vec::with_capacity(body.statements.len());
                self.parallel_depth += 1;
                for statement in &body.statements {
                    let instrs = match statement {
                        Stmt::Block { block, .. } => {
                            self.enter_scope();
                            let instrs = self.lower_statements(&block.statements);
                            self.leave_scope();
                            instrs
                        }
                        other => {
                            self.enter_scope();
                            let instrs = vec![self.lower_statement(other)];
                            self.leave_scope();
                            instrs
                        }
                    };
                    strands.push(instrs);
                }
                self.parallel_depth -= 1;
                Instr::Parallel {
                    strands,
                    span: *span,
                }
            }
            Stmt::Block { block, span } => Instr::Block {
                body: self.lower_block(block),
                span: *span,
            },
            Stmt::Try {
                body,
                error_binding,
                catch_body,
                span,
            } => {
                // The try body is its own declaration scope, and the catch binding is visible
                // only inside the catch body — hence a scope of its own for the declaration.
                self.enter_scope();
                let body_instrs = self.lower_statements(&body.statements);
                self.leave_scope();
                self.enter_scope();
                let slot = self.declare(error_binding, *span, None, Origin::None);
                let catch_instrs = self.lower_statements(&catch_body.statements);
                self.leave_scope();
                Instr::Try {
                    body: body_instrs,
                    error_slot: slot,
                    catch_body: catch_instrs,
                    span: *span,
                }
            }
            Stmt::Expr { value, span } => {
                let expr = self.lower_expression(value, None, Position::Statement);
                match &expr.node {
                    ExprNode::Literal(_)
                    | ExprNode::Local(_)
                    | ExprNode::ArgsObject
                    | ExprNode::ArgField { .. }
                    | ExprNode::Array(_)
                    | ExprNode::Object(_)
                    | ExprNode::Template(_)
                    | ExprNode::Member { .. }
                    | ExprNode::Index { .. }
                    | ExprNode::Unary { .. }
                    | ExprNode::Binary { .. } => {
                        self.push(
                            *span,
                            "nws_statement_has_no_effect",
                            "this statement computes a value and throws it away: an \
                             effectful statement is a facade call (`log(…)`, `await …`) or a \
                             `let`.",
                        );
                        Instr::Effect {
                            expr: Expr::new(*span, ExprNode::Literal(Value::Null)),
                        }
                    }
                    _ => Instr::Effect { expr },
                }
            }
        }
    }

    fn record_phase(&mut self, name: &str) {
        if self.phase_index.contains_key(name) {
            return;
        }
        let index = u32::try_from(self.phase_names.len()).unwrap_or(u32::MAX);
        self.phase_names.push(name.to_string());
        self.phase_index.insert(name.to_string(), index);
    }

    // —————————————————————— facade declaration pre-pass ——————————————————————

    /// Collect preset artifact declarations anywhere in the script. `report(item, tag)`
    /// is only legal against a declared preset, and the declaration conventionally sits
    /// above the reports it feeds, but the check must not depend on statement order.
    fn collect_preset_declarations(&mut self, workflow: &'a WorkflowDecl) {
        for statement in &workflow.body {
            self.scan_statement_for_presets(statement);
        }
    }

    fn scan_statement_for_presets(&mut self, statement: &'a Stmt) {
        match statement {
            Stmt::Phase { .. } | Stmt::Return { value: None, .. } => {}
            Stmt::Let { value, .. }
            | Stmt::Expr { value, .. }
            | Stmt::Report { item: value, .. }
            | Stmt::Return {
                value: Some(value), ..
            } => self.scan_expression_for_presets(value),
            Stmt::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                self.scan_expression_for_presets(condition);
                self.scan_block_for_presets(then_branch);
                match else_branch.as_ref() {
                    None => {}
                    Some(ElseBranch::Block(block)) => self.scan_block_for_presets(block),
                    Some(ElseBranch::If(nested)) => {
                        self.scan_statement_for_presets(nested.as_ref())
                    }
                }
            }
            Stmt::While {
                condition, body, ..
            }
            | Stmt::ForOf {
                iterable: condition,
                body,
                ..
            } => {
                self.scan_expression_for_presets(condition);
                self.scan_block_for_presets(body);
            }
            Stmt::Parallel { body, .. } | Stmt::Block { block: body, .. } => {
                self.scan_block_for_presets(body);
            }
            Stmt::Try {
                body, catch_body, ..
            } => {
                self.scan_block_for_presets(body);
                self.scan_block_for_presets(catch_body);
            }
        }
    }

    fn scan_block_for_presets(&mut self, block: &'a Block) {
        for statement in &block.statements {
            self.scan_statement_for_presets(statement);
        }
    }

    fn scan_expression_for_presets(&mut self, expr: &'a AstExpr) {
        if let AstExpr::Call { callee, args, .. } = expr {
            if let AstExpr::Member {
                object, property, ..
            } = callee.as_ref()
            {
                if let AstExpr::Identifier { name, .. } = object.as_ref() {
                    if name == "artifact" {
                        if let Some(row) = facade::artifact_row(property) {
                            if row.op.is_preset() {
                                self.register_preset_declaration(row, args);
                            }
                        }
                    }
                }
            }
        }
        for nested in child_expressions(expr) {
            self.scan_expression_for_presets(nested);
        }
    }

    fn register_preset_declaration(
        &mut self,
        row: &'static facade::ArtifactRow,
        args: &'a [AstExpr],
    ) {
        // The argument must be a literal for the id to be submittable at all; the
        // lowering pass reports it there, where the offending node is the subject.
        let Some(id) = args.first().and_then(literal_string) else {
            return;
        };
        if !facade::is_valid_artifact_id(id) {
            return;
        }
        if let Some(previous) = self.declared_presets.insert(id.to_string(), row.op) {
            if previous != row.op {
                self.push(
                    args[0].span(),
                    "nws_artifact_redeclared",
                    format!(
                        "artifact id \"{id}\" is declared as `{}` here and as `{}` earlier: \
                         one id belongs to exactly one member.",
                        row.op.as_str(),
                        previous.as_str()
                    ),
                );
            }
        }
    }

    // —————————————————————— types and schemas ——————————————————————

    fn check_schemas(&mut self) {
        let names: Vec<String> = self.schemas.keys().cloned().collect();
        for name in names {
            let declaration = self.schemas[&name];
            let mut seen = BTreeSet::new();
            for field in &declaration.fields {
                if !seen.insert(field.name.clone()) {
                    self.push(
                        field.span,
                        "nws_duplicate_schema_field",
                        format!(
                            "schema `{name}` declares `{}` twice: one field per name.",
                            field.name
                        ),
                    );
                }
                self.check_schema_field_type(&name, field);
            }
            if declaration.fields.is_empty() {
                self.push(
                    declaration.span,
                    "nws_empty_schema",
                    format!(
                        "schema `{name}` has no fields: a typed ask needs at least one \
                         property — drop the schema and its annotation instead."
                    ),
                );
            }
        }
    }

    fn check_schema_field_type(&mut self, owner: &str, field: &Field) {
        match &field.ty {
            TypeExpr::String | TypeExpr::Number | TypeExpr::Boolean => {}
            TypeExpr::Array(inner) => {
                if let TypeExpr::Named(reference) = &**inner {
                    self.check_named_schema_reference(owner, field, reference);
                }
            }
            TypeExpr::Named(reference) => {
                self.check_named_schema_reference(owner, field, reference)
            }
        }
    }

    fn check_named_schema_reference(&mut self, owner: &str, field: &Field, reference: &str) {
        if reference == owner {
            self.push(
                field.span,
                "nws_recursive_schema",
                format!(
                    "schema `{owner}` refers to itself through `{}`: an ask schema must be a \
                     finite tree.",
                    field.name
                ),
            );
        } else if !self.schemas.contains_key(reference) {
            self.push(
                field.span,
                "nws_unknown_schema",
                format!(
                    "schema `{owner}` field `{}` refers to `{reference}`, which is not \
                     declared: add `schema {reference} {{ … }}` or use one of {}.",
                    field.name,
                    self.known_schema_list()
                ),
            );
        }
    }

    fn known_schema_list(&self) -> String {
        if self.schema_order.is_empty() {
            return "(no schemas declared)".to_string();
        }
        self.schema_order
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn check_type_resolvable(&mut self, ty: &TypeExpr, span: Span) {
        match ty {
            TypeExpr::String | TypeExpr::Number | TypeExpr::Boolean => {}
            TypeExpr::Array(inner) => self.check_type_resolvable(inner, span),
            TypeExpr::Named(name) => {
                if !self.schemas.contains_key(name) {
                    self.push(
                        span,
                        "nws_unknown_schema",
                        format!(
                            "`{name}` is not a declared schema: add `schema {name} {{ … }}` \
                             above the workflow, or use one of {}.",
                            self.known_schema_list()
                        ),
                    );
                }
            }
        }
    }

    /// Compile a schema declaration to JSON Schema (closed object, `required` on the
    /// non-optional fields) — the shape the TypeScript synthesizer emitted.
    fn schema_json(&self, name: &str) -> Value {
        let Some(declaration) = self.schemas.get(name) else {
            return json!({"type": "object"});
        };
        self.compile_object_schema(
            declaration
                .fields
                .iter()
                .map(|field| (*field).clone())
                .collect(),
        )
    }

    /// The workflow's declared arguments as a closed object schema; no fields means no
    /// schema at all, which is what the host reads as "this workflow takes nothing".
    fn args_object_schema(&self) -> Option<Value> {
        if self.arg_fields.is_empty() {
            return None;
        }
        Some(
            self.compile_object_schema(
                self.arg_fields
                    .iter()
                    .map(|field| (*field).clone())
                    .collect(),
            ),
        )
    }

    fn compile_object_schema(&self, fields: Vec<Field>) -> Value {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();
        for field in &fields {
            properties.insert(field.name.clone(), self.type_json(&field.ty));
            if !field.optional {
                required.push(Value::String(field.name.clone()));
            }
        }
        let mut object = serde_json::Map::new();
        object.insert("type".to_string(), json!("object"));
        if !properties.is_empty() {
            object.insert("properties".to_string(), Value::Object(properties));
        }
        if !required.is_empty() {
            object.insert("required".to_string(), Value::Array(required));
        }
        // Closed object, as in the TypeScript path: a model that invents a key has
        // misunderstood the ask, and `false` turns that into one clear repair prompt.
        object.insert("additionalProperties".to_string(), json!(false));
        Value::Object(object)
    }

    pub fn type_json(&self, ty: &TypeExpr) -> Value {
        match ty {
            TypeExpr::String => json!({"type": "string"}),
            TypeExpr::Number => json!({"type": "number"}),
            TypeExpr::Boolean => json!({"type": "boolean"}),
            TypeExpr::Named(name) => self.schema_json(name),
            TypeExpr::Array(inner) => json!({"type": "array", "items": self.type_json(inner)}),
        }
    }

    fn compile_schemas(&self) -> BTreeMap<String, Value> {
        self.schema_order
            .iter()
            .map(|name| (name.clone(), self.schema_json(name)))
            .collect()
    }

    // —————————————————————— site table ——————————————————————

    pub fn add_site(&mut self, kind: SiteKind, member: &str, span: Span) -> SiteIndex {
        let next = self.counters.get(&kind).copied().unwrap_or(0) + 1;
        self.counters.insert(kind, next);
        self.sites.push(Site {
            id: format!("{}{next}", kind.counter_prefix()),
            kind,
            member: member.to_string(),
            span,
        });
        self.sites.len() - 1
    }

    pub fn site_id(&self, site: SiteIndex) -> String {
        self.sites[site].id.clone()
    }

    pub fn site_span(&self, site: SiteIndex) -> Span {
        self.sites[site].span
    }

    pub fn register_ask(
        &mut self,
        site: SiteIndex,
        actors: Vec<SiteIndex>,
        schema: Option<TypeExpr>,
    ) {
        self.ask_actors.insert(site, actors);
        // An untyped ask is recorded explicitly (`typed: false`): the engine reads a
        // missing spec as a wiring error, not as "untyped".
        let compiled = schema.map(|ty| self.type_json(&ty));
        self.ask_schemas.insert(site, compiled.clone());
        self.ask_specs.insert(
            self.sites[site].id.clone(),
            AskSpecWire {
                typed: compiled.is_some(),
                schema: compiled,
            },
        );
    }

    pub fn register_run_command(&mut self, command: String) {
        self.run_commands.insert(command);
    }

    pub fn register_artifact_id(&mut self, span: Span, id: &str, op: ArtifactOp) {
        if let Some(previous) = self.used_artifact_ids.get(id).copied() {
            if previous != op {
                self.push(
                    span,
                    "nws_artifact_kind_mismatch",
                    format!(
                        "artifact id \"{id}\" was used as `{}` earlier and as `{}` here: one \
                         id belongs to exactly one member.",
                        previous.as_str(),
                        op.as_str()
                    ),
                );
                return;
            }
        }
        self.used_artifact_ids.insert(id.to_string(), op);
    }

    pub fn preset_declaration(&self, id: &str) -> Option<ArtifactOp> {
        self.declared_presets.get(id).copied()
    }

    pub fn schema_names(&self) -> Vec<String> {
        self.schema_order.clone()
    }

    pub fn has_schema(&self, name: &str) -> bool {
        self.schemas.contains_key(name)
    }

    pub fn arg_field(&self, name: &str) -> Option<&'a Field> {
        self.arg_fields
            .iter()
            .copied()
            .find(|field| field.name == name)
    }

    pub fn fanout_depth(&self) -> u32 {
        self.fanout_depth
    }

    pub fn in_join_items(&self) -> bool {
        self.in_join_items
    }

    pub fn set_in_join_items(&mut self, value: bool) {
        self.in_join_items = value;
    }

    /// Claim a literal actor name for the run. Port of `collectDuplicateActorNames`: the
    /// authoritative rule is the engine's runtime `DuplicateActorName`, and this is the
    /// half a compiler can see — two sites with the same literal name, and a fixed name
    /// inside a body that runs once per element.
    pub fn claim_actor_name(&mut self, site: SiteIndex, name: &str) {
        if name.is_empty() {
            return;
        }
        let first = self.claimed_actor_names.insert(name.to_string(), site);
        if self.fanout_depth > 0 {
            self.push(
                self.sites[site].span,
                "nws_fanout_actor_name",
                format!(
                    "this `agent(…)` runs once per element of a loop but its name \"{name}\" \
                     is a fixed string, so every element creates a different actor under the \
                     same name and the run fails with DuplicateActorName as soon as the \
                     collection holds more than one item. An actor name is the identity an \
                     amended re-run matches its imported cache against, so it must be unique \
                     within a run: build a per-element name (`\"{name}-\" + item`) or drop the \
                     name — anonymous actors are legal, they just never reuse cached work."
                ),
            );
            return;
        }
        if let Some(first) = first {
            self.push(
                self.sites[site].span,
                "nws_duplicate_actor_name",
                format!(
                    "two actors are named \"{name}\" (the first is on line {}): an actor name \
                     must be unique within a run, because it is the identity an amended re-run \
                     matches its imported cache against. Give this one its own name, or drop \
                     the name — anonymous actors are legal.",
                    self.sites[first].span.line
                ),
            );
        }
    }

    // —————————————————————— values ——————————————————————

    /// What a lowered expression's value is, for the actor-receiver rule.
    fn origin_of(&self, expr: &Expr) -> Origin {
        match &expr.node {
            ExprNode::CreateActor { site, .. } => Origin::Actor(*site),
            ExprNode::Local(slot) => self
                .lookup_by_slot(*slot)
                .map(|binding| binding.origin)
                .unwrap_or(Origin::None),
            _ => Origin::None,
        }
    }

    fn lookup_by_slot(&self, slot: Slot) -> Option<Binding> {
        self.scopes
            .iter()
            .rev()
            .flat_map(|scope| scope.iter())
            .find(|binding| binding.slot == slot)
            .cloned()
    }

    pub fn binding_origin(&self, slot: Slot) -> Option<Origin> {
        self.lookup_by_slot(slot).map(|binding| binding.origin)
    }

    fn lower_report_tag(&mut self, tag: Option<&'a AstExpr>, span: Span) -> Option<String> {
        let tag = tag?;
        let Some(literal) = literal_string(tag) else {
            self.push(
                tag.span(),
                "nws_literal_required",
                "a report artifact tag must be a string literal: the set of cards a run can \
                 feed is fixed at submit time, so a computed tag cannot be checked against it.",
            );
            return None;
        };
        if !facade::is_valid_artifact_id(literal) {
            self.push(
                tag.span(),
                "nws_artifact_id_invalid",
                format!(
                    "\"{literal}\" is not an artifact id: use 1-{} characters of \
                     [A-Za-z0-9_.-].",
                    crate::types::ARTIFACT_CAPS.max_id_length
                ),
            );
            return None;
        }
        match self.declared_presets.get(literal) {
            Some(op) => {
                self.used_artifact_ids
                    .entry(literal.to_string())
                    .or_insert(*op);
                Some(literal.to_string())
            }
            None => {
                self.push(
                    span,
                    "nws_undeclared_artifact_tag",
                    format!(
                        "report tags \"{literal}\", but no `artifact.<preset>(\"{literal}\")` \
                         is declared in this script: declare the card it feeds, or drop the \
                         tag to report into the plain results list.",
                    ),
                );
                None
            }
        }
    }

    /// Port of `deriveActorSubmitProfiles`: `untyped` when no typed ask can reach the
    /// actor, `mono` when every typed ask that can reach it carries the identical schema,
    /// `generic` otherwise — and every actor is `generic` when any ask's actor set is
    /// unresolved. A `mono` profile freezes that actor's `submit_result` schema, so
    /// guessing one from a partial picture would take the tool away from an ask that
    /// needed it.
    fn derive_submit_profiles(&self, actor_sites: &[SiteIndex]) -> BTreeMap<String, Value> {
        let mut profiles = BTreeMap::new();
        if actor_sites.is_empty() {
            return profiles;
        }
        if self.ask_actors.values().any(|actors| actors.is_empty()) {
            for site in actor_sites {
                profiles.insert(self.sites[*site].id.clone(), json!({"kind": "generic"}));
            }
            return profiles;
        }

        let mut schemas_by_actor: BTreeMap<SiteIndex, BTreeMap<String, Value>> = BTreeMap::new();
        for (ask_site, actors) in &self.ask_actors {
            let Some(Some(schema)) = self.ask_schemas.get(ask_site) else {
                continue;
            };
            for actor in actors {
                schemas_by_actor
                    .entry(*actor)
                    .or_default()
                    .insert(crate::types::canonical_json(schema), schema.clone());
            }
        }
        for site in actor_sites {
            let bucket = schemas_by_actor.get(site);
            let value = match bucket.map(BTreeMap::len) {
                None | Some(0) => json!({"kind": "untyped"}),
                Some(1) => {
                    let schema = bucket
                        .as_ref()
                        .and_then(|bucket| bucket.values().next())
                        .cloned()
                        .unwrap_or(Value::Null);
                    json!({"kind": "mono", "schema": schema})
                }
                Some(_) => json!({"kind": "generic"}),
            };
            profiles.insert(self.sites[*site].id.clone(), value);
        }
        profiles
    }
}

/// The direct sub-expressions of an AST node, for the pre-pass walk.
fn child_expressions(expr: &AstExpr) -> Vec<&AstExpr> {
    match expr {
        AstExpr::Literal { .. } | AstExpr::Identifier { .. } => Vec::new(),
        AstExpr::Template { parts, .. } => parts
            .iter()
            .filter_map(|part| match part {
                super::ast::TemplatePart::Expr(expr) => Some(expr),
                super::ast::TemplatePart::Text(_) => None,
            })
            .collect(),
        AstExpr::Array { items, .. } => items.iter().collect(),
        AstExpr::Object { entries, .. } => entries.iter().map(|entry| &entry.value).collect(),
        AstExpr::Unary { value, .. } => vec![value.as_ref()],
        AstExpr::Binary { left, right, .. } => vec![left.as_ref(), right.as_ref()],
        AstExpr::Member { object, .. } => vec![object.as_ref()],
        AstExpr::Index { object, index, .. } => vec![object.as_ref(), index.as_ref()],
        AstExpr::Call { callee, args, .. } => {
            let mut nested = vec![callee.as_ref()];
            nested.extend(args.iter());
            nested
        }
        AstExpr::Await { value, .. } => vec![value.as_ref()],
    }
}

/// The `String` payload of a literal expression, `None` for anything else.
pub fn literal_string(expr: &AstExpr) -> Option<&str> {
    match expr {
        AstExpr::Literal {
            value: Value::String(text),
            ..
        } => Some(text.as_str()),
        _ => None,
    }
}

/// `phaseAlongside[i]` — the phases that may still be running when phase `i` is entered.
///
/// NWS states concurrency in the source (`parallel { … }`), so this is a structural read
/// rather than the taint walk the TypeScript flow graph needs: the phases one strand
/// enters are alongside every phase its sibling strands enter. The relation is recorded
/// per direct parallel block; a nested block's strands are already siblings of the outer
/// strands for the phases they enter, so no transitive closure is taken.
pub fn derive_phase_alongside(body: &[Instr], phase_index: &HashMap<String, u32>) -> Vec<Vec<u32>> {
    let mut pairs: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
    collect_alongside(body, phase_index, &mut pairs);
    // No parallel strands at all means the field is absent, the way the TypeScript path
    // leaves it out: absent is what tells the sidebar "this track is a straight line".
    // A script that does branch gets one entry per phase, position-aligned with the table.
    if pairs.is_empty() {
        return Vec::new();
    }
    (0..phase_index.len())
        .map(|index| {
            pairs
                .get(&(index as u32))
                .map(|set| set.iter().copied().collect())
                .unwrap_or_default()
        })
        .collect()
}

fn collect_alongside(
    instrs: &[Instr],
    phase_index: &HashMap<String, u32>,
    pairs: &mut BTreeMap<u32, BTreeSet<u32>>,
) -> BTreeSet<u32> {
    let mut reached = BTreeSet::new();
    for instr in instrs {
        match instr {
            Instr::EnterPhase { name, .. } => {
                if let Some(index) = phase_index.get(name) {
                    reached.insert(*index);
                }
            }
            Instr::Let { .. } | Instr::Return { .. } | Instr::Effect { .. } => {}
            Instr::If {
                then_branch,
                else_branch,
                ..
            } => {
                reached.extend(collect_alongside(then_branch, phase_index, pairs));
                reached.extend(collect_alongside(else_branch, phase_index, pairs));
            }
            Instr::While { body, .. } | Instr::ForOf { body, .. } | Instr::Block { body, .. } => {
                reached.extend(collect_alongside(body, phase_index, pairs));
            }
            Instr::Try {
                body, catch_body, ..
            } => {
                reached.extend(collect_alongside(body, phase_index, pairs));
                reached.extend(collect_alongside(catch_body, phase_index, pairs));
            }
            Instr::Parallel { strands, .. } => {
                let sets = strands
                    .iter()
                    .map(|strand| collect_alongside(strand, phase_index, pairs))
                    .collect::<Vec<_>>();
                for (index, set) in sets.iter().enumerate() {
                    for phase in set {
                        for (other_index, other) in sets.iter().enumerate() {
                            if other_index == index {
                                continue;
                            }
                            for partner in other {
                                if partner != phase {
                                    pairs.entry(*phase).or_default().insert(*partner);
                                }
                            }
                        }
                    }
                }
                reached.extend(sets.iter().flatten().copied());
            }
        }
    }
    reached
}
