//! Publishing / declaring / settling / resume restoration of user-facing artifacts (`artifact.*`) (port of
//! `engine-artifacts.ts` + `artifact-spec.ts`).
//!
//! ⚠ Terminology: this always means user-facing artifacts, never `RunSettlement.artifact` (the script's top-level return value).

use serde_json::Value;

use crate::engine_state::{ArtifactIdState, Core};
use crate::types::{
    canonical_json, input_hash, ArtifactOp, ArtifactPublishRequest, ArtifactRef,
    ArtifactVersionRecord, InstanceRef, NodeKind, NodeRecord, NodeRecordStatus, RunEvent,
    WorkflowError, WorkflowErrorCode, ARTIFACT_CAPS,
};

/// Admission result for a content artifact.
#[allow(clippy::large_enum_variant)]
pub(crate) enum ArtifactAdmission {
    /// Synchronous rejection (a deterministic guard or replay); the script receives it directly.
    Immediate(Result<ArtifactRef, WorkflowError>),
    /// Proceed to live publishing.
    Live {
        instance: InstanceRef,
        id: String,
        op: ArtifactOp,
        version: u32,
        primary: bool,
        hash: String,
        request: ArtifactPublishRequest,
    },
}

impl Core {
    /// Admission for publishing a content artifact (`artifact.file` / `artifact.markdown`) (synchronous, inside the lock).
    pub(crate) fn publish_artifact_admit(
        &mut self,
        site_id: &str,
        op: ArtifactOp,
        args: &[Value],
    ) -> ArtifactAdmission {
        if self.run_settled {
            return ArtifactAdmission::Immediate(Err(self.run_error()));
        }
        let ordinal = self.next_ordinal(site_id);
        let instance = InstanceRef {
            site_id: site_id.to_string(),
            ordinal,
        };

        let id = args
            .first()
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let payload = args.get(1).and_then(Value::as_str).map(str::to_string);
        // Argument-shape guard: a deterministic failure that writes no journal row and emits no event.
        if id.is_empty() {
            return ArtifactAdmission::Immediate(Err(WorkflowError::new(
                WorkflowErrorCode::DriverError,
                format!(
                    "artifact.{}: id must be a non-empty string (at {}).",
                    op.as_str(),
                    instance.key()
                ),
            )));
        }
        let Some(payload) = payload else {
            return ArtifactAdmission::Immediate(Err(WorkflowError::new(
                WorkflowErrorCode::DriverError,
                if op == ArtifactOp::File {
                    format!(
                        "artifact.file: path must be a string (at {}).",
                        instance.key()
                    )
                } else {
                    format!(
                        "artifact.markdown: content must be a string (at {}).",
                        instance.key()
                    )
                },
            )));
        };
        let primary_option = args
            .get(2)
            .and_then(|opts| opts.get("primary"))
            .and_then(Value::as_bool);
        if let Some(opts) = args.get(2) {
            if opts.get("primary").is_some_and(|v| !v.is_boolean()) {
                return ArtifactAdmission::Immediate(Err(WorkflowError::new(
                    WorkflowErrorCode::DriverError,
                    format!(
                        "artifact.{}: opts.primary must be a boolean (at {}).",
                        op.as_str(),
                        instance.key()
                    ),
                )));
            }
        }
        let hash = input_hash(&serde_json::json!({"args": args, "id": id, "op": op.as_str()}));

        if let Some(recorded) = self.journal().get_node(&self.run_id, site_id, ordinal) {
            if recorded.input_hash != hash {
                let err = crate::scheduler::hash_mismatch(&instance, &recorded.input_hash, &hash);
                self.fail_run(err.clone());
                return ArtifactAdmission::Immediate(Err(err));
            }
            if recorded.status == NodeRecordStatus::Completed {
                let record: Option<ArtifactVersionRecord> =
                    recorded.result.and_then(|v| serde_json::from_value(v).ok());
                return ArtifactAdmission::Immediate(match record {
                    Some(record) => Ok(ArtifactRef {
                        id: record.id,
                        version: record.version,
                    }),
                    None => Err(WorkflowError::new(
                        WorkflowErrorCode::DriverError,
                        "Recorded artifact row is missing its version record",
                    )),
                });
            }
            if recorded.status == NodeRecordStatus::Failed {
                let err = recorded
                    .error
                    .as_ref()
                    .map(WorkflowError::from_json)
                    .unwrap_or_else(|| {
                        WorkflowError::new(WorkflowErrorCode::DriverError, "Recorded failure")
                    });
                return ArtifactAdmission::Immediate(Err(err));
            }
            // running: crashed mid-execution, go live again (the version number is derived from the count of already completed rows, so no number is skipped).
        }

        // Sticky: if this id is already primary, then this version is too.
        let id_state = self.artifacts.get(&id).cloned();
        let primary = primary_option == Some(true) || id_state.as_ref().is_some_and(|s| s.primary);
        if let Some(error) = self.admit_artifact(&id, op, primary) {
            let error = self.settle_artifact_failure(&instance, &hash, &id, op, error);
            return ArtifactAdmission::Immediate(Err(error));
        }
        let version = id_state.as_ref().map(|s| s.versions).unwrap_or(0) + 1;

        self.journal().put_node(NodeRecord {
            run_id: self.run_id.clone(),
            site_id: site_id.to_string(),
            ordinal,
            kind: NodeKind::Artifact,
            actor_site_id: None,
            actor_ordinal: None,
            actor_seq: None,
            input_hash: hash.clone(),
            status: NodeRecordStatus::Running,
            result: None,
            error: None,
            stats: None,
            artifact_id: Some(id.clone()),
            input: None,
            message_boundary: None,
        });

        let request = ArtifactPublishRequest {
            run_id: self.run_id.clone(),
            site_id: site_id.to_string(),
            ordinal,
            op,
            id: id.clone(),
            version,
            path: (op == ArtifactOp::File).then(|| payload.clone()),
            content: (op == ArtifactOp::Markdown).then(|| payload.clone()),
            opts: args.get(2).cloned(),
        };
        ArtifactAdmission::Live {
            instance,
            id,
            op,
            version,
            primary,
            hash,
            request,
        }
    }

