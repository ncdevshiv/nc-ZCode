/**
* engine.ts reaches the upper limit of oxlint max-lines (400 lines), and the **user interface product** (`artifact.*`)
* Release/declaration/admission/settlement/resume resume is split into this file; the public side is still exported from engine.ts.
 *
* ⚠ Terminology: The following all refers to **user interface product** (the output released by the script for users to see), not RunSettlement.artifact.
* Free function to read and write engine state via the {@link EngineState} seam; the method of the same name on WorkflowEngine is just a thin delegate.
 */

import { canonicalJson, inputHash } from "./hash.js";
import { ARTIFACT_CAPS } from "../facade/artifact-caps.js";
import {
  isArtifactPresetOp,
  type ArtifactContentOp,
  type ArtifactPresetOp,
} from "../facade/registry.js";
import { validateArtifactSpec } from "./artifact-spec.js";
import { hashMismatch } from "./scheduler.js";
import type { EngineState } from "./engine-state.js";
import type {
  ArtifactPublishRequest,
  ArtifactRef,
  ArtifactVersionRecord,
  InstanceRef,
} from "./types.js";
import { refToString, WorkflowError } from "./types.js";

/**
* Publish a **content artifact** (`artifact.file` / `artifact.markdown`). Effect family: copied by driver
* store, successful redemption of {@link ArtifactRef}, failure ** catch-able rejection ** - the gating idiom for scripts is
* `try { await artifact.file(...) } catch { ...let the subagent rewrite... }`.
 *
* order:
* 1. `nextOrdinal` → journal hit? A hit is a short circuit: compared to inputHash, a successful record returns its ArtifactRef,
* **Failed records are re-rejected on a per-record basis** (resume is crash recovery, not retest: a failed release still remains after recovery)
* is that failure, and rerunning it will cause the catch branch of the script to take two different paths). The event will not be retransmitted if hit (same as report).
* 2. Missed: category ownership → upper limit of id number → upper limit of version number, all three are node-level rejections (there are promises that can be rejected).
* 3. The version number is calculated before dispatch ** and passed to the driver (it must be included in the toolCallId of the store, one byte for each version).
* 4. `putNode(running)` → driver → successful `putNode(completed)` + `artifact-published`;
* Failure `putNode(failed)` + `artifact-failed` → reject.
 *
* Both paths **do not send any node life cycle events** (node-queued / dispatched / settled): the product site is in two
* None of the graphs are nodes. A node-settled node that falls on such a site will be projected and reduced into `nodes[]` in the run panel.
* Becomes an unknown node that cannot be explained.
 */
