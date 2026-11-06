/* eslint-disable max-lines -- vendored Node kernel driver: it is one self-contained script executed by the user's Node (specs/rust-pure-native-roadmap.md §7 R2); splitting it would break the single-file sandbox contract. */
// zcode-node-repl kernel driver.
//
// This file is a script executed by the user's own Node, not a JS engine distributed with the application
// (specs/rust-pure-native-roadmap.md §7 R2: There is no JS runtime in the shipped app).
// The `spawn` on the Rust side is the `node` detected in the PATH / standard installation location. This script is only responsible for
// Maintain a persistent kernel in this Node and respond to requests with stdin/stdout NDJSON.
//
// TypeScript source of semantic mirroring (aligned line by line, read these lines before making changes):
// apps/zcode-cli/packages/core/src/repl/node-repl-session.ts:144-234 sandbox globals / scoped timers
// apps/zcode-cli/packages/core/src/repl/node-repl-session.ts:236-320 console tee / emitImage / meta / structured
// apps/zcode-cli/packages/core/src/repl/node-repl-session.ts:368-453 run() output assembly + force reset after cancel/timeout
// apps/zcode-cli/packages/core/src/repl/executors.ts:34-95 async-IIFE + vm synchronization budget + AbortSignal race
// apps/zcode-cli/packages/core/src/repl/instrument.ts:98-206 Top-level declaration persists across calls + end expression return
// apps/zcode-cli/packages/core/src/repl/node-repl-runtime-helpers.ts:5-72 process facade / require facade / stringify / error normalization
//
// All known differences with TS are written in the DEVIATIONS comment block at the end of this file and are not hidden.

import { createContext, runInContext, Script } from "node:vm";
import { createRequire } from "node:module";
import Module from "node:module";
import { inspect } from "node:util";
import { homedir, tmpdir } from "node:os";

export const KERNEL_PROTOCOL = "zcode-node-repl-kernel/1";

// The reset prompt copy of node-repl-session.ts:436, byte-by-byte mirroring: model and skill documents are judged based on this
// "After a timeout, all bindings are gone." In other words, it is behavior drift.
const RESET_SUFFIX =
  "; kernel reset, all previous bindings were cleared; reinitialize browser/tab bindings before rerunning";

// node-repl-runtime-helpers.ts:3 - A restricted set of importModule/require replacements for process.
const PROCESS_MODULE_IDS = new Set(["process", "node:process"]);

const MAX_CAPTURED_LOG_BYTES = 4 * 1024 * 1024;//── CLI input (args array, never go through shell)──────────────────────────────────
const argv = process.argv.slice(2);
const restrictProcess = argv.includes("--restrict-process");
const extraModuleDirs = [];
for (let index = 0; index < argv.length; index += 1) {
  if (argv[index] === "--module-dir" && typeof argv[index + 1] === "string") {
    extraModuleDirs.push(argv[index + 1]);
    index += 1;
  }
}

// ── NDJSON transport ─────────────────────────────────────────────────
// stdout is a protocol channel. Any non-protocol bytes will cause the Rust side to lose frames, so only writeFrame is used in the driver.
function writeFrame(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}

//── node-repl-runtime-helpers.ts:5-24: restricted process facade (Fresh/MCP host path)──
function createRestrictedProcessFacade() {
  return Object.freeze({
    arch: process.arch,
    argv: Object.freeze([...process.argv]),
    cwd: () => process.cwd(),
    env: Object.freeze({ ...process.env }),
    execArgv: Object.freeze([...process.execArgv]),
    execPath: process.execPath,
    hrtime: process.hrtime.bind(process),
    memoryUsage: process.memoryUsage.bind(process),
    nextTick: process.nextTick.bind(process),
    pid: process.pid,
    platform: process.platform,
    release: Object.freeze({ ...process.release }),
    resourceUsage: process.resourceUsage.bind(process),
    uptime: process.uptime.bind(process),
    version: process.version,
    versions: Object.freeze({ ...process.versions }),
  });
}

const restrictedFacade = restrictProcess ? createRestrictedProcessFacade() : undefined;

