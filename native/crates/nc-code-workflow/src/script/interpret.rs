//! The NWS interpreter: a tokio tree walk that calls the engine directly.
//!
//! This replaces the Node sandbox. The TypeScript path lowered the script to JavaScript,
//! wrote an entry file, spawned `node`, and bridged NDJSON messages back into the engine's
//! Boundary A; here the lowered plan is walked in-process and each facade call is a method
//! call on [`WorkflowEngine`]. The engine, its journal-first scheduling, the imported
//! cache and resume semantics are unchanged, because the calls carry the same site ids the
//! sandbox carried.
//!
//! Concurrency model:
//! - `await` on a facade call suspends this future; the engine admits the ask, dispatches
//!   it through the driver and resolves the receiver.
//! - `Promise.all([...])` polls its items concurrently inside one task (`join_all` over
//!   borrowed frames — every item only reads the frame).
//! - `parallel { … }` gives each strand a copy of the enclosing frame. Slots are
//!   per-declaration and NWS has no assignment statement, so a strand's writes stay in its
//!   own copy and reads of outer slots see the values as of the fork. Structured: the
//!   block completes when every strand has, and the first error ends it.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::{join_all, BoxFuture};
use serde_json::{json, Value};

use crate::engine::WorkflowEngine;
use crate::types::{RunSettlement, RunStopReason, WorkflowError, WorkflowErrorCode};

use super::ast::{BinaryOp, Span};
use super::ir::{Expr, ExprNode, Instr};
use super::plan::Plan;
use super::value;

/// A boxed interpreter future: `eval` and `exec` are mutually recursive, so the recursion
/// goes through the heap rather than requiring a fixed size.
type Step<'a, T> = BoxFuture<'a, T>;

/// What `run_script` was asked to execute.
pub struct ScriptOptions {
    /// The compiled plan. Shared so strands can reference their statement lists without
    /// copying the tree at every fork.
    pub plan: Arc<Plan>,
    /// The run's arguments, injected as the `args` value.
    pub args: Option<Value>,
    /// Wall-clock bound: on expiry the run stops `interrupted`, which is resumable.
    pub timeout_ms: Option<u64>,
}

impl ScriptOptions {
    pub fn new(plan: Arc<Plan>) -> Self {
        Self {
            plan,
            args: None,
            timeout_ms: None,
        }
    }
}

/// Run a compiled NWS script to the engine's settlement. The settlement is the only
/// verdict: an external stop or fail wins over the script finishing.
pub async fn run_script(engine: Arc<WorkflowEngine>, options: ScriptOptions) -> RunSettlement {
    let runner = Runner {
        engine: Arc::clone(&engine),
        plan: Arc::clone(&options.plan),
        args: options.args.unwrap_or_else(|| json!({})),
    };
    let timeout_ms = options.timeout_ms;

    let settled = engine.settled();
    tokio::pin!(settled);
    let body = runner.run_body();
    tokio::pin!(body);

    enum Outcome {
        Returned(Value),
        Failed(WorkflowError),
        Preempted(RunSettlement),
        TimedOut,
    }

    let outcome = match timeout_ms {
        None => tokio::select! {
            settlement = &mut settled => Outcome::Preempted(settlement),
            result = &mut body => match result {
                Ok(value) => Outcome::Returned(value),
                Err(error) => Outcome::Failed(error),
            },
        },
        Some(ms) => tokio::select! {
            settlement = &mut settled => Outcome::Preempted(settlement),
            result = tokio::time::timeout(Duration::from_millis(ms), &mut body) => match result {
                Ok(Ok(value)) => Outcome::Returned(value),
                Ok(Err(error)) => Outcome::Failed(error),
                Err(_) => Outcome::TimedOut,
            },
        },
    };

    match outcome {
        // The run was already settled by another path; the script's future is dropped, and
        // the engine's own stop handling aborts whatever was in flight.
        Outcome::Preempted(settlement) => settlement,
        Outcome::Returned(value) => {
            engine.complete(value);
            settled.await
        }
        Outcome::Failed(error) => {
            engine.fail(error);
            settled.await
        }
        Outcome::TimedOut => {
            let ms = timeout_ms.unwrap_or(0);
            engine.stop(
                RunStopReason::Interrupted,
                Some(WorkflowError::new(
                    WorkflowErrorCode::Interrupted,
                    format!("The workflow script exceeded its wall-clock timeout of {ms}ms"),
                )),
                None,
            );
            settled.await
        }
    }
}

