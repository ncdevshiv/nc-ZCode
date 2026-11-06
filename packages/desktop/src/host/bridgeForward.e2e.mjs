/**
 * Headless MessagePort bridge e2e (spec §6, no Electron).
 *
 * Exercises the REAL `bridgeForward.ts` `bridgePort()` byte pipe against the
 * REAL `zcode-server` binary: spawn server → parse `LISTENING` → open
 * `ws://addr/ws` → wire a fake MessagePort through `bridgePort` → drive a
 * real TS `ChannelClient` (`packages/rpc`) over that port → `system.info`
 * Invoke round-trip. No protocol translation: the server emits one binary WS
 * message per framed channel message, the bridge byte-forwards both ways.
 *
 * Run with bun (TS sources are imported directly):
 *   bun run packages/desktop/src/host/bridgeForward.e2e.mjs
 * Env: ZCODE_SERVER_BIN (defaults to native/target/debug/zcode-server(.exe)).
 * Exit code 0 = pass, 1 = fail. No mocks/stubs: a missing binary, a failed
 * round-trip, or a wrong payload all fail the run.
 */
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { WebSocket } from "ws";
import { bridgePort } from "./bridgeForward.ts";
import { ChannelClient } from "../../../rpc/src/channelClient.ts";
import { VSBuffer } from "../../../rpc/src/buffer.ts";
import { Emitter } from "../../../rpc/src/foundation.ts";

const HEADER_SIZE = 13;
const PROTOCOL_REGULAR = 1;

function serverBinary() {
  const fallback =
    process.platform === "win32"
      ? "native/target/debug/zcode-server.exe"
      : "native/target/debug/zcode-server";
  return process.env["ZCODE_SERVER_BIN"]?.trim() || fallback;
}

function frame(payload /* VSBuffer */) {
  const out = VSBuffer.alloc(HEADER_SIZE + payload.byteLength);
  out.writeUInt8(PROTOCOL_REGULAR, 0);
  out.writeUInt32BE(0, 1);
  out.writeUInt32BE(0, 5);
  out.writeUInt32BE(payload.byteLength, 9);
  out.set(payload, HEADER_SIZE);
  return out;
}

/** Deframe one binary chunk into zero or more channel-message payloads. */
function deframe(bytes /* Uint8Array */) {
  const payloads = [];
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  let off = 0;
  while (off + HEADER_SIZE <= bytes.byteLength) {
    const type = view.getUint8(off);
    const length = view.getUint32(off + 9, false);
    off += HEADER_SIZE;
    if (off + length > bytes.byteLength) break;
    if (type === PROTOCOL_REGULAR) payloads.push(bytes.slice(off, off + length));
    off += length;
  }
  return payloads;
}

/** Minimal MessagePort stand-in exposing the BridgePort surface. */
class FakePort {
  constructor() {
    this.handlers = [];
    this.inbound = null; // (arrayBuffer) => void, set by the test
  }
  on(event, cb) {
    if (event !== "message") throw new Error(`FakePort: unsupported event ${event}`);
    this.handlers.push(cb);
  }
  /** Client → bridge direction: deliver an ArrayBuffer into bridgePort. */
  emitMessage(data) {
    for (const cb of this.handlers) cb({ data });
  }
  /** Bridge → client direction: bridgePort calls postMessage(ArrayBuffer). */
  postMessage(message) {
    if (this.inbound) this.inbound(message);
  }
}

async function waitForListening(proc, timeoutMs = 30_000) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("zcode-server did not print LISTENING in time")), timeoutMs);
    const rl = createInterface({ input: proc.stdout });
    rl.on("line", (line) => {
      const match = /LISTENING\s+(\S+)/.exec(line);
      if (match?.[1]) {
        clearTimeout(timer);
        rl.close();
        resolve(match[1]);
      }
    });
    proc.on("error", (err) => {
      clearTimeout(timer);
      reject(err);
    });
    proc.on("exit", (code) => {
      clearTimeout(timer);
      reject(new Error(`zcode-server exited early (code=${code})`));
    });
  });
}

async function main() {
  const bin = serverBinary();
  const proc = spawn(bin, ["--host", "127.0.0.1", "--port", "0"], {
    stdio: ["ignore", "pipe", "inherit"],
    env: process.env,
    windowsHide: true,
  });
  try {
    const addr = await waitForListening(proc);
    const ws = new WebSocket(`ws://${addr}/ws`);
    const port = new FakePort();
    const bridgeErrors = [];
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("ws connect timeout")), 15_000);
      ws.on("open", () => {
        clearTimeout(timer);
        resolve();
      });
      ws.on("error", (err) => {
        clearTimeout(timer);
        reject(err);
      });
    });

    // Wire the REAL bridge pipe: FakePort ↔ ws.
    bridgePort(port, ws, (message) => bridgeErrors.push(message));

    // ChannelClient protocol over the FakePort (framing mirrors
    // native/crates/nc-code-server/src/ws.rs: 13-byte BE header + payload).
    const emitter = new Emitter();
    port.inbound = (message) => {
      const bytes = new Uint8Array(message);
      for (const payload of deframe(bytes)) emitter.fire(VSBuffer.wrap(payload));
    };
    const protocol = {
      get onMessage() {
        return emitter.event;
      },
      send(buffer /* VSBuffer */) {
        const framed = frame(buffer);
        const copy = framed.buffer.slice(
          framed.buffer.byteOffset,
          framed.buffer.byteOffset + framed.buffer.byteLength,
        );
        port.emitMessage(copy);
      },
    };

    const client = new ChannelClient(protocol);
    // ChannelClient initializes on the first Initialize frame.
    const system = client.getChannel("system");
    const info = await Promise.race([
      system.call("info", undefined),
      new Promise((_, reject) => setTimeout(() => reject(new Error("system.info timeout (10s)")), 10_000)),
    ]);
    if (typeof info?.homedir !== "string" || info.homedir.length === 0) {
      throw new Error(`system.info returned bad payload: ${JSON.stringify(info)}`);
    }
    if (typeof info?.platform !== "string" || info.platform.length === 0) {
      throw new Error(`system.info missing platform: ${JSON.stringify(info)}`);
    }
    client.dispose();
    ws.close();
    console.log(`PASS bridgeForward e2e: system.info via MessagePort bridge -> ${JSON.stringify(info)}`);
  } finally {
    proc.kill();
  }
}

main().then(
  () => process.exit(0),
  (err) => {
    console.error(`FAIL bridgeForward e2e: ${err instanceof Error ? err.message : String(err)}`);
    process.exit(1);
  },
);
