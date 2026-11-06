// ============================================================
//Execution side of product release (executeArtifactPublish of Boundary B)
// ============================================================
// ⚠ Terminology: artifacts in this file always refer to the user interface
// Artifact** - the bytes delivered by the script to the user via `artifact.file` / `artifact.markdown`; not internal to the engine
// The top-level return value of that artifact (`RunSettlement.artifact`, `serializeWorkflowArtifact`, that is for
// **Model** to see). This document does not touch on the latter.
//
// Same division of labor as workflow-world-read.ts: driver body (workflow-driver.ts) does actor session and
// The arrangement of turn, this half turns a `(op, id, path | content, opts)` into a real **byte copy**,
// And don't touch the session, model and journal at all. Four things are here, and only here:
// 1. **Realize the shape verification of opts**. The engine only guarantees that `id` and payload are non-empty strings (runtime guardrails).
// `opts` is the third actual parameter passed through the script as it is - "what does a title look like" belong to this side, discipline and
// Same as `worldReadStringArgs`: fails loudly, never casts to `String(...)`.
// 2. **Path analysis and out-of-bounds determination**. The one where the parser reuses world-read (`resolveWithinWorkspace`),
//Add another realpath review, see {@link resolveWorkspaceFile}.
// 3. **Execution of upper limit**. Constants are in pure packages (`ARTIFACT_CAPS`), implemented here because only this side can be read
// Bytes; if the limit is exceeded, it will be **rejected and not truncated** (same house policy as world-read - a PDF that was quietly truncated)
// is a bad deliverable, and a named capped rejection is a contract under which the script can rewrite itself).
// 4. **Write tool-artifact store** and unify the store's receipt into `ArtifactVersionRecord`.
//
// Rejections are always structured `WorkflowError` (error code: ArtifactSourceMissing /
//ArtifactPathOutsideWorkspace/ArtifactTooLarge/ArtifactStoreUnavailable) because of the content member
// A `catchable' promise in a script - gating idiom `try { await artifact.file(…) } catch { … }`
// The premise is that these rejections are in the hands of the script.

import { realpath } from "node:fs/promises";
import { basename } from "node:path";
import {
  isFileSystemPortError,
  type FileSystemPort,
  type FileSystemReadBytesResult,
  type SessionId,
  type ToolArtifactStorePort,
  type ToolArtifactWriteResult,
} from "@zcode/contracts";
import {
  ARTIFACT_CAPS,
  ARTIFACT_ID_PATTERN,
  WorkflowError,
  type ArtifactPublishRequest,
  type ArtifactVersionRecord,
} from "@zcode/dynamic-workflow";
import { resolveWithinWorkspace, toWorkspaceRelative } from "./workflow-world-read.js";

/** The port and base directory required for product release (a subset of driver deps, the same as {@link WorldReadDeps}). */
interface ArtifactPublishDeps {
/** The file system port used to read bytes from the published file. */
  readonly fileSystemPort: FileSystemPort;
/** The base directory (workspace root) for path resolution and relativization. */
  readonly cwd: string;
  /**
* The drop point of the byte. **OPTIONAL**: Pure replay/fake assembly without store.
* Absent content members loudly reject with `ArtifactStoreUnavailable` - not silently downgraded to
* "Publishing an empty product" does not mean returning to the writing workspace.
   */
  readonly artifactStore?: ToolArtifactStorePort;
  /**
* The session scope written by the store = the **parent session** of this run. with `artifactStore`
* Appears in pairs: Each write to the store is accounted for by session (`zcode-artifact://<session>/<id>`), without session id
* You cannot write a record that can be read back later. Only when both are present can the release capability be installed. If one is missing,
* `ArtifactStoreUnavailable` (missing store means "this assembly is not stored", missing id means wiring error - both are correct
*The script is the same thing: this release can't do it and has to be loud).
   */
  readonly parentSessionId?: SessionId;
}

