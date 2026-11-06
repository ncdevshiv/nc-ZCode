/**
* Sandbox process logic. **One implementation, one entry file, two startup methods**:
* - {@link renderChildEntry} renders {@link childMain} via `toString()` together with the payload.
* Self-contained ESM, harness written to `<cwd>/.zcode/workflow-runs/<runId>.mjs` (child-entry-file.ts);
* - Ordinary Node: `node --max-old-space-size=N <entry>`, the entry file starts automatically when it finds that it is the process entry;
* - SEA single-file binary: CLI's hidden subcommand `__zcode-dwf-child <entry>` `import()` imports this file and calls
* The `start(deps)` it exports is injected into vm/readline/stdio of the CLI process (the SEA main program does not interpret the Node CLI
* flag, `--eval` path is blocked).
 *
*The Windows command line limit is 32,767 characters,
* `--eval CHILD_SOURCE -- <base64url payload>` will put the entire lowered script on the command line. The script
* `spawn ENAMETOOLONG` once it gets past about 18 KB, and every slightly longer run on Windows won't start.
 *
* Structure:
* - The outer realm ({@link childMain} itself) is ordinary Node code: holds stdio, readline, vm, and is responsible for transmission.
* - The evaluation unit is an **independent realm** created by `vm.createContext(...)`: only containing ES intrinsics + injected `__host`.
* The bare vm context naturally does not have `process`/`require`/`Buffer`/`fetch`; we only add `__host` and runtime bans.
 *
* Cross-realm convergence (anti-prototype leakage/prototype pollution): everything touched by the script (Promise, JSON parsed
* host result, Error) are all constructed within **context**; there are only two cross-boundary values ​​between the outer layer and context - one `__send(string)`
* The outer function, and the inbound line string (the string is a primitive value, without realm ownership). Therefore `Array.isArray`/`instanceof`/
* The prototype chain is all context-native from the script perspective and never incorporates the intrinsics of the outer realm.
 *
* Dynamic import: runFn compiled by `vm.runInContext` (Script, no importModuleDynamically callback provided),
* Runtime `import()` throws `ERR_VM_DYNAMIC_IMPORT_CALLBACK_MISSING`. `eval`/`Function` are bound to the same set
* context global.
 *
* ⚠ This file is a **handwritten image** of the protocol.ts line protocol: childMain in the entry file runs as an embedded string and cannot be imported
*protocol.ts. Changing either location must synchronize the other location. Therefore {@link childMain} only allows import **type** (compile-time erasure),
* Never introduce runtime package dependencies.
 */

import type { ChildPayload } from "./protocol.js";

/** The entire surface of `node:vm` used by {@link childMain} (as narrow as just two functions - this is the vm contract of the child process). */
export interface ChildVmModule {
  createContext(sandbox: object, options?: { name?: string }): object;
  runInContext(code: string, sandbox: object, options?: { filename?: string }): unknown;
}

/** The `readline.Interface` surface used by {@link childMain}. */
export interface ChildReadlineInterface {
  on(event: "line", listener: (line: string) => void): unknown;
  close(): void;
}

/**
* Host capabilities injected into {@link childMain}. **The two startup methods each provide real implementation**: The entry file starts from
* `node:vm` / `node:readline` / `process` is fetched, the SEA subcommand is fetched from the CLI process.
 */
export interface ChildMainDeps {
  vm: ChildVmModule;
  createInterface(options: { input: NodeJS.ReadableStream }): ChildReadlineInterface;
/** The source of the inbound response (parent process stdin pipe). Holding it also keeps the event loop alive. */
  stdin: NodeJS.ReadableStream;
/** The destination for outbound NDJSON; {@link childMain} adds newlines by itself. */
  stdout: { write(chunk: string): unknown };
  /**
* The payload of this run ({@link import("./protocol.js").ChildPayload}). The entry file treats it as JSON
* Literals are embedded and passed in as-is - no more argv, no more base64.
   */
  payload: ChildPayload;
}

