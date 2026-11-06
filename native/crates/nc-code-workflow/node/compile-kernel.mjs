// zcode workflow compile kernel — protocol `zcode-workflow-compile/1`.
//
// Compile once = the same TypeScript Program feeds diagnostics, site table, schema synthesis, submit profile and
// lowering (corresponds to bootstrap's compileProgram). The host only does transmission and hashing, and does not recalculate any step:
// The only truth about front-end semantics is still in the dynamic-workflow package. This file is only responsible for long-term resident and reuse of its BUILT products.
//
// Lines: NDJSON on stdin/stdout, one request, one response, associated by id.
// in : { id, method, params }
// out: { id, ok: true, result } | { id, ok: false, error }
// Start by sending { kind: "hello", protocol }; when dist is missing/damaged, send { kind: "fatal", message } and exit 2.

import { existsSync, statSync, writeSync } from "node:fs";
import path from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath, pathToFileURL } from "node:url";

const PROTOCOL = "zcode-workflow-compile/1";

const KERNEL_DIR = path.dirname(fileURLToPath(import.meta.url));
// node/ → zcode-workflow → crates → native → repo root
const DEFAULT_DIST = path.resolve(
  KERNEL_DIR,
  "../../../../apps/zcode-cli/packages/dynamic-workflow/dist/index.js",
);

/** Fatal error during startup: one line of JSON to stdout, exit code 2 (the host will give an actionable error accordingly). */
function fatal(message) {
  try {
    writeSync(1, `${JSON.stringify({ kind: "fatal", message })}\n`);
  } catch {
    // stdout is closed: the exit code still expresses intent.
  }
  process.exit(2);
}

function describe(error) {
  return error instanceof Error ? error.message : String(error);
}

/** Parsing dist entry: env ZCODE_WORKFLOW_DIST (file or directory) takes precedence, otherwise the default path of this warehouse is used. */
function resolveDist() {
  const configured = process.env.ZCODE_WORKFLOW_DIST;
  const candidate =
    configured !== undefined && configured.trim() !== "" ? path.resolve(configured) : DEFAULT_DIST;
  if (!existsSync(candidate)) {
    throw new Error(
      `cannot find the dynamic-workflow dist entry at ${candidate}; ` +
        "build apps/zcode-cli/packages/dynamic-workflow (dist/index.js) or point ZCODE_WORKFLOW_DIST at it",
    );
  }
  const entry = statSync(candidate).isDirectory() ? path.join(candidate, "index.js") : candidate;
  if (!existsSync(entry)) {
    throw new Error(
      `${candidate} exists but ${entry} does not; ` +
        "ZCODE_WORKFLOW_DIST must name dist/index.js or the dist directory",
    );
  }
  return entry;
}

/** All front-end entries that compileProgram depends on (each one must be a function, missing one means a wiring error). */
const REQUIRED_EXPORTS = [
  "analyzeWorkflowScript",
  "buildAskSpecs",
  "collectDiagnostics",
  "collectSites",
  "collectWorldRunCommands",
  "createWorkflowProgram",
  "deriveActorSubmitProfilesFor",
  "lowerWorkflow",
  "synthesizeAskSchemas",
];

let distPath;
let distUrl;
let facade;

try {
  distPath = resolveDist();
  distUrl = pathToFileURL(distPath);
  facade = await import(distUrl.href);
} catch (error) {
  fatal(`workflow compile kernel cannot load the dynamic-workflow dist: ${describe(error)}`);
}

const missing = REQUIRED_EXPORTS.filter((name) => typeof facade[name] !== "function");
if (missing.length > 0) {
  fatal(`${distPath} is missing required exports: ${missing.join(", ")}`);
}

