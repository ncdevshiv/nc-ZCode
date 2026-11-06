//! Workflow script compilation and the runtime it selects.
//!
//! Two languages reach the run service, and the language decides the runtime:
//!
//! | language | compiler | executor | needs Node |
//! | --- | --- | --- | --- |
//! | nc-code Workflow Script (NWS) | `nc_code_workflow::script` (Rust) | `script::interpret`, in-process | no |
//! | the TypeScript workflow dialect | the resident compile kernel | the Node sandbox child | yes |
//!
//! The routing test is the script's **own first token**, read with the NWS lexer: an NWS
//! program begins with `schema` or `workflow`, and no TypeScript workflow script does (it
//! exports an async function). So a malformed NWS script is never handed to the TypeScript
//! compiler (where its errors would be nonsense) and a TypeScript script is never handed to
//! the NWS compiler. Nothing here sniffs for substrings.

use std::collections::BTreeMap;
use std::sync::Arc;

use nc_code_workflow::compile::{AskSpecWire, CompileDiagnostic, CompiledScript};
use nc_code_workflow::script::diagnostic::{self, Diagnostic};
use nc_code_workflow::script::lexer::{lex, TokenKind};
use nc_code_workflow::script::Plan;

/// Which language a submitted script is written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkflowLanguage {
    /// NWS: compiled and executed by Rust, entirely in-process.
    Nws,
    /// The TypeScript workflow dialect: the Node compile kernel plus the Node sandbox.
    TypeScript,
}

/// Read a script's first token with the NWS lexer. A lexer error means "not NWS" rather
/// than a diagnostic: the TypeScript path is the one written for the script that failed to
/// lex as NWS, and it has the diagnostics that describe it.
pub fn detect_language(source: &str) -> WorkflowLanguage {
    let Ok(tokens) = lex(source) else {
        return WorkflowLanguage::TypeScript;
    };
    match tokens.first().map(|token| &token.kind) {
        Some(TokenKind::Schema) | Some(TokenKind::Workflow) => WorkflowLanguage::Nws,
        _ => WorkflowLanguage::TypeScript,
    }
}

/// A compiled workflow script, together with the runtime that can execute it.
#[derive(Clone, Debug)]
pub enum CompiledWorkflow {
    /// Output of the TypeScript front-end: lowered body for the Node sandbox.
    TypeScript(Arc<CompiledScript>),
    /// An NWS plan: a tree the in-process interpreter walks.
    Nws(Arc<Plan>),
}

impl CompiledWorkflow {
    pub fn language(&self) -> WorkflowLanguage {
        match self {
            Self::TypeScript(_) => WorkflowLanguage::TypeScript,
            Self::Nws(_) => WorkflowLanguage::Nws,
        }
    }

    /// sha256 of the author text. Resume's byte-identity check compares this value, and both
    /// paths compute it on the source, never on a lowered form.
    pub fn script_hash(&self) -> &str {
        match self {
            Self::TypeScript(compiled) => &compiled.script_hash,
            Self::Nws(plan) => &plan.script_hash,
        }
    }

    /// Static spec per ask site. The engine hard-fails on a gap (`MissingAskSpec`), so every
    /// ask site either compiler creates is covered here.
    pub fn ask_specs(&self) -> BTreeMap<String, AskSpecWire> {
        match self {
            Self::TypeScript(compiled) => compiled.ask_specs.clone(),
            Self::Nws(plan) => plan.ask_specs.clone(),
        }
    }

    /// The `world.run` commands the script may execute; the driver authorizes from this set.
    pub fn declared_run_commands(&self) -> &[String] {
        match self {
            Self::TypeScript(compiled) => &compiled.declared_run_commands,
            Self::Nws(plan) => &plan.declared_run_commands,
        }
    }

    /// Per-actor `submit_result` shape (the `deriveActorSubmitProfiles` contract).
    pub fn actor_submit_profiles(&self) -> BTreeMap<String, serde_json::Value> {
        match self {
            Self::TypeScript(compiled) => compiled.actor_submit_profiles.clone(),
            Self::Nws(plan) => plan.actor_submit_profiles.clone(),
        }
    }

    /// The lowered JavaScript body the sandbox needs; `None` for NWS, which has no textual
    /// body — its executable form is the plan tree.
    pub fn lowered(&self) -> Option<&str> {
        match self {
            Self::TypeScript(compiled) => compiled.lowered.as_deref(),
            Self::Nws(_) => None,
        }
    }

