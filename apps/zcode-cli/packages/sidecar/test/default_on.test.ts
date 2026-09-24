/**
 * 默认开启路径（无 ZCODE_NATIVE_SIDECARS）真实跑一遍：facade 必须命中
 * 打包暂存目录的 release 二进制并与 jsdiff 一致。release 版在「默认开」这条
 * 真实产品路径上必须可用——不是只验证 debug 产物。
 *
 * 运行：cd apps/zcode-cli/packages/sidecar && node --import tsx --test test/default_on.test.ts
 */
import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { after, test } from "node:test";
import { createTwoFilesPatch, structuredPatch } from "diff";

// 显式清空 flag：验证「默认开」而不是「flag 开」。
delete process.env.ZCODE_NATIVE_SIDECARS;
delete process.env.ZCODE_NATIVE_SIDECARS_OFF;
delete process.env.ZCODE_SIDECAR_DIR;

const { nativeStructuredPatch, nativeUnifiedPatch, resolveSidecarBin, shutdownSidecars, sidecarEnabled } =
  await import("../src/index.js");

after(async () => {
  await shutdownSidecars();
});

test("diff is default-on and resolves the staged release binary", () => {
  assert.equal(sidecarEnabled("diff"), true, "diff must be default-on");
  const bin = resolveSidecarBin("zcode-diff");
  assert.ok(bin, "zcode-diff must resolve without any env override");
  assert.match(bin, /bundled-tools[\\/][^\\/]+[\\/]sidecar[\\/]zcode-diff(\.exe)?$/);
  assert.ok(existsSync(bin), `resolved binary must exist: ${bin}`);
});

test("default-on path returns jsdiff-identical results", async () => {
  const oldText = "a\nb\nc\n";
  const newText = "a\nB\nc\n";
  const structured = await nativeStructuredPatch("f.txt", "f.txt", oldText, newText, undefined, undefined, {
    context: 4,
  });
  const unified = await nativeUnifiedPatch("f.txt", "f.txt", oldText, newText, undefined, undefined, {
    context: 4,
  });
  assert.deepEqual(
    JSON.parse(JSON.stringify(structured ?? null)),
    JSON.parse(
      JSON.stringify(
        structuredPatch("f.txt", "f.txt", oldText, newText, undefined, undefined, { context: 4 }),
      ),
    ),
  );
  assert.equal(
    unified,
    createTwoFilesPatch("f.txt", "f.txt", oldText, newText, undefined, undefined, { context: 4 }),
  );
});
