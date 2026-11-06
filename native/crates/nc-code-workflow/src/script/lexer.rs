//! Lexer for the nc-code Workflow Script (NWS).
//!
//! The token stream is the only place that knows about whitespace and comments;
//! the parser works on significant tokens plus template chunks. Every token
//! carries its source position so diagnostics stay actionable.

use super::ast::Span;

/// A lexical token.
#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

/// A template chunk. Expression chunks carry their own token stream, lexed with
/// the same rules as the surrounding program.
#[derive(Clone, Debug, PartialEq)]
pub enum TemplateToken {
    Text(String),
    Expression(Vec<Token>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum TokenKind {
    Identifier(String),
    Number(f64),
    String(String),
    Template(Vec<TemplateToken>),
    Schema,
    Workflow,
    Phase,
    Let,
    Return,
    Report,
    If,
    Else,
    While,
    For,
    Of,
    Const,
    Parallel,
    Await,
    Try,
    Catch,
    True,
    False,
    Null,
    TypeString,
    TypeNumber,
    TypeBoolean,
    LeftBrace,
    RightBrace,
    LeftParen,
    RightParen,
    LeftBracket,
    RightBracket,
    Comma,
    Colon,
    Semicolon,
    Question,
    Dot,
    Assign,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    AndAnd,
    OrOr,
    Bang,
    Eof,
}

/// A lexical failure with a stable diagnostic code and a source position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LexError {
    pub code: &'static str,
    pub message: String,
    pub span: Span,
}

impl LexError {
    fn new(message: impl Into<String>, span: Span) -> Self {
        Self {
            code: "nws_lex_error",
            message: message.into(),
            span,
        }
    }
}

/// Lex a whole program, including the end-of-file token.
pub fn lex(source: &str) -> Result<Vec<Token>, LexError> {
    Lexer::new(source).lex_all()
}

struct Lexer {
    chars: Vec<char>,
    index: usize,
    line: u32,
    column: u32,
}

impl Lexer {
    fn new(source: &str) -> Self {
        Self {
            chars: source.chars().collect(),
            index: 0,
            line: 1,
            column: 1,
        }
    }

    fn span(&self) -> Span {
        Span::new(self.line, self.column)
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.index).copied()
    }

    fn peek_next(&self) -> Option<char> {
        self.chars.get(self.index + 1).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let character = self.peek()?;
        self.index += 1;
        if character == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(character)
    }