    /// The in-process plan, when this is an NWS compile.
    pub fn nws_plan(&self) -> Option<Arc<Plan>> {
        match self {
            Self::TypeScript(_) => None,
            Self::Nws(plan) => Some(Arc::clone(plan)),
        }
    }

    /// Facade sites carrying a journal row, in source order.
    pub fn site_ids(&self) -> Vec<String> {
        match self {
            Self::TypeScript(compiled) => compiled.site_ids.clone(),
            Self::Nws(plan) => plan.site_ids(),
        }
    }
}

/// Compile an NWS script. Every diagnostic fails the compile, as on the TypeScript side:
/// the service creates no run row for a script that is not clean.
pub fn compile_nws(source: &str) -> Result<Arc<Plan>, Vec<Diagnostic>> {
    Plan::compile(source).map(Arc::new)
}

/// NWS diagnostics in the wire shape the host already renders and bounds.
pub fn nws_diagnostics(diagnostics: &[Diagnostic]) -> Vec<CompileDiagnostic> {
    diagnostic::to_wire(diagnostics)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_nws_script_is_routed_by_its_first_token() {
        assert_eq!(
            detect_language("workflow \"w\" {\n  return 1;\n}"),
            WorkflowLanguage::Nws
        );
        assert_eq!(
            detect_language("schema Plan {\n  steps: string[]\n}\nworkflow \"w\" { return 1; }"),
            WorkflowLanguage::Nws
        );
        // Leading whitespace and comments do not change the routing.
        assert_eq!(
            detect_language("// plan stage\n\nworkflow \"w\" { return 1; }"),
            WorkflowLanguage::Nws
        );
    }

    #[test]
    fn a_typescript_script_never_reaches_the_nws_compiler() {
        assert_eq!(
            detect_language(
                "export default async function () {\n  const a = agent('planner');\n  \
                 return a.ask('plan');\n}"
            ),
            WorkflowLanguage::TypeScript
        );
        // Unlexable as NWS: routing says "not mine", and the TypeScript path owns the error.
        assert_eq!(
            detect_language("const x: number = (() => 1)();"),
            WorkflowLanguage::TypeScript
        );
    }

    #[test]
    fn both_runtimes_expose_the_same_compile_surface() {
        let plan = compile_nws("workflow \"w\" {\n  return 1;\n}").expect("compiles");
        let compiled = CompiledWorkflow::Nws(Arc::clone(&plan));
        assert_eq!(compiled.language(), WorkflowLanguage::Nws);
        assert_eq!(compiled.script_hash(), plan.script_hash);
        assert!(compiled.lowered().is_none(), "NWS has no textual body");
        assert!(compiled.nws_plan().is_some());
        assert!(compiled.ask_specs().is_empty());
        assert!(compiled.site_ids().is_empty());

        let node = CompiledScript {
            script_hash: plan.script_hash.clone(),
            lowered: Some("return 1;".to_string()),
            site_ids: vec!["actor#1".to_string()],
            ask_specs: BTreeMap::from([(
                "ask#1".to_string(),
                AskSpecWire {
                    typed: false,
                    schema: None,
                },
            )]),
            declared_run_commands: vec!["git status".to_string()],
            actor_submit_profiles: BTreeMap::new(),
            diagnostics: Vec::new(),
        };
        let compiled = CompiledWorkflow::TypeScript(Arc::new(node));
        assert_eq!(compiled.language(), WorkflowLanguage::TypeScript);
        assert_eq!(compiled.lowered(), Some("return 1;"));
        assert!(
            compiled.nws_plan().is_none(),
            "a TypeScript compile has no plan"
        );
        assert_eq!(compiled.declared_run_commands(), ["git status"]);
        assert_eq!(compiled.site_ids(), ["actor#1"]);
        assert!(
            !compiled.ask_specs().get("ask#1").expect("ask#1").typed,
            "the untyped spec is an explicit record, not an absence"
        );
    }

    #[test]
    fn nws_diagnostics_carry_the_code_in_the_message() {
        // The wire shape has a numeric code owned by the TypeScript protocol, so the stable
        // NWS code rides in the message text instead of inventing a numbering.
        let errors = Plan::compile("workflow \"w\" { return missingThing; }")
            .expect_err("unknown identifier");
        let wire = nws_diagnostics(&errors);
        assert_eq!(wire.len(), 1);
        assert!(
            wire[0].message.contains("nws_unknown_identifier"),
            "{}",
            wire[0].message
        );
        assert_eq!(wire[0].line, errors[0].span.line);
        assert_eq!(wire[0].column, errors[0].span.column);
    }
}
