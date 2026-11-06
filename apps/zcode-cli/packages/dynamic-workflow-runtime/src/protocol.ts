/**
* NDJSON wire protocol (transfer encoding for Boundary A).
 *
* Newline-delimited JSON communication between the sandbox process and the parent process harness on stdio. This module is the **only true source**,
* Describe the types and fields of each message; mirror these in pure handwriting in the child process source code (embedded string in child-source.ts)
* Shape - it cannot import this module (run through `toString()` embedded entry file), so both places must be changed together. The parent process side directly
* import these types to ensure that the bridge code is strictly typed.
 *
* Direction agreement:
* child → parent: create-actor (send and forget)/request (ask/world-read/publish-artifact, response required)/
 *                   event（log、report、declare-artifact）/ complete
* parent → child: response (respond to request and carry the latest budget snapshot)
 *
* Why create-actor does not go request/response: Boundary A requires `createActor` **synchronous** return handle. The child process cannot wait for a round-trip for synchronous return, so it creates a synchronous
* child-local handle (`local#N`) and forget a create-actor, the parent process is processing according to the FIFO order of stdio
* The mapping of local→engine ActorId is done before any ask that references this handle (the parent process's create-actor handling is purely synchronous).
 */

import type { ArtifactContentOp, ArtifactPresetOp, WorldReadOp } from "@zcode/dynamic-workflow";

// ————————————————————————————————————————————————————————————————
// Structural error line shape
// ————————————————————————————————————————————————————————————————

/**
* Error patterns across borders. The rejection of ask (WorkflowError) crosses the boundary, and the child process rebuilds a band in the sandbox accordingly.
* Error of `code`/`violations`/`finalText` enables the script's try/catch to be processed according to the structure.
* Model-side errors are no longer out of bounds: they are either retried within the runtime,
* Either the entire run stops and the script never sees it.
 */
export interface WireError {
  name: string;
  message: string;
  code?: string;
  violations?: unknown[];
  finalText?: string;
  stack?: string;
}

// ————————————————————————————————————————————————————————————————
// child → parent
// ————————————————————————————————————————————————————————————————

/** Create actor synchronously: fire and forget, the parent process establishes local→ActorId mapping according to FIFO before subsequent ask. */
export interface CreateActorMessage {
  kind: "create-actor";
  localId: string;
  siteId: string;
  name?: string;
  persona?: unknown;
}

/**
* Host calls to respond to: ask, world-read, or **release of content product**.
 *
* The two families of products split here: the content members (`file`/`markdown`) are
* Effect, the script awaits it and can catch its rejection, so request/response is used; the preset member is declared and returned
*void, take the event channel below.
 */
export interface RequestMessage {
  kind: "request";
  id: string;
  type: "ask" | "world-read" | "publish-artifact";
  siteId: string;
/** ask exclusive: child-local actor handle. */
  actor?: string;
/** Ask exclusive: command text. */
  instructions?: string;
/** Exclusive to world-read: op (the vocabulary is derived from the world-read registry, adding primitives does not change this file). */
  op?: WorldReadOp;
  /**
* publish-artifact exclusive: op for content members. **Deliberately split `op` into two fields** instead of widening `op` into
* The combination of two vocabularies: after widening, what the parent process gets when dispatching is a value that must be narrowed again, and the basis for narrowing
* Only `type` - that's exactly what the two fields express. With two registration forms and two fields, no one has to guess.
   */
  artifactOp?: ArtifactContentOp;
  /**
* Exclusive to world-read: **positional argument group**. lowering packages the actual parameters of the script call point as they are, and transparently transmits them as they are in this layer.
* The digits and checksums belong to the driver.
* Single parameter `arg?: string` cannot support multi-parameter/no-parameter ops (`files.grep(pattern, glob?)`, `git.status()`).
   */
  args?: unknown[];
}

/**
* Events that occur and are forgotten are judged by `type`. None of the three require a response, but the **durability is different**: `log` will only lose one line if it is lost.
* Gossip, if `report` is lost, it will lose a discovery (the parent process will drop it into the journal), so the FIFO order of the event channel is correct
* The report is load-bearing - the parent process's journal is what arrives.
 *
* `declare-artifact` is on the same channel as the report, and **must** be so: a tagged report is only on
* It is only valid after its artifact declaration has reached the parent process (otherwise the engine fails the entire run with `ArtifactUndeclared`).
* The declaration in the script comes first and the report comes after, and the same FIFO ensures that the parent process also sees them in this order. Change the statement to
* The request that needs to be answered cannot improve this, it will only make a facade member that returns void synchronously wait one more time out of thin air.
 */
