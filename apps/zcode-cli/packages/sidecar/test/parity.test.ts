/**
* diff sidecar parity e2e: real `zcode-diff` binary + real jsdiff, same input versus output.
 *
* fixture (native/crates/zcode-diff/tests/fixtures/jsdiff-parity.json, 87 examples) by
* Real npm diff@9.0.0 generation; byte-by-byte parity on the Rust side is locked by crate tests, locked by this test
* **Real link of TS side facade**: flag on → NDJSON stdio → Rust bin → return structure,
* Field-by-field equal to the local result of jsdiff with the same parameters. Missing fixture/missing binary = test failed, not skipped.
 *
* Run: cd apps/zcode-cli/packages/sidecar && node --import tsx --test test/parity.test.ts
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { after, test } from "node:test";
import { createTwoFilesPatch, diffLines, structuredPatch } from "diff";
import type { StructuredPatch } from "diff";

// The flag must take effect before calling facade (facade reads env every time it is called, and is set once at the process level).
process.env.ZCODE_NATIVE_SIDECARS = "diff";

const {
  nativeApplyPatch,
  nativeDiffLines,
  nativeStructuredPatch,
  nativeUnifiedPatch,
  resolveSidecarBin,
  shutdownSidecars,
  sidecarEnabled,
} = await import("../src/index.js");

test("flag semantics: default-on islands, explicit off, kill switch", async () => {
// Explicit flag (used by parity) is turned on
  assert.equal(sidecarEnabled("diff"), true);
  const { sidecarEnabled: freshEnabled } = await import(
    `../src/config.js?fresh=${Date.now()}`
  );
//Default set: diff/image/scan/watch is on, proc (waiting for soak) is off
  delete process.env.ZCODE_NATIVE_SIDECARS;
  assert.equal(freshEnabled("diff"), true, "diff is default-on after the cutover flip");
  assert.equal(freshEnabled("image"), true, "image is default-on per the user decision");
  assert.equal(freshEnabled("scan"), true, "scan is default-on after the divergence audit");
  assert.equal(freshEnabled("watch"), true, "watch is default-on after the divergence audit");
  assert.equal(freshEnabled("proc"), false, "proc stays off until MCP stdio soak");
// kill switch covers everything
  process.env.ZCODE_NATIVE_SIDECARS_OFF = "1";
  const { sidecarEnabled: killedEnabled } = await import(
    `../src/config.js?killed=${Date.now()}`
  );
  assert.equal(killedEnabled("diff"), false, "kill switch must force the TS path");
  assert.equal(killedEnabled("scan"), false, "kill switch must force the TS path");
  delete process.env.ZCODE_NATIVE_SIDECARS_OFF;
//Restore parity process status
  process.env.ZCODE_NATIVE_SIDECARS = "diff";
});

// End the sidecar before the process exits; otherwise recycling/exiting will compete with in-flight writing, and node:test will report
// "asynchronous activity after the test ended"（EPIPE）。
after(async () => {
  await shutdownSidecars();
});

const fixturePath = new URL(
  "../../../../../native/crates/nc-code-diff/tests/fixtures/jsdiff-parity.json",
  import.meta.url,
);
const fixtures = JSON.parse(readFileSync(fixturePath, "utf8")) as {
  unified: { name: string; params: Record<string, unknown>; expected: string }[];
  structured: {
    name: string;
    params: Record<string, unknown>;
    expected: Record<string, unknown>;
  }[];
  chunks: { name: string; params: Record<string, unknown>; expected: unknown }[];
  apply: { name: string; params: Record<string, unknown>; expected: unknown }[];
};

const str = (value: unknown): string => String(value);

test("sidecar binary resolves", () => {
  const bin = resolveSidecarBin("zcode-diff");
  assert.ok(bin, "zcode-diff binary must resolve (build native/target or set ZCODE_SIDECAR_DIR)");
});

for (const item of fixtures.unified) {
  test(`diff.unified parity: ${item.name}`, async () => {
    const params = item.params as {
      old: string;
      new: string;
      oldLabel?: string;
      newLabel?: string;
      oldHeader?: string;
      newHeader?: string;
      context?: number;
    };
    const viaSidecar = await nativeUnifiedPatch(
      params.oldLabel as string,
      params.newLabel as string,
      params.old,
      params.new,
      params.oldHeader,
      params.newHeader,
      { context: params.context },
    );
    const viaJsdiff = createTwoFilesPatch(
      params.oldLabel,
      params.newLabel,
      params.old,
      params.new,
      params.oldHeader,
      params.newHeader,
      { context: params.context },
    );
    assert.equal(viaSidecar, viaJsdiff);
    assert.equal(viaSidecar, item.expected, "jsdiff local results must first be consistent with fixture");
  });
}

for (const item of fixtures.structured) {
  test(`structuredPatch parity: ${item.name}`, async () => {
    const params = item.params as {
      old: string;
      new: string;
      oldLabel?: string;
      newLabel?: string;
      oldHeader?: string;
      newHeader?: string;
      context?: number;
    };
    const viaSidecar = await nativeStructuredPatch(
      params.oldLabel as string,
      params.newLabel as string,
      params.old,
      params.new,
      params.oldHeader,
      params.newHeader,
      { context: params.context },
    );
    const viaJsdiff = structuredPatch(
      params.oldLabel,
      params.newLabel,
      params.old,
      params.new,
      params.oldHeader,
      params.newHeader,
      { context: params.context },
    );
    assert.deepEqual(
      JSON.parse(JSON.stringify(viaSidecar ?? null)),
      JSON.parse(JSON.stringify(viaJsdiff ?? null)),
    );
    assert.deepEqual(
      JSON.parse(JSON.stringify(viaJsdiff ?? null)),
      item.expected,
      "jsdiff local results must first be consistent with fixture",
    );
  });
}

for (const item of fixtures.chunks) {
  test(`diff.lines parity: ${item.name}`, async () => {
    const params = item.params as { old: string; new: string };
    const viaSidecar = await nativeDiffLines(params.old, params.new);
    const viaJsdiff = diffLines(params.old, params.new);
    assert.deepEqual(viaSidecar, viaJsdiff);
    assert.deepEqual(viaJsdiff, item.expected, "jsdiff local results must first be consistent with fixture");
  });
}

for (const item of fixtures.apply) {
  test(`diff.applyPatch parity: ${item.name}`, async () => {
    const params = item.params as { source: string; hunks: unknown; fuzzFactor?: number };
    const viaSidecar = await nativeApplyPatch(params.source, params.hunks as StructuredPatch, {
      fuzzFactor: params.fuzzFactor ?? 0,
      autoConvertLineEndings: false,
    });
    const expected = item.expected as { applied: boolean; result?: string };
    if (!expected.applied) {
      assert.equal(viaSidecar, false);
      return;
    }
    assert.equal(viaSidecar, expected.result);
  });
}
