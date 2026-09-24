//! image facade：位图处理（替代 TS `adapters/src/image` 的 jimp 热路径）。
//!
//! 语义：sidecar `info` 读尺寸、`transform` 做 contain/cover/fill 变换 + 编码。
//! **parity 级别是语义级而非字节级**（jimp 与 Rust image 编码器字节不同，
//! 见 native/crates/zcode-image/README 差异表）：本 facade 保证「尺寸/格式/
//! mime 决策」一致，像素采样允许差异——model-vision 输入是视觉语义，不是文件
//! 字节。flag 关 = jimp；flag 开 = sidecar；不可用 → 告警并退回 jimp。

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
 * 变换并编码。ops 按顺序执行；`scaleToFit` 与 `contain` 同义（装入保持比例，
 * 对齐 sidecar 契约）。quality 默认 100，只对 jpeg 生效（镜像 TS）。
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

/** true = 已由 sidecar 完成；false = 调用方应走 jimp。 */
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
