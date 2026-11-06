//! Expression lowering and facade classification for NWS.
//!
//! This is where a call is decided to be a facade call — by shape, in the same way the
//! TypeScript front-end decided it by declaration. NWS has no user-defined functions and
//! no aliases of facade members, so the shape test is exact rather than approximate:
//! `agent(…)`, `log(…)`, `<container>.<member>(…)`, `<actor>.ask(…)`.

use super::ast::{Expr as AstExpr, TemplatePart, TypeExpr};
use super::facade::{self, is_facade_container, PROMISE_ALL_MEMBER, PROMISE_CONTAINER};
use super::ir::{Expr, ExprNode, ObjectField, SiteKind, TemplateChunk};
use super::lower::{literal_string, Lowerer, Origin};

/// Whether the expression being lowered is the whole statement (`await artifact.…;`).
/// Void facade calls — `log`, `report`, a preset declaration — are only legal there,
/// which is the same thing the TypeScript typechecker enforced by their `void` return.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Position {
    Statement,
    Value,
}

impl<'a> Lowerer<'a> {
    /// Lower one expression. `expected` is the annotation of the enclosing `let`, and it
    /// travels only through `await` — that is what makes `let p: Plan = await …ask(…)`
    /// a typed ask while `let p: Plan = (await …ask(…))[0]` does not type the ask.
    pub fn lower_expression(
        &mut self,
        expr: &'a AstExpr,
        expected: Option<&'a TypeExpr>,
        position: Position,
    ) -> Expr {
        let span = expr.span();
        let node = match expr {
            AstExpr::Literal { value, .. } => ExprNode::Literal(value.clone()),
            AstExpr::Template { parts, .. } => ExprNode::Template(
                parts
                    .iter()
                    .map(|part| match part {
                        TemplatePart::Text(text) => TemplateChunk::Text(text.clone()),
                        TemplatePart::Expr(inner) => {
                            TemplateChunk::Expr(self.lower_expression(inner, None, Position::Value))
                        }
                    })
                    .collect(),
            ),
            AstExpr::Identifier { name, .. } => self.lower_identifier(name, span),
            AstExpr::Array { items, .. } => ExprNode::Array(
                items
                    .iter()
                    .map(|item| self.lower_expression(item, None, Position::Value))
                    .collect(),
            ),
            AstExpr::Object { entries, .. } => ExprNode::Object(
                entries
                    .iter()
                    .map(|entry| ObjectField {
                        key: entry.key.clone(),
                        value: self.lower_expression(&entry.value, None, Position::Value),
                    })
                    .collect(),
            ),
            AstExpr::Unary { op, value, .. } => ExprNode::Unary {
                op: *op,
                value: Box::new(self.lower_expression(value, None, Position::Value)),
            },
            AstExpr::Binary {
                op, left, right, ..
            } => ExprNode::Binary {
                op: *op,
                left: Box::new(self.lower_expression(left, None, Position::Value)),
                right: Box::new(self.lower_expression(right, None, Position::Value)),
            },
            AstExpr::Member {
                object, property, ..
            } => {
                return self.lower_member(object, property, span);
            }
            AstExpr::Index { object, index, .. } => ExprNode::Index {
                object: Box::new(self.lower_expression(object, None, Position::Value)),
                index: Box::new(self.lower_expression(index, None, Position::Value)),
            },
            AstExpr::Call { callee, args, .. } => {
                return self.lower_call(callee, args, span, expected, position, false);
            }
            AstExpr::Await { value, .. } => {
                return self.lower_await(value, expected, position);
            }
        };
        Expr::new(span, node)
    }

    /// `await <async facade call>`. Anything else to await is a mistake NWS cannot make
    /// legal: there are no promises of its own making.
    fn lower_await(
        &mut self,
        value: &'a AstExpr,
        expected: Option<&'a TypeExpr>,
        position: Position,
    ) -> Expr {
        let span = value.span();
        let lowered = match value {
            AstExpr::Call { callee, args, .. } => {
                self.lower_call(callee, args, span, expected, position, true)
            }
            other => self.lower_expression(other, expected, position),
        };
        if !lowered.node.is_async_facade() {
            self.push(
                span,
                "nws_await_misplaced",
                "only a facade call can be awaited (`await planner.ask(…)`, `await \
                 files.read(…)`, `await artifact.markdown(…)`, `await Promise.all([…])`); \
                 everything else in NWS is already a value.",
            );
        }
        lowered
    }

