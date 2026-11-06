//! Native dynamic-workflow engine (specs/workflow-capability-boundary.md, 2026-10-30).
//!
//! Three layers, ported differently on purpose:
//! - engine + journal: deterministic state machine, no I/O, no clock, no randomness;
//! - sandbox: the lowered-JS body executes in a Node child process over NDJSON (same as TS);
//! - compile: the TypeScript front-end is driven through a persistent in-host compile kernel.

// WorkflowError carries structured violations / provider-stop details as part of the contract;
// boxing it at every Result would obscure the error type without changing behavior.
#![allow(clippy::result_large_err)]

pub mod compile;
pub mod engine;
pub mod engine_artifacts;
pub mod engine_report;
pub mod engine_settlement;
pub mod engine_state;
pub mod engine_world;
pub mod imported_cache;
pub mod journal;
pub mod journal_memory;
pub mod sandbox;
pub mod scheduler;
pub mod script;
pub mod types;
