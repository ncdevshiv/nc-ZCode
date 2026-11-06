//! Abstract syntax tree for the nc-code Workflow Script (NWS).
//!
//! NWS is the Rust-native authoring language that replaces the TypeScript compiler
//! front-end. The AST is deliberately small: every node maps to a workflow
//! capability the engine already understands (phases, actors, typed asks, world
//! reads, reports, artifacts, control flow, return).

use serde_json::Value;

/// Source position, 1-based line and column (UTF-8 column in characters).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub line: u32,
    pub column: u32,
}

impl Span {
    pub const fn new(line: u32, column: u32) -> Self {
        Self { line, column }
    }
}

/// A workflow program: zero or more schema declarations followed by one workflow.
#[derive(Clone, Debug, PartialEq)]
pub struct Program {
    pub schemas: Vec<SchemaDecl>,
    pub workflow: WorkflowDecl,
}

/// A named ask schema. Compiled to JSON Schema for typed asks.
#[derive(Clone, Debug, PartialEq)]
pub struct SchemaDecl {
    pub name: String,
    pub fields: Vec<Field>,
    pub span: Span,
}

/// A schema or workflow-args field.
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub name: String,
    pub optional: bool,
    pub ty: TypeExpr,
    pub span: Span,
}

/// NWS type expressions. Objects are expressed as named schema references.
#[derive(Clone, Debug, PartialEq)]
pub enum TypeExpr {
    String,
    Number,
    Boolean,
    /// A reference to a `schema` declaration.
    Named(String),
    /// `T[]`
    Array(Box<TypeExpr>),
}

/// The single workflow declaration in a program.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowDecl {
    pub name: String,
    pub args: Vec<Field>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

/// A statement.
#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    /// `phase "plan";`
    Phase { name: String, span: Span },
    /// `let name: Type = expr;`
    Let {
        name: String,
        ty: Option<TypeExpr>,
        value: Expr,
        span: Span,
    },
    /// `return expr;` / `return;`
    Return { value: Option<Expr>, span: Span },
    /// `report(item);` / `report(item, artifactId);`
    Report {
        item: Expr,
        artifact_id: Option<Expr>,
        span: Span,
    },
    If {
        condition: Expr,
        then_branch: Block,
        else_branch: Option<ElseBranch>,
        span: Span,
    },
    While {
        condition: Expr,
        body: Block,
        span: Span,
    },
    ForOf {
        binding: String,
        iterable: Expr,
        body: Block,
        span: Span,
    },
    /// `parallel { ... }`: the statements in the block run concurrently. A `{ … }` group
    /// inside it is one multi-statement strand.
    Parallel { body: Block, span: Span },
    /// `{ … }`: a group of statements that share a declaration scope.
    Block { block: Block, span: Span },
    /// `try { … } catch (err) { … }`: recovers from one facade rejection.
    ///
    /// NWS has no exceptions and no `throw`: the only errors a script can meet are the
    /// engine's own rejections (`ask` refused after its repair attempts, a content publish
    /// that found no file, a world read over its cap). The TypeScript facade documented
    /// `try { await artifact.file(…) } catch { … }` as the intended idiom, so this is the
    /// form that keeps that capability. There is no `finally`: nothing in NWS holds a
    /// resource that needs releasing.
    Try {
        body: Block,
        error_binding: String,
        catch_body: Block,
        span: Span,
    },
    /// An expression evaluated for its effect (`await artifact.markdown(...);`).
    Expr { value: Expr, span: Span },
}

/// A braced statement list.
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub statements: Vec<Stmt>,
    pub span: Span,
}

/// `else if (...) ...` or a plain `else { ... }`.
#[derive(Clone, Debug, PartialEq)]
pub enum ElseBranch {
    Block(Block),
    If(Box<Stmt>),
}

/// An expression.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Literal {
        value: Value,
        span: Span,
    },
    /// A backtick template with text and interpolated expressions.
    Template {
        parts: Vec<TemplatePart>,
        span: Span,
    },
    Identifier {
        name: String,
        span: Span,
    },
    Array {
        items: Vec<Expr>,
        span: Span,
    },
    Object {
        entries: Vec<ObjectEntry>,
        span: Span,
    },
    Unary {
        op: UnaryOp,
        value: Box<Expr>,
        span: Span,
    },
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
        span: Span,
    },
    Member {
        object: Box<Expr>,
        property: String,
        span: Span,
    },
    Index {
        object: Box<Expr>,
        index: Box<Expr>,
        span: Span,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
        span: Span,
    },
    Await {
        value: Box<Expr>,
        span: Span,
    },
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Self::Literal { span, .. }
            | Self::Template { span, .. }
            | Self::Identifier { span, .. }
            | Self::Array { span, .. }
            | Self::Object { span, .. }
            | Self::Unary { span, .. }
            | Self::Binary { span, .. }
            | Self::Member { span, .. }
            | Self::Index { span, .. }
            | Self::Call { span, .. }
            | Self::Await { span, .. } => *span,
        }
    }
}

/// One object literal entry.
#[derive(Clone, Debug, PartialEq)]
pub struct ObjectEntry {
    pub key: String,
    pub value: Expr,
}

/// A template chunk: literal text or an interpolated expression.
#[derive(Clone, Debug, PartialEq)]
pub enum TemplatePart {
    Text(String),
    Expr(Expr),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Not,
    Negate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Or,
    And,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
}