// facade-siting gate: The package is not exported from index, but the dirty script gate of lowering uses it.
// (lowerWorkflowScript is called after typecheck and before lower). Take it according to dist internal module path,
// Parsed in the same directory as index, there is no need to change this when dist is changed. Failure to obtain it is fatal - without it, the facade escapes
// The handle will be silently downgraded to a call without site, which is the script that this gate is rejecting.
let collectFacadeMisuse = facade.collectFacadeMisuse;
if (typeof collectFacadeMisuse !== "function") {
  try {
    const module = await import(new URL("./analysis/facade-misuse.js", distUrl).href);
    collectFacadeMisuse = module.collectFacadeMisuse;
  } catch (error) {
    fatal(
      `workflow compile kernel cannot load the facade-siting gate` +
        `(analysis/facade-misuse.js next to ${distPath}): ${describe(error)}`,
    );
  }
}
if (typeof collectFacadeMisuse !== "function") {
  fatal(`${distPath} does not expose the facade-siting gate collectFacadeMisuse`);
}

const {
  analyzeWorkflowScript,
  buildAskSpecs,
  collectDiagnostics,
  collectSites,
  collectWorldRunCommands,
  createWorkflowProgram,
  deriveActorSubmitProfilesFor,
  lowerWorkflow,
  synthesizeAskSchemas,
} = facade;

/** Diagnostic line shape: only retain positioning and copywriting (1-based line/column, author script coordinates). */
function diagnosticsJson(diagnostics) {
  return diagnostics.map((diagnostic) => ({
    code: typeof diagnostic.code === "number" ? diagnostic.code : 0,
    column: typeof diagnostic.column === "number" ? diagnostic.column : 1,
    line: typeof diagnostic.line === "number" ? diagnostic.line : 1,
    message:
      typeof diagnostic.message === "string"
        ? diagnostic.message
        : String(diagnostic.message ?? ""),
  }));
}

/**
 * Compile once: strictly follow the order of compileProgram.
 *
 * typecheck diagnostic → collectSites → facade-siting diagnostic → synthesizeAskSchemas (schema diagnostic)
 * → collectWorldRunCommands (non-literal cmd diagnostics) → buildAskSpecs
 * → deriveActorSubmitProfilesFor → lowerWorkflow.code
 *
 * If any diagnosis is not empty, `compileOk: false` + diagnostic array: the diagnosis is data, not transmission error,
 * The production side throws out accordingly (resume directly displays the `L:C message` to the user).
 */
function compileScript(scriptText) {
  const workflow = createWorkflowProgram(scriptText);

  // Same Program diagnostic (syntax + semantics) as compileProgram. Collect collectDiagnostics:
  // It flattens the messageText into copy and converts the position back to author script coordinates (the prelude line is subtracted).
  const diagnostics = collectDiagnostics(workflow.program);
  if (diagnostics.length > 0) {
    return { compileOk: false, diagnostics: diagnosticsJson(diagnostics) };
  }

  const table = collectSites(workflow);

  // The gate of lowerWorkflowScript: if the facade handle escapes to the value space, there will be no site, and the downgrade must be rejected.
  const misuse = collectFacadeMisuse(workflow, table);
  if (misuse.length > 0) {
    return { compileOk: false, diagnostics: diagnosticsJson(misuse) };
  }

  const { diagnostics: schemaDiagnostics, schemas } = synthesizeAskSchemas(workflow, table);
  if (schemaDiagnostics.length > 0) {
    return { compileOk: false, diagnostics: diagnosticsJson(schemaDiagnostics) };
  }

  // Authorization side of world.run: The command set must be a compile-time literal (confirmation window display + driver review).
  const worldRun = collectWorldRunCommands(workflow, table);
  if (worldRun.diagnostics.length > 0) {
    return { compileOk: false, diagnostics: diagnosticsJson(worldRun.diagnostics) };
  }

  // buildAskSpecs is the only correct construct for askSpecs: untyped site explicit notation {typed:false}.
  const askSpecs = buildAskSpecs(table, schemas);
  const actorSubmitProfiles = deriveActorSubmitProfilesFor(workflow, table, askSpecs);
  const lowered = lowerWorkflow(workflow, table);

  const askSpecsJson = {};
  for (const [siteId, spec] of askSpecs) {
    askSpecsJson[siteId] =
      spec.schema === undefined
        ? { typed: spec.typed === true }
        : { typed: spec.typed === true, schema: spec.schema };
  }
  const profilesJson = {};
  for (const [siteId, profile] of actorSubmitProfiles) {
    profilesJson[siteId] = profile;
  }

  return {
    compileOk: true,
    diagnostics: [],
    lowered: lowered.code,
    siteIds: [...lowered.siteIds],
    askSpecs: askSpecsJson,
    declaredRunCommands: [...worldRun.commands],
    actorSubmitProfiles: profilesJson,
  };
}