    /// Settlement of a content artifact publish (synchronous, inside the lock).
    pub(crate) fn publish_artifact_settle(
        &mut self,
        admission: ArtifactLiveParts,
        record: Result<ArtifactVersionRecord, WorkflowError>,
    ) -> Result<ArtifactRef, WorkflowError> {
        let ArtifactLiveParts {
            instance,
            id,
            op,
            version,
            primary,
            hash,
        } = admission;
        if self.run_settled {
            return Err(self.run_error());
        }
        match record {
            Ok(record) => {
                // Check primary once more at settlement (two concurrent publishes can both pass admission; at settlement the loser takes the failure path).
                if let Some(holder) = self.primary_conflict(&id, primary) {
                    let error = WorkflowError::new(
                        WorkflowErrorCode::ArtifactPrimaryConflict,
                        primary_conflict_message(&id, &holder, &instance),
                    );
                    return Err(self.settle_artifact_failure(&instance, &hash, &id, op, error));
                }
                // The flag is stamped by the engine; the identity (id/kind/version) comes from the engine's own computation.
                let mut stored = record;
                stored.id = id.clone();
                stored.kind = op;
                stored.version = version;
                if primary {
                    stored.primary = Some(true);
                }
                self.journal().put_node(NodeRecord {
                    run_id: self.run_id.clone(),
                    site_id: instance.site_id.clone(),
                    ordinal: instance.ordinal,
                    kind: NodeKind::Artifact,
                    actor_site_id: None,
                    actor_ordinal: None,
                    actor_seq: None,
                    input_hash: hash,
                    status: NodeRecordStatus::Completed,
                    result: Some(serde_json::to_value(&stored).unwrap_or(Value::Null)),
                    error: None,
                    stats: None,
                    artifact_id: Some(id.clone()),
                    input: None,
                    message_boundary: None,
                });
                self.artifacts.insert(
                    id.clone(),
                    ArtifactIdState {
                        kind: op,
                        versions: version,
                        spec: None,
                        primary,
                    },
                );
                self.record(RunEvent::ArtifactPublished {
                    instance,
                    artifact: stored,
                });
                Ok(ArtifactRef { id, version })
            }
            Err(error) => Err(self.settle_artifact_failure(&instance, &hash, &id, op, error)),
        }
    }

