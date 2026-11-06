//! The lowered NWS plan: the tree the interpreter runs.
//!
//! This is the Rust counterpart of the TypeScript front-end's lowering step
//! (`lowering/lower.ts`), with one difference that removes a whole layer: instead of
//! emitting JavaScript text for a sandbox to re-parse, lowering emits a typed tree in
//! which every facade call is already resolved to its site id, its arity has been
//! checked, and its await position is fixed. The interpreter walks this tree directly.
//!
//! Values are JSON (`serde_json::Value`) at every step. An actor handle is the opaque
//! `ActorId` string the engine returns from `create_actor`, and it only ever reaches
//! `.ask` through a binding the resolver proved holds an actor.

use serde_json::Value;

use crate::types::ArtifactOp;

use super::ast::{BinaryOp, Span, UnaryOp};

/// Index into [`super::plan::Plan::sites`].
pub type SiteIndex = usize;

/// A local variable slot. One slot per *lexical* declaration, so two `parallel`
/// strands never write the same slot: NWS has no closures and no assignment statement,
/// which makes each slot write-once within its own strand and lets a strand hold a copy
/// of the enclosing frame instead of sharing it.
pub type Slot = usize;

/// A statement of the lowered body.
#[derive(Clone, Debug, PartialEq)]
pub enum Instr {
    /// `phase "name"` — records the phase marker; no site, no journal key of its own.
    EnterPhase {
        name: String,
        span: Span,
    },
    Let {
        slot: Slot,
        value: Expr,
        span: Span,
    },
    /// `return expr` / `return`. The interpreter unwinds to the body's result.
    Return {
        value: Option<Expr>,
        span: Span,
    },
    If {
        condition: Expr,
        then_branch: Vec<Instr>,
        else_branch: Vec<Instr>,
        span: Span,
    },
    While {
        condition: Expr,
        body: Vec<Instr>,
        span: Span,
    },
    ForOf {
        slot: Slot,
        iterable: Expr,
        body: Vec<Instr>,
        span: Span,
    },
    /// Structured concurrency: each strand runs to completion, all together.
    Parallel {
        strands: Vec<Vec<Instr>>,
        span: Span,
    },
    /// `{ … }`: a group of statements executed in order. A compile-time scope only, since
    /// each declaration already has its own slot.
    Block {
        body: Vec<Instr>,
        span: Span,
    },
    /// `try { … } catch (err) { … }`: recovers from one facade rejection. `error_slot` holds
    /// the engine's structured error as JSON (`{ code, message, violations? }`) while the
    /// catch body runs. A `return` inside the body is not an error and is never caught.
    Try {
        body: Vec<Instr>,
        error_slot: Slot,
        catch_body: Vec<Instr>,
        span: Span,
    },
    /// An expression evaluated for its effect (a facade call, or a literal discarded).
    Effect {
        expr: Expr,
    },
}

/// An expression of the lowered body.
#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    pub span: Span,
    pub node: ExprNode,
}

impl Expr {
    pub fn new(span: Span, node: ExprNode) -> Self {
        Self { span, node }
    }
}

/// One evaluation step.
#[derive(Clone, Debug, PartialEq)]
pub enum ExprNode {
    Literal(Value),
    /// A template: text chunks and interpolated expressions, joined as strings.
    Template(Vec<TemplateChunk>),
    /// A local slot's value.
    Local(Slot),
    /// The whole workflow `args` object.
    ArgsObject,
    /// `args.<name>` for a field the workflow declared.
    ArgField {
        name: String,
    },
    Array(Vec<Expr>),
    Object(Vec<ObjectField>),
    Unary {
        op: UnaryOp,
        value: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    /// Member access on a JSON value (an ask result, a world read, an object literal).
    Member {
        object: Box<Expr>,
        property: String,
    },
    Index {
        object: Box<Expr>,
        index: Box<Expr>,
    },

    // ————— facade: synchronous —————
    /// `agent(name?, persona?)` → the engine's `create_actor` at this site.
    CreateActor {
        site: SiteIndex,
        name: Option<Box<Expr>>,
        persona: Option<Box<Expr>>,
    },
    /// `log(message)` — progress text, no site and no journal key.
    Log {
        message: Box<Expr>,
    },
    /// `artifact.<preset>(id, spec)` → the engine's `declare_artifact`.
    DeclareArtifact {
        site: SiteIndex,
        op: ArtifactOp,
        id: String,
        args: Vec<Expr>,
    },
    /// `report(item, tag?)` → the engine's `report`.
    Report {
        site: SiteIndex,
        item: Box<Expr>,
        artifact_id: Option<String>,
    },

    // ————— facade: asynchronous (must be awaited, or be a `Promise.all` item) —————
    /// `<actor>.ask(instruction)` → the engine's `ask` at this site.
    Ask {
        site: SiteIndex,
        actor: Box<Expr>,
        instructions: Box<Expr>,
    },
    /// `files.*` / `git.*` → the engine's `world_read`.
    WorldRead {
        site: SiteIndex,
        op: String,
        args: Vec<Expr>,
    },
    /// `artifact.file` / `artifact.markdown` → the engine's `publish_artifact`.
    PublishArtifact {
        site: SiteIndex,
        op: ArtifactOp,
        id: String,
        args: Vec<Expr>,
    },
    /// `Promise.all([...])`: await several async facade calls together. The join gets a
    /// site id because the analysis surface addresses joins, but nothing is journaled
    /// for it — the engine keys the nodes the items produce, exactly as the
    /// TypeScript path does.
    Join {
        site: SiteIndex,
        items: Vec<Expr>,
    },
}

impl ExprNode {
    /// Is this an asynchronous facade call? Used twice: to require `await` in the
    /// resolver, and to require an un-awaited call inside `Promise.all`.
    pub fn is_async_facade(&self) -> bool {
        matches!(
            self,
            Self::Ask { .. }
                | Self::WorldRead { .. }
                | Self::PublishArtifact { .. }
                | Self::Join { .. }
        )
    }
}

/// A template chunk.
#[derive(Clone, Debug, PartialEq)]
pub enum TemplateChunk {
    Text(String),
    Expr(Expr),
}

/// One object-literal field.
#[derive(Clone, Debug, PartialEq)]
pub struct ObjectField {
    pub key: String,
    pub value: Expr,
}

/// A journaled or analysed facade site, statically assigned in source order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Site {
    /// `actor#1`, `ask#2`, `world-read#1`, `report#1`, `artifact#1`, `join#1`.
    pub id: String,
    pub kind: SiteKind,
    /// The facade member this site came from (`read`, `markdown`, `run`, …).
    pub member: String,
    pub span: Span,
}

/// The site kinds, in the order their per-kind counters are named.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SiteKind {
    Actor,
    Ask,
    WorldRead,
    Report,
    Artifact,
    Join,
}

impl SiteKind {
    pub fn counter_prefix(&self) -> &'static str {
        match self {
            Self::Actor => "actor#",
            Self::Ask => "ask#",
            Self::WorldRead => "world-read#",
            Self::Report => "report#",
            Self::Artifact => "artifact#",
            Self::Join => "join#",
        }
    }

    /// Site kinds that carry a journal row. `actor` has its own table (`dwf_actor`);
    /// `join` is analysis-only, which is why the TypeScript path never instruments it.
    pub fn is_journaled(&self) -> bool {
        !matches!(self, Self::Join)
    }
}
