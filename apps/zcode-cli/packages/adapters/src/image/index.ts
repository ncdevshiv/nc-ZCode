import { Jimp, ResizeStrategy } from "jimp";
import {
  tryNativeResizeToFit,
  type ImageOutputFormat,
} from "@zcode/sidecar";
import {
  type ImagePrepareForModelRequest,
  type ImagePrepareForModelResult,
  type ImageProcessorPort,
  type ImageResizeRequest,
  type ImageResizeResult,
} from "@zcode/contracts";
import { prepareJimpImageForModel } from "./jimp-compression.js";
import { jimpOutputMediaType, throwIfAborted } from "./jimp-media.js";

export function createJimpImageProcessorAdapter(): ImageProcessorPort {
  return new JimpImageProcessorAdapter();
}

class JimpImageProcessorAdapter implements ImageProcessorPort {
  async resizeToFit(
    request: ImageResizeRequest,
    options: { signal?: AbortSignal } = {},
  ): Promise<ImageResizeResult> {
    throwIfAborted(options.signal);
    if (!Number.isFinite(request.maxDimension) || request.maxDimension <= 0) {
      throw new Error("Image resize maxDimension must be a positive finite number");
    }

    const input = Buffer.from(request.data);
    if (request.mediaType === "image/webp") {
      return {
        data: input,
        mediaType: request.mediaType,
        resized: false,
      };
    }

    // Rust zcode-image 路径（flag 开 + 可用）：尺寸/格式语义与 jimp 一致，
    // parity 为语义级（README 差异表）——真实字节在 test/image_parity.test.ts 断言。
    // 不可用或未超限时 facade 返回 null，走下面的 jimp 原路径。
    const native = await tryNativeResizeToFit(input, request.mediaType, request.maxDimension);
    if (native) {
      throwIfAborted(options.signal);
      return {
        data: native.data,
        mediaType: native.mime,
        width: native.width,
        height: native.height,
        resized: true,
      };
    }

    const image = await Jimp.read(input);
    throwIfAborted(options.signal);

    const originalWidth = image.bitmap.width;
    const originalHeight = image.bitmap.height;
    if (originalWidth <= request.maxDimension && originalHeight <= request.maxDimension) {
      return {
        data: input,
        mediaType: request.mediaType,
        originalWidth,
        originalHeight,
        width: originalWidth,
        height: originalHeight,
        resized: false,
      };
    }

    image.scaleToFit({
      h: request.maxDimension,
      mode: ResizeStrategy.BICUBIC,
      w: request.maxDimension,
    });
    throwIfAborted(options.signal);

    const outputMediaType = jimpOutputMediaType(request.mediaType, image.mime);
    const data = await image.getBuffer(outputMediaType);
    throwIfAborted(options.signal);

    return {
      data,
      mediaType: outputMediaType,
      originalWidth,
      originalHeight,
      width: image.bitmap.width,
      height: image.bitmap.height,
      resized: true,
    };
  }

  prepareForModel(
    request: ImagePrepareForModelRequest,
    options: { signal?: AbortSignal } = {},
  ): Promise<ImagePrepareForModelResult> {
    return prepareJimpImageForModel(request, options);
  }
}