    /// Declare a preset artifact (`artifact.chart` / `table` / `metrics` / `board`). Synchronous, no driver round-trip.
    pub fn declare_preset_artifact(&mut self, site_id: &str, op: ArtifactOp, args: &[Value]) {
        if self.run_settled {
            return;
        }
        let ordinal = self.next_ordinal(site_id);
        let instance = InstanceRef {
            site_id: site_id.to_string(),
            ordinal,
        };

        let id = args
            .first()
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if id.is_empty() {
            self.fail_run(WorkflowError::new(
                WorkflowErrorCode::DriverError,
                format!(
                    "artifact.{}: id must be a non-empty string (at {}).",
                    op.as_str(),
                    instance.key()
                ),
            ));
            return;
        }
        let spec = args.get(1).cloned().unwrap_or(Value::Null);
        let hash = input_hash(&serde_json::json!({"args": args, "id": id, "op": op.as_str()}));

        if let Some(recorded) = self.journal().get_node(&self.run_id, site_id, ordinal) {
            if recorded.input_hash != hash {
                let err = crate::scheduler::hash_mismatch(&instance, &recorded.input_hash, &hash);
                self.fail_run(err);
            }
            return; // replay dedup
        }

        if let Some(problem) = validate_artifact_spec(op, &spec) {
            self.fail_run(WorkflowError::new(
                WorkflowErrorCode::ArtifactSpecInvalid,
                format!(
                    "Artifact \"{id}\" has an invalid spec (at {}): {problem}",
                    instance.key()
                ),
            ));
            return;
        }

        let canonical = canonical_json(&spec);
        if let Some(existing) = self.artifacts.get(&id) {
            if existing.kind != op {
                self.fail_run(WorkflowError::new(
                    WorkflowErrorCode::ArtifactKindMismatch,
                    format!(
                        "Artifact id \"{id}\" is already a {} and cannot be declared as a {} (at {}). \
                         An id belongs to one artifact kind for the whole run; use a different id.",
                        existing.kind.as_str(),
                        op.as_str(),
                        instance.key()
                    ),
                ));
                return;
            }
            if existing.spec.as_deref() == Some(canonical.as_str()) {
                return; // idempotent no-op
            }
            self.fail_run(WorkflowError::new(
                WorkflowErrorCode::ArtifactRedeclared,
                format!(
                    "Artifact \"{id}\" was already declared with a different spec (at {}). The spec for \
                     one id must stay identical (an identical re-declaration is a no-op); declare it \
                     once at the top of the script.",
                    instance.key()
                ),
            ));
            return;
        }
        let wants_primary = spec.get("primary").and_then(Value::as_bool) == Some(true);
        if wants_primary {
            if let Some(holder) = self.primary_artifact_id() {
                if holder != id {
                    self.fail_run(WorkflowError::new(
                        WorkflowErrorCode::ArtifactPrimaryConflict,
                        primary_conflict_message(&id, &holder, &instance),
                    ));
                    return;
                }
            }
        }
        if self.artifacts.len() >= ARTIFACT_CAPS.max_artifacts_per_run {
            self.fail_run(WorkflowError::new(
                WorkflowErrorCode::ArtifactCapExceeded,
                format!(
                    "Cannot declare \"{id}\": this run already has {} artifact ids, the maximum. \
                     Reuse an existing id or publish fewer artifacts.",
                    ARTIFACT_CAPS.max_artifacts_per_run
                ),
            ));
            return;
        }

        // A declaration is one write: there is no driver call between admission and settlement.
        let record = preset_record(&id, op, &spec);
        self.artifacts.insert(
            id.clone(),
            ArtifactIdState {
                kind: op,
                versions: 1,
                spec: Some(canonical),
                primary: wants_primary,
            },
        );
        self.journal().put_node(NodeRecord {
            run_id: self.run_id.clone(),
            site_id: site_id.to_string(),
            ordinal,
            kind: NodeKind::Artifact,
            actor_site_id: None,
            actor_ordinal: None,
            actor_seq: None,
            input_hash: hash,
            status: NodeRecordStatus::Completed,
            result: Some(serde_json::to_value(&record).unwrap_or(Value::Null)),
            error: None,
            stats: None,
            artifact_id: Some(id),
            input: None,
            message_boundary: None,
        });
        self.record(RunEvent::ArtifactPublished {
            instance,
            artifact: record,
        });
    }