export function publishContentArtifact(
  state: EngineState,
  siteId: string,
  op: ArtifactContentOp,
  args: unknown[],
): Promise<ArtifactRef> {
  if (state.isRunSettled()) return Promise.reject(state.runError());
  const ordinal = state.nextOrdinal(siteId);
  const instance: InstanceRef = { siteId, ordinal };

  const id = args[0];
  const payload = args[1];
// Runtime guardrails for actual parameter shapes (compile-time literal id diagnostics and facade types are the front door): non-strings that come here
// It's only possible that something is bypassing compilation. Use DriverError instead of an Artifact* code - let those codes only represent them
//Respective thing (same as probeReportItem's coding argument for "item is not JSON").
  //
// These two **do not drop the journal line and do not send events**, which are different from all the failure paths below. The reason is that they are deterministic:
// The conclusion is only determined by args. There is no driver round-trip. Resume will inevitably get the same rejection when running the same call again, so
// There is no need for a line of records to pin it down (the requirement of "settled failure" is for the driver that has been executed,
// The kind that may not fail next time). Besides, there is no id to write into `artifact_id` or event - make one up
// It will only make readers think that there is such a product.
  if (typeof id !== "string" || id === "") {
    return Promise.reject(
      new WorkflowError(
        "DriverError",
        `artifact.${op}: id must be a non-empty string (at ${refToString(instance)}).`,
      ),
    );
  }
  if (typeof payload !== "string") {
    return Promise.reject(
      new WorkflowError(
        "DriverError",
        op === "file"
          ? `artifact.file: path must be a string (at ${refToString(instance)}).`
          : `artifact.markdown: content must be a string (at ${refToString(instance)}).`,
      ),
    );
  }
// `opts.primary` is read by **engine**: driver does not know
// The opts key is always allowed, and "all runs at most one primary" are run-level facts that only the engine knows. non-boolean values ​​vs. above
// Two deterministic guardrails - no failure, no events.
  const primaryOption = (args[2] as { primary?: unknown } | undefined)?.primary;
  if (primaryOption !== undefined && typeof primaryOption !== "boolean") {
    return Promise.reject(
      new WorkflowError(
        "DriverError",
        `artifact.${op}: opts.primary must be a boolean (at ${refToString(instance)}).`,
      ),
    );
  }
  const hash = inputHash({ args, id, op });

  const recorded = state.journal.getNode(state.runId, siteId, ordinal);
  if (recorded !== undefined) {
    if (recorded.inputHash !== hash) {
      const err = hashMismatch(instance, recorded.inputHash, hash);
      state.failRun(err);
      return Promise.reject(err);
    }
    if (recorded.status === "completed") {
      const record = recorded.result as ArtifactVersionRecord;
      return Promise.resolve({ id: record.id, version: record.version });
    }
    if (recorded.status === "failed") {
      return Promise.reject(WorkflowError.fromJSON(recorded.error!));
    }
// status === "running": Crash during execution, fall down and execute live again (the copy of bytes is idempotent,
// The version number is derived from the number of completed lines, so re-running will not skip the number).
  }

// Sticky: This id is already primary, so this version is also, regardless of whether `primary` is written again this time.
  const idState = state.artifacts.get(id);
  const primary = primaryOption === true || idState?.primary === true;
  const admission = admitArtifact(state, id, op, primary);
  if (admission !== undefined) {
    return Promise.reject(settleArtifactFailure(state, instance, hash, { id, op }, admission));
  }

  const version = (idState?.versions ?? 0) + 1;
  const publish = state.driver.executeArtifactPublish;
  if (publish === undefined) {
// The assembly does not have the release capability on the driver side (pure replay / fake without connecting to the store). **Loud one naming failure**,
// It is not silently downgraded to "released an empty product" - the script is visible and the node is dropped as failed.
    return Promise.reject(
      settleArtifactFailure(
        state,
        instance,
        hash,
        { id, op },
        new WorkflowError(
          "ArtifactStoreUnavailable",
          `Cannot publish "${id}": this host has no artifact store (at ${refToString(instance)}).`,
        ),
      ),
    );
  }

  state.journal.putNode({
    runId: state.runId,
    siteId,
    ordinal,
    kind: "artifact",
    inputHash: hash,
    status: "running",
    artifactId: id,
  });

  const request: ArtifactPublishRequest = {
    runId: state.runId,
    siteId,
    ordinal,
    op,
    id,
    version,
    ...(op === "file" ? { path: payload } : { content: payload }),
    ...(args[2] === undefined ? {} : { opts: args[2] }),
  };
  return publish.call(state.driver, request).then(
    (record) => settleArtifactPublish(state, instance, hash, { id, op, version, primary }, record),
    (cause: unknown) => {
      const err =
        cause instanceof WorkflowError
          ? cause
          : new WorkflowError("DriverError", `Artifact publish failed: ${op} "${id}".`, {
              cause,
            });
      throw settleArtifactFailure(state, instance, hash, { id, op }, err);
    },
  );
}

/**
* Declare a **preset product** (`artifact.chart` / `table` / `metrics` / `board`). Statement family: sync,
* No return value, **without driver** - a statement that has nothing to wait for.
 *
* Order: `nextOrdinal` → Return on hit → spec shape check
* → Existing statement with same id? Same canonical ⇒ idempotent no-op (no new lines, no events); different ⇒ failRun
* → upper limit → `putNode(completed)` + `artifact-published`.
 *
* Three types of failures (illegal spec / same id but different spec / exceeding the upper limit) are all failRun instead of rejection: void returns no rejection
* channel, same argument as the two upper bounds of `report`.
 */
