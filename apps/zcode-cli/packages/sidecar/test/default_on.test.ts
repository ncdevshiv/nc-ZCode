/**
* The default open path (without ZCODE_NATIVE_SIDECARS) is actually run: the facade must hit
* Package the release binary of the staging directory and make it consistent with jsdiff. The release version is in the "Default enabled" section.
* Must be available on the real product path - not just verify the debug product.
 *
* Run: cd apps/zcode-cli/packages/sidecar && node --import tsx --test test/default_on.test.ts
 */
import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { after, test } from "node:test";
import { createTwoFilesPatch, structuredPatch } from "diff";

// Explicitly clear flag: verify "default on" rather than "flag on".
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
