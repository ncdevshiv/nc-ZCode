//! Diagnostics for the nc-code Workflow Script (NWS) front-end.
//!
//! One diagnostic is `line:column code message`, the same information the TypeScript
//! front-end put on the wire (`CompileDiagnostic`). The stable `code` is new: NWS
//! diagnostics are addressed by name because the model reads them to repair a script,
//! and a numeric code would need a second table to decode.

use std::fmt;

use crate::compile::CompileDiagnostic;

use super::ast::Span;

/// A single compile diagnostic, always produced with a source position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub span: Span,
    /// Stable diagnostic code, e.g. `nws_unknown_identifier`.
    pub code: &'static str,
    pub message: String,
}

impl Diagnostic {
    pub fn new(span: Span, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            span,
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "L{}:C{} {} {}",
            self.span.line, self.span.column, self.code, self.message
        )
    }
}

/// Convert to the wire shape the host already renders and bounds (`L{}:C{} message`).
///
/// The code is folded into the message text rather than dropped: `CompileDiagnostic.code`
/// is a number in the TypeScript protocol, and inventing a numbering here would make two
/// representations of the same failure.
pub fn to_wire(diagnostics: &[Diagnostic]) -> Vec<CompileDiagnostic> {
    diagnostics
        .iter()
        .map(|diagnostic| CompileDiagnostic {
            code: 0,
            line: diagnostic.span.line,
            column: diagnostic.span.column,
            message: format!("{} {}", diagnostic.code, diagnostic.message),
        })
        .collect()
}

/// Render a diagnostic list the way the host renders compile diagnostics.
pub fn render(diagnostics: &[Diagnostic]) -> String {
    diagnostics
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}
