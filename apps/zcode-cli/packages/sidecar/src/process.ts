//! SidecarProcess：单个 sidecar 进程的 NDJSON 生命周期管理。
//!
//! 契约（镜像 native/crates/zcode-sidecar-proto 的 serve 循环）：
//!   - spawn 后第一行必须是 `event:"ready"`（data.sidecar 名与 proto 版本都要校验），
//!     否则 SidecarProtocolError——版本不匹配必须炸在启动时，不是第一个请求时；
//!   - `request()` 写一行、按 id 关联响应；过程中的 event 帧分发给 listener；
//!   - 进程退出：在途请求全部 reject（SidecarUnavailableError），进程标记为 dead；
//!   - `shutdown()` 发 `shutdown` 方法，等响应与 exit(0)；超时 kill。

import { type ChildProcess, spawn } from "node:child_process";
import readline from "node:readline";

import {
  SidecarMethodError,
  SidecarProtocolError,
  SidecarUnavailableError,
  SIDECAR_PROTOCOL_VERSION,
  type SidecarEventListener,
  type SidecarFrame,
  type SidecarReadyData,
} from "./types.js";

interface Pending {
  resolve: (result: unknown) => void;
  reject: (error: Error) => void;
  timer: NodeJS.Timeout;
}

const DEFAULT_REQUEST_TIMEOUT_MS = 15_000;
const SHUTDOWN_GRACE_MS = 3_000;

export class SidecarProcess {
  readonly name: string;
  private readonly child: ChildProcess;
  private readonly pending = new Map<number, Pending>();
  private readonly listeners = new Set<SidecarEventListener>();
  private nextId = 1;
  private dead = false;

  private constructor(name: string, child: ChildProcess, ready: SidecarReadyData) {
    this.name = name;
    this.child = child;
    this.assertReady(ready);
    const stderr = child.stderr;
    if (stderr) {
      stderr.on("data", (chunk: Buffer) => {
        process.stderr.write(`[sidecar:${name}] ${chunk.toString()}`);
      });
    }
    child.on("exit", (code, signal) => {
      this.failAll(
        new SidecarUnavailableError(
          `sidecar ${name} exited (code=${code ?? "null"}, signal=${signal ?? "null"})`,
        ),
      );
    });
    child.on("error", (error) => {
      this.failAll(new SidecarUnavailableError(`sidecar ${name} process error: ${error.message}`));
    });
    const stdout = child.stdout;
    if (!stdout) {
      throw new SidecarProtocolError(`sidecar ${name} has no stdout pipe`);
    }
    readline
      .createInterface({ input: stdout, crlfDelay: Infinity })
      .on("line", (line) => this.handleLine(line));
  }

  static async start(binPath: string, expectedName: string): Promise<SidecarProcess> {
    const child = spawn(binPath, [], {
      stdio: ["pipe", "pipe", "pipe"],
      windowsHide: true,
    });
    const ready = await readReadyFrame(child, expectedName);
    return new SidecarProcess(expectedName, child, ready);
  }

  private assertReady(ready: SidecarReadyData): void {
    if (ready.sidecar !== this.name) {
      throw new SidecarProtocolError(
        `sidecar ready mismatch: expected ${this.name}, got ${ready.sidecar}`,
      );
    }
    if (ready.proto !== SIDECAR_PROTOCOL_VERSION) {
      throw new SidecarProtocolError(
        `sidecar ${this.name} protocol ${ready.proto} != ${SIDECAR_PROTOCOL_VERSION}`,
      );
    }
  }

  onEvent(listener: SidecarEventListener): void {
    this.listeners.add(listener);
  }

  isAlive(): boolean {
    return !this.dead && this.child.exitCode === null && this.child.signalCode === null;
  }