/**
* Execute a content product release: verify the shape → confirm the presence of the store → get bytes (file reading / markdown text) →
* Write store → Normalize into storage record.
 *
* The order is not arbitrary: **shape first, then store, then IO**. Shape verification is the cheapest and assembly-independent; when store is absent
* Reading the disk once is in vain, and what's worse is that it will cover up the real cause with an `ArtifactSourceMissing`
* ("This assembly is not stored at all").
 */
export async function executeArtifactPublish(
  deps: ArtifactPublishDeps,
  request: ArtifactPublishRequest,
): Promise<ArtifactVersionRecord> {
  assertArtifactId(request);
  const opts = artifactPublishOptions(request);
  const store = requireArtifactStore(deps, request);
  const payload =
    request.op === "file"
      ? await readFilePayload(deps, request, opts)
      : markdownPayload(request);
  const written = await writePayload(store, request, payload);
  return {
    id: request.id,
    kind: request.op,
    version: request.version,
    ...(opts.title === undefined ? {} : { title: opts.title }),
    ...(opts.description === undefined ? {} : { description: opts.description }),
// contentType takes the locally calculated one instead of the store receipt: extension table + `opts.contentType`
// Overrides jointly determine the content type, and the UI does switch dispatch on this string; let the store normalize (it presses
// The file name pushback type) becomes authoritative, making the result of a dispatch dependent on the store implementation.
    contentType: payload.contentType,
// bytes / uri Get the store's receipt: This is the one that actually falls into the store, and it is also the one that will be read back in the future.
    bytes: written.bytes,
    uri: written.uri,
    ...(payload.kind === "binary" ? { sourcePath: payload.sourcePath } : {}),
// The engine has no clock (pure core, replayable), so the release time is filled in by this side. Release time is a required field.
    publishedAt: Date.now(),
  };
}

// ————————————————————————————————— Internal: Shape Verification —————————————————————————————

/** Validated `opts` (facade’s `ArtifactOptions` / `ArtifactFileOptions`). */
interface ArtifactPublishOptions {
  title?: string;
  description?: string;
/** Only `file` reads it: markdown constant `text/markdown`. */
  contentType?: string;
}

/**
* Runtime guardrails for product ids. The compile-time literal diagnosis (analyzer) is the front door, and the engine blocks it again "must be non-null"
* String"——The character set and length are rechecked here because **only this side uses id as a key**: it enters the store
* `toolCallId`, which eventually becomes a section of the file name. The illegal ids that come here can only be caused by something bypassing compilation.
* Use `DriverError` instead of an `Artifact*` code, and let those codes mean only their respective things.
 */
function assertArtifactId(request: ArtifactPublishRequest): void {
  const { id } = request;
  if (id.length > ARTIFACT_CAPS.maxIdLength || !ARTIFACT_ID_PATTERN.test(id)) {
    throw new WorkflowError(
      "DriverError",
      `artifact.${request.op}: id '${id}' is not a valid artifact id. Use at most ` +
        `${ARTIFACT_CAPS.maxIdLength} characters from [A-Za-z0-9_.-].`,
    );
  }
}

/**
* Take out and verify the `opts` given by the script. Three rules have the same origin as {@link worldRunArgs}:
* 1. Absence is legal (`opts?`), and presence must be an options object (not an array, not null).
* 2. Check the type and upper limit of the recognized keys one by one, **never force conversion** - one is turned into `String(undefined)`
* The title of `"undefined"` will be displayed on the card as an unintelligible string instead of a correctable error.
* 3. Unrecognized keys are silently ignored (the type signature of the facade blocks them at compile time; reporting errors for them at runtime will only
* Move a compile-time problem to run-time). The `markdown` family therefore does not read `contentType` at all.
 */
function artifactPublishOptions(request: ArtifactPublishRequest): ArtifactPublishOptions {
  const raw = request.opts;
  if (raw === undefined) return {};
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) {
    throw new WorkflowError(
      "DriverError",
      `artifact.${request.op}: opts must be an options object, got ${describeArg(raw)}. ` +
        `Pass an object or omit the argument.`,
    );
  }
  const bag = raw as { title?: unknown; description?: unknown; contentType?: unknown };
  const title = optionalText(request, "title", bag.title, ARTIFACT_CAPS.maxTitleLength);
  const description = optionalText(
    request,
    "description",
    bag.description,
    ARTIFACT_CAPS.maxDescriptionLength,
  );
  const contentType = request.op === "file" ? optionalContentType(request, bag.contentType) : undefined;
  return {
    ...(title === undefined ? {} : { title }),
    ...(description === undefined ? {} : { description }),
    ...(contentType === undefined ? {} : { contentType }),
  };
}

