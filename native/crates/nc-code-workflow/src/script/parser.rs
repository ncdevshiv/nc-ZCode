//! Recursive-descent parser for the nc-code Workflow Script (NWS).
//!
//! The parser produces the AST in [`super::ast`] and stops at the first syntax
//! error with a `line:column` diagnostic. Semantic checks (names, facade misuse,
//! schema references) live in the resolver, not here.

use super::ast::{
    BinaryOp, Block, ElseBranch, Expr, Field, ObjectEntry, Program, SchemaDecl, Span, Stmt,
    TemplatePart, TypeExpr, UnaryOp, WorkflowDecl,
};
use super::lexer::{lex, LexError, TemplateToken, Token, TokenKind};

/// A syntax failure with a stable diagnostic code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub code: &'static str,
    pub message: String,
    pub span: Span,
}

impl ParseError {
    fn new(message: impl Into<String>, span: Span) -> Self {
        Self {
            code: "nws_parse_error",
            message: message.into(),
            span,
        }
    }

    /// A syntax failure is one diagnostic; the compiler's error type is shared with the
    /// semantic passes so callers handle one list regardless of where it came from.
    pub fn to_diagnostic(&self) -> super::diagnostic::Diagnostic {
        super::diagnostic::Diagnostic::new(self.span, self.code, self.message.clone())
    }
}

impl From<LexError> for ParseError {
    fn from(error: LexError) -> Self {
        Self {
            code: error.code,
            message: error.message,
            span: error.span,
        }
    }
}

/// Parse a complete NWS program.
pub fn parse(source: &str) -> Result<Program, ParseError> {
    let tokens = lex(source)?;
    Parser::new(tokens).parse_program()
}