/**
* The **only** exit for sandbox processes. The returned promise is fulfilled after the script execution terminates (complete is emitted, stdin is closed).
 *
* Why it must be awaitable: The SEA subcommand runs in the CLI process, and the CLI entry will hang for 1s after `run()` returns.
* Exit watchdog (`scheduleCliExitWatchdog` of shutdown.ts). If the child process logic is just "install readline"
* Just return synchronously", watchdog will force the entire sub-process to exit when the run just starts. This promise is not needed when the entry file is started.
* (The event loop naturally exits when it is empty), but the two entries share the same implementation, so the termination signal is uniformly given by childMain.
 *
* ⚠ Self-contained constraints (loadability, not style). {@link renderChildEntry} relies on `childMain.toString()` to convert this function
* When **source code** is embedded, the function body must be a program that can be established independently. Two rules:
* 1. Only reference Node globals such as parameters, language intrinsics and `Buffer`, **never reference any bindings in the module scope**
* (constants, auxiliary functions, import). The sandbox bootstrap is therefore inline in the function body, rather than module-level constants.
* 2. **Inner functions are not allowed to have names** - For reasons and pitfalls encountered, see the comment in the function body (esbuild's
* `minify + keepNames` will apply the module-scoped `__name` helper to them).
* Both constraints need to be verified in real packaging/compression form. This type of regression cannot be caught by source code testing alone.
 */
