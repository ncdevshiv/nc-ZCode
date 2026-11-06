// Sandbox child program (port of @zcode/dynamic-workflow-runtime child-source.ts).
//
// This file is embedded into the generated entry with include_str! and evaluated as a
// function declaration — it is NOT imported as a module. It must stay self-contained:
// only its parameter, language intrinsics and Node globals; no module-scope bindings.
// Inner functions must stay anonymous-assigned (no named function expressions), because a
// minified+keepNames build would wrap them with a module-scope __name helper that does not
// exist once this text is embedded (the TS source documents the same trap).
function childMain(deps) {
  const BOOTSTRAP = String.raw`
"use strict";

var __nextLocal = 0;
var __nextReq = 0;
var __pending = new Map();

function __emit(obj) {
  __send(JSON.stringify(obj));
}

function __createActor(siteId, name, persona) {
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
  __emit({ kind: "event", type: "phase-entered", name: String(name) });
}

function __publishArtifact(siteId, op, args) {
  var id = "r" + (++__nextReq);
  return new Promise(function (resolve, reject) {
    __pending.set(id, { resolve: resolve, reject: reject });
    __emit({ kind: "request", id: id, type: "publish-artifact", siteId: siteId, artifactOp: op, args: args });
  });
}

function __declareArtifact(siteId, op, args) {
  __emit({ kind: "event", type: "declare-artifact", siteId: siteId, op: op, args: args });
}

function __report(siteId, item, artifactId) {
  __emit({ kind: "event", type: "report", siteId: siteId, item: item, artifactId: artifactId });
}

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

globalThis.__execute = async function (runFn) {
  try {
    var value = await runFn(globalThis.__host);
    __complete(true, value);
  } catch (e) {
    __complete(false, e);
  }
};

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

  const payload = deps.payload;

  const sandbox = {
    __argsJson: JSON.stringify(payload.args ?? {}),
  };
  sandbox.__send = (line) => {
    deps.stdout.write(`${line}\n`);
  };
  deps.vm.createContext(sandbox, { name: "workflow-sandbox" });
  deps.vm.runInContext(BOOTSTRAP, sandbox, { filename: "workflow-bootstrap.js" });

  let runFn;
  try {
    runFn = deps.vm.runInContext(`(async (__host) => {\n${payload.lowered}\n})`, sandbox, {
      filename: "workflow-script.js",
    });
  } catch (error) {
    const err = error;
    deps.stdout.write(
      `${JSON.stringify({
        kind: "complete",
        ok: false,
        error: {
          name: (err && err.name) || "SyntaxError",
          message: (err && err.message) || String(error),
          stack: err && err.stack,
        },
      })}\n`,
    );
    return Promise.resolve();
  }

  const reader = deps.createInterface({ input: deps.stdin });
  reader.on("line", (line) => {
    if (!line.trim()) return;
    sandbox.__deliver(line);
  });

  return sandbox.__execute(runFn).then(
    () => reader.close(),
    () => reader.close(),
  );
}