export function declarePresetArtifact(
  state: EngineState,
  siteId: string,
  op: ArtifactPresetOp,
  args: unknown[],
): void {
  if (state.isRunSettled()) return; // 与 report / log 同
  const ordinal = state.nextOrdinal(siteId);
  const instance: InstanceRef = { siteId, ordinal };

  const id = args[0];
  if (typeof id !== "string" || id === "") {
    state.failRun(
      new WorkflowError(
        "DriverError",
        `artifact.${op}: id must be a non-empty string (at ${refToString(instance)}).`,
      ),
    );
    return;
  }
  const spec = args[1];
  const hash = inputHash({ args, id, op });

  const recorded = state.journal.getNode(state.runId, siteId, ordinal);
  if (recorded !== undefined) {
    if (recorded.inputHash !== hash) {
      state.failRun(hashMismatch(instance, recorded.inputHash, hash));
      return;
    }
    return; // replay 去重：静默跳过（同 report）
  }

  const problem = validateArtifactSpec(op, spec);
  if (problem !== undefined) {
    state.failRun(
      new WorkflowError(
        "ArtifactSpecInvalid",
        `Artifact "${id}" has an invalid spec (at ${refToString(instance)}): ${problem}`,
      ),
    );
    return;
  }

  const canonical = canonicalJson(spec);
  const idState = state.artifacts.get(id);
  if (idState !== undefined) {
    if (idState.kind !== op) {
      state.failRun(
        new WorkflowError(
          "ArtifactKindMismatch",
          `Artifact id "${id}" is already a ${idState.kind} and cannot be declared as a ${op} ` +
            `(at ${refToString(instance)}). An id belongs to one artifact kind for the whole run; ` +
            `use a different id.`,
        ),
      );
      return;
    }
    if (idState.spec === canonical) return; // 幂等 no-op：不落新行、不发事件
    state.failRun(
      new WorkflowError(
        "ArtifactRedeclared",
        `Artifact "${id}" was already declared with a different spec (at ${refToString(instance)}). ` +
          `The spec for one id must stay identical (an identical re-declaration is a no-op); ` +
          `declare it once at the top of the script.`,
      ),
    );
    return;
  }
// `primary` enters the canonical spec, so the flag with the same id is changed to ArtifactRedeclared (rejected above); here only
// **New** id wants to be primary and other ids are already - the preset family does not reject the channel, failRun.
  const wantsPrimary = (spec as { primary?: unknown }).primary === true;
  const holder = wantsPrimary ? primaryArtifactId(state) : undefined;
  if (holder !== undefined && holder !== id) {
    state.failRun(
      new WorkflowError(
        "ArtifactPrimaryConflict",
        primaryConflictMessage(id, holder, `declare "${id}" without primary`, instance),
      ),
    );
    return;
  }
  if (state.artifacts.size >= ARTIFACT_CAPS.maxArtifactsPerRun) {
    state.failRun(
      new WorkflowError(
        "ArtifactCapExceeded",
        `Cannot declare "${id}": this run already has ${ARTIFACT_CAPS.maxArtifactsPerRun} artifact ` +
          `ids, the maximum. Reuse an existing id or publish fewer artifacts.`,
      ),
    );
    return;
  }

// The statement is **write-once** (same as report): there are no driver calls between admission and settlement, nothing can fail in the middle.
  const record = presetRecord(id, op, spec);
  state.artifacts.set(id, {
    kind: op,
    spec: canonical,
    versions: 1,
    ...(wantsPrimary ? { primary: true as const } : {}),
  });
  state.journal.putNode({
    runId: state.runId,
    siteId,
    ordinal,
    kind: "artifact",
    inputHash: hash,
    status: "completed",
    result: record,
    artifactId: id,
  });
  state.record({ type: "artifact-published", instance, artifact: record });
}

/**
* Access to content products: category attribution → upper limit of number of ids → upper limit of number of versions. Return the rejection reason, `undefined` means release.
 *
* All three items are **node-level** rejections (content members have promises that can be rejected), in contrast to the three items with the same name in the preset family——
* That family can only failRun. Same fact, two channels, difference in return type not severity.
 */