export function childMain(deps: ChildMainDeps): Promise<void> {
  /**
* Boot script that runs within the vm context (pure JS, no backtick/${} for safe inlining).
* Define `__host` (Boundary A shims), `__deliver` (consume inbound response), `__execute` (run lowered fn),
* and impose runtime bans (Date.now / argless new Date() / Math.random) - belt; compile-time diagnostics are suspenders.
   */
  const BOOTSTRAP = String.raw`
"use strict";

var __nextLocal = 0;
var __nextReq = 0;
var __pending = new Map();

function __emit(obj) {
  __send(JSON.stringify(obj));
}

function __createActor(siteId, name, persona) {
// Synchronously create the child-local handle and forget about create-actor; the parent process completes the mapping according to stdio FIFO before subsequent ask.
  var localId = "local#" + (++__nextLocal);
  __emit({ kind: "create-actor", localId: localId, siteId: siteId, name: name, persona: persona });
  return localId;
}

function __ask(siteId, actor, instructions) {
  var id = "r" + (++__nextReq);
  return new Promise(function (resolve, reject) {
    __pending.set(id, { resolve: resolve, reject: reject });
    __emit({ kind: "request", id: id, type: "ask", siteId: siteId, actor: actor, instructions: instructions });
  });
}

function __worldRead(siteId, op, args) {
// args is the lowering packaged position actual parameter group; this shim is transparently transmitted as it is, without looking at the op or verifying the arity (owned by the driver).
  var id = "r" + (++__nextReq);
  return new Promise(function (resolve, reject) {
    __pending.set(id, { resolve: resolve, reject: reject });
    __emit({ kind: "request", id: id, type: "world-read", siteId: siteId, op: op, args: args });
  });
}

function __log(message) {
  __emit({ kind: "event", type: "log", message: String(message) });
}

function __enterPhase(name) {
// Stage mark: No site, no journal, only let the engine send one event - so go through the event channel with the log.
  __emit({ kind: "event", type: "phase-entered", name: String(name) });
}

function __publishArtifact(siteId, op, args) {
// The content product is an effect: the script awaits it and can catch its rejection, so go with ask / world-read
// request/response. args is lowering packaged positional arguments ([id, path|content, opts]), which are passed through as they are.
  var id = "r" + (++__nextReq);
  return new Promise(function (resolve, reject) {
    __pending.set(id, { resolve: resolve, reject: reject });
    __emit({ kind: "request", id: id, type: "publish-artifact", siteId: siteId, artifactOp: op, args: args });
  });
}

function __declareArtifact(siteId, op, args) {
// The preset product is a statement: synchronously returns void, there is nothing to wait for, so the event channel is used. It shares the same clause with report
// FIFO - a tagged report must arrive at the parent process later than its declaration, and order is guaranteed by this.
  __emit({ kind: "event", type: "declare-artifact", siteId: siteId, op: op, args: args });
}

function __report(siteId, item, artifactId) {
// Fire-and-forget: the script never awaits it, so uses the event channel instead of request/response. Parent process by siteId ×
// ordinal drops a line into the journal, so the **arrival order** of this message is load-bearing (stdio FIFO guarantees it).
// If there is a loop in item, the JSON.stringify here will throw an error - at the call point of report(), the script will either catch
// Or let run fail. Both are better than silently emitting a complete item; the parent process has the same guardrails.
// When artifactId is absent, JSON.stringify directly discards the key, so the online result is "no label".
  __emit({ kind: "event", type: "report", siteId: siteId, item: item, artifactId: artifactId });
}

// -- Run arguments (args of Boundary A) --
// Like the budget, it is constructed using JSON.parse within the context, so the script gets the context-native object.
// The intrinsics of the outer realm are not included at all (the cross-realm convergence constraints at the top of this document).
// ⚠ Object.freeze is a **shallow** freeze: nested objects are not frozen, and scripts can still modify args.foo.bar. v1 accept this
// Boundary - it blocks "hand-swipe reassignment of args", not a safety boundary (the actual parameters are originally given by the caller).
var __args = Object.freeze(JSON.parse(__argsJson));

globalThis.__host = {
  args: __args,
  createActor: __createActor,
  ask: __ask,
  worldRead: __worldRead,
  publishArtifact: __publishArtifact,
  declareArtifact: __declareArtifact,
  report: __report,
  log: __log,
  enterPhase: __enterPhase,
};

// —— Inbound response consumption (called by the outer realm as a line string) ——
globalThis.__deliver = function (line) {
  var msg = JSON.parse(line);
  if (msg.kind !== "response") return;
  var waiter = __pending.get(msg.id);
  if (!waiter) return;
  __pending.delete(msg.id);
  if (msg.ok) {
    waiter.resolve(msg.value);
    return;
  }
// Refuse to cross the boundary with context-native Error, and bring code/violations/finalText for script try/catch structured processing.
  var e = msg.error || {};
  var err = new Error(e.message || "workflow host error");
  if (e.name) err.name = e.name;
  if (e.code !== undefined) err.code = e.code;
  if (e.violations !== undefined) err.violations = e.violations;
  if (e.finalText !== undefined) err.finalText = e.finalText;
  waiter.reject(err);
};

function __complete(ok, payload) {
  if (ok) {
    __emit({ kind: "complete", ok: true, value: payload });
    return;
  }
  var wire;
  if (payload instanceof Error) {
    wire = {
      name: payload.name,
      message: payload.message,
      stack: payload.stack,
      code: payload.code,
      violations: payload.violations,
      finalText: payload.finalText,
    };
  } else {
    wire = { name: "Error", message: String(payload) };
  }
  __emit({ kind: "complete", ok: false, error: wire });
}

// The outer realm calls it to run the script; it always resolves (the error has been converted into a complete message), which facilitates the outer finishing (closing stdin).
globalThis.__execute = async function (runFn) {
  try {
    var value = await runFn(globalThis.__host);
    __complete(true, value);
  } catch (e) {
    __complete(false, e);
  }
};

// —— Runtime bans (belt; compile diagnostics are suspenders): Date.now / argless new Date() / Math.random ——
var __NativeDate = Date;
class __WorkflowDate extends __NativeDate {
  constructor() {
    if (arguments.length === 0) throw new Error("argless new Date() is disabled in workflows");
    super(...arguments);
  }
  static now() {
    throw new Error("Date.now() is disabled in workflows");
  }
  static parse(value) {
    return __NativeDate.parse(value);
  }
  static UTC() {
    return __NativeDate.UTC.apply(__NativeDate, arguments);
  }
}
globalThis.Date = __WorkflowDate;
Math.random = function () {
  throw new Error("Math.random() is disabled in workflows");
};
`;

// ⚠ The second self-contained constraint (see the comment above the function) is easier to step on than "don't import": **Inner functions are not allowed to have names**.
// Root cause (I actually stepped on it before writing this test): CLI's desktop-agent is built with `minify + keepNames`,
// Under that combination, esbuild will **infer the name** of the inner function (variable declaration `const send = ...`, function declaration,
// Object literal attribute) is rewritten as `__name(fn, "send")`, and `__name` is a helper injected in the module scope.
// Once it appears in the text of `toString()`, the embedded subprocess program will raise ReferenceError——**Only in compressed
// The released product contains bad **, and the source code test is all green. Member assignment (`sandbox.__send = ...`) and anonymous functions in actual parameter positions are not overridden.
//So these two forms will be used below. During verification, you need to run the actual packaged and compressed product.

  const payload = deps.payload;

// Independent realm: bare context only has ES intrinsics, injected __send transmission + actual parameter JSON.
// Absent arguments (inline run, old journal lines) are encoded as "{}": `args` constant definitions are script-side invariants,
// This side of the sandbox is where it's set up.
  const sandbox = {
    __argsJson: JSON.stringify(payload.args ?? {}),
  } as {
    __send: (line: string) => void;
    __argsJson: string;
    __deliver: (line: string) => void;
    __execute: (runFn: unknown) => Promise<void>;
  };
  sandbox.__send = (line) => {
    deps.stdout.write(`${line}\n`);
  };
  deps.vm.createContext(sandbox, { name: "workflow-sandbox" });
  deps.vm.runInContext(BOOTSTRAP, sandbox, { filename: "workflow-bootstrap.js" });

// Compile lowered function body (context-native async fn; its await generates context Promise, import() without callback will throw an error).
  let runFn: unknown;
  try {
    runFn = deps.vm.runInContext(`(async (__host) => {\n${payload.lowered}\n})`, sandbox, {
      filename: "workflow-script.js",
    });
  } catch (error) {
// Lowered body compilation fails: terminate directly after sending error-complete. Deliberately **not** call process.exit - written to the pipeline
// stdout is asynchronous, and exiting immediately will truncate the complete message just sent (the parent process will only see "exited without completion" instead.
// Discard SyntaxError details). At this time, there is no reader in the event loop, and the child process will exit naturally after flushing.
    const err = error as { name?: string; message?: string; stack?: string } | undefined;
    deps.stdout.write(
      `${JSON.stringify({
        kind: "complete",
        ok: false,
        error: {
          name: err?.name ?? "SyntaxError",
          message: err?.message ?? String(error),
          stack: err?.stack,
        },
      })}\n`,
    );
    return Promise.resolve();
  }

// stdin keeps the event loop alive; each line is fed to the context's __deliver.
  const reader = deps.createInterface({ input: deps.stdin });
  reader.on("line", (line: string) => {
    if (!line.trim()) return;
    sandbox.__deliver(line);
  });

//Run the script; close stdin at the end, and the process will naturally exit with the idle event loop (the parent process will also be killed after receiving complete).
  return sandbox.__execute(runFn).then(
    () => reader.close(),
    () => reader.close(),
  );
}