// node-repl-runtime-helpers.ts:26-40: require facade - process uses restricted objects, and the rest transparently uses require.
function createReplRequire(baseRequire) {
  if (!restrictedFacade) return baseRequire;
  const facade = ((specifier) =>PROCESS_MODULE_IDS.has(specifier) ? restrictedFacade : baseRequire(specifier));
  facade.cache = baseRequire.cache;
  facade.extensions = baseRequire.extensions;
  facade.main = baseRequire.main;
  facade.resolve = baseRequire.resolve;
  return facade;
}

//──Module parsing path expansion (mechanism of js_add_node_module_dir)────────────────────────
//The candidate node_modules directory is determined by Module._nodeModulePaths; just add the directory of this kernel
// Let the bare specifier hit the node_modules that comes with the user/skill. Explicit failure when private API is missing,
// Do not downgrade silently (fail-closed is consistent with R2 acceptance).
const moduleDirs = [...extraModuleDirs];
let nodeModulePathsPatched = false;
function patchModuleResolution() {
  if (nodeModulePathsPatched) return true;
  if (typeof Module._nodeModulePaths !== "function") return false;
  const original = Module._nodeModulePaths;
  Module._nodeModulePaths = function patched(from) {
    const paths = original.call(this, from);
    for (const dir of moduleDirs) if (!paths.includes(dir)) paths.push(dir);
    return paths;
  };
  nodeModulePathsPatched = true;
  return true;
}
for (const dir of moduleDirs) if (!patchModuleResolution()) break;

//──Output serialization (node-repl-runtime-helpers.ts:42-56 line-by-line mirroring)─────────────────
function stringifyReplResult(value) {
  if (value === undefined) return undefined;
  if (typeof value === "string") return value;
  if (ArrayBuffer.isView(value) && !(value instanceof DataView)) {
    // Binary returns such as screenshots are bounded by inspect to avoid JSON.stringify from expanding each byte into a numeric key (same as TS:45-49).
    return inspect(value, { maxArrayLength: 100 });
  }
  try {
    const json = JSON.stringify(value, null, 2);
    return json === undefined ? String(value) : json;
  } catch {
    return String(value);
  }
}

// node-repl-runtime-helpers.ts:59-72: Error of vm realm crosses realm, instanceof is invalid, extracted by field duck.
function normalizeReplError(error) {
  if (error && typeof error === "object") {const name = typeof error.name === "string" ? error.name : "Error";
    const message = typeof error.message === "string" ? error.message : String(error);
    const stack = typeof error.stack === "string" ? error.stack : undefined;
    return { name, message, ...(stack === undefined ? {} : { stack }) };
  }
  return { name: "Error", message: String(error) };
}

//──Output sink for each run (node-repl-session.ts:391-400)────────────────────
function createSink() {
  let buffer = "";
  return {
    buffer,
    images: [],
    structuredResults: [],
    responseMeta: {},
    write(text) {
      // Write items one by one and connect them with "\n" (node-repl-session.ts:392-394); discard and mark after exceeding the budget.
      // Protocol frames cannot expand unbounded (there is an additional 64 KiB model budget on the Rust side, see src/output.rs).
      if (this.buffer.length >= MAX_CAPTURED_LOG_BYTES) return;
      this.buffer += (this.buffer ? "\n" : "") + text;
      this.bufferText = this.buffer;
    },
    bufferText: "",
  };
}

let currentSink = null;

// node-repl-session.ts:236-244: console.* All tee to the current sink, non-string stringify.
function emit(args) {
  if (!currentSink) return;
  const text = args
    .map((value) => (typeof value === "string" ? value : stringifyReplResult(value) ?? "undefined"))
    .join(" ");
  currentSink.write(text);
}

// node-repl-session.ts:251-295: accept bytes/dataUrl/{base64}/{bytes}, illegal input parameters will throw TypeError (original text).
function emitImage(image) {
  if (!currentSink) return;
  const directBytes =
    (ArrayBuffer.isView(image) && !(image instanceof DataView)) || Array.isArray(image) ? image : null;
  const record = directBytes !== null ? { bytes: directBytes } : (image ?? {});
  let base64 = null;
  let dataUrlMimeType;
  if (typeof record.dataUrl === "string") {
    const match = /^data:([^;,]+);base64,(.+)$/u.exec(record.dataUrl);
    if (match) {
      dataUrlMimeType = match[1];base64 = match[2];
    }
  } else if (typeof record.base64 === "string" && record.base64.length > 0) {
    base64 = record.base64;
  } else if (record.bytes != null) {
    try {
      base64 = Buffer.from(record.bytes).toString("base64");
    } catch {
      base64 = null;
    }
  }
  if (!base64) {
    throw new TypeError(
      "nodeRepl.emitImage requires bytes, dataUrl, { base64 }, or { bytes }; e.g. emitImage(await tab.screenshot())",
    );
  }
  const mimeType =
    typeof record.mimeType === "string" && record.mimeType ? record.mimeType : (dataUrlMimeType ?? "image/png");
  currentSink.images.push({ base64, mimeType });
}