    fn skip_trivia(&mut self) -> Result<(), LexError> {
        loop {
            match self.peek() {
                Some(character) if character.is_whitespace() => {
                    self.bump();
                }
                Some('/') if self.peek_next() == Some('/') => {
                    while let Some(character) = self.bump() {
                        if character == '\n' {
                            break;
                        }
                    }
                }
                Some('/') if self.peek_next() == Some('*') => {
                    let start = self.span();
                    self.bump();
                    self.bump();
                    let mut closed = false;
                    while let Some(character) = self.bump() {
                        if character == '*' && self.peek() == Some('/') {
                            self.bump();
                            closed = true;
                            break;
                        }
                    }
                    if !closed {
                        return Err(LexError::new("unterminated block comment", start));
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn lex_all(mut self) -> Result<Vec<Token>, LexError> {
        let mut tokens = Vec::new();
        loop {
            self.skip_trivia()?;
            let span = self.span();
            let Some(character) = self.peek() else {
                tokens.push(Token {
                    kind: TokenKind::Eof,
                    span,
                });
                return Ok(tokens);
            };
            let kind = match character {
                '{' => {
                    self.bump();
                    TokenKind::LeftBrace
                }
                '}' => {
                    self.bump();
                    TokenKind::RightBrace
                }
                '(' => {
                    self.bump();
                    TokenKind::LeftParen
                }
                ')' => {
                    self.bump();
                    TokenKind::RightParen
                }
                '[' => {
                    self.bump();
                    TokenKind::LeftBracket
                }
                ']' => {
                    self.bump();
                    TokenKind::RightBracket
                }
                ',' => {
                    self.bump();
                    TokenKind::Comma
                }
                ':' => {
                    self.bump();
                    TokenKind::Colon
                }
                ';' => {
                    self.bump();
                    TokenKind::Semicolon
                }
                '?' => {
                    self.bump();
                    TokenKind::Question
                }
                '.' => {
                    self.bump();
                    TokenKind::Dot
                }
                '+' => {
                    self.bump();
                    TokenKind::Plus
                }
                '-' => {
                    self.bump();
                    TokenKind::Minus
                }
                '*' => {
                    self.bump();
                    TokenKind::Star
                }
                '/' => {
                    self.bump();
                    TokenKind::Slash
                }
                '%' => {
                    self.bump();
                    TokenKind::Percent
                }
                '=' => {
                    self.bump();
                    match self.peek() {
                        Some('=') => {
                            self.bump();
                            if self.peek() == Some('=') {
                                self.bump();
                            }
                            TokenKind::Equal
                        }
                        _ => TokenKind::Assign,
                    }
                }
                '!' => {
                    self.bump();
                    match self.peek() {
                        Some('=') => {
                            self.bump();
                            if self.peek() == Some('=') {
                                self.bump();
                            }
                            TokenKind::NotEqual
                        }
                        _ => TokenKind::Bang,
                    }
                }
                '<' => {
                    self.bump();
                    if self.peek() == Some('=') {
                        self.bump();
                        TokenKind::LessEqual
                    } else {
                        TokenKind::Less
                    }
                }
                '>' => {
                    self.bump();
                    if self.peek() == Some('=') {
                        self.bump();
                        TokenKind::GreaterEqual
                    } else {
                        TokenKind::Greater
                    }
                }
                '&' if self.peek_next() == Some('&') => {
                    self.bump();
                    self.bump();
                    TokenKind::AndAnd
                }
                '|' if self.peek_next() == Some('|') => {
                    self.bump();
                    self.bump();
                    TokenKind::OrOr
                }
                '"' | '\'' => self.lex_string(character, span)?,
                '`' => self.lex_template(span)?,
                digit if digit.is_ascii_digit() => self.lex_number(span)?,
                identifier if is_identifier_start(identifier) => self.lex_identifier(),
                other => {
                    return Err(LexError::new(
                        format!("unexpected character {other:?}"),
                        span,
                    ))
                }
            };
            tokens.push(Token { kind, span });
        }
    }

    fn lex_identifier(&mut self) -> TokenKind {
        let mut text = String::new();
        while let Some(character) = self.peek() {
            if is_identifier_continue(character) {
                text.push(character);
                self.bump();
            } else {
                break;
            }
        }
        match text.as_str() {
            "schema" => TokenKind::Schema,
            "workflow" => TokenKind::Workflow,
            "phase" => TokenKind::Phase,
            "let" => TokenKind::Let,
            "return" => TokenKind::Return,
            "report" => TokenKind::Report,
            "if" => TokenKind::If,
            "else" => TokenKind::Else,
            "while" => TokenKind::While,
            "for" => TokenKind::For,
            "of" => TokenKind::Of,
            "const" => TokenKind::Const,
            "parallel" => TokenKind::Parallel,
            "await" => TokenKind::Await,
            "try" => TokenKind::Try,
            "catch" => TokenKind::Catch,
            "true" => TokenKind::True,
            "false" => TokenKind::False,
            "null" => TokenKind::Null,
            "string" => TokenKind::TypeString,
            "number" => TokenKind::TypeNumber,
            "boolean" => TokenKind::TypeBoolean,
            _ => TokenKind::Identifier(text),
        }
    }

    fn lex_number(&mut self, span: Span) -> Result<TokenKind, LexError> {
        let mut text = String::new();
        while let Some(character) = self.peek() {
            if character.is_ascii_digit() {
                text.push(character);
                self.bump();
            } else {
                break;
            }
        }
        if self.peek() == Some('.') && self.peek_next().is_some_and(|next| next.is_ascii_digit()) {
            text.push('.');
            self.bump();
            while let Some(character) = self.peek() {
                if character.is_ascii_digit() {
                    text.push(character);
                    self.bump();
                } else {
                    break;
                }
            }
        }
        let value = text
            .parse::<f64>()
            .map_err(|error| LexError::new(format!("invalid number {text:?}: {error}"), span))?;
        Ok(TokenKind::Number(value))
    }

    fn lex_string(&mut self, quote: char, span: Span) -> Result<TokenKind, LexError> {
        self.bump();
        let mut text = String::new();
        loop {
            let Some(character) = self.bump() else {
                return Err(LexError::new("unterminated string literal", span));
            };
            if character == quote {
                return Ok(TokenKind::String(text));
            }
            if character == '\n' {
                return Err(LexError::new("newline inside string literal", span));
            }
            if character != '\\' {
                text.push(character);
                continue;
            }
            let Some(escape) = self.bump() else {
                return Err(LexError::new("unterminated escape sequence", span));
            };
            text.push(match escape {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                '\\' => '\\',
                '"' => '"',
                '\'' => '\'',
                '`' => '`',
                '$' => '$',
                other => {
                    return Err(LexError::new(
                        format!("unsupported escape sequence \\{other}"),
                        span,
                    ))
                }
            });
        }
    }

    fn lex_template(&mut self, span: Span) -> Result<TokenKind, LexError> {
        self.bump();
        let mut parts = Vec::new();
        let mut text = String::new();
        loop {
            let Some(character) = self.bump() else {
                return Err(LexError::new("unterminated template literal", span));
            };
            match character {
                '`' => {
                    if !text.is_empty() {
                        parts.push(TemplateToken::Text(text));
                    }
                    return Ok(TokenKind::Template(parts));
                }
                '\\' => {
                    let Some(escape) = self.bump() else {
                        return Err(LexError::new("unterminated template escape", span));
                    };
                    text.push(match escape {
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        '\\' => '\\',
                        '`' => '`',
                        '$' => '$',
                        other => {
                            return Err(LexError::new(
                                format!("unsupported escape sequence \\{other}"),
                                span,
                            ))
                        }
                    });
                }
                '$' if self.peek() == Some('{') => {
                    self.bump();
                    if !text.is_empty() {
                        parts.push(TemplateToken::Text(std::mem::take(&mut text)));
                    }
                    let expression_span = self.span();
                    let source = self.capture_template_expression(expression_span)?;
                    let tokens = Lexer::new(&source).lex_all().map_err(|mut error| {
                        offset_error(&mut error, expression_span);
                        error
                    })?;
                    parts.push(TemplateToken::Expression(tokens));
                }
                other => text.push(other),
            }
        }
    }

    fn capture_template_expression(&mut self, span: Span) -> Result<String, LexError> {
        let mut depth = 1usize;
        let mut source = String::new();
        loop {
            let Some(character) = self.bump() else {
                return Err(LexError::new("unterminated template expression", span));
            };
            match character {
                '{' => {
                    depth += 1;
                    source.push(character);
                }
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(source);
                    }
                    source.push(character);
                }
                '"' | '\'' | '`' => {
                    source.push(character);
                    self.capture_nested_string(character, &mut source, span)?;
                }
                other => source.push(other),
            }
        }
    }

    fn capture_nested_string(
        &mut self,
        quote: char,
        source: &mut String,
        span: Span,
    ) -> Result<(), LexError> {
        loop {
            let Some(character) = self.bump() else {
                return Err(LexError::new(
                    "unterminated string inside template expression",
                    span,
                ));
            };
            source.push(character);
            if character == '\\' {
                let Some(escaped) = self.bump() else {
                    return Err(LexError::new(
                        "unterminated escape inside template expression",
                        span,
                    ));
                };
                source.push(escaped);
                continue;
            }
            if character == quote {
                return Ok(());
            }
        }
    }
}

fn offset_error(error: &mut LexError, offset: Span) {
    error.span.line += offset.line.saturating_sub(1);
    error.span.column += if offset.line == 1 {
        offset.column.saturating_sub(1)
    } else {
        0
    };
}

fn is_identifier_start(character: char) -> bool {
    character == '_' || character == '$' || character.is_alphabetic()
}

fn is_identifier_continue(character: char) -> bool {
    is_identifier_start(character) || character.is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexes_keywords_operators_and_positions() {
        let tokens = lex("let x = 1 + 2 * 3;").expect("lexes");
        assert_eq!(tokens[0].kind, TokenKind::Let);
        assert_eq!(tokens[0].span, Span::new(1, 1));
        assert_eq!(tokens[1].kind, TokenKind::Identifier("x".to_string()));
        assert_eq!(tokens[2].kind, TokenKind::Assign);
        assert_eq!(tokens[3].kind, TokenKind::Number(1.0));
        assert_eq!(tokens[5].kind, TokenKind::Number(2.0));
        assert_eq!(tokens[7].kind, TokenKind::Number(3.0));
        assert_eq!(tokens[8].kind, TokenKind::Semicolon);
        assert_eq!(tokens[9].kind, TokenKind::Eof);
    }

    #[test]
    fn lexes_strings_escapes_and_comments() {
        let tokens = lex("// comment\n\"a\\n\\\"b\" /* block */ 'c'").expect("lexes");
        assert_eq!(tokens[0].kind, TokenKind::String("a\n\"b".to_string()));
        assert_eq!(tokens[1].kind, TokenKind::String("c".to_string()));
    }

    #[test]
    fn lexes_templates_with_expressions() {
        let tokens = lex("`plan ${plan.steps.length} for ${args.repo}`").expect("lexes");
        let TokenKind::Template(parts) = &tokens[0].kind else {
            panic!("expected a template, got {:?}", tokens[0].kind);
        };
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], TemplateToken::Text("plan ".to_string()));
        assert!(matches!(parts[1], TemplateToken::Expression(_)));
        assert_eq!(parts[2], TemplateToken::Text(" for ".to_string()));
        assert!(matches!(parts[3], TemplateToken::Expression(_)));
    }

    #[test]
    fn reports_unterminated_literals_with_a_position() {
        let error = lex("\"oops").expect_err("unterminated");
        assert_eq!(error.code, "nws_lex_error");
        assert_eq!(error.span, Span::new(1, 1));
        let error = lex("let x = `oops").expect_err("unterminated template");
        assert_eq!(error.span, Span::new(1, 9));
    }
}
