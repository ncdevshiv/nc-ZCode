// Dev-only tracing proxy in front of the TypeScript backend.
//
// WHY: the Rust port needs the REAL boot contract (which channel/method the current
// frontend calls, with which positional args, and what comes back). The TS server's
// logging middleware logs method names only. This proxy forwards HTTP and WebSocket
// bytes untouched while decoding channel messages for a JSONL trace.
//
//   node native/tools/trace-channel-proxy.mjs --listen 3031 --target 3030 --log <file>
//
// Not part of the product; never imported by app code.

import { createServer, request as httpRequest } from "node:http";
import { mkdtempSync, writeFileSync, createWriteStream } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const repoRoot = fileURLToPath(new URL("../../", import.meta.url));
const require = createRequire(import.meta.url);
const esbuild = require(join(repoRoot, "node_modules", "esbuild", "lib", "main.js"));
const { WebSocketServer, WebSocket } = require(join(repoRoot, "node_modules", "ws"));

const args = process.argv.slice(2);
const argValue = (name, fallback) => {
  const index = args.indexOf(name);
  return index >= 0 && args[index + 1] ? args[index + 1] : fallback;
};
const listenPort = Number(argValue("--listen", "3031"));
const targetPort = Number(argValue("--target", "3030"));
const logFile = argValue("--log", join(tmpdir(), "zcode-channel-trace.jsonl"));

const workDir = mkdtempSync(join(tmpdir(), "zcode-trace-"));
const bundlePath = join(workDir, "rpc.cjs");
esbuild.buildSync({
  stdin: {
    contents: `
      export * from "./serialization.js";
      export * from "./protocol.js";
      export * from "./buffer.js";
    `,
    resolveDir: join(repoRoot, "packages", "rpc", "src"),
    sourcefile: "trace-entry.ts",
    loader: "ts",
  },
  bundle: true,
  format: "cjs",
  platform: "node",
  outfile: bundlePath,
  logLevel: "silent",
});
const rpc = require(bundlePath);
const { BufferReader, deserialize, VSBuffer } = rpc;

const log = createWriteStream(logFile, { flags: "w" });
const trace = (entry) => log.write(`${JSON.stringify(entry)}\n`);

function summarize(value) {
  if (value instanceof Uint8Array || value instanceof VSBuffer)
    return `<bytes:${value.byteLength}>`;
  if (value === undefined) return "<undefined>";
  try {
    const text = JSON.stringify(value);
    return text && text.length > 200000 ? `${text.slice(0, 200000)}…` : text;
  } catch {
    return String(value);
  }
}

function readUInt32BE(bytes, offset) {
  return (
    ((bytes[offset] << 24) |
      (bytes[offset + 1] << 16) |
      (bytes[offset + 2] << 8) |
      bytes[offset + 3]) >>>
    0
  );
}

function decodeFrames(bytes) {
  const events = [];
  let offset = 0;
  while (offset + 13 <= bytes.length) {
    const messageType = bytes[offset];
    const length = readUInt32BE(bytes, offset + 9);
    const start = offset + 13;
    const end = start + length;
    if (end > bytes.length) {
      events.push({ dir: "?", decodeError: "truncated frame", bytes: bytes.length });
      break;
    }
    if (messageType === 1) {
      events.push(decodeChannelPayload(bytes.subarray(start, end)));
    }
    offset = end;
  }
  if (events.length === 0)
    events.push({ dir: "?", decodeError: "no regular frame", bytes: bytes.length });
  return events;
}

function decodeChannelPayload(bytes) {
  const reader = new BufferReader(VSBuffer.wrap(bytes));
  const header = deserialize(reader);
  const body = deserialize(reader);
  const type = header[0];
  if (type === 100) {
    return {
      dir: "client→server",
      type: "Promise",
      id: header[1],
      channel: header[2],
      method: header[3],
      arg: summarize(body),
    };
  }
  if (type === 101) return { dir: "client→server", type: "PromiseCancel", id: header[1] };
  if (type === 102) {
    return {
      dir: "client→server",
      type: "EventListen",
      id: header[1],
      channel: header[2],
      method: header[3],
      arg: summarize(body),
    };
  }
  if (type === 103) return { dir: "client→server", type: "EventDispose", id: header[1] };
  if (type === 200) return { dir: "server→client", type: "Initialize" };
  return { dir: "server→client", type, id: header[1], data: summarize(body) };
}

const httpServer = createServer((incoming, response) => {
  const upstream = httpRequest(
    {
      host: "127.0.0.1",
      port: targetPort,
      path: incoming.url,
      method: incoming.method,
      headers: incoming.headers,
    },
    (upstreamResponse) => {
      response.writeHead(upstreamResponse.statusCode ?? 502, upstreamResponse.headers);
      upstreamResponse.pipe(response);
    },
  );
  upstream.on("error", (error) => {
    response.writeHead(502, { "content-type": "application/json" });
    response.end(JSON.stringify({ error: String(error) }));
  });
  incoming.pipe(upstream);
});

const wss = new WebSocketServer({ noServer: true });

httpServer.on("upgrade", (incoming, socket, head) => {
  if (!incoming.url?.startsWith("/ws")) {
    socket.destroy();
    return;
  }
  wss.handleUpgrade(incoming, socket, head, (client) => {
    const upstream = new WebSocket(`ws://127.0.0.1:${targetPort}${incoming.url}`);
    const pending = [];
    client.on("message", (data) => {
      const bytes = new Uint8Array(data);
      try {
        for (const event of decodeFrames(bytes)) trace(event);
      } catch (error) {
        trace({ dir: "client→server", decodeError: String(error), bytes: bytes.length });
      }
      if (upstream.readyState === WebSocket.OPEN) upstream.send(bytes);
      else pending.push(bytes);
    });
    upstream.on("open", () => {
      for (const bytes of pending.splice(0)) upstream.send(bytes);
    });
    upstream.on("message", (data) => {
      const bytes = new Uint8Array(data);
      try {
        for (const event of decodeFrames(bytes)) trace(event);
      } catch (error) {
        trace({ dir: "server→client", decodeError: String(error), bytes: bytes.length });
      }
      if (client.readyState === WebSocket.OPEN) client.send(bytes);
    });
    const close = () => {
      upstream.close();
      client.close();
    };
    client.on("close", close);
    upstream.on("close", close);
    client.on("error", close);
    upstream.on("error", close);
  });
});

httpServer.listen(listenPort, "127.0.0.1", () => {
  console.log(
    `TRACING http://127.0.0.1:${listenPort} -> http://127.0.0.1:${targetPort} (log: ${logFile})`,
  );
});