/** An optional bounded text option (title/description). Length is measured in characters (UTF-16 code units). */
function optionalText(
  request: ArtifactPublishRequest,
  name: string,
  value: unknown,
  max: number,
): string | undefined {
  if (value === undefined) return undefined;
  if (typeof value !== "string") {
    throw new WorkflowError(
      "DriverError",
      `artifact.${request.op}: opts.${name} must be a string, got ${describeArg(value)}.`,
    );
  }
  if (value.length > max) {
// Reject rather than truncate: a title with the last half of the sentence chopped off is a bad deliverable that looks normal, and the card
//Nowhere to display "truncated here".
    throw new WorkflowError(
      "ArtifactTooLarge",
      `artifact.${request.op}: opts.${name} is ${value.length} characters, over the cap of ` +
        `${max}. Shorten it.`,
    );
  }
  return value;
}

/**
* Override value for `opts.contentType`. The requirement is bare MIME without parameters (`type/subtype`): the string
* The consumer is a precise dispatch of the UI (`text/markdown` takes the markdown card, `application/pdf` takes the pdf card),
* A `text/markdown; charset=utf-8` will completely miss that branch, and the performance is "the type is obviously written but it falls into the download card".
 */
function optionalContentType(request: ArtifactPublishRequest, value: unknown): string | undefined {
  if (value === undefined) return undefined;
  if (typeof value !== "string") {
    throw new WorkflowError(
      "DriverError",
      `artifact.file: opts.contentType must be a string, got ${describeArg(value)}.`,
    );
  }
  if (!CONTENT_TYPE_PATTERN.test(value)) {
    throw new WorkflowError(
      "DriverError",
      `artifact.file: opts.contentType '${value}' is not a bare MIME type such as ` +
        `'application/pdf'. Drop any parameters ('; charset=...'); the viewer dispatches on ` +
        `the exact string.`,
    );
  }
  return value;
}

// ————————————————————————————————— Internal: get bytes ——————————————————————————————

/** A copy of the content to be written to the store. `file` is written in binary, and `markdown` is written in text. */
type ArtifactPayload =
  | {
      readonly kind: "binary";
      readonly bytes: Uint8Array;
      readonly contentType: string;
/** The extension of the dropped file name (the type is pushed back by the file name when the store reads it back, so fidelity must be maintained). */
      readonly extension?: string;
/** The original path relative to the workspace (source + "display in workspace"). */
      readonly sourcePath: string;
    }
  | { readonly kind: "text"; readonly text: string; readonly contentType: "text/markdown" };

/**
* `artifact.file(id, path, opts)`: parse path → read bytes (cap+1 detection) → set contentType.
 */
async function readFilePayload(
  deps: ArtifactPublishDeps,
  request: ArtifactPublishRequest,
  opts: ArtifactPublishOptions,
): Promise<ArtifactPayload> {
  const given = request.path;
  if (typeof given !== "string" || given === "") {
// The engine has verified that the payload is a string; an empty string can only come from a wiring error.
    throw new WorkflowError(
      "DriverError",
      `artifact.file: path must be a non-empty string, got ${describeArg(given)}. Pass the ` +
        `workspace-relative path the subagent wrote.`,
    );
  }
  const { real, relative } = await resolveWorkspaceFile(deps, given);
  const bytes = await readCappedBytes(deps, given, real);
  const extension = fileExtension(real);
  return {
    kind: "binary",
    bytes,
    contentType:
      opts.contentType ??
      (extension === undefined ? undefined : EXTENSION_CONTENT_TYPES[extension]) ??
// Not listed in the extension table = no guessing (no magic bytes sniffing). The UI accordingly gives "Download / in the workspace
// Show the card instead of rendering something it can't read.
      "application/octet-stream",
    ...(extension === undefined ? {} : { extension }),
    sourcePath: relative,
  };
}

