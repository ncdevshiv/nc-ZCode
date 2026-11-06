//! image facade: Bitmap processing (replaces the jimp hot path of TS `adapters/src/image`).
//!
//! Semantics: sidecar `info` reads size, `transform` does contain/cover/fill transformation + encoding.
//! **parity level is semantic level rather than byte level** (jimp is different from Rust image encoder byte,
//! See native/crates/zcode-image/README difference table): This facade guarantees "size/format/
//! mime decisions are consistent, pixel sampling allows differences - model-vision input is visual semantics, not files
//! byte. flag off = jimp; flag on = sidecar; unavailable → warn and return to jimp.

import { sidecarEnabled } from "./config.js";
import { sidecarPool } from "./pool.js";
import { SidecarUnavailableError } from "./types.js";

const IMAGE_SIDECAR = "zcode-image";
const REQUEST_TIMEOUT_MS = 30_000;

export type ImageOutputFormat = "jpeg" | "png" | "webp";

export interface ImageInfo {
  width: number;
  height: number;
  format: string;
  mime: string;
}

export interface ImageTransformOp {
  type: "contain" | "cover" | "fill" | "scaleToFit";
  width: number;
  height: number;
}

export interface ImageTransformResult {
  data: Buffer;
  width: number;
  height: number;
  mime: string;
  sizeBytes: number;
}

function asOutputFormat(mime: string | undefined): ImageOutputFormat {
  if (mime === "image/png") return "png";
  if (mime === "image/webp") return "webp";
  return "jpeg";
}

function warnOnce(key: string, message: string): void {
  process.stderr.write(`[sidecar:${IMAGE_SIDECAR}] ${message}\n`);
  void key;
}

export function imageEnabled(): boolean {
  return sidecarEnabled("image");
}

export async function nativeImageInfo(data: Buffer): Promise<ImageInfo> {
  const sidecar = await sidecarPool().acquire(IMAGE_SIDECAR);
  try {
    return (await sidecar.request(
      "info",
      { data: data.toString("base64") },
      REQUEST_TIMEOUT_MS,
    )) as ImageInfo;
  } finally {
    sidecarPool().release(sidecar);
  }
}

/**
* Transform and encode. ops are executed sequentially; `scaleToFit` is synonymous with `contain` (loads maintain scale,
* align sidecar contract). Quality defaults to 100, only effective for jpeg (mirror TS).
 */
export async function nativeTransformImage(
  data: Buffer,
  ops: ImageTransformOp[],
  output: ImageOutputFormat,
  quality = 100,
): Promise<ImageTransformResult> {
  const sidecar = await sidecarPool().acquire(IMAGE_SIDECAR);
  try {
    const result = (await sidecar.request(
      "transform",
      {
        data: data.toString("base64"),
        ops: ops.map((op) => ({ type: op.type, width: op.width, height: op.height })),
        output,
        quality,
      },
      REQUEST_TIMEOUT_MS,
    )) as { data: string; width: number; height: number; mime: string; sizeBytes: number };
    return {
      data: Buffer.from(result.data, "base64"),
      width: result.width,
      height: result.height,
      mime: result.mime,
      sizeBytes: result.sizeBytes,
    };
  } finally {
    sidecarPool().release(sidecar);
  }
}

/** true = Completed by sidecar; false = caller should go jimp. */
export async function tryNativeResizeToFit(
  data: Buffer,
  mediaType: string,
  maxDimension: number,
): Promise<ImageTransformResult | null> {
  if (!imageEnabled()) return null;
  if (mediaType === "image/webp") return null; // TS 对 webp 直通，不缩放。
  try {
    const info = await nativeImageInfo(data);
    if (info.width <= maxDimension && info.height <= maxDimension) {
      return null; // 不需要缩放：调用方保留原图（与 TS 同分支）。
    }
    return await nativeTransformImage(
      data,
      [{ type: "contain", width: maxDimension, height: maxDimension }],
      asOutputFormat(mediaType),
    );
  } catch (error) {
    if (error instanceof SidecarUnavailableError) {
      warnOnce("resizeToFit", `${error.message}; falling back to jimp`);
      return null;
    }
    throw error;
  }
}

export { IMAGE_SIDECAR };