struct Runner {
    engine: Arc<WorkflowEngine>,
    plan: Arc<Plan>,
    args: Value,
}

impl Runner {
    /// Execute the workflow body. A body without a `return` yields `null`, which is what
    /// the sandbox produced for an async function that fell off the end.
    fn run_body(&self) -> Step<'_, Result<Value, WorkflowError>> {
        Box::pin(async move {
            let mut frame: Vec<Value> = vec![Value::Null; self.plan.slot_count];
            Ok(match self.exec(&self.plan.body, &mut frame).await? {
                Some(value) => value,
                None => Value::Null,
            })
        })
    }

    fn exec<'a>(
        &'a self,
        instrs: &'a [Instr],
        frame: &'a mut Vec<Value>,
    ) -> Step<'a, Result<Option<Value>, WorkflowError>> {
        Box::pin(async move {
            for instr in instrs {
                match instr {
                    Instr::EnterPhase { name, .. } => self.engine.enter_phase(name),
                    Instr::Let { slot, value, .. } => {
                        let evaluated = self.eval(value, frame).await?;
                        frame[*slot] = evaluated;
                    }
                    Instr::Return { value, span } => {
                        let evaluated = match value {
                            Some(expr) => self.eval(expr, frame).await?,
                            None => Value::Null,
                        };
                        let _ = span;
                        return Ok(Some(evaluated));
                    }
                    Instr::If {
                        condition,
                        then_branch,
                        else_branch,
                        ..
                    } => {
                        let tested = self.eval(condition, frame).await?;
                        let taken = if value::truthy(&tested) {
                            then_branch
                        } else {
                            else_branch
                        };
                        if let Some(returned) = self.exec(taken, frame).await? {
                            return Ok(Some(returned));
                        }
                    }
                    Instr::While {
                        condition, body, ..
                    } => loop {
                        let tested = self.eval(condition, frame).await?;
                        if !value::truthy(&tested) {
                            break;
                        }
                        if let Some(returned) = self.exec(body, frame).await? {
                            return Ok(Some(returned));
                        }
                    },
                    Instr::ForOf {
                        slot,
                        iterable,
                        body,
                        ..
                    } => {
                        let sequence = self.eval(iterable, frame).await?;
                        let mut items = items_of(&sequence, iterable.span)?;
                        while let Some(item) = items.pop_front() {
                            frame[*slot] = item;
                            if let Some(returned) = self.exec(body, frame).await? {
                                return Ok(Some(returned));
                            }
                        }
                    }
                    Instr::Parallel { strands, span } => {
                        let mut frames = strands
                            .iter()
                            .map(|_| frame.clone())
                            .collect::<Vec<Vec<Value>>>();
                        let results = join_all(
                            strands
                                .iter()
                                .zip(frames.iter_mut())
                                .map(|(strand, local)| self.exec(strand, local)),
                        )
                        .await;
                        // First error wins, and nothing after it runs: a strand that fails
                        // is the script failing, exactly as an uncaught rejection ended the
                        // sandboxed body.
                        for result in results {
                            if result?.is_some() {
                                // `return` inside a strand is a compile error, so this
                                // cannot happen; the arm keeps the invariant explicit
                                // rather than silently dropping a value.
                                return Err(value::script_error(
                                    *span,
                                    "a parallel strand returned, which the compiler rejects",
                                ));
                            }
                        }
                    }
                    Instr::Block { body, .. } => {
                        if let Some(returned) = self.exec(body, frame).await? {
                            return Ok(Some(returned));
                        }
                    }
                    Instr::Try {
                        body,
                        error_slot,
                        catch_body,
                        ..
                    } => {
                        // `Ok(Some(..))` is a `return` unwinding the try body: it is not an
                        // error and the catch body must not see it. Only a rejected facade call
                        // — or a script-level type error — is caught.
                        match self.exec(body, frame).await {
                            Ok(returned) => {
                                if let Some(returned) = returned {
                                    return Ok(Some(returned));
                                }
                            }
                            Err(error) => {
                                frame[*error_slot] = error_value(&error);
                                if let Some(returned) = self.exec(catch_body, frame).await? {
                                    return Ok(Some(returned));
                                }
                            }
                        }
                    }
                    Instr::Effect { expr } => {
                        self.eval(expr, frame).await?;
                    }
                }
            }
            Ok(None)
        })
    }

    fn eval<'a>(
        &'a self,
        expr: &'a Expr,
        frame: &'a [Value],
    ) -> Step<'a, Result<Value, WorkflowError>> {
        Box::pin(async move {
            let span = expr.span;
            match &expr.node {
                ExprNode::Literal(value) => Ok(value.clone()),
                ExprNode::Local(slot) => Ok(frame.get(*slot).cloned().unwrap_or(Value::Null)),
                ExprNode::ArgsObject => Ok(self.args.clone()),
                ExprNode::ArgField { name } => {
                    Ok(self.args.get(name.as_str()).cloned().unwrap_or(Value::Null))
                }
                ExprNode::Template(chunks) => {
                    let mut text = String::new();
                    for chunk in chunks {
                        match chunk {
                            super::ir::TemplateChunk::Text(literal) => text.push_str(literal),
                            super::ir::TemplateChunk::Expr(inner) => {
                                text.push_str(&value::display(&self.eval(inner, frame).await?));
                            }
                        }
                    }
                    Ok(Value::String(text))
                }
                ExprNode::Array(items) => {
                    let mut values = Vec::with_capacity(items.len());
                    for item in items {
                        values.push(self.eval(item, frame).await?);
                    }
                    Ok(Value::Array(values))
                }
                ExprNode::Object(fields) => {
                    let mut object = serde_json::Map::new();
                    for field in fields {
                        object.insert(field.key.clone(), self.eval(&field.value, frame).await?);
                    }
                    Ok(Value::Object(object))
                }
                ExprNode::Unary { op, value } => {
                    let inner = self.eval(value, frame).await?;
                    value::apply_unary(*op, inner, span)
                }
                ExprNode::Binary { op, left, right } => {
                    // `&&` and `||` decide on the left alone, and the right side is not
                    // evaluated when it is not needed — evaluating it would run a facade
                    // call the author guarded against. The result is the operand JavaScript
                    // would have produced, not a boolean.
                    let left_value = self.eval(left, frame).await?;
                    let logical = matches!(op, BinaryOp::And | BinaryOp::Or);
                    if logical {
                        let take_left = matches!(op, BinaryOp::And) != value::truthy(&left_value);
                        if take_left {
                            return Ok(left_value);
                        }
                        return self.eval(right, frame).await;
                    }
                    let right_value = self.eval(right, frame).await?;
                    value::apply_binary(*op, left_value, right_value, span)
                }
                ExprNode::Member { object, property } => {
                    let inner = self.eval(object, frame).await?;
                    value::member(&inner, property, span)
                }
                ExprNode::Index { object, index } => {
                    let inner = self.eval(object, frame).await?;
                    let position = self.eval(index, frame).await?;
                    value::index(&inner, &position, span)
                }

                // ————— synchronous facade —————
                ExprNode::CreateActor {
                    site,
                    name,
                    persona,
                } => {
                    let site_id = self.plan.site_id(*site);
                    let evaluated_name = match name.as_deref() {
                        Some(expr) => value::actor_name(&self.eval(expr, frame).await?, span)?,
                        None => None,
                    };
                    let evaluated_persona = match persona.as_deref() {
                        Some(expr) => Some(self.eval(expr, frame).await?),
                        None => None,
                    };
                    let handle = self
                        .engine
                        .create_actor(site_id, evaluated_name, evaluated_persona)
                        .map_err(|error| with_site(&error, site_id, span))?;
                    Ok(Value::String(handle))
                }
                ExprNode::Log { message } => {
                    let text = value::display(&self.eval(message, frame).await?);
                    self.engine.log(&text);
                    Ok(Value::Null)
                }
                ExprNode::Report {
                    site,
                    item,
                    artifact_id,
                } => {
                    let site_id = self.plan.site_id(*site).to_string();
                    let value = self.eval(item, frame).await?;
                    self.engine.report(&site_id, value, artifact_id.as_deref());
                    Ok(Value::Null)
                }
                ExprNode::DeclareArtifact { site, op, args, .. } => {
                    let site_id = self.plan.site_id(*site).to_string();
                    let values = self.eval_args(args, frame).await?;
                    self.engine.declare_artifact(&site_id, *op, values);
                    Ok(Value::Null)
                }

                // ————— asynchronous facade —————
                ExprNode::Ask {
                    site,
                    actor,
                    instructions,
                } => {
                    let site_id = self.plan.site_id(*site).to_string();
                    let handle = match self.eval(actor, frame).await? {
                        Value::String(handle) => handle,
                        other => {
                            return Err(value::script_error(
                                actor.span,
                                format!(
                                    "the receiver of ask is {} , not an actor created by \
                                     agent(…)",
                                    value::kind_of(&other)
                                ),
                            ))
                        }
                    };
                    let text = value::instructions(&self.eval(instructions, frame).await?);
                    self.engine
                        .ask(&site_id, &handle, &text)
                        .await
                        .map_err(|error| with_site(&error, &site_id, span))
                }
                ExprNode::WorldRead { site, op, args } => {
                    let site_id = self.plan.site_id(*site).to_string();
                    let values = self.eval_args(args, frame).await?;
                    self.engine
                        .world_read(&site_id, op, values)
                        .await
                        .map_err(|error| with_site(&error, &site_id, span))
                }
                ExprNode::PublishArtifact { site, op, args, .. } => {
                    let site_id = self.plan.site_id(*site).to_string();
                    let values = self.eval_args(args, frame).await?;
                    let reference = self
                        .engine
                        .publish_artifact(&site_id, *op, values)
                        .await
                        .map_err(|error| with_site(&error, &site_id, span))?;
                    Ok(json!({"id": reference.id, "version": reference.version}))
                }
                ExprNode::Join { site: _, items } => {
                    // Every item borrows the same read-only frame, so the futures poll
                    // concurrently inside this task without any cloning. Results keep the
                    // written order, as `Promise.all` does.
                    let results = join_all(items.iter().map(|item| self.eval(item, frame))).await;
                    let mut values = Vec::with_capacity(results.len());
                    for result in results {
                        values.push(result?);
                    }
                    Ok(Value::Array(values))
                }
            }
        })
    }

    async fn eval_args(&self, args: &[Expr], frame: &[Value]) -> Result<Vec<Value>, WorkflowError> {
        let mut values = Vec::with_capacity(args.len());
        for arg in args {
            values.push(self.eval(arg, frame).await?);
        }
        Ok(values)
    }
}