// node-repl-session.ts:297-303
function setResponseMeta(meta) {
  if (!currentSink) return;
  if (!meta || typeof meta !== "object" || Array.isArray(meta)) {
    throw new TypeError("nodeRepl.setResponseMeta requires a plain object");
  }
  Object.assign(currentSink.responseMeta, meta);
}

// node-repl-session.ts:305-320
function emitStructuredResult(result) {
  if (!currentSink) return;
  if (!result || typeof result !== "object" || Array.isArray(result)) {
    throw new TypeError("nodeRepl.emitStructuredResult requires a result object");
  }
  if (!Array.isArray(result.content)) {
    throw new TypeError("nodeRepl.emitStructuredResult requires a content array");
  }
  for (const block of result.content) {
    if (!block || typeof block !== "object" || typeof block.type !== "string") {
      throw new TypeError("nodeRepl.emitStructuredResult content blocks require a type");
    }
  }
  currentSink.structuredResults.push(result);
}

//──Context construction (node-repl-session.ts:144-234)───────────────────────────
let nodeReplApi = null;
let cleanupContextResources = () => {};

// Cells in Persistent mode can `process.stdout.write` - that's true Node semantics.// But that will push the bytes into the NDJSON channel (TS is executed in-process and does not have this problem). Put here
// The stdout/stderr of the sandbox is redirected to the sink, which retains the write capability without destroying the protocol frame.
function createCapturedStream(kind) {
  return {
    isTTY: false,
    writableEnded: false,
    write(chunk, _encoding, callback) {
      currentSink?.write(String(chunk));
      if (typeof _encoding === "function") _encoding();
      else if (typeof callback === "function") callback();
      return true;
    },
    end(chunk, _encoding, callback) {
      if (chunk !== undefined && chunk !== null) this.write(chunk);
      if (typeof _encoding === "function") _encoding();
      else if (typeof callback === "function") callback();
    },
    on() {
      return this;
    },
    once() {
      return this;
    },
    off() {
      return this;
    },
    removeAllListeners() {
      return this;
    },
    [Symbol.for("nodejs.util.inspect.custom")]: () => `[zcode-node-repl captured ${kind} stream]`,
  };
}

function createSandboxProcess() {
  if (restrictedFacade) return restrictedFacade;
  const capturedOut = createCapturedStream("stdout");
  const capturedErr = createCapturedStream("stderr");
  return new Proxy(process, {
    get(target, key) {
      if (key === "stdout") return capturedOut;
      if (key === "stderr") return capturedErr;
      const value = target[key];
      return typeof value === "function" ? value.bind(target) : value;
    },
  });
}

function importModule(specifier) {
  // node-repl-session.ts:455-460: The import of restricted process also goes through the facade.
  if (restrictedFacade && PROCESS_MODULE_IDS.has(specifier)) {
    return Promise.resolve({ ...restrictedFacade, default: restrictedFacade });
  }
  return import(specifier);
}