function admitArtifact(
  state: EngineState,
  id: string,
  op: ArtifactContentOp,
  primary: boolean,
): WorkflowError | undefined {
  const idState = state.artifacts.get(id);
  if (idState !== undefined && idState.kind !== op) {
    return new WorkflowError(
      "ArtifactKindMismatch",
      `Artifact id "${id}" is already a ${idState.kind} and cannot be published as a ${op}. An id ` +
        `belongs to one artifact kind for the whole run (publishing the same id again creates a ` +
        `new version, and a version cannot change what it is); use a different id.`,
    );
  }
// Primary conflicts are ranked before the upper limit: an id that wants to be a deliverable is rejected. The reason should be "there is already a deliverable", not "full".
  const holder = primaryConflict(state, id, primary);
  if (holder !== undefined) {
    return new WorkflowError(
      "ArtifactPrimaryConflict",
      primaryConflictMessage(id, holder, `publish "${id}" without primary`, undefined),
    );
  }
  if (idState === undefined && state.artifacts.size >= ARTIFACT_CAPS.maxArtifactsPerRun) {
    return new WorkflowError(
      "ArtifactCapExceeded",
      `Cannot publish "${id}": this run already has ${ARTIFACT_CAPS.maxArtifactsPerRun} artifact ` +
        `ids, the maximum. Publish a new version of an existing id instead.`,
    );
  }
  if (idState !== undefined && idState.versions >= ARTIFACT_CAPS.maxVersionsPerArtifact) {
    return new WorkflowError(
      "ArtifactVersionCapExceeded",
      `Artifact "${id}" already has ${idState.versions} versions, the maximum of ` +
        `${ARTIFACT_CAPS.maxVersionsPerArtifact}. Publish the final content in fewer versions.`,
    );
  }
  return undefined;
}

/**
* The content product is published successfully: enter the completed line + accounting + send `artifact-published`, and return the ref required by the script.
 *
* The identity (id / type / version number) will always be the one calculated by the engine itself (`issued`), not the one returned by the driver
* The field with the same name in the record: the version number is derived and issued by the engine according to the journal, and it is allowed to go around the driver and read it back.
* Just one more place to be inconsistent. The driver's record is responsible for what it really owns - uri/bytes/
* contentType / sourcePath - those that are left intact in the journal.
 */
function settleArtifactPublish(
  state: EngineState,
  instance: InstanceRef,
  hash: string,
  issued: { id: string; op: ArtifactContentOp; version: number; primary: boolean },
  record: ArtifactVersionRecord,
): ArtifactRef {
  if (state.isRunSettled()) throw state.runError();
// There is a driver round trip between admission and settlement: two concurrent primary releases can pass admission. Check again during checkout.
// The losing one takes the failure path - the failed line does not claim the flag, so there will never be two completed primary in the journal.
  const holder = primaryConflict(state, issued.id, issued.primary);
  if (holder !== undefined) {
    throw settleArtifactFailure(
      state,
      instance,
      hash,
      issued,
      new WorkflowError(
        "ArtifactPrimaryConflict",
        primaryConflictMessage(issued.id, holder, `publish "${issued.id}" without primary`, instance),
      ),
    );
  }
// Flags are stamped by the engine: the driver's record is only responsible for what it really owns (uri/bytes/contentType/sourcePath).
  const stored: ArtifactVersionRecord = issued.primary ? { ...record, primary: true } : record;
  state.journal.putNode({
    runId: state.runId,
    siteId: instance.siteId,
    ordinal: instance.ordinal,
    kind: "artifact",
    inputHash: hash,
    status: "completed",
    result: stored,
    artifactId: issued.id,
  });
  state.artifacts.set(issued.id, {
    kind: issued.op,
    versions: issued.version,
    ...(issued.primary ? { primary: true as const } : {}),
  });
  state.record({ type: "artifact-published", instance, artifact: stored });
  return { id: issued.id, version: issued.version };
}

/** The current primary id of this run (at most one); if not, it is undefined. Derived from `state.artifacts`, naturally consistent after resume. */
function primaryArtifactId(state: EngineState): string | undefined {
  for (const [id, idState] of state.artifacts) if (idState.primary) return id;
  return undefined;
}

/** `id` wants to be primary and **other** id is already ⇒ return that id; otherwise undefined (does not want to be / is itself). */
function primaryConflict(state: EngineState, id: string, primary: boolean): string | undefined {
  if (!primary) return undefined;
  const holder = primaryArtifactId(state);
  return holder === undefined || holder === id ? undefined : holder;
}

