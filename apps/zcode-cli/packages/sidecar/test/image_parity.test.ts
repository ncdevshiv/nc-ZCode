/**
* image sidecar semantic parity e2e: real zcode-image binary vs jimp.
 *
* parity level is **semantic level** (README diff table: jimp differs from Rust image encoder bytes),
* Therefore what is asserted here is "observable product semantics" rather than byte equality:
* 1. Consistent size decision-making: when the upper limit is not exceeded, neither side is scaled; when the upper limit is exceeded, the output size is consistent;
* 2. The original size reading is consistent (info vs Jimp.read);
* 3. The output is a **decodable image of the same format**, and the pixels are not blank (real visual input, not garbage bytes);
* 4. The mime decisions of the three output formats of png/jpeg/webp are consistent.
* These four are the properties that the model-vision path really depends on.
 *
* Run: cd apps/zcode-cli/packages/sidecar && node --import tsx --test test/image_parity.test.ts
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

/** The pixel value of jimp 1.6 is the 0xRRGGBBAA integer (no intToRGBA helper). */
function rgba(r: number, g: number, b: number, a = 0xff): number {
  return ((r << 24) | (g << 16) | (b << 8) | a) >>> 0;
}

/** Construct a deterministic gradient PNG (RGB changes to facilitate detection of non-blank/dislocated output). */
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

//Do not exceed the upper limit: do not scale on both sides
  const noResize = await tryNativeResizeToFit(wide, "image/png", 400);
  assert.equal(noResize, null, "under-limit images must pass through unchanged");

// Exceeding the upper limit: sidecar scaling, the output size is proportionally loaded into the upper limit
  const resized = await tryNativeResizeToFit(wide, "image/png", 80);
  assert.ok(resized, "over-limit images must be resized by the sidecar");
  assert.equal(resized.mime, "image/png");
  assert.ok(resized.width <= 80 && resized.height <= 80, "output must fit the limit");
// 200x100 loads 80 → 80x40
  assert.equal(resized.width, 80);
  assert.equal(resized.height, 40);

// Decode back to jimp to verify it is a real image
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
// At least two different colors = non-blank
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
// jimp 1.6 does not understand webp, instead verify the bytes according to the RIFF/WEBP container header (compared with sidecar's webp
// The encoder has nothing to do, it only proves that the output is a real webp file and not garbled).
  assert.equal(webp.data.subarray(0, 4).toString("ascii"), "RIFF");
  assert.equal(webp.data.subarray(8, 12).toString("ascii"), "WEBP");
  assert.equal(webp.width, 80, "empty ops = re-encode only, no resize");
  assert.equal(webp.height, 80);
});