function buildContext() {
  const timeouts = new Set();
  const intervals = new Set();
  const scopedSetTimeout = (callback, delay, ...rest) => {const handle = setTimeout(() => {
      timeouts.delete(handle);
      callback(...rest);
    }, delay);
    timeouts.add(handle);
    return handle;
  };
  const scopedClearTimeout = (handle) => {
    timeouts.delete(handle);
    clearTimeout(handle);
  };
  const scopedSetInterval = (callback, delay, ...rest) => {
    const handle = setInterval(callback, delay, ...rest);
    intervals.add(handle);
    return handle;
  };
  const scopedClearInterval = (handle) => {
    intervals.delete(handle);
    clearInterval(handle);
  };
  cleanupContextResources = () => {
    for (const handle of timeouts) clearTimeout(handle);
    for (const handle of intervals) clearInterval(handle);
    timeouts.clear();
    intervals.clear();
  };
  const teeConsole = {
    log: (...args) => emit(args),
    info: (...args) => emit(args),
    warn: (...args) => emit(args),
    error: (...args) => emit(args),
    debug: (...args) => emit(args),
  };
  // Use kernel's cwd as require benchmark (node-repl-session.ts:191).
  const baseRequire = createRequire(`${process.cwd()}/`);
  const api = {
    cwd: process.cwd(),
    homeDir: homedir(),
    tmpDir: tmpdir(),
    requestMeta: {},
    write: (text) => emit([text]),
    emitImage: (image) => emitImage(image),
    emitStructuredResult: (result) => emitStructuredResult(result),
    setResponseMeta: (meta) => setResponseMeta(meta),
  };
  nodeReplApi = api;
  const sandbox = {
    console: teeConsole,
    process: createSandboxProcess(),
    Buffer,
    URL,
    URLSearchParams,
    TextEncoder,
    TextDecoder,
    setTimeout: scopedSetTimeout,
    clearTimeout: scopedClearTimeout,
    setInterval: scopedSetInterval,
    clearInterval: scopedClearInterval,
    queueMicrotask,
    structuredClone,
    require: createReplRequire(baseRequire),importModule: (specifier) => importModule(specifier),
    nodeRepl: api,
    // DEVIATION(1): TS's manifest (node-repl-session.ts:206-229) does not have fetch/AbortController,
    // And R2 explicitly promises that "fetch is consistent with today", so it injects the implementation of real Node. This is an intentional superset.
    fetch: globalThis.fetch,
    Headers: globalThis.Headers,
    Request: globalThis.Request,
    Response: globalThis.Response,
    FormData: globalThis.FormData,
    Blob: globalThis.Blob,
    AbortController: globalThis.AbortController,
    AbortSignal: globalThis.AbortSignal,
    Event: globalThis.Event,
    EventTarget: globalThis.EventTarget,
    CustomEvent: globalThis.CustomEvent,
    crypto: globalThis.crypto,
    performance: globalThis.performance,
  };
  const context = createContext(sandbox);
  // globalThis is self-referential, model available globalThis.x = ... explicit persistence (node-repl-session.ts:232).
  runInContext("globalThis.globalThis = globalThis;", context);
  return context;
}

let context = buildContext();
let generation = 0;

//── Top-level declaration persists across calls (equivalent implementation of instrument.ts:98-206)──────────────────
// TS uses meriyah AST; this kernel does not introduce parser dependencies, but instead uses deep tracking lexical scanning +
// "Copy injection of try/catch package" + fallback to the original code if compilation fails. The effect contract is consistent:
// Top-level var/let/const/function/class bindings are copied to globalThis after run ends;
//The last top-level expression statement is converted to return as the completion value of the cell (REPL echo).
// When the scan fails (the injection name does not exist), try/catch swallows the ReferenceError and does not affect the user code;
// If the syntax is illegal after conversion, the original code will be rolled back as a whole - synonymous with the rollback of executors.ts:44-50.
const REGEX_PRECEDERS = new Set([
  "(",
  ",",
  "=",
  ":",
  "[",
  "!",
  "&",
  "|",
  "?",
  "{",
  "}",
  ";",
  "+",
  "-",
  "*",
  "%",
  "~",
  "^",
  "<",
  ">",
]);
const KEYWORDS_BEFORE_REGEX = new Set([
  "return",
  "typeof",
  "instanceof",
  "in",
  "of",
  "new",
  "delete",
  "void",
  "do",
  "else",
  "case",
  "yield",
  "await",
]);

function significantBefore(code, index) {
  let cursor = index - 1;
  while (cursor >= 0) {
    const char = code[cursor];if (/\s/.test(char)) {
      cursor -= 1;
      continue;
    }
    if (/[A-Za-z0-9_$]/.test(char)) {
      let end = cursor + 1;
      while (cursor >= 0 && /[A-Za-z0-9_$]/.test(code[cursor])) cursor -= 1;
      return { word: code.slice(cursor + 1, end), char: null };
    }
    return { word: null, char };
  }
  return { word: null, char: null };
}