function primaryConflictMessage(
  id: string,
  holder: string,
  fix: string,
  instance: InstanceRef | undefined,
): string {
  const where = instance === undefined ? "" : ` (at ${refToString(instance)})`;
  return (
    `Cannot mark "${id}" as primary${where}: "${holder}" is already this run's primary artifact. ` +
    `A run has one deliverable; ${fix}, or publish it as a new version of "${holder}".`
  );
}

/**
* Content product publishing failed: drop failed line + send `artifact-failed`, return the error to be thrown.
 *
* Dropping the library is an existing requirement for replay health (settled failure must be in the journal), otherwise resume will
* Rerun the failure that has been caught by the script - that is what "resume is crash recovery, not retest" to prevent.
 *
* reader for node-settled
* It will be reduced into `nodes[]` and find the corresponding node in the graph according to the site id. The product site will not enter any graph - it will be in run
* The panel becomes an unknown node that cannot be explained. `artifact-failed` takes everything needed to render a failed card
* (id/kind/structural error), not pretending to be a one-step job. The successful side only sends `artifact-published` in the same way.
 */
function settleArtifactFailure(
  state: EngineState,
  instance: InstanceRef,
  hash: string,
  issued: { id: string; op: ArtifactContentOp },
  error: WorkflowError,
): WorkflowError {
  const json = error.toJSON();
  state.journal.putNode({
    runId: state.runId,
    siteId: instance.siteId,
    ordinal: instance.ordinal,
    kind: "artifact",
    inputHash: hash,
    status: "failed",
    error: json,
    artifactId: issued.id,
  });
  state.record({
    type: "artifact-failed",
    instance,
    id: issued.id,
    op: issued.op,
    error: json,
  });
  return error;
}

/**
* Resume: Merge the **completed** product record of a line in the journal into the memory state.
 *
* **Only completed rows can claim the id**: Failed releases do not occupy the id, category, or version. This is not just a simple picture -
* The `result` of the failed line is empty, and the type cannot be restored. If the live side is allowed to claim it but the resume side cannot be claimed,
* The same script will get two sets of upper limit accounts before and after the crash. There is also compile-time diagnostic monitoring for type conflicts, and this is run-time
* (`ArtifactKindMismatch`) controls the IDs that have been successfully published.
 */
export function rememberArtifactRow(
  state: EngineState,
  id: string | undefined,
  result: unknown,
): void {
  if (id === undefined || id === "") return;
  const record = (result ?? undefined) as ArtifactVersionRecord | undefined;
  if (record?.kind === undefined) return;
  const existing = state.artifacts.get(id);
  const spec = record.spec === undefined ? existing?.spec : canonicalJson(record.spec);
  const primary = record.primary === true || existing?.primary === true;
  state.artifacts.set(id, {
    kind: record.kind,
    ...(spec === undefined ? {} : { spec }),
    versions: (existing?.versions ?? 0) + 1,
    ...(primary ? { primary: true as const } : {}),
  });
}

/** Whether this id has been declared as a **preset** product (a legal target of the `report` tag). */
export function isDeclaredPreset(state: EngineState, id: string): boolean {
  const idState = state.artifacts.get(id);
  return idState !== undefined && isArtifactPresetOp(idState.kind);
}

/** Declared preset id (lexicographic order) - only used to write `ArtifactUndeclared` messages to do so. */
export function declaredPresetIds(state: EngineState): string[] {
  return [...state.artifacts.entries()]
    .filter(([, idState]) => isArtifactPresetOp(idState.kind))
    .map(([id]) => id)
    .sort();
}

/**
* Preset declared inventory records. `title` / `description` **raised** from spec: reader of card (run side panel,
* Notification, hub) only gets this record, and should not resolve the spec in order to display a title; the spec is still retained as is, and the renderer needs to
*Full shape. Version is always 1 - declares that there is no version, it only exists once.
 */
function presetRecord(id: string, op: ArtifactPresetOp, spec: unknown): ArtifactVersionRecord {
  const options = (spec ?? {}) as { title?: unknown; description?: unknown; primary?: unknown };
  return {
    id,
    kind: op,
    version: 1,
    ...(typeof options.title === "string" ? { title: options.title } : {}),
    ...(typeof options.description === "string" ? { description: options.description } : {}),
    spec,
    ...(options.primary === true ? { primary: true as const } : {}),
  };
}
