//! The identity and transcript seed of an actor session (port of the id minting in `workflow-driver-helpers.ts` +
//! the copying in `workflow-actor-transcript.ts`).
//!
//! The difference from TS (recorded honestly): TS's `mintActorSessionId` directly **decides** the child session id; this host's
//! `HostServer::open_session` mints the id itself (`next_id("session")`), and there is no "create a session with a given id" API.
//! So the string minted here is only the key in the driver registry (plus a readable identity for logs/debugging); the real session
//! identity is the id minted by the host. Both identities are written together into `dwf_actor.session_id` in `create_actor_session`,
//! and resume reattaches from that record -- the record is the cross-process authority.

use std::sync::Arc;

use nc_code_workflow::types::{ActorRef, ActorSessionSeed, WorkflowError, WorkflowErrorCode};

use crate::session::Session;
use crate::HostServer;

/// Fold any string into `[A-Za-z0-9.\-_]`, where `_` only ever appears as escape output.
///
/// The two-step order is a contract: first escape a literal `_` into `__`, then map every character outside the whitelist into a single `_`. Reversing the
/// order defeats the escape. Without the escape step, `my_actor#1` and `my#actor#1` would fold into the same string.
fn sanitize_id_segment(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '_' => escaped.push_str("__"),
            'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '-' => escaped.push(character),
            _ => escaped.push('_'),
        }
    }
    escaped
}

/// The deterministic key of an actor session in the driver registry: `dwf-<runId>-<siteId@ordinal>` (charset-safe).
pub(crate) fn actor_session_key(run_id: &str, actor: &ActorRef) -> String {
    format!(
        "dwf-{}-{}",
        sanitize_id_segment(run_id),
        sanitize_id_segment(&nc_code_workflow::types::ref_to_string(
            &actor.site_id,
            actor.ordinal
        ))
    )
}

/// Copy the first `message_count` **model history messages** of the source session into the freshly created actor session.
///
/// The host's storage capability boundary (not pretending there is more):
/// - the host only has append APIs for rows/history, with no dedicated "copy a message prefix" path; this goes through
///   `Session::append_history` (the host's only history write entry, which persists to disk before entering memory).
/// - What is copied is the model history (`Message`), **not** the UI row projection. An actor child session is never shown to users anyway,
///   and what amend-resume needs is the transcript prefix the model sees; the cost of the missing row projection is recorded in this module and the delivery report.
/// - When the source session is not in memory it is rehydrated with `HostServer::load_session` (the host's normal cold-resume path),
///   which runs SessionStart hooks -- this is the host's existing semantics for "reading a persisted session", not a second hydration
///   path invented by this module.
/// - Idempotent: when the target session already has history (a reattached session), skip outright and never append twice.
pub(crate) fn copy_history_prefix(
    host: &HostServer,
    child: &Arc<Session>,
    seed: &ActorSessionSeed,
) -> Result<(), WorkflowError> {
    let existing = child.history_snapshot().map_err(|error| {
        WorkflowError::new(
            WorkflowErrorCode::DriverError,
            format!(
                "Cannot inspect the seeded subagent session {} before copying its transcript: {error}",
                child.id
            ),
        )
    })?;
    if !existing.is_empty() {
        return Ok(());
    }
    let source = host.load_session(&seed.source_session_id).ok_or_else(|| {
        WorkflowError::new(
            WorkflowErrorCode::DriverError,
            format!(
                "Subagent session {} carries a transcript seed from {}, but that session does \
                 not exist in this host's store (check the data directory).",
                child.id, seed.source_session_id
            ),
        )
    })?;
    let history = source.history_snapshot().map_err(|error| {
        WorkflowError::new(
            WorkflowErrorCode::DriverError,
            format!(
                "Cannot read the transcript of seed source session {}: {error}",
                seed.source_session_id
            ),
        )
    })?;
    let take = usize::try_from(seed.message_count).unwrap_or(usize::MAX);
    for message in history.iter().take(take) {
        child.append_history(message.clone()).map_err(|error| {
            WorkflowError::new(
                WorkflowErrorCode::DriverError,
                format!(
                    "Copying the transcript prefix into subagent session {} failed: {error}",
                    child.id
                ),
            )
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_keys_are_run_scoped_and_charset_safe() {
        let actor = ActorRef {
            site_id: "actor#1".to_string(),
            ordinal: 2,
        };
        let key = actor_session_key("dwfrun-abc:def", &actor);
        assert!(key.starts_with("dwf-dwfrun-abc_def-"));
        assert!(
            key.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')),
            "{key}"
        );
        // Actors of the same site x ordinal in different runs must not collide into the same key.
        assert_ne!(
            key,
            actor_session_key(
                "dwfrun-other",
                &ActorRef {
                    site_id: "actor#1".into(),
                    ordinal: 2
                }
            )
        );
    }

    #[test]
    fn underscores_are_escaped_before_the_whitelist() {
        assert_eq!(sanitize_id_segment("my_actor#1"), "my__actor_1");
        assert_ne!(
            sanitize_id_segment("my_actor#1"),
            sanitize_id_segment("my#actor#1")
        );
    }
}
