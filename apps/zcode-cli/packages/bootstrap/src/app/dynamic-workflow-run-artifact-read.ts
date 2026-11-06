// ============================================================
// Read back the bytes of user plane products (implementation of DynamicWorkflowRunPort.readArtifact)
// ============================================================
//
// ⚠ Terminology: The artifact here is the output (journal) published by the script to **users** via `artifact.*`
// `kind = "artifact"` line), **not** the engine internal `RunSettlement.artifact` (the top-level return value of the script,
// The port is called `output` / `result`).
//
// Remove from dynamic-workflow-run-service.ts (that file is already in the existing debt of max-lines and will not be added),
// It’s the same reason as splitting the driver’s publishing path into workflow-artifact-publish.ts: read a byte once
// It takes three steps: "Authorization → Positioning → Store". Each step has a discipline that must be written down.
//
// **Authorization chain** (same discipline as attachmentRead):
//
// Given by the caller (runId, artifactId, version)
//        │
// ├─① Does the dwf_run line for runId exist?                     No ⇒ undefined
// ├─② Is the parent_session_id of this row == the parent session of this service?   No ⇒ undefined
// ├─③ The journal contains (artifactId, version) completed No ⇒ undefined
// │ artifact OK?
// ├─④ Is there a uri in the record of that row? (There is no preset billboard) No ⇒ undefined
// └─⑤ Take the ** uri on the ** line and read it in the store
//
// Step 5 is the whole point: the id passed by the renderer is only used to **look up rows in the journal** and never directly becomes a path.
// If any step in the middle fails, undefined will be returned instead of throwing an error - "There is no such version" "It is not your run" "This is a Kanban board"
// It is the same business fact for the caller (the gateway is normalized to not found), distinguishing them will only tell an unauthorized caller
// Which half did it guess correctly.

import type {
  DynamicWorkflowRunArtifactBytes,
  ToolArtifactStorePort,
} from "@zcode/contracts";
import type { JournalStorePort } from "@zcode/dynamic-workflow";

import {
  artifactRowId,
  supportsArtifactReads,
} from "./dynamic-workflow-run-artifact-queries.js";

interface WorkflowArtifactReadDeps {
  journal: JournalStorePort;
/** The parent session of this service (= the session of this app). The comparison object in step ② of the authorization chain. */
  parentSessionId: string;
  /**
* Home of Bytes. **Optional**: Pure replay/no store assembly cannot get the bytes, and the entire line is read back in this case.
* (with driver-side `ArtifactStoreUnavailable` being two representations of the same assembly fact).
   */
  artifactStore?: ToolArtifactStorePort;
}

/**
* Read all bytes of a product version. Return to the gateway in chunks (≤ 512 KiB per block), where the entire portion is returned at once - the upper limit
* 20 MiB is at the same level as attachment. Compared with "one read counts as one authorization", the authorization chain is cut into the block loop.
* It will only make the journal run through again for each piece.
 */
export async function readWorkflowArtifactBytes(
  deps: WorkflowArtifactReadDeps,
  runId: string,
  artifactId: string,
  version: number,
): Promise<DynamicWorkflowRunArtifactBytes | undefined> {
  const store = deps.artifactStore;
// Binary readback is an **optional member**: a store without it must not return text to read and then decode - that's exactly
// The bug recorded on ToolBinaryArtifactReadResult (the office file is damaged when it is decoded as utf8).
  if (store?.readToolResultBinaryArtifact === undefined) return undefined;

// ①②: run exists and belongs to this session. getRun is on the engine port and does not require introspection capability detection.
  const run = deps.journal.getRun(runId);
  if (run === undefined) return undefined;
//Old rows whose parent_session_id is NULL are **not released**: The design requires sessionId to be equal to that of the run
// parentSessionId, and NULL means there is no such thing to compare - "unable to determine" does not mean "passed".
  if (run.parentSessionId === undefined || run.parentSessionId !== deps.parentSessionId) {
    return undefined;
  }

// ③④: Locate that row in the journal and get the uri on the row.
  const located = locateArtifactVersion(deps.journal, runId, artifactId, version);
  if (located === undefined) return undefined;

// ⑤: The store is only touched when you get here, and what is fed to it is the uri on the ** line.
  const result = await store.readToolResultBinaryArtifact({ uri: located.uri });
  return {
    bytes: result.bytes,
// contentType takes the value on the **journal record** instead of the one pushed by the store by file name: the value in the record is
// The driver calculates the one that `opts.contentType` can override according to the extension table, which is also the one that the UI dispatches the renderer to.
// Exactly match the contract. The store's inference table only recognizes 8 extensions, and using it will turn `.csv` into application/json.
    contentType: located.contentType ?? result.contentType,
  };
}

/**
* Find the **completed** artifact line of `(artifactId, version)` in the journal and take out its
* `uri` and `contentType`.
 *
* Only recognized completed: The failed release does not claim the id / type / version, and its `result` is empty.
* Release will only read an undefined uri. The record of the preset kanban board does not have `uri` (it has no bytes, its data is
* Label report line in journal) - it is also blocked here, and the caller gets undefined.
 */
function locateArtifactVersion(
  journal: JournalStorePort,
  runId: string,
  artifactId: string,
  version: number,
): { uri: string; contentType?: string } | undefined {
  if (!supportsArtifactReads(journal)) return undefined;
  for (const row of journal.listArtifactRows(runId)) {
    if (row.status !== "completed") continue;
    if (artifactRowId(row) !== artifactId) continue;
    const record = row.result;
    if (record === null || typeof record !== "object" || Array.isArray(record)) continue;
    const fields = record as Record<string, unknown>;
    if (fields.version !== version) continue;
    const uri = typeof fields.uri === "string" && fields.uri.length > 0 ? fields.uri : undefined;
    if (uri === undefined) return undefined;
    const contentType =
      typeof fields.contentType === "string" && fields.contentType.length > 0
        ? fields.contentType
        : undefined;
    return { uri, ...(contentType === undefined ? {} : { contentType }) };
  }
  return undefined;
}