    /// Resume restoration: fold a completed artifact record row from the journal into the in-memory state.
    pub fn remember_artifact_row(&mut self, id: Option<&str>, result: &Value) {
        let Some(id) = id.filter(|id| !id.is_empty()) else {
            return;
        };
        let Ok(record) = serde_json::from_value::<ArtifactVersionRecord>(result.clone()) else {
            return;
        };
        let existing = self.artifacts.get(id).cloned();
        let spec = match &record.spec {
            Some(spec) => Some(canonical_json(spec)),
            None => existing.as_ref().and_then(|s| s.spec.clone()),
        };
        let primary = record.primary == Some(true) || existing.as_ref().is_some_and(|s| s.primary);
        self.artifacts.insert(
            id.to_string(),
            ArtifactIdState {
                kind: record.kind,
                versions: existing.as_ref().map(|s| s.versions).unwrap_or(0) + 1,
                spec,
                primary,
            },
        );
    }

    pub(crate) fn is_declared_preset(&self, id: &str) -> bool {
        self.artifacts
            .get(id)
            .map(|s| s.kind.is_preset())
            .unwrap_or(false)
    }

    pub(crate) fn declared_preset_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .artifacts
            .iter()
            .filter(|(_, state)| state.kind.is_preset())
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        ids
    }

    fn admit_artifact(&self, id: &str, op: ArtifactOp, primary: bool) -> Option<WorkflowError> {
        if let Some(existing) = self.artifacts.get(id) {
            if existing.kind != op {
                return Some(WorkflowError::new(
                    WorkflowErrorCode::ArtifactKindMismatch,
                    format!(
                        "Artifact id \"{id}\" is already a {} and cannot be published as a {}. An id \
                         belongs to one artifact kind for the whole run (publishing the same id again \
                         creates a new version, and a version cannot change what it is); use a \
                         different id.",
                        existing.kind.as_str(),
                        op.as_str()
                    ),
                ));
            }
        }
        if let Some(holder) = self.primary_conflict(id, primary) {
            return Some(WorkflowError::new(
                WorkflowErrorCode::ArtifactPrimaryConflict,
                format!(
                    "Cannot mark \"{id}\" as primary: \"{holder}\" is already this run's primary \
                     artifact. A run has one deliverable; publish \"{id}\" without primary, or publish \
                     it as a new version of \"{holder}\"."
                ),
            ));
        }
        let existing = self.artifacts.get(id);
        if existing.is_none() && self.artifacts.len() >= ARTIFACT_CAPS.max_artifacts_per_run {
            return Some(WorkflowError::new(
                WorkflowErrorCode::ArtifactCapExceeded,
                format!(
                    "Cannot publish \"{id}\": this run already has {} artifact ids, the maximum. \
                     Publish a new version of an existing id instead.",
                    ARTIFACT_CAPS.max_artifacts_per_run
                ),
            ));
        }
        if let Some(state) = existing {
            if state.versions >= ARTIFACT_CAPS.max_versions_per_artifact {
                return Some(WorkflowError::new(
                    WorkflowErrorCode::ArtifactVersionCapExceeded,
                    format!(
                        "Artifact \"{id}\" already has {} versions, the maximum of {}. Publish the final \
                         content in fewer versions.",
                        state.versions, ARTIFACT_CAPS.max_versions_per_artifact
                    ),
                ));
            }
        }
        None
    }

    fn settle_artifact_failure(
        &mut self,
        instance: &InstanceRef,
        hash: &str,
        id: &str,
        op: ArtifactOp,
        error: WorkflowError,
    ) -> WorkflowError {
        let json = error.to_json();
        self.journal().put_node(NodeRecord {
            run_id: self.run_id.clone(),
            site_id: instance.site_id.clone(),
            ordinal: instance.ordinal,
            kind: NodeKind::Artifact,
            actor_site_id: None,
            actor_ordinal: None,
            actor_seq: None,
            input_hash: hash.to_string(),
            status: NodeRecordStatus::Failed,
            result: None,
            error: Some(json.clone()),
            stats: None,
            artifact_id: Some(id.to_string()),
            input: None,
            message_boundary: None,
        });
        self.record(RunEvent::ArtifactFailed {
            instance: instance.clone(),
            id: id.to_string(),
            op,
            error: json,
        });
        error
    }

    fn primary_artifact_id(&self) -> Option<String> {
        self.artifacts
            .iter()
            .find(|(_, s)| s.primary)
            .map(|(id, _)| id.clone())
    }

    fn primary_conflict(&self, id: &str, primary: bool) -> Option<String> {
        if !primary {
            return None;
        }
        match self.primary_artifact_id() {
            Some(holder) if holder != id => Some(holder),
            _ => None,
        }
    }
}

