//! @zcode/sidecar - TS bridge for Rust native sidecar (S1 islands, spec:
//! specs/rust-native-crates.md cutover chapter).
//!
//! The public surface is deliberately narrowed: protocol type + process/pool life cycle + diff facade. When new island is connected
//! Add facade here, do not spawn the process in the business code.

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
  PROC_SIDECAR,
  nativeKillTree,
  nativeListTree,
  tryNativeTerminateProcessTree,
  type KillTreeFailure,
  type KillTreeResult,
  type ProcessInfo,
} from "./proc.js";
export {
  SCAN_SIDECAR,
  nativeScanStorageRoot,
  scanEnabled,
  type NativeScanEntry,
  type NativeScanError,
  type NativeScanOptions,
  type NativeScanResult,
} from "./scan.js";
export {
  WATCH_SIDECAR,
  createNativeFileWatcher,
  shutdownNativeWatchers,
  watchEnabled,
  type FileWatchEvent,
  type NativeWatchHandle,
} from "./watch.js";
export {
  IMAGE_SIDECAR,
  imageEnabled,
  nativeImageInfo,
  nativeTransformImage,
  tryNativeResizeToFit,
  type ImageInfo,
  type ImageOutputFormat,
  type ImageTransformOp,
  type ImageTransformResult,
} from "./image.js";
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
// The structural shape of jsdiff is exclusive to this package (the runtime and types are all converged in the adapter); the business package is no longer
// Directly depends on `diff`.
export type { StructuredPatch } from "diff";
