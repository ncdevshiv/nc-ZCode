import type { WebSocket } from "ws";

/**
 * Pure MessagePort ↔ WebSocket byte pipe used by the Rust channel bridge.
 * No Electron `parentPort` dependency: the utilityProcess bridge and the
 * headless e2e share this exact forward logic.
 */

export function toArrayBuffer(chunk: Buffer): ArrayBuffer {
  return chunk.buffer.slice(chunk.byteOffset, chunk.byteOffset + chunk.byteLength) as ArrayBuffer;
}

export interface BridgePort {
  on: (event: "message", cb: (ev: { data: unknown }) => void) => void;
  postMessage: (message: unknown) => void;
}

export function bridgePort(port: BridgePort, ws: WebSocket, onError?: (message: string) => void): void {
  port.on("message", (event) => {
    const data = event.data as ArrayBuffer | Buffer | Uint8Array;
    const buf = Buffer.isBuffer(data) ? data : Buffer.from(data as ArrayBuffer);
    if (ws.readyState === 1) {
      ws.send(buf);
    }
  });
  ws.on("message", (data: Buffer) => {
    port.postMessage(toArrayBuffer(Buffer.isBuffer(data) ? data : Buffer.from(data)));
  });
  ws.on("error", (error) => onError?.(`bridge ws error: ${error.message}`));
  ws.on("close", () => onError?.("bridge ws closed"));
}