  async request(method: string, params: unknown, timeoutMs = DEFAULT_REQUEST_TIMEOUT_MS): Promise<unknown> {
    if (!this.isAlive()) {
      throw new SidecarUnavailableError(`sidecar ${this.name} is not running`);
    }
    const id = this.nextId++;
    const frame: SidecarFrame = { v: SIDECAR_PROTOCOL_VERSION, id, method, params };
    return new Promise<unknown>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(
          new SidecarUnavailableError(`sidecar ${this.name} request ${method} timed out after ${timeoutMs}ms`),
        );
      }, timeoutMs);
      this.pending.set(id, { resolve, reject, timer });
      const stdin = this.child.stdin;
      if (!stdin) {
        this.pending.delete(id);
        clearTimeout(timer);
        reject(new SidecarUnavailableError(`sidecar ${this.name} has no stdin pipe`));
        return;
      }
      stdin.write(`${JSON.stringify(frame)}\n`, (error) => {
        if (error) {
          this.pending.delete(id);
          clearTimeout(timer);
          reject(new SidecarUnavailableError(`sidecar ${this.name} write failed: ${error.message}`));
        }
      });
    });
  }

  async shutdown(): Promise<void> {
    if (!this.isAlive()) return;
    try {
      await this.request("shutdown", undefined, SHUTDOWN_GRACE_MS);
    } catch {
      // shutdown 响应超时/进程已退场都走 kill 收尾；shutdown 是尽力而为的礼貌路径。
    }
    await new Promise<void>((resolve) => {
      const timer = setTimeout(() => {
        this.child.kill("SIGKILL");
        resolve();
      }, SHUTDOWN_GRACE_MS);
      this.child.once("exit", () => {
        clearTimeout(timer);
        resolve();
      });
    });
  }

  private handleLine(line: string): void {
    let frame: SidecarFrame;
    try {
      frame = JSON.parse(line) as SidecarFrame;
    } catch {
      return; // 非 JSON 行：协议允许的 proto_error 通道之外只有诊断；忽略。
    }
    if ("event" in frame) {
      for (const listener of this.listeners) {
        listener(frame.event, frame.data);
      }
      return;
    }
    if (!("ok" in frame)) return;
    const pending = this.pending.get(frame.id);
    if (!pending) return;
    this.pending.delete(frame.id);
    clearTimeout(pending.timer);
    if (frame.ok) {
      pending.resolve(frame.result);
    } else {
      pending.reject(new SidecarMethodError(frame.error.code, frame.error.message));
    }
  }

  private failAll(error: Error): void {
    if (this.dead) return;
    this.dead = true;
    for (const [, pending] of this.pending) {
      clearTimeout(pending.timer);
      pending.reject(error);
    }
    this.pending.clear();
    for (const listener of this.listeners) {
      listener("__exited__", { name: this.name });
    }
    this.listeners.clear();
  }
}

async function readReadyFrame(child: ChildProcess, expectedName: string): Promise<SidecarReadyData> {
  const stdout = child.stdout;
  if (!stdout) {
    child.kill("SIGKILL");
    throw new SidecarProtocolError(`sidecar ${expectedName} has no stdout pipe`);
  }
  return new Promise<SidecarReadyData>((resolve, reject) => {
    const timer = setTimeout(() => {
      child.kill("SIGKILL");
      reject(new SidecarUnavailableError(`sidecar ${expectedName} did not send ready in time`));
    }, 10_000);
    readline
      .createInterface({ input: stdout, crlfDelay: Infinity })
      .once("line", (line) => {
        clearTimeout(timer);
        try {
          const frame = JSON.parse(line) as SidecarFrame;
          if (!("event" in frame) || frame.event !== "ready") {
            throw new Error("first frame is not event:ready");
          }
          const data = frame.data as SidecarReadyData;
          if (typeof data?.sidecar !== "string" || typeof data?.proto !== "number") {
            throw new Error("ready data missing sidecar/proto");
          }
          resolve(data);
        } catch (error) {
          child.kill("SIGKILL");
          reject(
            new SidecarProtocolError(
              `sidecar ${expectedName} ready handshake failed: ${(error as Error).message}`,
            ),
          );
        }
      });
  });
}
