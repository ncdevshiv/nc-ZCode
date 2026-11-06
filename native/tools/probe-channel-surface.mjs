#!/usr/bin/env node
// Channel surface behavior probe - use real wire calls to obtain evidence of migration completion.
//
// Why it is needed: check-channel-coverage.mjs can only prove "there are distribution points in the source code", which is static evidence.
// This tool connects to the real running Rust backend and sends one channel/method one by one according to service-channels.json
// Promise frame (null position parameter), classified by response:
// unknown-channel —— channel is not registered
// unknown-method - registered but the method is not wired (`Method not found`)
// boundary-removed - explicitly reference the boundary spec in the response / explicit removal code (product decision, not missing porting)
// dispatched - entered the real implementation (parameter verification errors, provider not configured, etc. are counted)
// success - true return
// Static lists drift, responses don't lie: so behavioral evidence takes precedence over string evidence.
//
// Usage:
// node native/tools/probe-channel-surface.mjs --url ws://127.0.0.1:PORT/ws [--token T]
// [--channel name]... [--timeout 4000] [--out report.json] --allow-calls
//
// Must be explicitly given to --allow-calls: the probe will send real requests to the backend. Only for temporary data-dir /
// Run the backend of the temporary workspace, not against the instance the user is using.

import { createRequire } from "node:module";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const repoRoot = fileURLToPath(new URL("../../", import.meta.url));
const esbuild = require(join(repoRoot, "node_modules", "esbuild", "lib", "main.js"));
const { WebSocket } = require(join(repoRoot, "node_modules", "ws"));

const args = process.argv.slice(2);
const flag = (name) => args.includes(name);
const argValue = (name, fallback) => {
  const index = args.indexOf(name);
  return index >= 0 && args[index + 1] ? args[index + 1] : fallback;
};

if (!flag("--allow-calls")) {
  console.error(
    "[probe] Refuse to run: The probe will send real calls to the backend. Make sure the target is a temporary data-dir and add --allow-calls.",
  );
  process.exitCode = 2;
} else {
  await run();
}