//Return scan results: string literals/comments/template bodies do not participate in structure judgment.
function scanTopLevel(code) {
  const declarations = [];
  const importSpans = [];
  let index = 0;
  let depth = 0;
  let last = { word: null, char: null };
  const n = code.length;
  while (index < n) {
    const char = code[index];
    // comments
    if (char === "/" && code[index + 1] === "/") {
      const end = code.indexOf("\n", index);
      index = end === -1 ? n : end;
      continue;
    }
    if (char === "/" && code[index + 1] === "*") {
      const end = code.indexOf("*/", index + 2);
      index = end === -1 ? n : end + 2;
      continue;
    }
    // Strings and templates
    if (char === '"' || char === "'") {
      index += 1;
      while (index < n && code[index] !== char) {
        index += code[index] === "\\" ? 2 : 1;
      }
      index += 1;
      last = { word: null, char };
      continue;
    }
    if (char === "`") {
      index += 1;
      while (index < n) {
        if (code[index] === "\\") {
          index += 2;
          continue;
        }
        if (code[index] === "`") {
          index += 1;
          break;
        }
        if (code[index] === "$" && code[index + 1] === "{") {
          // Nested } in template interpolation needs to be paired and skipped.
          let nested = 1;
          index += 2;
          while (index < n && nested > 0) {
            if (code[index] === "{") nested += 1;
            else if (code[index] === "}") nested -= 1;
            index += 1;
          }
          continue;}
        index += 1;
      }
      last = { word: null, char };
      continue;
    }
    // Regular literal
    if (char === "/") {
      const startsRegex =
        last.char !== null ? REGEX_PRECEDERS.has(last.char) : KEYWORDS_BEFORE_REGEX.has(last.word ?? "");
      if (startsRegex) {
        index += 1;
        let inClass = false;
        while (index < n) {
          const current = code[index];
          if (current === "\\") {
            index += 2;
            continue;
          }
          if (current === "[") inClass = true;
          else if (current === "]") inClass = false;
          else if (current === "/" && !inClass) {
            index += 1;
            break;
          }
          index += 1;
        }
        while (index < n && /[a-z]/.test(code[index])) index += 1;
        last = { word: null, char: "/" };
        continue;
      }
    }
    if (/[([{]/.test(char)) {
      depth += 1;
      index += 1;
      last = { word: null, char };
      continue;
    }
    if (/[)\]}]/.test(char)) {
      depth -= 1;
      index += 1;
      last = { word: null, char };
      continue;
    }
    if (/[A-Za-z_$]/.test(char)) {
      let end = index;
      while (end < n && /[A-Za-z0-9_$]/.test(code[end])) end += 1;
      const word = code.slice(index, end);
      if (word === "import" && depth === 0) {
        const after = code.slice(end).match(/^\s*\(/);
        if (after) importSpans.push(index);
      }
      if (depth === 0 && (word === "var" || word === "let" || word === "const")) {
        const parsed = scanDeclarators(code, end);
        if (parsed) {
          declarations.push({ kind: word, names: parsed.names, end: parsed.end });
          index = parsed.end;
          last = { word: null, char: ";" };
          continue;
        }
      }if (depth === 0 && (word === "function" || word === "class" || (word === "async" && /^\s*function/.test(code.slice(end))))) {
        const parsed = scanNamedBody(code, index, word);
        if (parsed) {
          declarations.push({ kind: "named", names: parsed.names, end: parsed.end });
          index = parsed.end;
          last = { word: null, char: "}" };
          continue;
        }
      }
      last = { word, char: null };
      index = end;
      continue;
    }
    last = { word: null, char };
    index += 1;
  }
  return { declarations, importSpans };
}

function readIdentifier(code, start) {
  const match = code.slice(start).match(/^[A-Za-z_$][A-Za-z0-9_$]*/);
  return match ? { name: match[0], end: start + match[0].length } : null;
}

// var/let/const: Collect binding names in the declaration area until the end of this statement (`;` or a newline of depth 0).
function scanDeclarators(code, afterKeyword) {
  const names = [];
  let index = afterKeyword;
  while (index < code.length && /\s/.test(code[index])) index += 1;
  let braceDepth = 0;
  let bracketDepth = 0;
  let parenDepth = 0;
  let statementEnd = -1;
  while (index < code.length) {
    const char = code[index];
    if (char === "{") braceDepth += 1;
    else if (char === "}") braceDepth -= 1;
    else if (char === "[") bracketDepth += 1;
    else if (char === "]") bracketDepth -= 1;
    else if (char === "(") parenDepth += 1;
    else if (char === ")") parenDepth -= 1;
    else if (/[A-Za-z_$]/.test(char)) {
      const read = readIdentifier(code, index);
      const prev = code.slice(0, index).match(/(\w|\s|\})$/);
      // Skip keywords and object mode property keys (`{a: b}` only binds b).
      const isKey = /^\s*:/.test(code.slice(read.end));
      const reserved = new Set(["const", "let", "var", "of", "in", "function", "class", "async"]);if (braceDepth === 0 && !reserved.has(read.name)) names.push(read.name);
      else if (braceDepth > 0 && !isKey && !reserved.has(read.name) && !prevSourceIsDot(code, index))
        names.push(read.name);
      index = read.end;
      continue;
    } else if ((char === ";" || char === "\n") && braceDepth === 0 && bracketDepth === 0 && parenDepth === 0) {
      if (char === ";") {
        statementEnd = index + 1;
        break;
      }
      // ASI: There is a complete statement before the line break and the next line is not a continuation → end of statement.
      const before = code.slice(0, index).trimEnd();
      const continues = /[,\+\-\*\/%=?:&|.<([{]$/.test(before) || /\b(const|let|var|of|in|async|function|class|return|typeof)$/.test(before);
      if (!continues) {
        statementEnd = index;
        break;
      }
    }
    index += 1;
  }
  if (statementEnd === -1) statementEnd = code.length;
  const unique = [...new Set(names)];
  return unique.length > 0 ? { names: unique, end: statementEnd } : null;
}

function prevSourceIsDot(code, index) {
  const before = code.slice(0, index).match(/(\.|\?\.)\s*$/);
  return Boolean(before);
}

// function/class: Take the name, and then jump to after the matching right bracket of the function body/class body.
function scanNamedBody(code, start, keyword) {
  let index = start;
  if (keyword === "async") {
    const rest = code.slice(index).match(/^async\s+function/);
    if (!rest) return null;
    index += rest[0].length;
  } else {
    index += keyword.length;
  }
  while (index < code.length && /\s|\*/.test(code[index])) index += 1;
  let name = null;
  const read = readIdentifier(code, index);
  if (read) {
    name = read.name;
    index = read.end;
  }
  const bodyStart = code.slice(index).indexOf("{");
  if (bodyStart === -1) return null;
  index += bodyStart;
  let depth = 0;
  while (index < code.length) {
    if (code[index] === "{") depth += 1;else if (code[index] === "}") {
      depth -= 1;
      if (depth === 0) {
        index += 1;
        break;
      }
    }
    index += 1;
  }
  return name ? { names: [name], end: index } : null;
}

function prepare(code) {
  // instrument.ts:138-149: Dynamic import() is rewritten as injected importModule().
  const { declarations, importSpans } = scanTopLevel(code);
  let rewritten = code;
  for (const start of importSpans.sort((left, right) => right - left)) {
    rewritten = `${rewritten.slice(0, start)}importModule${rewritten.slice(start + "import".length)}`;
  }
  let out = "";
  let cursor = 0;
  for (const declaration of declarations) {
    out += rewritten.slice(cursor, declaration.end);
    cursor = declaration.end;
    for (const name of declaration.names) {
      if (/^[A-Za-z_$][A-Za-z0-9_$]*$/.test(name)) out += `;try{globalThis.${name}=${name}}catch(e){}`;
    }
  }
  out += rewritten.slice(cursor);
  // instrument.ts:178-192: The last top-level expression is converted to return to return the completion value.
  out = rewriteTrailingExpression(out);
  const wrapped = `(async () => {\n${out}\n})()`;
  // Compilation self-test: When the scan is uncertain and the syntax is illegal, fall back to the original code (the fallback contract of executors.ts:44-50).
  try {
    new Script(wrapped);
  } catch {
    return `(async () => {\n${rewritten}\n})()`;
  }
  return wrapped;
}

function rewriteTrailingExpression(code) {
  const lines = code.split("\n");
  for (let index = lines.length - 1; index >= 0; index -= 1) {
    const trimmed = lines[index].trim();
    if (!trimmed || trimmed.startsWith("//")) continue;
    if (/^(return|const|let|var|function|class|if|for|while|switch|throw|try|})/.test(trimmed)) return code;
    if (/[;{}]$/.test(trimmed) && !/[;]$/.test(trimmed)) return code;
    const withoutSemicolon = trimmed.replace(/;$/, "");lines[index] = `${lines[index].slice(0, lines[index].indexOf(withoutSemicolon))}return (${withoutSemicolon});`;
    return lines.join("\n");
  }
  return code;
}

//──Request processing ─────────────────────────────────────────────────────────
const activeRuns = new Map();

function buildOutput(value, sink) {
  const result = stringifyReplResult(value);
  return {
    ...(result === undefined ? {} : { result }),
    logs: sink.bufferText,
    ...(sink.images.length > 0 ? { images: sink.images } : {}),
    ...(sink.structuredResults.length > 0 ? { structuredResults: sink.structuredResults } : {}),
    ...(Object.keys(sink.responseMeta).length > 0 ? { responseMeta: sink.responseMeta } : {}),
  };
}

async function handleEvaluate(frame) {
  const id = frame.id;
  const code = typeof frame.code === "string" ? frame.code : "";
  const syncTimeoutMs = Math.max(1, Math.trunc(Number(frame.syncTimeoutMs) || 5000));
  const sink = createSink();
  sink.bufferText = "";
  currentSink = sink;
  if (nodeReplApi) nodeReplApi.requestMeta = { ...(frame.requestMeta ?? {}) };
  const controller = new AbortController();
  activeRuns.set(id, controller);
  try {
    // executors.ts:59-61: The synchronous execution budget of the vm is interrupted while(true){}; the waiting after await is handled by signal racing.
    const promise = runInContext(prepare(code), context, { timeout: syncTimeoutMs });
    const value = await raceAbort(promise, controller);
    writeFrame({ id, kind: "result", output: buildOutput(value, sink) });
  } catch (error) {
    const normalized = normalizeReplError(error);
    // node-repl-session.ts:424-437: The entire context is invalidated after cancellation/timeout - late continuation
    // It is still possible to write to the same global, but "stop waiting" will allow the next call to observe the mutation.
    const mustReset =
      normalized.name === "AbortError" ||
      normalized.name === "TimeoutError" ||normalized.message.includes("Script execution timed out");
    if (mustReset) {
      resetKernel();
      normalized.message = `${normalized.message}${RESET_SUFFIX}`;
    }
    writeFrame({
      id,
      kind: "result",
      output: {
        logs: sink.bufferText,
        error: normalized,
        ...(sink.images.length > 0 ? { images: sink.images } : {}),
        ...(sink.structuredResults.length > 0 ? { structuredResults: sink.structuredResults } : {}),
        ...(Object.keys(sink.responseMeta).length > 0 ? { responseMeta: sink.responseMeta } : {}),
      },
    });
  } finally {
    activeRuns.delete(id);
    currentSink = null;
    if (nodeReplApi) nodeReplApi.requestMeta = {};
  }
}

function raceAbort(promise, controller) {
  // executors.ts:76-95: abort immediately rejects with reason, otherwise the promise result will be transparently transmitted.
  return new Promise((resolve, reject) => {
    const onAbort = () => reject(controller.signal.reason);
    if (controller.signal.aborted) {
      onAbort();
      return;
    }
    controller.signal.addEventListener("abort", onAbort, { once: true });
    promise.then(
      (value) => {
        controller.signal.removeEventListener("abort", onAbort);
        resolve(value);
      },
      (error) => {
        controller.signal.removeEventListener("abort", onAbort);
        reject(error);
      },
    );
  });
}

function resetKernel() {
  cleanupContextResources();
  context = buildContext();
  generation += 1;
}

async function handleFrame(frame) {
  if (!frame || typeof frame !== "object") {
    writeFrame({ ok: false, error: { name: "InvalidRequest", message: "kernel frame must be an object" } });
    return;
  }
  switch (frame.op) {
    case "evaluate":
      await handleEvaluate(frame);
      return;
    case "cancel": {const controller = activeRuns.get(frame.id);
      if (!controller) {
        writeFrame({ id: frame.id, kind: "cancel", acknowledged: false });
        return;
      }
      // executors.ts:67-73: Keep TimeoutError/AbortError distinction, not rewrite uniformly.
      const reason = frame.reason === "TimeoutError" ? "TimeoutError" : "AbortError";
      controller.abort(
        Object.assign(new Error(typeof frame.message === "string" ? frame.message : "aborted"), { name: reason }),
      );
      writeFrame({ id: frame.id, kind: "cancel", acknowledged: true });
      return;
    }
    case "reset":
      resetKernel();
      writeFrame({ id: frame.id, kind: "reset", acknowledged: true, generation });
      return;
    case "status":
      writeFrame({
        id: frame.id,
        kind: "status",
        generation,
        busy: activeRuns.size > 0,
        restrictProcess,
        moduleDirs: [...moduleDirs],
      });
      return;
    case "addModuleDir": {
      const dir = typeof frame.dir === "string" ? frame.dir : "";
      if (!dir) {
        writeFrame({
          id: frame.id,
          kind: "moduleDir",
          OK: false,
          error: { name: "InvalidInput", message: "addModuleDir requires an absolute directory" },
        });
        return;
      }
      if (!patchModuleResolution()) {
        writeFrame({
          id: frame.id,
          kind: "moduleDir",
          OK: false,
          error: {
            name: "ModuleResolutionUnsupported",
            message: `this Node runtime does not expose the module lookup hook; cannot add ${dir}`,
          },
        });
        return;
      }
      if (!moduleDirs.includes(dir)) moduleDirs.push(dir);
      writeFrame({ id: frame.id, kind: "moduleDir", ok: true, dirs: [...moduleDirs] });
      return;
    }case "shutdown":
      writeFrame({ id: frame.id, kind: "shutdown", acknowledged: true });
      process.exit(0);
      return;
    default:
      writeFrame({
        id: frame.id,
        kind: "error",
        OK: false,
        error: { name: "UnknownOperation", message: `unknown kernel operation: ${String(frame.op)}` },
      });
  }
}

function readStdin() {
  let buffer = "";
  process.stdin.setEncoding("utf8");
  process.stdin.on("data", (chunk) => {
    buffer += chunk;
    for (;;) {
      const newline = buffer.indexOf("\n");
      if (newline === -1) break;
      const line = buffer.slice(0, newline).trim();
      buffer = buffer.slice(newline + 1);
      if (!line) continue;
      let frame;
      try {
        frame = JSON.parse(line);
      } catch {
        writeFrame({ ok: false, kind: "error", error: { name: "InvalidRequest", message: "malformed JSON frame" } });
        continue;
      }
      // evaluate may be pending for a long time, but cancel must still be received - do not block the read loop before awaiting other ops.
      Promise.resolve()
        .then(() => handleFrame(frame))
        .catch((error) => {
          writeFrame({
            id: frame?.id,
            kind: "error",
            OK: false,
            error: normalizeReplError(error),
          });
        });
    }
  });
  process.stdin.on("end", () => process.exit(0));
  process.stdin.on("close", () => process.exit(0));
}

process.on("uncaughtException", (error) => {
  writeFrame({ kind: "fatal", error: normalizeReplError(error) });
  process.exit(1);
});
process.on("unhandledRejection", (error) => {
  writeFrame({ kind: "fatal", error: normalizeReplError(error) });
  process.exit(1);
});

writeFrame({
  kind: "event",
  event: "ready",
  protocol: KERNEL_PROTOCOL,
  pid: process.pid,
  node: process.version,execPath: process.execPath,
  cwd: process.cwd(),
  restrictProcess,
  generation,
  moduleDirs: [...moduleDirs],
});
readStdin();

// DEVIATIONS (relative to TS implementation, both intentional and documented):
// 1. Inject fetch/Headers/Request/Response/FormData/Blob/AbortController/AbortSignal/
// Event/EventTarget/CustomEvent/crypto/performance; TS sandbox list
// (node-repl-session.ts:206-229) does not have these, and R2 promises that fetch semantics are available.
// 2. In Persistent mode, sandbox's process.stdout/stderr writes are redirected to logs.
// Because stdout is a protocol channel after crossing the process; the writing ability itself is retained.
// 3. Top-level declaration persistence uses lexical scanning + try/catch injection + syntax self-check fallback,
// instead of meriyah AST (instrument.ts). Observable contract consistency: declarations are visible across calls,
// If unsure, fall back to the original code (equivalent to executors.ts:44-50).
// 4. browser/CUA injects globals (agent.browsers, cua bridge, screenshot traceability,
// cuaApp identity) does not belong to this island: those injected by other bridges of the host, this kernel only guarantees
// The structuredResults/responseMeta channel is transparently transmitted word by word.