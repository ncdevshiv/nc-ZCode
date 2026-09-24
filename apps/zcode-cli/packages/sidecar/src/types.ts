//! sidecar 协议类型（镜像 native/crates/zcode-sidecar-proto 的 Frame）。
//!
//! wire 形状（协议版本 1）：
//!   request  {v, id, method, params}
//!   response {v, id, ok, result | error{code,message}}
//!   event    {v, event, data}
//! 顺序帧：`event:"ready"`（data = {sidecar, proto}）是进程启动后的第一行。

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

/** 协议层错误：版本不匹配、帧形状非法、进程已退出。属于配置/构建问题，必须冒泡。 */
export class SidecarProtocolError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "SidecarProtocolError";
  }
}

/** 可用性错误：二进制缺失 / spawn 失败 / 进程崩溃。调用方可降级（diff facade 降级到 jsdiff）。 */
export class SidecarUnavailableError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "SidecarUnavailableError";
  }
}

/** handler 返回的错误帧（400/404/500……）。 */
export class SidecarMethodError extends Error {
  readonly code: number;
  constructor(code: number, message: string) {
    super(message);
    this.name = "SidecarMethodError";
    this.code = code;
  }
}

export type SidecarEventListener = (event: string, data: unknown) => void;