/** In the entry file name, all runId characters outside the safe character set will be replaced by `_` (the file name and header comments share the same sanitization). */
function safeRunId(runId: string): string {
  return runId.replace(/[^A-Za-z0-9._-]/g, "_");
}

/**
* Rendering sandbox entry file: a self-contained ESM, harness writes it to disk and starts it with `node <entry>`.
 *
* Content: payload as JSON literal (the output of `JSON.stringify` is a legal JS expression), via `toString()`
* Embedded {@link childMain}, exported `start(deps)`, and two pieces of top-level logic:
* - Heap upper limit best-effort: The default path is taken into effect by the true flag `--max-old-space-size`; SEA cannot be passed from re-exec
* Flag, `v8.setFlagsFromString` is used only when `execArgv` does not have it (the effect is not guaranteed, it is the same as the atomic command side);
* - Autostart judgment: `realpath(argv[1])` is the same path as this file (`node <entry>`) with process
* vm/readline/stdio starts automatically; judged to be false when SEA subcommand `import()`, the subcommand calls `start`.
* Compare realpath instead of bare path: macOS's tmpdir via `/var → /private/var` symlink,
* `import.meta.url` is the real path after parsing. Naked comparison will cause the child process to silently not start.
 *
* The only interpolation is between childMain's source text and the payload JSON. It's not a problem for childMain to bring its own template literal - interpolation is
* **Run-time string concatenation**, the embedded text will not be parsed again; the real risk lies in the self-contained constraints of childMain
* Which side (see its note). The generated top-level code deliberately does not use template literals to avoid fighting with the outer `${}`.
 */
export function renderChildEntry(payload: ChildPayload, meta: { runId: string }): string {
  const runId = safeRunId(meta.runId);
  return `// zcode dynamic workflow run ${runId}
// Generated by @zcode/dynamic-workflow-runtime before every launch; safe to delete once the run has settled.
import { realpathSync } from "node:fs";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import { setFlagsFromString } from "node:v8";
import vm from "node:vm";

export const payload = ${JSON.stringify(payload)};

const main = ${childMain.toString()};

export const start = (deps) => main({ ...deps, payload });

if (
  typeof payload.maxOldSpaceSizeMb === "number" &&
  Number.isFinite(payload.maxOldSpaceSizeMb) &&
  payload.maxOldSpaceSizeMb > 0 &&
  !process.execArgv.some((arg) => arg.startsWith("--max-old-space-size"))
) {
  try {
    setFlagsFromString("--max-old-space-size=" + Math.trunc(payload.maxOldSpaceSizeMb));
  } catch {
// best-effort: V8 may no longer pay attention to flags that have been consumed during the startup period, and failure shall not affect the run.
  }
}

let isProcessEntry = false;
try {
  isProcessEntry =
    process.argv[1] !== undefined &&
    realpathSync(process.argv[1]) === realpathSync(fileURLToPath(import.meta.url));
} catch {
  isProcessEntry = false;
}
if (isProcessEntry) {
  void start({ vm, createInterface, stdin: process.stdin, stdout: process.stdout });
}
`;
}
