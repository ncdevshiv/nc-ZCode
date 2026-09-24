/**
 * image sidecar 语义 parity e2e：真实 zcode-image 二进制 vs jimp。
 *
 * parity 级别是**语义级**（README 差异表：jimp 与 Rust image 编码器字节不同），
 * 因此这里断言的是「可观察的产品语义」而不是字节相等：
 *   1. 尺寸决策一致：不超过上限时两侧都不缩放；超过时输出尺寸一致；
 *   2. 原始尺寸读取一致（info vs Jimp.read）；
 *   3. 输出是**可解码的同格式图像**，且像素非空白（真实视觉输入，不是垃圾字节）；
 *   4. png/jpeg/webp 三种输出格式的 mime 决策一致。
 * 这四条是 model-vision 路径真正依赖的性质。
 *
 * 运行：cd apps/zcode-cli/packages/sidecar && node --import tsx --test test/image_parity.test.ts
 */
import assert from "node:assert/strict";
import { after, test } from "node:test";
import { Jimp } from "jimp";

process.env.ZCODE_NATIVE_SIDECARS = "image";

const { nativeImageInfo, nativeTransformImage, tryNativeResizeToFit, shutdownSidecars, imageEnabled } =
  await import("../src/index.js");

after(async () => {
  await shutdownSidecars();
});

/** jimp 1.6 的像素值是 0xRRGGBBAA 整数（无 intToRGBA 辅助）。 */
function rgba(r: number, g: number, b: number, a = 0xff): number {
  return ((r << 24) | (g << 16) | (b << 8) | a) >>> 0;
}

/** 构造一张确定性渐变 PNG（RGB 变化，便于检测输出非空白/错位）。 */
async function makePng(width: number, height: number): Promise<Buffer> {
  const image = new Jimp({ width, height, color: 0x000000ff });
  for (let y = 0; y < height; y += 1) {
    for (let x = 0; x < width; x += 1) {
      const r = Math.floor((x / Math.max(1, width - 1)) * 255);
      const g = Math.floor((y / Math.max(1, height - 1)) * 255);
      image.setPixelColor(rgba(r, g, 0), x, y);
    }
  }
  return image.getBuffer("image/png");
}

test("image is default-on after the flip", () => {
  assert.equal(imageEnabled(), true, "image must be default-on per the user decision");
});

test("info matches jimp dimensions and mime", async () => {
  const png = await makePng(64, 32);
  const nativeInfo = await nativeImageInfo(png);
  const jimpImage = await Jimp.read(png);
  assert.equal(nativeInfo.width, jimpImage.bitmap.width);
  assert.equal(nativeInfo.height, jimpImage.bitmap.height);
  assert.equal(nativeInfo.mime, "image/png");
});

test("resize dimension decisions match jimp semantics", async () => {
  const wide = await makePng(200, 100);

  // 不超过上限：两侧都不缩放
  const noResize = await tryNativeResizeToFit(wide, "image/png", 400);
  assert.equal(noResize, null, "under-limit images must pass through unchanged");

  // 超过上限：sidecar 缩放，输出尺寸按比例装入上限内
  const resized = await tryNativeResizeToFit(wide, "image/png", 80);
  assert.ok(resized, "over-limit images must be resized by the sidecar");
  assert.equal(resized.mime, "image/png");
  assert.ok(resized.width <= 80 && resized.height <= 80, "output must fit the limit");
  // 200x100 装入 80 → 80x40
  assert.equal(resized.width, 80);
  assert.equal(resized.height, 40);

  // 解码回 jimp 验证是真实图像
  const decoded = await Jimp.read(resized.data);
  assert.equal(decoded.bitmap.width, 80);
  assert.equal(decoded.bitmap.height, 40);
});

test("output is a decodable non-blank image (real vision input)", async () => {
  const source = await makePng(120, 120);
  const output = await nativeTransformImage(
    source,
    [{ type: "contain", width: 60, height: 60 }],
    "png",
  );
  assert.equal(output.mime, "image/png");
  const decoded = await Jimp.read(output.data);
  assert.equal(decoded.bitmap.width, 60);
  assert.equal(decoded.bitmap.height, 60);
  // 至少两种不同颜色 = 非空白
  const first = decoded.getPixelColor(0, 0);
  const last = decoded.getPixelColor(59, 59);
  assert.notEqual(first, last, "resized gradient must preserve color variation");
});

test("jpeg/webp output formats encode and report correct mime", async () => {
  const source = await makePng(80, 80);
  const jpeg = await nativeTransformImage(source, [{ type: "contain", width: 40, height: 40 }], "jpeg");
  assert.equal(jpeg.mime, "image/jpeg");
  const decodedJpeg = await Jimp.read(jpeg.data);
  assert.equal(decodedJpeg.bitmap.width, 40);

  const webp = await nativeTransformImage(source, [], "webp");
  assert.equal(webp.mime, "image/webp");
  // jimp 1.6 不解 webp，改按 RIFF/WEBP 容器头验证字节（与 sidecar 的 webp
  // 编码器无关，只证明输出是真正的 webp 文件而不是乱码）。
  assert.equal(webp.data.subarray(0, 4).toString("ascii"), "RIFF");
  assert.equal(webp.data.subarray(8, 12).toString("ascii"), "WEBP");
  assert.equal(webp.width, 80, "empty ops = re-encode only, no resize");
  assert.equal(webp.height, 80);
});
