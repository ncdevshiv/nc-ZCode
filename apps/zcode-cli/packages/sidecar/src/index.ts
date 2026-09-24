//! @zcode/sidecar —— Rust native sidecar 的 TS 桥（S1 islands，spec:
//! specs/rust-native-crates.md cutover 章节）。
//!
//! 公开面刻意收窄：协议类型 + 进程/pool 生命周期 + diff facade。新 island 接入时
//! 在此加 facade，不在业务代码里 spawn 进程。

export {
  SidecarMethodError,
  SidecarProtocolError,
  SidecarUnavailableError,
  SIDECAR_PROTOCOL_VERSION,
  type SidecarEventListener,
  type SidecarFrame,
  type SidecarReadyData,
} from "./types.js";
export { SidecarPool } from "./pool.js";
export { SidecarProcess } from "./process.js";
export { resolveSidecarBin, sidecarEnabled } from "./config.js";
export { shutdownSidecars, sidecarPool } from "./pool.js";
export {
  DIFF_SIDECAR,
  DIFF_TIMEOUT_MS,
  nativeApplyPatch,
  nativeDiffLines,
  nativeStructuredPatch,
  nativeUnifiedPatch,
  type ApplyPatchOptions,
  type DiffLinesChunk,
  type StructuredPatchOptions,
} from "./diff.js";
// jsdiff 的结构形状由本包独占（运行时与类型都收敛在 adapter 内）；业务包不再
// 直接依赖 `diff`。
export type { StructuredPatch } from "diff";