export type EventMessage =
  | LogEventMessage
  | ReportEventMessage
  | DeclareArtifactEventMessage
  | PhaseEnteredEventMessage;

/**
* The control flow passes through a `phase("...")` mark. Take the event channel: the script never awaits it; there is no site, no journal, and the parent process only lets the engine send one
* `phase-entered`. The order of arrival is still load-bearing - "Go to B first and then dispatch the ask in B" is the basis for lighting up the timeline.
 */
export interface PhaseEnteredEventMessage {
  kind: "event";
  type: "phase-entered";
/** The author's original word, lowering, has been blanked at both ends. */
  name: string;
}

/** Progress message: No site, no journal. */
export interface LogEventMessage {
  kind: "event";
  type: "log";
  message: string;
}

/**
* An intermediate result. Go the event channel instead of request/response because the script never awaits it; but unlike `log`,
* The parent process will put it into a row `dwf_node` by `siteId` × ordinal.
 */
export interface ReportEventMessage {
  kind: "event";
  type: "report";
  siteId: string;
  item: unknown;
  /**
* Product tag (`report(item, "perf")`): Which preset product is fed to this item at the same time. Absence means no label
* (JSON.stringify will throw away the entire undefined key, so the line is "there is no such key").
   */
  artifactId?: string;
}

/**
* A **declaration** of the preset artifact (`artifact.chart` etc.). Take the event channel instead of request/response because
* The facade member returns void synchronously - the script never awaits it. The parent process drops a line by `siteId` × ordinal
* `dwf_node` (except for idempotent repeated declarations), so the arrival order of this message is also load-bearing (see {@link EventMessage}).
 */
export interface DeclareArtifactEventMessage {
  kind: "event";
  type: "declare-artifact";
  siteId: string;
  op: ArtifactPresetOp;
/** Position actual parameter group (`[id, spec]`), lowering is packaged as it is, and this layer is transparently transmitted as it is. */
  args: unknown[];
}

/** Script execution termination: top-level artifact or error thrown. */
export interface CompleteMessage {
  kind: "complete";
  ok: boolean;
  value?: unknown;
  error?: WireError;
}

/** All messages of child → parent. */
export type ChildMessage = CreateActorMessage | RequestMessage | EventMessage | CompleteMessage;

// ————————————————————————————————————————————————————————————————
// parent → child
// ————————————————————————————————————————————————————————————————

/** The response to a request. */
export interface ResponseMessage {
  kind: "response";
  id: string;
  ok: boolean;
  value?: unknown;
  error?: WireError;
}

/** All messages of parent → child. */
export type ParentMessage = ResponseMessage;

/**
* The initial payload of the child process. As a JSON literal embedded in the entry file (`renderChildEntry` of child-source.ts),
* No longer via argv/base64: Windows command line limit of 32,767 characters.
 */
export interface ChildPayload {
/** Lowered script’s async function body (free identifier only `__host`). */
  lowered: string;
  /**
* The actual parameters of this run are injected into the sandbox and become the **frozen** `args` global (lowering reads the `args` in the script
* rewritten as `__host.args`).
   *
* Only crosses the bounds once when spawning, and is no longer updated after that - the actual parameters are constant throughout the life cycle of the run. Absence is interpreted as
* `{}`: inline run and old journal
* lines go this way, so `args.x` in the script is always a valid property read and not a crash.
   */
  args?: Record<string, unknown>;
  /**
* Heap limit (MB). **Only takes effect in the argsPrefix (SEA from re-exec) path**: Unable to upload on that path
* `--max-old-space-size`, when the entry file cannot see the flag in `execArgv`, use `v8.setFlagsFromString`
* best-effort. The default path is taken into effect by the real Node flag. If the entry file sees the flag, it will skip this field.
* The effect is not guaranteed (V8 may no longer ignore flags that have been consumed during the startup period), recorded as SEA restrictions.
   */
  maxOldSpaceSizeMb?: number;
}