/// The identity data a live publish needs (held outside the lock).
pub(crate) struct ArtifactLiveParts {
    pub instance: InstanceRef,
    pub id: String,
    pub op: ArtifactOp,
    pub version: u32,
    pub primary: bool,
    pub hash: String,
}

fn primary_conflict_message(id: &str, holder: &str, instance: &InstanceRef) -> String {
    format!(
        "Cannot mark \"{id}\" as primary (at {}): \"{holder}\" is already this run's primary artifact. \
         A run has one deliverable; publish \"{id}\" without primary, or publish it as a new version of \
         \"{holder}\".",
        instance.key()
    )
}

/// The persisted record for a preset declaration (title/description are lifted out of the spec; the version is always 1).
fn preset_record(id: &str, op: ArtifactOp, spec: &Value) -> ArtifactVersionRecord {
    ArtifactVersionRecord {
        id: id.to_string(),
        kind: op,
        version: 1,
        title: spec
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_string),
        description: spec
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        content_type: None,
        bytes: None,
        uri: None,
        source_path: None,
        spec: Some(spec.clone()),
        published_at: None,
        primary: (spec.get("primary").and_then(Value::as_bool) == Some(true)).then_some(true),
    }
}

// ——————————————————————— preset spec shape validation (artifact-spec.ts) ———————————————————————

/// The reason an entry is invalid; None means it passes.
pub(crate) fn validate_artifact_spec(op: ArtifactOp, spec: &Value) -> Option<String> {
    let Value::Object(map) = spec else {
        return Some(format!(
            "{} spec must be an object literal, got {}",
            op.as_str(),
            describe(spec)
        ));
    };
    if let Some(problem) = check_options(map) {
        return Some(problem);
    }
    if let Some(problem) = check_shape(op, map) {
        return Some(problem);
    }
    let bytes = canonical_json(spec).len();
    if bytes > ARTIFACT_CAPS.max_spec_serialized_bytes {
        return Some(format!(
            "{} spec is {bytes} bytes serialized, over the {}-byte limit",
            op.as_str(),
            ARTIFACT_CAPS.max_spec_serialized_bytes
        ));
    }
    None
}