async function run() {
  const url = argValue("--url", "");
  if (!url) {console.error("[probe] requires --url ws://127.0.0.1:PORT/ws");
    process.exitCode = 2;
    return;
  }
  const token = argValue("--token", "");
  const timeoutMs = Number(argValue("--timeout", "4000"));
  const only = args.reduce((list, value, index) => {
    if (value === "--channel") list.push(args[index + 1]);
    return list;
  }, []);
  const inventory = JSON.parse(
    readFileSync(join(repoRoot, "native", "fixtures", "service-channels.json"), "utf8"),
  );

  //Reuse the real wire codec on the front end (the same method as trace-channel-proxy.mjs), without guessing the format by yourself.
  const workDir = mkdtempSync(join(tmpdir(), "zcode-probe-"));
  const bundlePath = join(workDir, "rpc.cjs");
  esbuild.buildSync({
    stdin: {
      contents: `
        export * from "./serialization.js";
        export * from "./protocol.js";
        export * from "./buffer.js";
      `,
      resolveDir: join(repoRoot, "packages", "rpc", "src"),
      sourcefile: "probe-entry.ts",
      loader: "ts",
    },
    outfile: bundlePath,
    bundle: true,
    platform: "node",
    format: "cjs",
    logLevel: "silent",
  });
  const rpc = require(bundlePath);
  const { BufferWriter, BufferReader, deserialize, serialize, VSBuffer } = rpc;

  const encodeValue = (value) => {
    const writer = new BufferWriter();
    serialize(writer, value);
    return writer.buffer.buffer;
  };

  const encodePromiseFrame = (id, channel, method, argBytes) => {
    const header = encodeValue([100, id, channel, method]);
    const payload = new Uint8Array(header.length + argBytes.length);
    payload.set(header, 0);
    payload.set(argBytes, header.length);
    const frame = new Uint8Array(13 + payload.length);
    frame[0] = 1; // Regular
    new DataView(frame.buffer).setUint32(1, 0, false);
    new DataView(frame.buffer).setUint32(5, 0, false);new DataView(frame.buffer).setUint32(9, payload.length, false);
    frame.set(payload, 13);
    return frame;
  };

  const decodeResponse = (bytes) => {
    const reader = new BufferReader(VSBuffer.wrap(bytes));
    const header = deserialize(reader);
    const body = deserialize(reader);
    return { type: header[0], id: header[1], detail: header[2], body };
  };

  const text = (value) => {
    if (value === undefined || value === null) return "";
    if (value instanceof Uint8Array) return `<bytes:${value.byteLength}>`;
    if (typeof value === "object") {
      const name = value.name ?? value.message ?? "";
      const message = value.message ?? "";
      return `${name} ${message}`.trim();
    }
    return String(value);
  };

  const classify = (response) => {
    if (!response) return { kind: "timeout", detail: "" };
    const { type, body } = response;
    if (type === 201) return { kind: "success", detail: text(body) };
    const label = text(body) + " " + text(body?.name) + " " + text(body?.message ?? body?.data);
    if (/Unknown channel/i.test(label)) return { kind: "unknown-channel", detail: label };
    if (/Method not found|unknown method|Unknown method/i.test(label)) {
      return { kind: "unknown-method", detail: label };
    }
    if (/claim boundary|workflow-capability-boundary|CodingPlanPurchaseRemoved|removed/i.test(label)) {
      return { kind: "boundary-removed", detail: label };
    }
    return { kind: "dispatched", detail: label };
  };

  const socket = new WebSocket(token ? `${url}?token=${encodeURIComponent(token)}` : url);
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("[probe] WS connection timeout")), timeoutMs);
    socket.once("open", () => {
      clearTimeout(timer);
      resolve();
    });socket.once("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
  });

  // Single in-flight request + matching response by id: the backend is associated by frame id, and the probe is sent serially to avoid serialization.
  let nextId = 1;
  const pending = new Map();
  let bufferBytes = new Uint8Array(0);
  socket.on("message", (data) => {
    const incoming = new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    const merged = new Uint8Array(bufferBytes.length + incoming.length);
    merged.set(bufferBytes, 0);
    merged.set(incoming, bufferBytes.length);
    bufferBytes = merged;
    for (;;) {
      if (bufferBytes.length < 13) return;
      const view = new DataView(bufferBytes.buffer, bufferBytes.byteOffset, bufferBytes.byteLength);
      const length = view.getUint32(9, false);
      const total = 13 + length;
      if (bufferBytes.length < total) return;
      const payload = bufferBytes.subarray(13, total);
      bufferBytes = bufferBytes.subarray(total);
      if (payload.length === 0) continue;
      let response;
      try {
        //Copy slices: BufferReader relies on an independent buffer and cannot be reused with byteOffset.
        response = decodeResponse(new Uint8Array(payload));
      } catch {
        continue;
      }
      const waiter = pending.get(response.id);
      if (waiter) {
        pending.delete(response.id);
        waiter(response);
      }
    }
  });

  const call = (channel, method) =>
    new Promise((resolve) => {
      const id = nextId++;
      const timer = setTimeout(() => {
        pending.delete(id);
        resolve(null);
      }, timeoutMs);
      pending.set(id, (response) => {
        clearTimeout(timer);
        resolve(response);
      });
      socket.send(encodePromiseFrame(id, channel, method, encodeValue([])));
    });

  const results = [];
  const channels = inventory.services.filter((service) => only.length === 0 || only.includes(service.channelName),
  );
  for (const service of channels) {
    for (const method of service.methods) {
      if (method.kind !== "rpc" && method.role === "event") continue;
      const response = await call(service.channelName, method.name);
      const verdict = classify(response);
      results.push({
        channel: service.channelName,
        method: method.name,
        kind: verdict.kind,
        detail: verdict.detail.slice(0, 300),
      });
    }
  }
  socket.close();

  const counts = {};
  for (const row of results) counts[row.kind] = (counts[row.kind] ?? 0) + 1;
  const report = { url, probedAt: new Date().toISOString(), counts, results };
  const out = argValue("--out", join(tmpdir(), "zcode-surface-probe.json"));
  writeFileSync(out, JSON.stringify(report, null, 2));

  console.log(`[probe] probe ${results.length} members → ${JSON.stringify(counts)}`);
  console.log(`[probe] report: ${out}`);
  for (const kind of ["unknown-channel", "unknown-method"]) {
    const rows = results.filter((row) => row.kind === kind);
    if (rows. length > 0) {
      console.log(`[probe] ${kind}: ${rows.map((row) => `${row.channel}.${row.method}`).join(", ")}`.slice(0, 2000));
    }
  }
}