// Fixture compile kernel for `src/compile/kernel.rs` unit tests.
//
// Same protocol as the real kernel (`zcode-workflow-compile/1`) but zero dependencies and
// canned answers: the tests must exercise framing, id correlation, timeouts, crash
// recovery and the cache without a TypeScript build. Modes arrive on argv so tests never
// mutate process-global env (cargo runs test functions concurrently):
//
// node compile-kernel.fixture.mjs --mode=normal|crash-once|hang-once|noise|fatal [--marker=<path>]
//
// `crash-once` / `hang-once` use the marker file as a one-shot latch: the first process
// consumes it (and crashes or hangs), the restarted process sees it and answers. That is
// exactly what "restart once for the next request" needs to be observable from outside.

import { existsSync, writeFileSync } from "node:fs";
import { createInterface } from "node:readline";

const PROTOCOL = "zcode-workflow-compile/1";

function arg(name, fallback) {
  const prefix = `--${name}=`;
  const found = process.argv.slice(2).find((value) => value.startsWith(prefix));
  return found === undefined ? fallback : found.slice(prefix.length);
}

const mode = arg("mode", "normal");
const marker = arg("marker", "");

if (mode === "fatal") {
  process.stdout.write(
    `${JSON.stringify({ kind: "fatal", message: "fixture kernel refuses to start (fatal mode)" })}\n`,
    () => process.exit(2),
  );
} else {
  process.stdout.write(`${JSON.stringify({ kind: "hello", protocol: PROTOCOL })}\n`);
}

/** One-time latch: true means "this is a new process after restarting", false means "the first process is just the time to fail". */
function latchOpen() {
  if (marker === "") return true;
  if (existsSync(marker)) return true;
  writeFileSync(marker, "1");
  return false;
}

let compileCount = 0;

function cannedCompile(scriptText) {
  return {
    compileOk: true,diagnostics: [],
    lowered: `// fixture for ${scriptText.length} chars\nconst a = __host.createActor("actor#1", "planner", "p");\nreturn { ok: true };`,
    siteIds: ["actor#1", "ask#1"],
    askSpecs: {
      "ask#1": { typed: true, schema: { type: "object", properties: { steps: { type: "array" } } } },
      "ask#2": { typed: false },
    },
    declaredRunCommands: [],
    actorSubmitProfiles: {
      "actor#1": { kind: "mono", schema: { type: "object" } },
    },
  };
}

function respond(id, payload, after) {
  process.stdout.write(`${JSON.stringify({ id, ...payload })}\n`, () => {
    if (after !== undefined) after();
  });
}

function handle(message) {
  const id = message.id ?? null;
  switch (message.method) {
    case "ping":
      respond(id, { ok: true, result: { node: process.version } });
      return;
    case "shutdown":
      respond(id, { ok: true, result: {} }, () => process.exit(0));
      return;
    case "analyze":
      respond(id, {
        ok: true,
        result: {
          ok: true,
          diagnostics: [],
          declaredArtifacts: [{ id: "summary", kind: "markdown" }],
          core: { sites: { actors: [{ id: "actor#1" }] } },
        },
      });
      return;
    case "compile": {
      compileCount += 1;
      if (mode === "crash-once" && !latchOpen()) {
        process.exit(7);
      }
      if (mode === "hang-once" && !latchOpen()) {
        // Keep the process alive but never answer: the host must timeout, kill, and restart on the next request.
        return;
      }
      if (mode === "noise") {
        // Late old response + an irrelevant line: the host must associate by id and cannot treat the first one as the answer.
        respond(Number(id) + 1000, { ok: true, result: cannedCompile("stale") });
        process.stdout.write(`${JSON.stringify({ kind: "hello", protocol: PROTOCOL })}\n`);
      }
      if (compileCount > 1) {// Normal mode only answers once: the second time you get here, it means a cache miss.
        respond(id, { ok: false, error: "fixture received a second compile; the cache should have served it" });
        return;
      }
      respond(id, { ok: true, result: cannedCompile(String(message.params?.scriptText ?? "")) });
      return;
    }
    default:
      respond(id, { ok: false, error: `unknown method: ${String(message.method)}` });
  }
}

if (mode !== "fatal") {
  const reader = createInterface({ input: process.stdin, crlfDelay: Infinity });
  reader.on("line", (line) => {
    const text = line.trim();
    if (text === "") return;
    let message;
    try {
      message = JSON.parse(text);
    } catch {
      respond(null, { ok: false, error: "request is not JSON" });
      return;
    }
    handle(message);
  });
  reader.on("close", () => process.exit(0));
}