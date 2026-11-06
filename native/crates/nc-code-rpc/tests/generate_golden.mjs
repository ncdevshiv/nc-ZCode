// Golden-vector generator for the Rust `zcode-rpc` crate.
//
// WHY: the channel RPC wire format (VS Code-derived) is defined by the TypeScript
// implementation in `packages/rpc/src` (serialization.ts / protocol.ts / channelClient.ts /
// channelServer.ts). A Rust port is only correct if it produces byte-identical frames.
// This script bundles the real TS modules with esbuild, drives a real ChannelClient against
// a real ChannelServer over an in-memory protocol pair, and prints the vectors that
// `tests/golden_parity.rs` asserts.
//
//   node native/crates/zcode-rpc/tests/generate_golden.mjs > native/crates/zcode-rpc/tests/golden.json
//
// Requires: repo `node_modules` (esbuild) and a writable temp dir.

import { execFileSync } from "node:child_process";
import { mkdtempSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const repoRoot = fileURLToPath(new URL("../../../../", import.meta.url));
const require = createRequire(import.meta.url);
const esbuild = require(join(repoRoot, "node_modules", "esbuild", "lib", "main.js"));

const workDir = mkdtempSync(join(tmpdir(), "zcode-rpc-golden-"));

// One bundle for everything: `instanceof VSBuffer` checks in serialization.ts only
// hold when writer/protocol/buffer come from the *same* module instance.
const bundlePath = join(workDir, "rpc.cjs");
esbuild.buildSync({
  stdin: {
    contents: `
      export * from "./serialization.js";
      export * from "./protocol.js";
      export * from "./buffer.js";
      export * from "./foundation.js";
      export * from "./channels.shared.js";
      export * from "./channelClient.js";
      export * from "./channelServer.js";
    `,
resolveDir: join(repoRoot, "packages", "rpc", "src"),
    sourcefile: "golden-entry.ts",
    loader: "ts",
  },
  bundle: true,
  format: "cjs",
  platform: "node",
  outfile: bundlePath,
  logLevel: "silent",
});

const runner = `
const rpc = require(${JSON.stringify(bundlePath)});
const { VSBuffer } = rpc;

function hex(buffer) {
  return Buffer.from(buffer.buffer ? buffer.buffer : buffer).toString("hex");
}

function serializeValue(value) {
  const writer = new rpc.BufferWriter();
  rpc.serialize(writer, value);
  return hex(writer.buffer);
}

const cases = [
  { kind: "undefined" },
  { kind: "string", value: "hello" },
  { kind: "string", value: "" },
  { kind: "string", value: "Chinese ✓" },
  { kind: "string", value: "x".repeat(200) },
  { kind: "int", value: 0 },
  { kind: "int", value: 127 },
  { kind: "int", value: 128 },
  { kind: "int", value: 2147483647 },
  { kind: "int", value: -7 },
  { kind: "float", value: 1.5 },
  { kind: "object", value: { a: 1, b: "two", c: [true, null], d: { e: 3 } } },
  { kind: "array", value: [1, "two", { three: 3 }, [4]] },
  // Top-level VSBuffer -> DataType.VSBuffer (tag 3).
  { kind: "bytes", hex: "00ff10" },
  // Top-level Uint8Array -> DataType.Buffer (tag 2); service returns use this.
  { kind: "uint8array", hex: "00ff10" },
  // Node Buffer nested in an object: Buffer.toJSON wins over the replacer, so the
  // wire keeps Node's {type:"Buffer",data:[...]} JSON shape.
  { kind: "nested-buffer", hex: "0102" },
  // Real Uint8Array nested in an object: the marker + base64 fallback.
  { kind: "nested-uint8array", hex: "0102" },
];

const vectors = [];
for (const c of cases) {
  let value;
  if (c.kind === "undefined") value = undefined;
  else if (c.kind === "bytes") value = VSBuffer.wrap(Buffer.from(c.hex, "hex"));  else if (c.kind === "uint8array") value = new Uint8Array(Buffer.from(c.hex, "hex"));
  else if (c.kind === "nested-buffer") value = { payload: Buffer.from(c.hex, "hex") };
  else if (c.kind === "nested-uint8array") value = { payload: new Uint8Array(Buffer.from(c.hex, "hex")) };
  else value = c.value;
  vectors.push({ ...c, serialized: serializeValue(value) });
}

const payloads = ["", "00", "deadbeef", "68656c6c6f"];
const frames = payloads.map((payloadHex) => {
  const payload = VSBuffer.wrap(Buffer.from(payloadHex, "hex"));
  const msg = new rpc.ProtocolMessage(rpc.ProtocolMessageType.Regular, 0, 0, payload);
  return { payload: payloadHex, frame: hex(rpc.writeProtocolMessage(msg)) };
});

// ── channel traffic: real ChannelClient ↔ real ChannelServer ──────────────────

function createProtocol() {
  const handlers = [];
  return {
    queue: [],
    captured: [],
    onMessage(listener) {
      handlers.push(listener);
      return {
        dispose() {
          const index = handlers.indexOf(listener);
          if (index >= 0) handlers.splice(index, 1);
        },
      };
    },
    send(buffer) {
      const copy = VSBuffer.wrap(Uint8Array.from(buffer.buffer));
      this.queue.push(copy);
      this.captured.push(hex(copy));
    },
    deliver(message) {
      for (const handler of [...handlers]) handler(message);
    },
  };
}

function pumpOnce(clientP, serverP) {
  let delivered = false;
  while (clientP.queue.length) {
    serverP.deliver(clientP.queue.shift());
    delivered = true;
  }
  while (serverP.queue.length) {
    clientP.deliver(serverP.queue.shift());
    delivered = true;
  }
  return delivered;
}

async function drain(clientP, serverP) {
  for (let i = 0; i < 200; i++) {
    const delivered = pumpOnce(clientP, serverP);
    await new Promise((resolve) => setImmediate(resolve));
    if (!delivered && !clientP.queue.length && !serverP.queue.length) return;
  }
  throw new Error("channel golden: pump did not settle");
}

async function settle(promise, clientP, serverP) {
  const state = { status: "pending" };
  Promise.resolve(promise).then(
    (value) => {
      state.status = "fulfilled";
      state.value = value;
    },
    (reason) => {
      state.status = "rejected";
      state.reason = reason instanceof Error
        ? { message: reason.message, name: reason.name, code: reason.code, data: reason.data }
        : reason;
    },
  );
  for (let i = 0; i < 200 && state.status === "pending"; i++) {
    pumpOnce(clientP, serverP);
    await new Promise((resolve) => setImmediate(resolve));
  }
  await drain(clientP, serverP);
  return state;
}

async function main() {
  const clientP = createProtocol();
  const serverP = createProtocol();
  const server = new rpc.ChannelServer(serverP, "golden-ctx", 1000, true);
  const client = new rpc.ChannelClient(clientP);
  const events = new Map();
  server.registerChannel("golden", {
    async call(_ctx, command, arg) {
      switch (command) {
        case "echo":
          return { echoed: arg };
        case "undefinedResult":
          return undefined;
        case "error": {
          const error = new Error("boom");
          error.stack = "";
          error.name = "BoomError";
          error.code = "golden.error";
          error.data = { why: "because" };
          throw error;
        }
        case "errorObj":
          throw { custom: true, n: 7 };
        case "never":
          return new Promise(() => {});
        default:
          throw new Error("unknown command " + command);
      }
    },
    listen(_ctx, event) {
      if (event !== "tick") throw new Error("unknown event " + event);
      return (listener) => {
        events.set(event, listener);
        return {
          dispose() {
            events.delete(event);
          },
        };
      };
    },
  });
  server.ready();
  await drain(clientP, serverP);
  const golden = client.getChannel("golden");
  const channelVectors = [];
  const record = (label, protocol, start) => {
    const captured = protocol.captured.slice(start);
    if (captured.length !== 1) {
      throw new Error(label + ": expected 1 message, captured " + captured.length);
    }
    channelVectors.push({ label, bytes: captured[0] });
  };

  {
    const start = clientP.captured.length;
    const serverStart = serverP.captured.length;
    const outcome = await settle(golden.call("echo", { a: 1 }), clientP, serverP);
    if (outcome.status !== "fulfilled") throw new Error("echo failed: " + JSON.stringify(outcome));
    record("promise.request", clientP, start);
    record("promise.success", serverP, serverStart);
  }
  {
    const start = clientP.captured.length;
    const serverStart = serverP.captured.length;
    const outcome = await settle(golden.call("undefinedResult"), clientP, serverP);
    if (outcome.status !== "fulfilled" || outcome.value !== undefined) {
      throw new Error("undefinedResult failed: " + JSON.stringify(outcome));
    }
    record("promise.undefined.request", clientP, start);
    record("promise.undefined.success", serverP, serverStart);
  }
  {
    const start = clientP.captured.length;
    const serverStart = serverP.captured.length;
    const outcome = await settle(golden.call("error"), clientP, serverP);
    if (outcome.status !== "rejected" || outcome.reason.message !== "boom") {
      throw new Error("error case failed: " + JSON.stringify(outcome));
    }
    record("promise.error.request", clientP, start);
    record("promise.error.response", serverP, serverStart);
  }
  {
    const start = clientP.captured.length;
    const serverStart = serverP.captured.length;
    const outcome = await settle(golden.call("errorObj"), clientP, serverP);
    if (outcome.status !== "rejected" || outcome.reason.custom !== true) {
      throw new Error("errorObj case failed: " + JSON.stringify(outcome));
    }
    record("promise.errorObj.request", clientP, start);
    record("promise.errorObj.response", serverP, serverStart);
  }
  {
    const start = clientP.captured.length;
    const cts = new rpc.CancellationTokenSource();
    const never = golden.call("never", { wait: true }, cts.token);
    never.catch(() => {});
    pumpOnce(clientP, serverP);
    await new Promise((resolve) => setImmediate(resolve));
    record("promise.cancel.request", clientP, start);
    const cancelStart = clientP.captured.length;
    cts.cancel();
    pumpOnce(clientP, serverP);
    await new Promise((resolve) => setImmediate(resolve));
    record("promise.cancel.cancel", clientP, cancelStart);
  }
  {
    const listenStart = clientP.captured.length;
    const received = [];
    const subscription = golden.listen("tick", { topic: "t" })((value) => received.push(value));
    pumpOnce(clientP, serverP);
    await new Promise((resolve) => setImmediate(resolve));
    record("event.listen.request", clientP, listenStart);
    const fireStart = serverP.captured.length;
    const listener = events.get("tick");
    if (!listener) throw new Error("event.listen: server did not register the listener");
    listener({ n: 1 });
    pumpOnce(clientP, serverP);
    await new Promise((resolve) => setImmediate(resolve));
    record("event.fire.response", serverP, fireStart);
    if (received.length !== 1 || received[0].n !== 1) {
      throw new Error("event.listen: client did not receive the event");
    }
    const disposeStart = clientP.captured.length;
    subscription.dispose();
    pumpOnce(clientP, serverP);
    await new Promise((resolve) => setImmediate(resolve));
    record("event.dispose.request", clientP, disposeStart);
  }

  channelVectors.unshift({ label: "initialize", bytes: serverP.captured[0] });

  return { serialization: vectors, frames, channel: channelVectors };
}

main().then((result) => {
  process.stdout.write(JSON.stringify(result, null, 2));
}, (error) => {
  console.error(error);
  process.exit(1);
});
`;

writeFileSync(join(workDir, "run.cjs"), runner);
const out = execFileSync(process.execPath, [join(workDir, "run.cjs")], { encoding: "utf8" });
process.stdout.write(out);
