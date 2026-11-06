//! The **host execution layer** of dynamic workflows (specs/workflow-capability-boundary.md, 2026-10-30).
//!
//! The engine lives in `nc-code-workflow` (deterministic state machine + journal + sandbox bridge); this module implements its
//! downlink port [`nc_code_workflow::types::WorkflowDriver`]: dispatching a run's asks onto the turn loop of real host
//! sub-sessions, and providing the landing points for world reads (world-read / world-run) and user-facing artifacts (content artifact).
//!
//! Mapping to the TS driver (`apps/nc-code-cli/packages/bootstrap/src/app/`):
//!
//! | This directory          | TS                                                         |
//! | ----------------------- | ---------------------------------------------------------- |
//! | `driver.rs`             | `workflow-driver.ts` (session and turn orchestration, submit bridge, role sessions) |
//! | `ask.rs`                | runTurn/onTurnResolved/submit bridging in `workflow-driver.ts` |
//! | `prompt.rs`             | `workflow-driver-helpers.ts`, `workflow-ask-epilogue.ts`    |
//! | `transcript.rs`         | `workflow-driver-helpers.ts` (session id), `workflow-driver-transcript.ts` |
//! | `actor_tools.rs`        | `workflow-actor-tools.ts` (the session tool surface), `workflow-driver-escalation.ts` |
//! | `world_read.rs` + `git.rs` | `workflow-world-read.ts` + `workflow-git-world-read.ts` |
//! | `artifacts.rs`          | `workflow-artifact-publish.ts` (the store is built by the host itself) |
//!
//! Boundaries: this module does not interpret run events (`emit` only forwards), does not own the engine's journal
//! decisions, and does not register workflow's twelve model tools (that is the run service's layer). It only answers
//! "how does this ask / world read / artifact publish actually happen on the host".
//!
//! `WorkflowError` carrying structured violations / provider-stop details is part of the contract; boxing it in every
//! `Result` would only make the error type harder to read without changing behavior (the same decision as nc-code-workflow).
#![allow(clippy::result_large_err)]

pub mod actor_tools;
pub mod artifacts;
pub mod driver;
pub mod world_read;

mod ask;
mod failure;
mod git;
mod prompt;
mod transcript;

pub use actor_tools::{EscalationRegistry, PendingQuestion};
pub use driver::{HostWorkflowDriver, HostWorkflowDriverConfig};