/**
* Parse the **workspace relative** path given by the script into a readable real path, and determine it twice as out of bounds.
 *
* The first time is lexical ({@link resolveWithinWorkspace}, the same criterion as `files.read`); the second time is in
* After realpath - **Symbolic links are judged to be out of bounds based on the parsed real path**. world read
* That side deliberately only performs lexical checking (the bytes read out are the values ​​seen by the script itself, and the script is responsible for whether it crosses the boundary).
* This side cannot be copied: the release copies the bytes into a persistent store and puts them in front of the user. A soft link can make
* `~/.ssh/id_rsa` becomes a card. This review is where the invariant falls on the writing side.
 *
* `cwd` also needs its own realpath: `/tmp` on macOS is the soft link of `/private/tmp`. If it is not normalized, it will be in the workspace.
* Every file will be judged as out of bounds.
 */
async function resolveWorkspaceFile(
  deps: ArtifactPublishDeps,
  given: string,
): Promise<{ real: string; relative: string }> {
  const resolved = resolveWithinWorkspace(deps.cwd, given);
  if (resolved === undefined) throw outsideWorkspace(given);
// The source is the path given by the script (after normalization), not realpath: the "display in workspace" on the card should be
// Point back to the location recognized by the user, not the landing point of the soft link.
  const relative = toWorkspaceRelative(deps.cwd, resolved);

  let realCwd: string;
  let real: string;
  try {
    realCwd = await realpath(deps.cwd);
  } catch (cause) {
    throw new WorkflowError(
      "DriverError",
      `artifact.file: cannot resolve the workspace root '${deps.cwd}': ${errorText(cause)}`,
      { cause },
    );
  }
  try {
    real = await realpath(resolved);
  } catch (cause) {
// ENOENT (including a certain section in the middle does not exist) go here: the path does not exist and "pointing to a non-existent soft link target" are the same thing.
    throw sourceMissing(given, cause);
  }
  if (resolveWithinWorkspace(realCwd, real) === undefined) throw outsideWorkspace(given);
  return { real, relative };
}

/**
* Read bytes, **cap+1 detection**: The `maxBytes` of the port is "too_large` if exceeded, without truncation", so take cap+1
*, "exactly cap bytes" (acceptance) and "exceeding cap" (rejection) can be distinguished, and at worst only one more byte is read.
* Same idiom as `files.grep` / `git.diff`.
 */
async function readCappedBytes(
  deps: ArtifactPublishDeps,
  given: string,
  path: string,
): Promise<Uint8Array> {
  const cap = ARTIFACT_CAPS.maxFileBytes;
  let result: FileSystemReadBytesResult;
  try {
    result = await deps.fileSystemPort.readBinaryFile({ path, maxBytes: cap + 1 });
  } catch (cause) {
    if (isFileSystemPortError(cause)) {
      if (cause.code === "too_large") throw tooLarge(given, cap);
// not_found / is_directory / not_file are all "there is no ordinary file that can be published here".
      if (cause.code === "not_found" || cause.code === "is_directory" || cause.code === "not_file") {
        throw sourceMissing(given, cause);
      }
    }
    throw new WorkflowError(
      "DriverError",
      `artifact.file: failed to read '${given}': ${errorText(cause)}`,
      { cause },
    );
  }
  if (result.content.byteLength > cap) throw tooLarge(given, cap);
  return result.content;
}

/** `artifact.markdown(id, content, opts)`: The text is counted in bytes in UTF-8 and will be rejected if it exceeds the limit. */
function markdownPayload(request: ArtifactPublishRequest): ArtifactPayload {
  const content = request.content;
  if (typeof content !== "string") {
    throw new WorkflowError(
      "DriverError",
      `artifact.markdown: content must be a string, got ${describeArg(content)}.`,
    );
  }
  const bytes = Buffer.byteLength(content, "utf8");
  const cap = ARTIFACT_CAPS.maxMarkdownBytes;
  if (bytes > cap) {
    throw new WorkflowError(
      "ArtifactTooLarge",
      `artifact.markdown: content is ${bytes} bytes, over the cap of ${cap} bytes. Shorten ` +
        `it, or write the long content to a workspace file and publish it with artifact.file.`,
    );
  }
  return { kind: "text", text: content, contentType: "text/markdown" };
}

