//! sidecar protocol type (mirror Frame of native/crates/zcode-sidecar-proto).
//!
//! wire shape (protocol version 1):
//!   request  {v, id, method, params}
//!   response {v, id, ok, result | error{code,message}}
//!   event    {v, event, data}
//! Sequential frame: `event:"ready"` (data = {sidecar, proto}) is the first line after the process starts.

export const SIDECAR_PROTOCOL_VERSION = 1;

export interface SidecarReadyData {
  sidecar: string;
  proto: number;
}

export type SidecarFrame =
  | { v: number; id: number; method: string; params: unknown }
  | { v: number; id: number; ok: true; result: unknown }
  | { v: number; id: number; ok: false; error: { code: number; message: string } }
  | { v: number; event: string; data: unknown };

export interface SidecarRequest {
  method: string;
  params?: unknown;
  timeoutMs?: number;
}

/** Protocol layer error: version mismatch, illegal frame shape, process has exited. It's a configuration/build issue and must bubble up. */
export class SidecarProtocolError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "SidecarProtocolError";
  }
}

/** Availability error: missing binary/spawn failed/process crashed. The caller can downgrade (diff facade downgrades to jsdiff). */
export class SidecarUnavailableError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "SidecarUnavailableError";
  }
}

/** The error frame returned by the handler (400/404/500...). */
export class SidecarMethodError extends Error {
  readonly code: number;
  constructor(code: number, message: string) {
    super(message);
    this.name = "SidecarMethodError";
    this.code = code;
  }
}

export type SidecarEventListener = (event: string, data: unknown) => void;