/// The elements a `for (const x of …)` iterates: an array by position, an object by its
/// keys (the JavaScript path iterated `Object.keys` for `for…in`, so NWS says so
/// explicitly by iterating keys of an object), and a string by character.
fn items_of(value: &Value, span: Span) -> Result<VecDeque<Value>, WorkflowError> {
    match value {
        Value::Array(items) => Ok(items.iter().cloned().collect()),
        Value::Object(map) => Ok(map.keys().map(|key| Value::String(key.clone())).collect()),
        Value::String(text) => Ok(text.chars().map(|c| json!(c.to_string())).collect()),
        other => Err(value::script_error(
            span,
            format!(
                "for-of iterates an array, an object's keys or a string's characters, got {}",
                value::kind_of(other)
            ),
        )),
    }
}

/// The `catch (err)` value: the engine's own structured error, in the same camelCase shape
/// the journal records it. A script tests `err.code` (`"ValidationFailed"`,
/// `"ArtifactSourceMissing"`, …) rather than matching on a message, because the code is the
/// contract and the message is prose.
fn error_value(error: &WorkflowError) -> Value {
    serde_json::to_value(error.to_json())
        .unwrap_or_else(|_| json!({"code": "DriverError", "message": error.message.clone()}))
}

/// Attach the site the engine was called at to an error message. The engine's own codes
/// and structured fields survive; only the message gains the position the author needs.
fn with_site(error: &WorkflowError, site_id: &str, span: Span) -> WorkflowError {
    if error.message.contains(site_id) {
        return error.clone();
    }
    WorkflowError {
        message: format!(
            "{} (at {site_id}, L{}:C{})",
            error.message, span.line, span.column
        ),
        ..error.clone()
    }
}