    fn lower_identifier(&mut self, name: &str, span: super::ast::Span) -> ExprNode {
        if let Some(binding) = self.lookup(name) {
            return ExprNode::Local(binding.slot);
        }
        // `args` is contextual: a declared field access is `args.<field>`, the bare name
        // is the whole argument object (a legal ask instruction or report item).
        if name == "args" {
            return ExprNode::ArgsObject;
        }
        if is_facade_container(name) || name == "agent" || name == "log" {
            self.push(
                span,
                "nws_facade_not_callable",
                format!(
                    "`{name}` is a facade namespace, not a value: call one of its members \
                     (`{name}(…)` where that is a call) instead of using it on its own."
                ),
            );
            return ExprNode::Literal(serde_json::Value::Null);
        }
        self.push(
            span,
            "nws_unknown_identifier",
            format!(
                "`{name}` is not declared in this scope and is not a facade name: NWS has no \
                 globals beyond `args`, `agent`, `log`, `files`, `git`, `world`, `artifact` \
                 and `Promise`."
            ),
        );
        ExprNode::Literal(serde_json::Value::Null)
    }

    fn lower_member(
        &mut self,
        object: &'a AstExpr,
        property: &str,
        span: super::ast::Span,
    ) -> Expr {
        // `args.<field>` resolves against the workflow's declared arguments; the field
        // set is fixed at submit time, so a missing one is a compile error, not a
        // runtime `undefined`.
        if let AstExpr::Identifier { name, .. } = object {
            if name == "args" && self.lookup(name).is_none() {
                if self.arg_field(property).is_some() {
                    return Expr::new(
                        span,
                        ExprNode::ArgField {
                            name: property.to_string(),
                        },
                    );
                }
                let declared = if self.arg_fields.is_empty() {
                    "(this workflow declares no args)".to_string()
                } else {
                    self.arg_fields
                        .iter()
                        .map(|field| format!("`{}`", field.name))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                self.push(
                    span,
                    "nws_unknown_args_field",
                    format!(
                        "`args.{property}` is not a declared argument: this workflow takes \
                         {declared}."
                    ),
                );
                return Expr::new(span, ExprNode::Literal(serde_json::Value::Null));
            }
            if is_facade_container(name) && self.lookup(name).is_none() {
                let hint = if name == "artifact" {
                    facade::ARTIFACT_ROWS
                        .iter()
                        .map(|row| format!("`{name}.{}(…)`", row.member))
                        .collect::<Vec<_>>()
                        .join(", ")
                } else {
                    facade::world_read_members(name)
                        .iter()
                        .map(|member| format!("`{name}.{member}(…)`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                self.push(
                    span,
                    "nws_facade_member_not_called",
                    format!(
                        "`{name}.{property}` is a facade member and must be called, never \
                         taken as a value: NWS has no aliases of facade members because \
                         each call's site id is what the journal keys on. Use one of {hint}."
                    ),
                );
                return Expr::new(span, ExprNode::Literal(serde_json::Value::Null));
            }
        }
        Expr::new(
            span,
            ExprNode::Member {
                object: Box::new(self.lower_expression(object, None, Position::Value)),
                property: property.to_string(),
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn lower_call(
        &mut self,
        callee: &'a AstExpr,
        args: &'a [AstExpr],
        span: super::ast::Span,
        expected: Option<&'a TypeExpr>,
        position: Position,
        awaited: bool,
    ) -> Expr {
        // `Promise.all([…])` and the container calls are keyed on the callee's receiver
        // name; an actor `.ask` is keyed on the receiver's *value*.
        if let AstExpr::Member {
            object, property, ..
        } = callee
        {
            if let AstExpr::Identifier { name, .. } = object.as_ref() {
                if self.lookup(name).is_none() && is_facade_container(name) {
                    return self
                        .lower_container_call(name, property, args, span, position, awaited);
                }
                if property == "ask" || name == "Promise" {
                    // `Promise` is not a container in the registry sense; handle the join
                    // before falling through to the actor-receiver rule.
                    if name == PROMISE_CONTAINER {
                        return self.lower_join(args, property, span, awaited);
                    }
                }
                if property == "ask" {
                    return self.lower_ask(object, args, span, expected, awaited);
                }
            } else if property == "ask" {
                // A computed receiver (`actors[i].ask(…)`): still an ask, but the actor
                // it lands on cannot be named statically.
                return self.lower_ask(object, args, span, expected, awaited);
            }
            if property == "all" {
                return self.lower_join(args, property, span, awaited);
            }
            return self.lower_value_call(callee, args, span);
        }
        if let AstExpr::Identifier { name, .. } = callee {
            if self.lookup(name).is_none() {
                if name == "agent" {
                    return self.lower_agent(args, span, position);
                }
                if name == "log" {
                    return self.lower_log(args, span, position, awaited);
                }
            }
        }
        self.lower_value_call(callee, args, span)
    }

    /// `agent(name?, persona?)`: a synchronous facade call that creates an actor.
    fn lower_agent(
        &mut self,
        args: &'a [AstExpr],
        span: super::ast::Span,
        position: Position,
    ) -> Expr {
        if args.len() > 2 {
            self.push(
                span,
                "nws_arity_mismatch",
                format!(
                    "agent takes a name and a persona, got {} arguments.",
                    args.len()
                ),
            );
        }
        let name = args
            .first()
            .map(|arg| Box::new(self.lower_expression(arg, None, Position::Value)));
        let persona = args
            .get(1)
            .map(|arg| Box::new(self.lower_expression(arg, None, Position::Value)));
        let site = self.add_site(SiteKind::Actor, "agent", span);
        // A literal name can be checked at compile time; a computed one is the engine's
        // runtime uniqueness rule.
        if let Some(first) = args.first().and_then(literal_string) {
            self.claim_actor_name(site, first);
        }
        let _ = position;
        Expr::new(
            span,
            ExprNode::CreateActor {
                site,
                name,
                persona,
            },
        )
    }

    /// `log(message)`: progress text. No site, no journal key — that is the difference
    /// between it and `report`.
    fn lower_log(
        &mut self,
        args: &'a [AstExpr],
        span: super::ast::Span,
        position: Position,
        awaited: bool,
    ) -> Expr {
        if args.len() != 1 {
            self.push(
                span,
                "nws_arity_mismatch",
                format!("log takes one message, got {} arguments.", args.len()),
            );
        }
        if position != Position::Statement {
            self.push(
                span,
                "nws_facade_returns_nothing",
                "log(...) emits progress and returns nothing, so it cannot be part of an \
                 expression: put it on its own line.",
            );
        }
        if awaited {
            self.push(
                span,
                "nws_await_misplaced",
                "log(...) returns nothing; there is nothing to await.",
            );
        }
        let message = args
            .first()
            .map(|arg| Box::new(self.lower_expression(arg, None, Position::Value)))
            .unwrap_or_else(|| {
                Box::new(Expr::new(span, ExprNode::Literal(serde_json::Value::Null)))
            });
        Expr::new(span, ExprNode::Log { message })
    }

    /// Calls on `files` / `git` / `world` / `artifact`.
    fn lower_container_call(
        &mut self,
        container: &str,
        member: &str,
        args: &'a [AstExpr],
        span: super::ast::Span,
        position: Position,
        awaited: bool,
    ) -> Expr {
        if container == "artifact" {
            return self.lower_artifact_call(member, args, span, position, awaited);
        }
        let Some(row) = facade::world_read_row(container, member) else {
            self.push(
                span,
                "nws_unknown_facade_member",
                format!(
                    "`{container}.{member}` is not a facade primitive: this container \
                     provides {}.",
                    members_of(container)
                ),
            );
            return Expr::new(span, ExprNode::Literal(serde_json::Value::Null));
        };
        self.check_arity(
            span,
            &format!("{container}.{member}"),
            args.len(),
            row.min_args,
            row.max_args,
        );
        if row.literal_first_arg {
            let Some(command) = args.first().and_then(literal_string) else {
                self.push(
                    args.first().map_or(span, AstExpr::span),
                    "nws_literal_required",
                    "world.run's command must be a compile-time string literal: the set of \
                     commands a run may execute is fixed at submit time and confirmed with \
                     the user, so a computed command cannot be authorized.",
                );
                return Expr::new(span, ExprNode::Literal(serde_json::Value::Null));
            };
            self.register_run_command(command.to_string());
        }
        let site = self.add_site(SiteKind::WorldRead, &format!("{container}.{member}"), span);
        let lowered_args = args
            .iter()
            .map(|arg| self.lower_expression(arg, None, Position::Value))
            .collect();
        let expr = Expr::new(
            span,
            ExprNode::WorldRead {
                site,
                op: row.op.to_string(),
                args: lowered_args,
            },
        );
        self.require_await(expr, position, awaited)
    }

    fn lower_artifact_call(
        &mut self,
        member: &str,
        args: &'a [AstExpr],
        span: super::ast::Span,
        position: Position,
        awaited: bool,
    ) -> Expr {
        let Some(row) = facade::artifact_row(member) else {
            self.push(
                span,
                "nws_unknown_facade_member",
                format!(
                    "`artifact.{member}` is not an artifact member: it is one of {}.",
                    facade::ARTIFACT_ROWS
                        .iter()
                        .map(|row| format!("`artifact.{}`", row.member))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
            return Expr::new(span, ExprNode::Literal(serde_json::Value::Null));
        };
        self.check_arity(
            span,
            &format!("artifact.{member}"),
            args.len(),
            row.min_args,
            row.max_args,
        );
        let Some(id_expr) = args.first() else {
            return Expr::new(span, ExprNode::Literal(serde_json::Value::Null));
        };
        let Some(id) = literal_string(id_expr) else {
            self.push(
                id_expr.span(),
                "nws_literal_required",
                format!(
                    "artifact.{member}'s id must be a compile-time string literal: the set \
                     an NWS run can publish is fixed at submit time, so the store and the \
                     sidebar are keyed on ids the compiler can see.",
                ),
            );
            return Expr::new(span, ExprNode::Literal(serde_json::Value::Null));
        };
        if !facade::is_valid_artifact_id(id) {
            self.push(
                id_expr.span(),
                "nws_artifact_id_invalid",
                format!(
                    "\"{id}\" is not an artifact id: use 1-{} characters of [A-Za-z0-9_.-], \
                     no path separators or spaces.",
                    crate::types::ARTIFACT_CAPS.max_id_length
                ),
            );
            return Expr::new(span, ExprNode::Literal(serde_json::Value::Null));
        }
        self.register_artifact_id(id_expr.span(), id, row.op);
        if row.op.is_preset() {
            if position != Position::Statement {
                self.push(
                    span,
                    "nws_facade_returns_nothing",
                    format!(
                        "artifact.{member} declares a card and returns nothing, so it cannot \
                         be part of an expression: put it on its own line.",
                    ),
                );
            }
            if awaited {
                self.push(
                    span,
                    "nws_await_misplaced",
                    format!("artifact.{member} declares a card synchronously; there is nothing to await."),
                );
            }
            let site = self.add_site(SiteKind::Artifact, member, span);
            let lowered_args = args
                .iter()
                .map(|arg| self.lower_expression(arg, None, Position::Value))
                .collect();
            return Expr::new(
                span,
                ExprNode::DeclareArtifact {
                    site,
                    op: row.op,
                    id: id.to_string(),
                    args: lowered_args,
                },
            );
        }
        let site = self.add_site(SiteKind::Artifact, member, span);
        let lowered_args = args
            .iter()
            .map(|arg| self.lower_expression(arg, None, Position::Value))
            .collect();
        let expr = Expr::new(
            span,
            ExprNode::PublishArtifact {
                site,
                op: row.op,
                id: id.to_string(),
                args: lowered_args,
            },
        );
        self.require_await(expr, position, awaited)
    }

    /// `Promise.all([…])`: several async facade calls awaited together. The join has a
    /// site id for the analysis surface and no journal row of its own, exactly as in the
    /// TypeScript path, where `Promise.all` was plain JavaScript.
    fn lower_join(
        &mut self,
        args: &'a [AstExpr],
        member: &str,
        span: super::ast::Span,
        awaited: bool,
    ) -> Expr {
        if member != PROMISE_ALL_MEMBER {
            self.push(
                span,
                "nws_unknown_facade_member",
                format!(
                    "`Promise.{member}` does not exist in NWS: the only join is \
                     `Promise.all([…])`, and a race has no deterministic journal identity."
                ),
            );
            return Expr::new(span, ExprNode::Literal(serde_json::Value::Null));
        }
        let Some(AstExpr::Array { items, .. }) = args.first() else {
            self.push(
                args.first().map_or(span, AstExpr::span),
                "nws_join_needs_array_literal",
                "Promise.all takes an array literal of facade calls: `Promise.all([a.ask(…), \
                 b.ask(…)])`. A computed list of pending calls cannot be given static site \
                 ids.",
            );
            return Expr::new(span, ExprNode::Literal(serde_json::Value::Null));
        };
        if args.len() > 1 {
            self.push(
                span,
                "nws_arity_mismatch",
                "Promise.all takes exactly one array literal.",
            );
        }
        if items.is_empty() {
            self.push(
                span,
                "nws_empty_join",
                "Promise.all([]) waits for nothing: drop it, the values are already there.",
            );
        }
        let site = self.add_site(SiteKind::Join, "all", span);
        let outer = self.in_join_items();
        self.set_in_join_items(true);
        let mut lowered = Vec::with_capacity(items.len());
        for item in items {
            let expr = match item {
                AstExpr::Call { callee, args, .. } => {
                    self.lower_call(callee, args, item.span(), None, Position::Value, false)
                }
                other => self.lower_expression(other, None, Position::Value),
            };
            if !expr.node.is_async_facade() {
                self.push(
                    item.span(),
                    "nws_join_item_not_async",
                    "every Promise.all item must be a facade call that can be awaited \
                     (`ask`, `files.*`, `git.*`, `world.run`, `artifact.file`/`markdown`, or \
                     a nested `Promise.all`); anything else is already a value and needs no \
                     join.",
                );
            }
            lowered.push(expr);
        }
        self.set_in_join_items(outer);
        let expr = Expr::new(
            span,
            ExprNode::Join {
                site,
                items: lowered,
            },
        );
        if !awaited && !self.in_join_items() {
            self.push(
                span,
                "nws_await_required",
                "Promise.all(…) must be awaited or be an item of another Promise.all: NWS \
                 has no un-awaited pending values to pass around.",
            );
        }
        expr
    }

    /// `<actor>.ask(instruction[, Schema])`.
    fn lower_ask(
        &mut self,
        receiver: &'a AstExpr,
        args: &'a [AstExpr],
        span: super::ast::Span,
        expected: Option<&'a TypeExpr>,
        awaited: bool,
    ) -> Expr {
        if args.is_empty() || args.len() > 2 {
            self.push(
                span,
                "nws_arity_mismatch",
                format!(
                    "ask takes an instruction and optionally a schema name, got {} arguments.",
                    args.len()
                ),
            );
        }
        let lowered_receiver = self.lower_expression(receiver, None, Position::Value);
        let actors = match &lowered_receiver.node {
            ExprNode::Local(slot) => match self.binding_origin(*slot) {
                Some(Origin::Actor(site)) => vec![site],
                // An unresolvable handle (`for (const a of actors)`) leaves the actor set
                // empty, which is the signal the profile derivation reads as "cannot say".
                Some(Origin::Unknown) | None => Vec::new(),
                // A binding the compiler can prove holds something else is the author's
                // mistake, and it is named now rather than as an UnknownActor rejection
                // halfway through a run that has already spent tokens.
                Some(Origin::None) => {
                    self.push(
                        span,
                        "nws_ask_receiver_not_an_actor",
                        "this binding does not hold an actor created by `agent(…)`: `.ask` \
                         belongs to an actor handle.",
                    );
                    Vec::new()
                }
            },
            other => {
                // A computed receiver (`workers[i].ask(…)`, `cfg.worker.ask(…)`) cannot be
                // resolved per site, so it stays unresolved instead of being called wrong. A
                // receiver that is obviously a value is a mistake the author can see.
                if matches!(
                    other,
                    ExprNode::Literal(_)
                        | ExprNode::Template(_)
                        | ExprNode::Array(_)
                        | ExprNode::Object(_)
                        | ExprNode::ArgsObject
                        | ExprNode::ArgField { .. }
                ) {
                    self.push(
                        span,
                        "nws_ask_receiver_not_an_actor",
                        "only an actor created by `agent(…)` can be asked: `.ask` is not a \
                         method of a JSON value. Name the actor with `let` and ask that.",
                    );
                }
                Vec::new()
            }
        };
        let instructions = args
            .first()
            .map(|arg| Box::new(self.lower_expression(arg, None, Position::Value)))
            .unwrap_or_else(|| {
                Box::new(Expr::new(span, ExprNode::Literal(serde_json::Value::Null)))
            });
        let typed = match args.get(1) {
            Some(schema_arg) => self.lower_ask_schema_argument(schema_arg, expected),
            None => expected.cloned(),
        };
        let site = self.add_site(SiteKind::Ask, "ask", span);
        self.register_ask(site, actors, typed);
        let expr = Expr::new(
            span,
            ExprNode::Ask {
                site,
                actor: lowered_receiver.into(),
                instructions,
            },
        );
        if !awaited && !self.in_join_items() {
            self.push(
                span,
                "nws_await_required",
                "an ask must be awaited or be an item of `Promise.all([…])`: NWS has no \
                 un-awaited nodes to store in a variable.",
            );
        }
        expr
    }

    /// The second argument of `ask` is a schema name. This form exists because a join
    /// item has no `let` annotation to read the type from: `Promise.all([a.ask(…, Plan)])`
    /// says what the ask must submit, the way TypeScript's `ask<Plan>()` type argument did.
    fn lower_ask_schema_argument(
        &mut self,
        schema_arg: &'a AstExpr,
        expected: Option<&'a TypeExpr>,
    ) -> Option<TypeExpr> {
        let AstExpr::Identifier { name, .. } = schema_arg else {
            self.push(
                schema_arg.span(),
                "nws_schema_argument_required",
                "the second argument of ask is a declared schema name (`ask(…, Plan)`), not \
                 an expression: the type has to be something the compiler can see.",
            );
            return expected.cloned();
        };
        if !self.has_schema(name) {
            self.push(
                schema_arg.span(),
                "nws_unknown_schema",
                format!(
                    "`{name}` is not a declared schema: declare `schema {name} {{ … }}` above \
                     the workflow, or use one of {}.",
                    self.schema_names()
                        .iter()
                        .map(|name| format!("`{name}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
            return None;
        }
        Some(TypeExpr::Named(name.clone()))
    }

    /// A call on something that is not a facade: NWS has no user-defined functions, so
    /// this is always a mistake, and saying so at compile time is the whole point of not
    /// shipping a JavaScript engine.
    fn lower_value_call(
        &mut self,
        callee: &'a AstExpr,
        args: &'a [AstExpr],
        span: super::ast::Span,
    ) -> Expr {
        self.push(
            callee.span(),
            "nws_not_callable",
            format!(
                "NWS has no user-defined functions: `{}` is not callable. Move this work to a \
                 subagent (`agent(…)` + `ask(…)`) or express it with the control-flow \
                 statements (`if`, `while`, `for (… of …)`, `parallel`).",
                describe_callee(callee)
            ),
        );
        let _ = args;
        Expr::new(span, ExprNode::Literal(serde_json::Value::Null))
    }

    fn require_await(&mut self, expr: Expr, position: Position, awaited: bool) -> Expr {
        if !awaited && !self.in_join_items() {
            self.push(
                expr.span,
                "nws_await_required",
                "this facade call returns a value the script must await (or list inside \
                 `Promise.all([…])`); NWS has no un-awaited pending values.",
            );
        }
        let _ = position;
        expr
    }

    fn check_arity(
        &mut self,
        span: super::ast::Span,
        name: &str,
        given: usize,
        min: usize,
        max: usize,
    ) {
        if given >= min && given <= max {
            return;
        }
        self.push(
            span,
            "nws_arity_mismatch",
            format!(
                "{name} takes {}{}, got {given} argument{}.",
                if min == max {
                    min.to_string()
                } else {
                    format!("{min} to {max}")
                },
                "",
                if given == 1 { "" } else { "s" }
            ),
        );
    }
}

/// The members a container offers, for the "unknown member" message.
fn members_of(container: &str) -> String {
    facade::WORLD_READ_ROWS
        .iter()
        .filter(|row| row.container == container)
        .map(|row| format!("`{container}.{}(…)`", row.member))
        .collect::<Vec<_>>()
        .join(", ")
}

fn describe_callee(callee: &AstExpr) -> String {
    match callee {
        AstExpr::Identifier { name, .. } => name.clone(),
        AstExpr::Member {
            object, property, ..
        } => {
            format!("{}.{}", describe_callee(object), property)
        }
        other => format!("an expression on line {}", other.span().line),
    }
}