struct Parser {
    tokens: Vec<Token>,
    index: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, index: 0 }
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.index.min(self.tokens.len() - 1)]
    }

    fn peek_kind(&self) -> &TokenKind {
        &self.peek().kind
    }

    fn bump(&mut self) -> Token {
        let token = self.peek().clone();
        if self.index + 1 < self.tokens.len() {
            self.index += 1;
        }
        token
    }

    fn at(&self, kind: &TokenKind) -> bool {
        self.peek_kind() == kind
    }

    fn eat(&mut self, kind: &TokenKind) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: &TokenKind, expected: &str) -> Result<Token, ParseError> {
        if self.at(kind) {
            Ok(self.bump())
        } else {
            Err(ParseError::new(
                format!("expected {expected}, found {}", describe(self.peek_kind())),
                self.peek().span,
            ))
        }
    }

    fn expect_identifier(&mut self, expected: &str) -> Result<Token, ParseError> {
        match self.peek_kind().clone() {
            TokenKind::Identifier(_) => Ok(self.bump()),
            _ => Err(ParseError::new(
                format!("expected {expected}, found {}", describe(self.peek_kind())),
                self.peek().span,
            )),
        }
    }

    fn expect_string(&mut self, expected: &str) -> Result<(String, Span), ParseError> {
        let token = self.bump();
        match token.kind {
            TokenKind::String(value) => Ok((value, token.span)),
            _ => Err(ParseError::new(
                format!("expected {expected}, found {}", describe(&token.kind)),
                token.span,
            )),
        }
    }

    fn identifier_text(token: &Token) -> String {
        match &token.kind {
            TokenKind::Identifier(name) => name.clone(),
            _ => String::new(),
        }
    }

    fn parse_program(&mut self) -> Result<Program, ParseError> {
        let mut schemas = Vec::new();
        while self.at(&TokenKind::Schema) {
            schemas.push(self.parse_schema()?);
        }
        if self.at(&TokenKind::Eof) {
            return Err(ParseError::new(
                "the program contains no `workflow` declaration",
                self.peek().span,
            ));
        }
        let workflow = self.parse_workflow()?;
        if !self.at(&TokenKind::Eof) {
            return Err(ParseError::new(
                format!(
                    "unexpected {} after the workflow declaration",
                    describe(self.peek_kind())
                ),
                self.peek().span,
            ));
        }
        Ok(Program { schemas, workflow })
    }

    fn parse_schema(&mut self) -> Result<SchemaDecl, ParseError> {
        let span = self.expect(&TokenKind::Schema, "`schema`")?.span;
        let name = Self::identifier_text(&self.expect_identifier("a schema name")?);
        self.expect(&TokenKind::LeftBrace, "`{` after the schema name")?;
        let fields = self.parse_fields_until_right_brace()?;
        Ok(SchemaDecl { name, fields, span })
    }

    fn parse_fields_until_right_brace(&mut self) -> Result<Vec<Field>, ParseError> {
        let mut fields = Vec::new();
        while !self.at(&TokenKind::RightBrace) {
            let span = self.peek().span;
            let name = Self::identifier_text(&self.expect_identifier("a field name")?);
            let optional = self.eat(&TokenKind::Question);
            self.expect(&TokenKind::Colon, "`:` after a field name")?;
            let ty = self.parse_type()?;
            fields.push(Field {
                name,
                optional,
                ty,
                span,
            });
            if !self.eat(&TokenKind::Comma) {
                let _ = self.eat(&TokenKind::Semicolon);
            }
        }
        self.expect(&TokenKind::RightBrace, "`}`")?;
        Ok(fields)
    }

    fn parse_type(&mut self) -> Result<TypeExpr, ParseError> {
        let span = self.peek().span;
        let mut ty = match self.peek_kind().clone() {
            TokenKind::TypeString => {
                self.bump();
                TypeExpr::String
            }
            TokenKind::TypeNumber => {
                self.bump();
                TypeExpr::Number
            }
            TokenKind::TypeBoolean => {
                self.bump();
                TypeExpr::Boolean
            }
            TokenKind::Identifier(name) => {
                self.bump();
                TypeExpr::Named(name)
            }
            other => {
                return Err(ParseError::new(
                    format!("expected a type, found {}", describe(&other)),
                    span,
                ))
            }
        };
        while self.at(&TokenKind::LeftBracket) {
            self.bump();
            self.expect(&TokenKind::RightBracket, "`]` in an array type")?;
            ty = TypeExpr::Array(Box::new(ty));
        }
        Ok(ty)
    }

    fn parse_workflow(&mut self) -> Result<WorkflowDecl, ParseError> {
        let span = self.expect(&TokenKind::Workflow, "`workflow`")?.span;
        let (name, _) = self.expect_string("a workflow name string")?;
        self.expect(&TokenKind::LeftBrace, "`{` after the workflow name")?;
        // `args` is contextual: it declares the workflow-args block only when the
        // identifier is immediately followed by `{`; otherwise it is the bound
        // argument object in expressions.
        let declares_args = matches!(
            self.peek_kind(),
            TokenKind::Identifier(name) if name == "args"
        ) && matches!(
            self.tokens.get(self.index + 1).map(|token| &token.kind),
            Some(TokenKind::LeftBrace)
        );
        let args = if declares_args {
            self.bump();
            self.expect(&TokenKind::LeftBrace, "`{` after `args`")?;
            self.parse_fields_until_right_brace()?
        } else {
            Vec::new()
        };
        let mut body = Vec::new();
        while !self.at(&TokenKind::RightBrace) {
            if self.at(&TokenKind::Eof) {
                return Err(ParseError::new(
                    "unterminated workflow block: missing `}`",
                    self.peek().span,
                ));
            }
            body.push(self.parse_statement()?);
        }
        self.expect(&TokenKind::RightBrace, "`}` closing the workflow")?;
        Ok(WorkflowDecl {
            name,
            args,
            body,
            span,
        })
    }

    fn parse_statement(&mut self) -> Result<Stmt, ParseError> {
        let span = self.peek().span;
        match self.peek_kind().clone() {
            TokenKind::Phase => {
                self.bump();
                let name_token = self.bump();
                let TokenKind::String(name) = name_token.kind else {
                    return Err(ParseError::new(
                        "`phase` expects a string literal",
                        name_token.span,
                    ));
                };
                self.expect(&TokenKind::Semicolon, "`;` after a phase statement")?;
                Ok(Stmt::Phase { name, span })
            }
            TokenKind::Let => {
                self.bump();
                let name = Self::identifier_text(&self.expect_identifier("a binding name")?);
                let ty = if self.eat(&TokenKind::Colon) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                self.expect(&TokenKind::Assign, "`=` in a let statement")?;
                let value = self.parse_expression()?;
                self.expect(&TokenKind::Semicolon, "`;` after a let statement")?;
                Ok(Stmt::Let {
                    name,
                    ty,
                    value,
                    span,
                })
            }
            TokenKind::Return => {
                self.bump();
                let value = if self.at(&TokenKind::Semicolon) {
                    None
                } else {
                    Some(self.parse_expression()?)
                };
                self.expect(&TokenKind::Semicolon, "`;` after a return statement")?;
                Ok(Stmt::Return { value, span })
            }
            TokenKind::Report => {
                self.bump();
                self.expect(&TokenKind::LeftParen, "`(` after `report`")?;
                let item = self.parse_expression()?;
                let artifact_id = if self.eat(&TokenKind::Comma) {
                    Some(self.parse_expression()?)
                } else {
                    None
                };
                self.expect(&TokenKind::RightParen, "`)` closing the report call")?;
                self.expect(&TokenKind::Semicolon, "`;` after a report statement")?;
                Ok(Stmt::Report {
                    item,
                    artifact_id,
                    span,
                })
            }
            TokenKind::If => Ok(self.parse_if_statement()?),
            TokenKind::While => {
                self.bump();
                self.expect(&TokenKind::LeftParen, "`(` after `while`")?;
                let condition = self.parse_expression()?;
                self.expect(&TokenKind::RightParen, "`)` after the while condition")?;
                let body = self.parse_block()?;
                Ok(Stmt::While {
                    condition,
                    body,
                    span,
                })
            }
            TokenKind::For => {
                self.bump();
                self.expect(&TokenKind::LeftParen, "`(` after `for`")?;
                self.expect(&TokenKind::Const, "`const` in a for-of binding")?;
                let binding = Self::identifier_text(&self.expect_identifier("a for-of binding")?);
                self.expect(&TokenKind::Of, "`of` in a for-of loop")?;
                let iterable = self.parse_expression()?;
                self.expect(&TokenKind::RightParen, "`)` after the for-of iterable")?;
                let body = self.parse_block()?;
                Ok(Stmt::ForOf {
                    binding,
                    iterable,
                    body,
                    span,
                })
            }
            TokenKind::Parallel => {
                self.bump();
                let body = self.parse_block()?;
                Ok(Stmt::Parallel { body, span })
            }
            TokenKind::Try => {
                self.bump();
                let body = self.parse_block()?;
                self.expect(&TokenKind::Catch, "`catch` after a try block")?;
                self.expect(&TokenKind::LeftParen, "`(` after `catch`")?;
                let error_binding =
                    Self::identifier_text(&self.expect_identifier("the catch binding")?);
                self.expect(&TokenKind::RightParen, "`)` after the catch binding")?;
                let catch_body = self.parse_block()?;
                Ok(Stmt::Try {
                    body,
                    error_binding,
                    catch_body,
                    span,
                })
            }
            // A `{ … }` group at statement position: one scope, several statements. Inside
            // `parallel { … }` it is what makes a multi-statement strand possible.
            TokenKind::LeftBrace => {
                let block = self.parse_block()?;
                let span = block.span;
                Ok(Stmt::Block { block, span })
            }
            _ => {
                let value = self.parse_expression()?;
                self.expect(&TokenKind::Semicolon, "`;` after an expression statement")?;
                Ok(Stmt::Expr { value, span })
            }
        }
    }

    fn parse_if_statement(&mut self) -> Result<Stmt, ParseError> {
        let span = self.expect(&TokenKind::If, "`if`")?.span;
        self.expect(&TokenKind::LeftParen, "`(` after `if`")?;
        let condition = self.parse_expression()?;
        self.expect(&TokenKind::RightParen, "`)` after the if condition")?;
        let then_branch = self.parse_block()?;
        let else_branch = if self.eat(&TokenKind::Else) {
            if self.at(&TokenKind::If) {
                Some(ElseBranch::If(Box::new(self.parse_if_statement()?)))
            } else {
                Some(ElseBranch::Block(self.parse_block()?))
            }
        } else {
            None
        };
        Ok(Stmt::If {
            condition,
            then_branch,
            else_branch,
            span,
        })
    }

    fn parse_block(&mut self) -> Result<Block, ParseError> {
        let span = self
            .expect(&TokenKind::LeftBrace, "`{` opening a block")?
            .span;
        let mut statements = Vec::new();
        while !self.at(&TokenKind::RightBrace) {
            if self.at(&TokenKind::Eof) {
                return Err(ParseError::new(
                    "unterminated block: missing `}`",
                    self.peek().span,
                ));
            }
            statements.push(self.parse_statement()?);
        }
        self.expect(&TokenKind::RightBrace, "`}` closing the block")?;
        Ok(Block { statements, span })
    }

    fn parse_expression(&mut self) -> Result<Expr, ParseError> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_and()?;
        while self.eat(&TokenKind::OrOr) {
            let right = self.parse_and()?;
            let span = left.span();
            left = Expr::Binary {
                op: BinaryOp::Or,
                left: Box::new(left),
                right: Box::new(right),
                span,
            };
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_equality()?;
        while self.eat(&TokenKind::AndAnd) {
            let right = self.parse_equality()?;
            let span = left.span();
            left = Expr::Binary {
                op: BinaryOp::And,
                left: Box::new(left),
                right: Box::new(right),
                span,
            };
        }
        Ok(left)
    }

    fn parse_equality(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_comparison()?;
        loop {
            let op = if self.eat(&TokenKind::Equal) {
                BinaryOp::Equal
            } else if self.eat(&TokenKind::NotEqual) {
                BinaryOp::NotEqual
            } else {
                return Ok(left);
            };
            let right = self.parse_comparison()?;
            let span = left.span();
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
                span,
            };
        }
    }

    fn parse_comparison(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_additive()?;
        loop {
            let op = if self.eat(&TokenKind::Less) {
                BinaryOp::Less
            } else if self.eat(&TokenKind::LessEqual) {
                BinaryOp::LessEqual
            } else if self.eat(&TokenKind::Greater) {
                BinaryOp::Greater
            } else if self.eat(&TokenKind::GreaterEqual) {
                BinaryOp::GreaterEqual
            } else {
                return Ok(left);
            };
            let right = self.parse_additive()?;
            let span = left.span();
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
                span,
            };
        }
    }

    fn parse_additive(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = if self.eat(&TokenKind::Plus) {
                BinaryOp::Add
            } else if self.eat(&TokenKind::Minus) {
                BinaryOp::Subtract
            } else {
                return Ok(left);
            };
            let right = self.parse_multiplicative()?;
            let span = left.span();
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
                span,
            };
        }
    }

    fn parse_multiplicative(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_unary()?;
        loop {
            let op = if self.eat(&TokenKind::Star) {
                BinaryOp::Multiply
            } else if self.eat(&TokenKind::Slash) {
                BinaryOp::Divide
            } else if self.eat(&TokenKind::Percent) {
                BinaryOp::Remainder
            } else {
                return Ok(left);
            };
            let right = self.parse_unary()?;
            let span = left.span();
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
                span,
            };
        }
    }

    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        if self.at(&TokenKind::Bang) || self.at(&TokenKind::Minus) {
            let token = self.bump();
            let op = if token.kind == TokenKind::Bang {
                UnaryOp::Not
            } else {
                UnaryOp::Negate
            };
            let value = self.parse_unary()?;
            return Ok(Expr::Unary {
                op,
                value: Box::new(value),
                span: token.span,
            });
        }
        if self.at(&TokenKind::Await) {
            let token = self.bump();
            let value = self.parse_unary()?;
            return Ok(Expr::Await {
                value: Box::new(value),
                span: token.span,
            });
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Result<Expr, ParseError> {
        let mut expression = self.parse_primary()?;
        loop {
            if self.eat(&TokenKind::Dot) {
                let property = Self::identifier_text(&self.expect_identifier("a property name")?);
                let span = expression.span();
                expression = Expr::Member {
                    object: Box::new(expression),
                    property,
                    span,
                };
                continue;
            }
            if self.eat(&TokenKind::LeftBracket) {
                let index = self.parse_expression()?;
                self.expect(&TokenKind::RightBracket, "`]` after an index")?;
                let span = expression.span();
                expression = Expr::Index {
                    object: Box::new(expression),
                    index: Box::new(index),
                    span,
                };
                continue;
            }
            if self.eat(&TokenKind::LeftParen) {
                let mut args = Vec::new();
                if !self.at(&TokenKind::RightParen) {
                    loop {
                        args.push(self.parse_expression()?);
                        if !self.eat(&TokenKind::Comma) {
                            break;
                        }
                        if self.at(&TokenKind::RightParen) {
                            break;
                        }
                    }
                }
                self.expect(&TokenKind::RightParen, "`)` after call arguments")?;
                let span = expression.span();
                expression = Expr::Call {
                    callee: Box::new(expression),
                    args,
                    span,
                };
                continue;
            }
            return Ok(expression);
        }
    }

    fn parse_primary(&mut self) -> Result<Expr, ParseError> {
        let token = self.peek().clone();
        match token.kind {
            TokenKind::Number(value) => {
                self.bump();
                Ok(Expr::Literal {
                    value: serde_json::json!(value),
                    span: token.span,
                })
            }
            TokenKind::String(value) => {
                self.bump();
                Ok(Expr::Literal {
                    value: serde_json::Value::String(value),
                    span: token.span,
                })
            }
            TokenKind::True => {
                self.bump();
                Ok(Expr::Literal {
                    value: serde_json::Value::Bool(true),
                    span: token.span,
                })
            }
            TokenKind::False => {
                self.bump();
                Ok(Expr::Literal {
                    value: serde_json::Value::Bool(false),
                    span: token.span,
                })
            }
            TokenKind::Null => {
                self.bump();
                Ok(Expr::Literal {
                    value: serde_json::Value::Null,
                    span: token.span,
                })
            }
            TokenKind::Identifier(name) => {
                self.bump();
                Ok(Expr::Identifier {
                    name,
                    span: token.span,
                })
            }
            TokenKind::Template(parts) => {
                self.bump();
                self.parse_template(parts, token.span)
            }
            TokenKind::LeftParen => {
                self.bump();
                let expression = self.parse_expression()?;
                self.expect(&TokenKind::RightParen, "`)` after a grouped expression")?;
                Ok(expression)
            }
            TokenKind::LeftBracket => self.parse_array(),
            TokenKind::LeftBrace => self.parse_object(),
            other => Err(ParseError::new(
                format!("expected an expression, found {}", describe(&other)),
                token.span,
            )),
        }
    }

    fn parse_template(
        &mut self,
        parts: Vec<TemplateToken>,
        span: Span,
    ) -> Result<Expr, ParseError> {
        let mut parsed = Vec::with_capacity(parts.len());
        for part in parts {
            match part {
                TemplateToken::Text(text) => parsed.push(TemplatePart::Text(text)),
                TemplateToken::Expression(tokens) => {
                    let mut parser = Parser::new(tokens);
                    let expression = parser.parse_expression()?;
                    if !parser.at(&TokenKind::Eof) {
                        return Err(ParseError::new(
                            format!(
                                "unexpected {} in a template expression",
                                describe(parser.peek_kind())
                            ),
                            parser.peek().span,
                        ));
                    }
                    parsed.push(TemplatePart::Expr(expression));
                }
            }
        }
        Ok(Expr::Template {
            parts: parsed,
            span,
        })
    }

    fn parse_array(&mut self) -> Result<Expr, ParseError> {
        let span = self.expect(&TokenKind::LeftBracket, "`[`")?.span;
        let mut items = Vec::new();
        if !self.at(&TokenKind::RightBracket) {
            loop {
                items.push(self.parse_expression()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
                if self.at(&TokenKind::RightBracket) {
                    break;
                }
            }
        }
        self.expect(&TokenKind::RightBracket, "`]` closing an array")?;
        Ok(Expr::Array { items, span })
    }

    fn parse_object(&mut self) -> Result<Expr, ParseError> {
        let span = self.expect(&TokenKind::LeftBrace, "`{`")?.span;
        let mut entries = Vec::new();
        if !self.at(&TokenKind::RightBrace) {
            loop {
                let key = match self.peek_kind().clone() {
                    TokenKind::Identifier(name) => {
                        self.bump();
                        name
                    }
                    TokenKind::String(value) => {
                        self.bump();
                        value
                    }
                    other => {
                        return Err(ParseError::new(
                            format!("expected an object key, found {}", describe(&other)),
                            self.peek().span,
                        ))
                    }
                };
                let value = if self.eat(&TokenKind::Colon) {
                    self.parse_expression()?
                } else if self.at(&TokenKind::Comma) || self.at(&TokenKind::RightBrace) {
                    Expr::Identifier {
                        name: key.clone(),
                        span: self.peek().span,
                    }
                } else {
                    return Err(ParseError::new(
                        "expected `:` after an object key",
                        self.peek().span,
                    ));
                };
                entries.push(ObjectEntry { key, value });
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
                if self.at(&TokenKind::RightBrace) {
                    break;
                }
            }
        }
        self.expect(&TokenKind::RightBrace, "`}` closing an object")?;
        Ok(Expr::Object { entries, span })
    }
}

fn describe(kind: &TokenKind) -> String {
    match kind {
        TokenKind::Identifier(name) => format!("identifier `{name}`"),
        TokenKind::Number(value) => format!("number `{value}`"),
        TokenKind::String(value) => format!("string {value:?}"),
        TokenKind::Template(_) => "a template literal".to_string(),
        TokenKind::Eof => "end of file".to_string(),
        other => format!("`{}`", token_text(other)),
    }
}

fn token_text(kind: &TokenKind) -> &'static str {
    match kind {
        TokenKind::Schema => "schema",
        TokenKind::Workflow => "workflow",
        TokenKind::Phase => "phase",
        TokenKind::Let => "let",
        TokenKind::Return => "return",
        TokenKind::Report => "report",
        TokenKind::If => "if",
        TokenKind::Else => "else",
        TokenKind::While => "while",
        TokenKind::For => "for",
        TokenKind::Of => "of",
        TokenKind::Const => "const",
        TokenKind::Parallel => "parallel",
        TokenKind::Try => "try",
        TokenKind::Catch => "catch",
        TokenKind::Await => "await",
        TokenKind::True => "true",
        TokenKind::False => "false",
        TokenKind::Null => "null",
        TokenKind::TypeString => "string",
        TokenKind::TypeNumber => "number",
        TokenKind::TypeBoolean => "boolean",
        TokenKind::LeftBrace => "{",
        TokenKind::RightBrace => "}",
        TokenKind::LeftParen => "(",
        TokenKind::RightParen => ")",
        TokenKind::LeftBracket => "[",
        TokenKind::RightBracket => "]",
        TokenKind::Comma => ",",
        TokenKind::Colon => ":",
        TokenKind::Semicolon => ";",
        TokenKind::Question => "?",
        TokenKind::Dot => ".",
        TokenKind::Assign => "=",
        TokenKind::Equal => "==",
        TokenKind::NotEqual => "!=",
        TokenKind::Less => "<",
        TokenKind::LessEqual => "<=",
        TokenKind::Greater => ">",
        TokenKind::GreaterEqual => ">=",
        TokenKind::Plus => "+",
        TokenKind::Minus => "-",
        TokenKind::Star => "*",
        TokenKind::Slash => "/",
        TokenKind::Percent => "%",
        TokenKind::AndAnd => "&&",
        TokenKind::OrOr => "||",
        TokenKind::Bang => "!",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(source: &str) -> Program {
        parse(source).unwrap_or_else(|error| {
            panic!(
                "parse failed at {}:{}: {}",
                error.span.line, error.span.column, error.message
            )
        })
    }

    #[test]
    fn parses_a_full_workflow_with_schema_args_phases_and_calls() {
        let program = parse_ok(
            r##"
schema Plan {
  steps: string[]
  rationale: string
}

workflow "readme-plan" {
  args { repo: string, verbose?: boolean }

  phase "plan";
  let planner = agent("planner", "You plan.");
  let plan: Plan = await planner.ask("Draft a plan for " + args.repo);

  phase "execute";
  let readme = await files.read("README.md");
  report({ repo: args.repo, bytes: readme.length, steps: plan.steps }, "summary");
  await artifact.markdown("summary", "Plan\n\n" + plan.steps.join("\n"));
  return { rationale: plan.rationale, steps: plan.steps };
}
"##,
        );
        assert_eq!(program.schemas.len(), 1);
        assert_eq!(program.schemas[0].name, "Plan");
        assert_eq!(program.schemas[0].fields.len(), 2);
        assert_eq!(
            program.schemas[0].fields[0].ty,
            TypeExpr::Array(Box::new(TypeExpr::String))
        );
        assert_eq!(program.workflow.name, "readme-plan");
        assert_eq!(program.workflow.args.len(), 2);
        assert!(program.workflow.args[1].optional);
        assert_eq!(program.workflow.body.len(), 8);
        let Stmt::Let { name, value, .. } = &program.workflow.body[2] else {
            panic!("expected the plan binding");
        };
        assert_eq!(name, "plan");
        let Expr::Await { value, .. } = value else {
            panic!("expected await");
        };
        let Expr::Call { callee, args, .. } = value.as_ref() else {
            panic!("expected a call");
        };
        assert_eq!(args.len(), 1);
        assert!(matches!(
            callee.as_ref(),
            Expr::Member { property, .. } if property == "ask"
        ));
    }

    #[test]
    fn parses_operator_precedence() {
        let program = parse_ok("workflow \"w\" { return 1 + 2 * 3; }");
        let Stmt::Return {
            value: Some(Expr::Binary { op, right, .. }),
            ..
        } = &program.workflow.body[0]
        else {
            panic!("expected a return expression");
        };
        assert_eq!(*op, BinaryOp::Add);
        assert!(matches!(
            right.as_ref(),
            Expr::Binary {
                op: BinaryOp::Multiply,
                ..
            }
        ));
    }

    #[test]
    fn parses_boolean_and_comparison_precedence() {
        let program = parse_ok("workflow \"w\" { return a || b && c == d + 1; }");
        let Stmt::Return {
            value: Some(Expr::Binary { op, right, .. }),
            ..
        } = &program.workflow.body[0]
        else {
            panic!("expected a return expression");
        };
        assert_eq!(*op, BinaryOp::Or);
        assert!(matches!(
            right.as_ref(),
            Expr::Binary {
                op: BinaryOp::And,
                ..
            }
        ));
    }

    #[test]
    fn parses_templates_with_interpolation() {
        let program = parse_ok("workflow \"w\" { return `n=${args.n}`; }");
        let Stmt::Return {
            value: Some(Expr::Template { parts, .. }),
            ..
        } = &program.workflow.body[0]
        else {
            panic!("expected a template return");
        };
        assert_eq!(parts[0], TemplatePart::Text("n=".to_string()));
        assert!(matches!(parts[1], TemplatePart::Expr(_)));
    }

    #[test]
    fn parses_control_flow_and_parallel_blocks() {
        let program = parse_ok(
            r#"
workflow "w" {
  if (args.n > 0) { phase "yes"; } else if (args.n == 0) { phase "zero"; } else { phase "no"; }
  while (args.n < 3) { let x = 1; }
  for (const item of args.items) { report(item); }
  parallel { let a = 1; let b = 2; }
  return null;
}
"#,
        );
        assert_eq!(program.workflow.body.len(), 5);
        assert!(matches!(program.workflow.body[0], Stmt::If { .. }));
        assert!(matches!(program.workflow.body[1], Stmt::While { .. }));
        assert!(matches!(program.workflow.body[2], Stmt::ForOf { .. }));
        assert!(matches!(program.workflow.body[3], Stmt::Parallel { .. }));
    }

    #[test]
    fn rejects_missing_semicolons_with_a_position() {
        let error = parse("workflow \"w\" { phase \"x\" }").expect_err("missing semicolon");
        assert_eq!(error.code, "nws_parse_error");
        assert_eq!(error.span.line, 1);
        assert!(error.message.contains("`;`"), "{}", error.message);
    }

    #[test]
    fn accepts_named_types_for_the_resolver() {
        let program = parse_ok("schema S { x: Thing } workflow \"w\" { return 1; }");
        assert_eq!(
            program.schemas[0].fields[0].ty,
            TypeExpr::Named("Thing".to_string())
        );
    }

    #[test]
    fn rejects_a_program_without_a_workflow() {
        let error = parse("schema S { x: string }").expect_err("no workflow");
        assert!(error.message.contains("no `workflow`"), "{}", error.message);
    }

    #[test]
    fn rejects_javascript_syntax_with_a_position() {
        let error = parse("workflow \"w\" { const x = () => 1; }").expect_err("arrow function");
        assert_eq!(error.code, "nws_parse_error");
        assert!(error.message.contains("expected"), "{}", error.message);
    }
}