// ——————————————————————————————— Internal: write store ——————————————————————————————

/** Confirmed available release points: store ontology + session scope (see {@link ArtifactPublishDeps}). */
interface ArtifactStoreTarget {
  readonly store: ToolArtifactStorePort;
  readonly sessionId: SessionId;
}

/**
* Confirm that this release is on target, otherwise `ArtifactStoreUnavailable`. **Press op to detect separately**: The file must be written in binary
* (`writeToolResultBinaryArtifact` is an optional method on the port), markdown only writes text. one with only text
* The store written can therefore send markdown, but cannot send files - and "cannot send" is a named rejection, not treating PDF as
* UTF-8 stuffed into the text channel (that silently destroys bytes).
 */
function requireArtifactStore(
  deps: ArtifactPublishDeps,
  request: ArtifactPublishRequest,
): ArtifactStoreTarget {
  const store = deps.artifactStore;
  if (store === undefined) {
    throw new WorkflowError(
      "ArtifactStoreUnavailable",
      `artifact.${request.op}: cannot publish "${request.id}" because this assembly has no ` +
        `artifact store.`,
    );
  }
  if (request.op === "file" && store.writeToolResultBinaryArtifact === undefined) {
    throw new WorkflowError(
      "ArtifactStoreUnavailable",
      `artifact.file: cannot publish "${request.id}" because this assembly's artifact store ` +
        `does not support binary writes.`,
    );
  }
  const sessionId = deps.parentSessionId;
  if (sessionId === undefined || sessionId === "") {
// The presence of the store but the absence of the session id can only be a wiring error (both come in and out of the production assembly at the same time). Still reporting the same
// Code: This is the same thing for scripts - there is no landing point for this release.
    throw new WorkflowError(
      "ArtifactStoreUnavailable",
      `artifact.${request.op}: cannot publish "${request.id}" because the artifact store has ` +
        `no session scope (parent session id not wired).`,
    );
  }
  return { store, sessionId };
}

/**
* Write to store. The constraints of the four parameters are as follows:
* - `retention: "project"` - the product must survive the session (the hub reads products in the run history across sessions).
* - `toolName: "CreateWorkflow"` - The published tool belongs to the one that started this run.
* - `toolCallId: `${runId}:${siteId}@${ordinal}``——One byte per version, and (run, site, serial number)
* Exactly one release is calibrated (the version number is calculated by the engine before dispatch, re-running the same version = the same ordinal).
* - `sessionId` = parent session.
 */
async function writePayload(
  target: ArtifactStoreTarget,
  request: ArtifactPublishRequest,
  payload: ArtifactPayload,
): Promise<ToolArtifactWriteResult> {
  const common = {
    sessionId: target.sessionId,
    toolCallId: `${request.runId}:${request.siteId}@${request.ordinal}`,
    toolName: ARTIFACT_TOOL_NAME,
    retention: "project" as const,
  };
  try {
    if (payload.kind === "text") {
      return await target.store.writeToolResultArtifact({
        ...common,
        content: payload.text,
        contentType: payload.contentType,
      });
    }
// The existence of binary writes has been detected in requireArtifactStore (op === "file" branch).
    const writeBinary = target.store.writeToolResultBinaryArtifact;
    if (writeBinary === undefined) {
      throw new WorkflowError(
        "ArtifactStoreUnavailable",
        `artifact.file: cannot publish "${request.id}" because this assembly's artifact store ` +
          `does not support binary writes.`,
      );
    }
    return await writeBinary.call(target.store, {
      ...common,
      content: payload.bytes,
      contentType: payload.contentType,
//The extension is handed to the store along with the bytes: when it reads it back, it pushes back the type according to the file name on the disk. Losing the extension is equivalent to losing a copy of the file.
// .xlsx is read as application/octet-stream.
      ...(payload.extension === undefined ? {} : { extension: payload.extension }),
    });
  } catch (cause) {
    if (cause instanceof WorkflowError) throw cause;
// Write failure (disk full, permissions, store's own error) is a **specific** failure, not the same as "no store"
// The same thing, so don’t reuse ArtifactStoreUnavailable: bring the reason text and let the journal
// failure_json tells you why.
    throw new WorkflowError(
      "DriverError",
      `artifact.${request.op}: writing "${request.id}" v${request.version} to the artifact ` +
        `store failed: ${errorText(cause)}`,
      { cause },
    );
  }
}

