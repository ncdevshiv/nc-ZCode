//! Dynamic-workflow durable journal (`dwf_*` tables, SQLite).
//!
//! Physical-schema parity with the TypeScript session store
//! (`apps/nc-code-cli/packages/adapters/src/storage/session-store/`, migration
//! `0019_dwf_journal`) is a hard requirement: runs created by the TS runtime must stay
//! readable and resumable from the Rust runtime (specs/workflow-capability-boundary.md).
//!
//! Everything that is not the engine-facing [`JournalStorePort`](crate::types::JournalStorePort)
//! — run enumeration, artifact read surfaces, node status counts — is deliberately absent here:
//! those are host-side introspection queries in TS (`dwf-journal-introspection.ts`,
//! `dwf-journal-artifacts.ts`) and adding them to the domain port would force every journal
//! implementation to answer questions the engine never asks.

pub mod sqlite;

pub use sqlite::SqliteJournal;