fn check_shape(op: ArtifactOp, spec: &serde_json::Map<String, Value>) -> Option<String> {
    match op {
        ArtifactOp::Chart => {
            if let Some(problem) = check_field(spec.get("x").unwrap_or(&Value::Null), "x") {
                return Some(problem);
            }
            match spec.get("y") {
                Some(Value::Array(entries)) => {
                    if entries.is_empty() {
                        return Some(
                            "chart spec y is an empty array; give it at least one series".into(),
                        );
                    }
                    for (index, entry) in entries.iter().enumerate() {
                        if let Some(problem) = check_field(entry, &format!("y[{index}]")) {
                            return Some(problem);
                        }
                    }
                }
                Some(other) => {
                    if let Some(problem) = check_field(other, "y") {
                        return Some(problem);
                    }
                }
                None => {
                    if let Some(problem) = check_field(&Value::Null, "y") {
                        return Some(problem);
                    }
                }
            }
            if let Some(kind) = spec.get("type") {
                if !matches!(kind.as_str(), Some("line" | "bar" | "scatter")) {
                    return Some(format!(
                        "chart spec type must be \"line\", \"bar\" or \"scatter\", got {}",
                        describe(kind)
                    ));
                }
            }
            if let Some(scale) = spec.get("scale") {
                if !matches!(scale.as_str(), Some("linear" | "log")) {
                    return Some(format!(
                        "chart spec scale must be \"linear\" or \"log\", got {}",
                        describe(scale)
                    ));
                }
            }
            if let Some(baseline) = spec.get("baseline") {
                return check_field(baseline, "baseline");
            }
            None
        }
        ArtifactOp::Table => {
            if let Some(problem) =
                check_field_list(spec.get("columns").unwrap_or(&Value::Null), "columns")
            {
                return Some(problem);
            }
            if let Some(key) = spec.get("key") {
                if !is_non_empty_string(key) {
                    return Some("table spec key must be a non-empty string (the field that identifies a row)".into());
                }
            }
            None
        }
        ArtifactOp::Metrics => {
            check_field_list(spec.get("metrics").unwrap_or(&Value::Null), "metrics")
        }
        ArtifactOp::Board => {
            if !is_non_empty_string(spec.get("key").unwrap_or(&Value::Null)) {
                return Some(
                    "board spec key must be a non-empty string (the field that identifies a card)"
                        .into(),
                );
            }
            if !is_non_empty_string(spec.get("status").unwrap_or(&Value::Null)) {
                return Some("board spec status must be a non-empty string (the field that picks a card's column)".into());
            }
            match spec.get("columns") {
                Some(Value::Array(columns)) if !columns.is_empty() => {
                    for (index, column) in columns.iter().enumerate() {
                        if !is_non_empty_string(column) {
                            return Some(format!(
                                "board spec columns[{index}] must be a non-empty string"
                            ));
                        }
                    }
                }
                _ => {
                    return Some("board spec columns must be a non-empty array of strings (the column order)".into());
                }
            }
            if let Some(card_title) = spec.get("cardTitle") {
                if !is_non_empty_string(card_title) {
                    return Some("board spec cardTitle must be a non-empty string (the field used as the card title)".into());
                }
            }
            if let Some(detail) = spec.get("detail") {
                return check_field_list(detail, "detail");
            }
            None
        }
        // The content members have no preset shape.
        ArtifactOp::File | ArtifactOp::Markdown => None,
    }
}

fn check_options(spec: &serde_json::Map<String, Value>) -> Option<String> {
    if let Some(title) = spec.get("title") {
        let Some(title) = title.as_str() else {
            return Some(format!(
                "spec title must be a string, got {}",
                describe(title)
            ));
        };
        if title.chars().count() > ARTIFACT_CAPS.max_title_length {
            return Some(format!(
                "spec title is {} characters, over the {} limit",
                title.chars().count(),
                ARTIFACT_CAPS.max_title_length
            ));
        }
    }
    if let Some(description) = spec.get("description") {
        let Some(description) = description.as_str() else {
            return Some(format!(
                "spec description must be a string, got {}",
                describe(description)
            ));
        };
        if description.chars().count() > ARTIFACT_CAPS.max_description_length {
            return Some(format!(
                "spec description is {} characters, over the {} limit",
                description.chars().count(),
                ARTIFACT_CAPS.max_description_length
            ));
        }
    }
    if let Some(primary) = spec.get("primary") {
        if !primary.is_boolean() {
            return Some(format!(
                "spec primary must be a boolean, got {}",
                describe(primary)
            ));
        }
    }
    None
}