// ——————————————————————————————— Pure support ————————————————————————————

/** The tool name of the product in the store: Release is the product of this call to `CreateWorkflow`. */
const ARTIFACT_TOOL_NAME = "CreateWorkflow";

/**
* extension → MIME. Everything outside the table is always `application/octet-stream`:
* **No content sniffing** (magic bytes will introduce a second criterion, and the two criteria will give two answers sooner or later).
 *
* Why not reuse core's `inferAttachmentMimeFromPath`: that table serves **model input attachment**,
* The bottom line is `text/plain` (feeding anything that looks like text to the model is its desired behavior). The consumer here is UI
* Assignment, the bottom line must be "I don't recognize it, give me the download card" - the correct value of the same bottom line in two places is exactly opposite.
 */
const EXTENSION_CONTENT_TYPES: Readonly<Record<string, string>> = {
  csv: "text/csv",
  docx: "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
  gif: "image/gif",
  htm: "text/html",
  html: "text/html",
  jpeg: "image/jpeg",
  jpg: "image/jpeg",
  json: "application/json",
  markdown: "text/markdown",
  md: "text/markdown",
  pdf: "application/pdf",
  png: "image/png",
  pptx: "application/vnd.openxmlformats-officedocument.presentationml.presentation",
  svg: "image/svg+xml",
  txt: "text/plain",
  webp: "image/webp",
  xlsx: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
};

/** Naked MIME (`type/subtype`, no parameters). See {@link optionalContentType} for justification. */
const CONTENT_TYPE_PATTERN = /^[A-Za-z0-9][A-Za-z0-9!#$&^_.+-]*\/[A-Za-z0-9][A-Za-z0-9!#$&^_.+-]*$/;

/**
* The extension of the file name (lowercase, without dot). Only recognize `[a-z0-9]+`: `archive.tar.gz` takes `gz`,
* `Makefile`, `.gitignore` (no suffix at the beginning of dot) and `report.v2 final` have no extension.
 */
function fileExtension(path: string): string | undefined {
  const name = basename(path);
  const dot = name.lastIndexOf(".");
  if (dot <= 0 || dot === name.length - 1) return undefined;
  const ext = name.slice(dot + 1).toLowerCase();
  return /^[a-z0-9]+$/.test(ext) ? ext : undefined;
}

function outsideWorkspace(given: string): WorkflowError {
  return new WorkflowError(
    "ArtifactPathOutsideWorkspace",
    `artifact.file: path '${given}' resolves outside the workspace (symlinks are judged by ` +
      `their real path). Only files inside the workspace can be published.`,
  );
}

function sourceMissing(given: string, cause: unknown): WorkflowError {
  return new WorkflowError(
    "ArtifactSourceMissing",
    `artifact.file: '${given}' does not exist or is not a regular file. Confirm the subagent ` +
      `actually wrote it before publishing.`,
    { cause },
  );
}

function tooLarge(given: string, cap: number): WorkflowError {
  return new WorkflowError(
    "ArtifactTooLarge",
    `artifact.file: '${given}' is over the cap of ${cap} bytes. Publish a smaller artifact ` +
      `(a summary, a slice, or a compressed version).`,
  );
}

/** A short description of the shape of the actual parameter (only used in error messages, the full content is not echoed). */
function describeArg(value: unknown): string {
  if (value === undefined) return "undefined";
  if (value === null) return "null";
  return Array.isArray(value) ? "array" : typeof value;
}

/** A single line of text for any thrown object (for error messages). */
function errorText(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}