/**
 * Analyze results are converted to JSON safe format: Map → Object, Set → Array, Date → ISO, function/undefined are discarded.
 * "Visited" only records the ancestor chain (deleted when popped out of the stack), the shared subtree (DAG) is expanded as usual, and the true link is recorded as "[circular]".
 */
function jsonSafe(value, ancestors = new WeakSet()) {
  if (value === null) return null;
  const type = typeof value;
  if (type === "function" || type === "undefined" || type === "symbol") return undefined;
  if (type !== "object") return value;
  if (value instanceof Date) return value.toISOString();
  if (ancestors.has(value)) return "[circular]";

  ancestors.add(value);
  let out;
  if (value instanceof Map) {
    out = {};
    for (const [key, entry] of value) {
      const safe = jsonSafe(entry, ancestors);
      if (safe !== undefined) out[String(key)] = safe;
    }
  } else if (value instanceof Set) {
    out = [];
    for (const entry of value) {
      const safe = jsonSafe(entry, ancestors);
      if (safe !== undefined) out.push(safe);
    }
  } else if (Array.isArray(value)) {
    out = value.map((entry) => {
      const safe = jsonSafe(entry, ancestors);
      return safe === undefined ? null : safe;
    });
  } else {
    out = {};
    for (const [key, entry] of Object.entries(value)) {
      const safe = jsonSafe(entry, ancestors);
      if (safe !== undefined) out[key] = safe;
    }
  }
  ancestors.delete(value);
  return out;
}

/** Write a response line; `after` is executed after the line is placed (shutdown uses it to ensure that the response has been sent before exiting). */
function respond(id, payload, after) {
  const line = `${JSON.stringify({ id, ...payload })}\n`;
  process.stdout.write(line, () => {
    if (after !== undefined) after();
  });
}

async function handleLine(line) {
  let message;
  try {
    message = JSON.parse(line);
  } catch (error) {
    respond(null, { ok: false, error: `request is not JSON: ${describe(error)}` });
    return;
  }
  if (message === null || typeof message !== "object") {
    respond(null, { ok: false, error: "request is not a JSON object" });
    return;
  }

  const id = message.id ?? null;
  const method = message.method;
  const params = message.params ?? {};

  try {
    switch (method) {
      case "compile": {
        if (typeof params.scriptText !== "string") {
          throw new Error("compile.params.scriptText must be a string");
        }
        respond(id, { ok: true, result: compileScript(params.scriptText) });
        return;
      }
      case "analyze": {
        if (typeof params.scriptText !== "string") {
          throw new Error("analyze.params.scriptText must be a string");
        }
        respond(id, { ok: true, result: jsonSafe(analyzeWorkflowScript(params.scriptText)) });
        return;
      }
      case "ping": {
        respond(id, { ok: true, result: { node: process.version } });
        return;
      }
      case "shutdown": {
        respond(id, { ok: true, result: {} }, () => process.exit(0));
        return;
      }
      default: {
        throw new Error(`unknown method: ${String(method)}`);
      }
    }
  } catch (error) {
    // Any handler exception only pollutes this response and does not kill the process: bad scripts/bad input cannot bring down the host's kernel.
    respond(id, { ok: false, error: describe(error) });
  }
}

process.stdout.on("error", () => process.exit(0));

process.stdout.write(`${JSON.stringify({ kind: "hello", protocol: PROTOCOL })}\n`);

// Serial processing: Compilation in the same process is not concurrent (the front-end Program build itself is synchronous CPU activity),
// The response order is therefore consistent with the request order; the id is still associated with the host.
const reader = createInterface({ input: process.stdin, crlfDelay: Infinity });
let queue = Promise.resolve();
reader.on("line", (line) => {
  const text = line.trim();
  if (text === "") return;
  queue = queue.then(() => handleLine(text)).catch(() => {});
});
reader.on("close", () => {
  // The host closes stdin (process exits / killed_on_drop): the kernel exits accordingly, leaving no orphans.
  process.exit(0);
});