fn check_field_list(value: &Value, where_: &str) -> Option<String> {
    let Value::Array(entries) = value else {
        return Some(format!(
            "spec {where_} must be a non-empty array of fields ({{ field: \"…\" }})"
        ));
    };
    if entries.is_empty() {
        return Some(format!(
            "spec {where_} must be a non-empty array of fields ({{ field: \"…\" }})"
        ));
    }
    for (index, entry) in entries.iter().enumerate() {
        if let Some(problem) = check_field(entry, &format!("{where_}[{index}]")) {
            return Some(problem);
        }
    }
    None
}

fn check_field(value: &Value, where_: &str) -> Option<String> {
    let Value::Object(map) = value else {
        return Some(format!(
            "spec {where_} must be {{ field: \"…\" }}, got {}",
            describe(value)
        ));
    };
    if !is_non_empty_string(map.get("field").unwrap_or(&Value::Null)) {
        return Some(format!(
            "spec {where_}.field must be a non-empty string (a dot path into the item, e.g. \"timing.after\")"
        ));
    }
    for key in ["label", "unit"] {
        if let Some(inner) = map.get(key) {
            if !inner.is_string() {
                return Some(format!(
                    "spec {where_}.{key} must be a string, got {}",
                    describe(inner)
                ));
            }
        }
    }
    None
}

fn is_non_empty_string(value: &Value) -> bool {
    value
        .as_str()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

fn describe(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Array(items) => format!("array ({} items)", items.len()),
        Value::Object(_) => "object".into(),
        Value::String(s) => serde_json::to_string(&s.chars().take(40).collect::<String>())
            .unwrap_or_else(|_| "\"\"".into()),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chart_spec_requires_fields() {
        assert!(validate_artifact_spec(
            ArtifactOp::Chart,
            &json!({"x": {"field": "t"}, "y": {"field": "v"}})
        )
        .is_none());
        assert!(validate_artifact_spec(ArtifactOp::Chart, &json!({"y": {"field": "v"}})).is_some());
        assert!(
            validate_artifact_spec(ArtifactOp::Chart, &json!({"x": {"field": "t"}, "y": []}))
                .is_some()
        );
        assert!(validate_artifact_spec(
            ArtifactOp::Chart,
            &json!({"x": {"field": "t"}, "y": {"field": "v"}, "type": "pie"})
        )
        .is_some());
    }

    #[test]
    fn board_and_table_shapes() {
        assert!(validate_artifact_spec(
            ArtifactOp::Board,
            &json!({"key": "id", "status": "state", "columns": ["todo", "done"]})
        )
        .is_none());
        assert!(validate_artifact_spec(
            ArtifactOp::Board,
            &json!({"key": "id", "status": "state", "columns": []})
        )
        .is_some());
        assert!(
            validate_artifact_spec(ArtifactOp::Table, &json!({"columns": [{"field": "a"}]}))
                .is_none()
        );
        assert!(validate_artifact_spec(ArtifactOp::Table, &json!({"columns": []})).is_some());
        assert!(validate_artifact_spec(
            ArtifactOp::Metrics,
            &json!({"metrics": [{"field": "m", "unit": "ms"}]})
        )
        .is_none());
    }

    #[test]
    fn options_are_bounded() {
        let long = "x".repeat(ARTIFACT_CAPS.max_title_length + 1);
        assert!(validate_artifact_spec(
            ArtifactOp::Metrics,
            &json!({"metrics": [{"field": "m"}], "title": long})
        )
        .is_some());
        assert!(validate_artifact_spec(
            ArtifactOp::Metrics,
            &json!({"metrics": [{"field": "m"}], "primary": "yes"})
        )
        .is_some());
        assert!(validate_artifact_spec(
            ArtifactOp::Metrics,
            &json!({"metrics": [{"field": "m"}], "primary": true})
        )
        .is_none());
    }

    #[test]
    fn oversized_spec_is_rejected() {
        let big = "x".repeat(ARTIFACT_CAPS.max_spec_serialized_bytes);
        assert!(
            validate_artifact_spec(ArtifactOp::Metrics, &json!({"metrics": [{"field": big}]}))
                .is_some()
        );
    }
}
