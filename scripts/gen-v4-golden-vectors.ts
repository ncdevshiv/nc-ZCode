/**
 * S0 golden-vector generator (specs/rust-pure-native-roadmap.md).
 *
 * Source of truth during TS↔Rust coexistence: packages/shared/src/zcode-protocol-v4.
 * This script derives shared fixtures into native/fixtures/v4-wire/ from the real
 * TS implementations — it must never hand-write expected values. The Rust crate
 * (native/crates/zcode-v4-wire) consumes the same files as its conformance net.
 *
 * Usage:
 *   pnpm golden:vectors          regenerate fixtures
 *   pnpm golden:check --         fail (exit 1) when fixtures drift from TS source
 */
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import {
  PROTOCOL_V4_LIMITS,
  V4_WIRE_PROTOCOL_VERSION,
} from "../packages/shared/src/zcode-protocol-v4/core.js";
import type { TopicWireFrameCandidate } from "../packages/shared/src/zcode-protocol-v4/wire.js";
import {
  crc32WireBytes,
  decodeWireBase64,
  encodeWireBytesBase64,
} from "../packages/shared/src/zcode-protocol-v4/wire-binary.js";
import {
  measureTopicNotificationEnvelopeBytes,
  utf8JsonByteLength,
} from "../packages/shared/src/zcode-protocol-v4/wire-codec.js";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const fixturesDir = join(repoRoot, "native", "fixtures", "v4-wire");

const encoder = new TextEncoder();

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(`golden-vector generator: ${message}`);
}

function utf8Bytes(text: string): number[] {
  return Array.from(encoder.encode(text));
}

/** Deterministic 32-bit LCG byte pattern — stable across runs and platforms. */
function lcgBytes(count: number): number[] {
  let state = 0x5eed;
  const out: number[] = [];
  for (let i = 0; i < count; i += 1) {
    state = (Math.imul(state, 1103515245) + 12345) >>> 0;
    out.push((state >>> 16) & 0xff);
  }
  return out;
}

function serialize(name: string, value: unknown): string {
  return `${JSON.stringify(value, null, 2)}\n`;
}

// ---------------------------------------------------------------------------
// protocol-constants.json — only the PROTOCOL_V4_LIMITS subset the crate mirrors.
// ---------------------------------------------------------------------------
function buildConstantsFixture(): string {
  return serialize("protocol-constants", {
    wireVersion: V4_WIRE_PROTOCOL_VERSION,
    source: "packages/shared/src/zcode-protocol-v4/core.ts",
    limits: {
      maxFrameBytes: PROTOCOL_V4_LIMITS.maxFrameBytes,
      logicalFrameAssemblyMaxFragments: PROTOCOL_V4_LIMITS.logicalFrameAssemblyMaxFragments,
      transportEnvelopeIdMaxChars: PROTOCOL_V4_LIMITS.transportEnvelopeIdMaxChars,
    },
  });
}

// ---------------------------------------------------------------------------
// binary-vectors.json — crc32 / base64 encode + lenient decode from the TS code.
// ---------------------------------------------------------------------------
function buildBinaryFixture(): string {
  const inputs: Array<{ name: string; bytes: number[] }> = [
    { name: "empty", bytes: [] },
    { name: "single-zero", bytes: [0] },
    { name: "single-ff", bytes: [255] },
    { name: "two-bytes-pad2", bytes: [0xde, 0xad] },
    { name: "three-bytes-pad0", bytes: [0xde, 0xad, 0xbe] },
    { name: "four-bytes-pad1", bytes: [0xde, 0xad, 0xbe, 0xef] },
    { name: "seq-0-255", bytes: lcgBytes(0).concat(Array.from({ length: 256 }, (_, i) => i)) },
    { name: "ascii-hello", bytes: utf8Bytes("hello zcode") },
    { name: "unicode-cjk-emoji", bytes: utf8Bytes('中文 🚀 line\nend\t"quote" end') },
    { name: "lcg-1000", bytes: lcgBytes(1000) },
  ];

  const cases = inputs.map(({ name, bytes }) => {
    const u8 = Uint8Array.from(bytes);
    const encoded = encodeWireBytesBase64(u8);
    const decoded = decodeWireBase64(encoded);
    const decodedBytes = decoded === null ? null : Array.from(decoded);
    // Real TS invariant: every non-empty input round-trips; empty encodes to ""
    // which the `.min(4)` schema rejects on decode — recorded as-is (null).
    if (bytes.length > 0) {
      assert(
        decodedBytes !== null &&
          decodedBytes.length === bytes.length &&
          decodedBytes.every((b, i) => b === bytes[i]),
        `${name}: TS roundtrip must reproduce input`,
      );
    } else {
      assert(decodedBytes === null, `${name}: TS must reject empty encoded value`);
    }
    return { name, bytes, crc32: crc32WireBytes(u8), encoded, decoded: decodedBytes };
  });

  // TS decode edge semantics (incl. leniency like non-canonical "AB==") are the
  // contract; expected bytes come from running the TS decode itself.
  const decodeInputs = [
    { name: "canonical-two-pad", value: "QQ==" },
    { name: "canonical-three-pad", value: "UWE=" },
    { name: "noncanonical-two-pad-lenient", value: "AB==" },
    { name: "all-groups-no-pad", value: "QUJDRA==" },
    { name: "empty-too-short", value: "" },
    { name: "one-char", value: "A" },
    { name: "three-chars", value: "AAA" },
    { name: "five-chars", value: "AB===" },
    { name: "equals-in-middle", value: "AA==AA==" },
    { name: "pad-not-at-end", value: "AA=A" },
    { name: "nonalphabet-unicode", value: "中文==" },
    { name: "junk-chars", value: "A!==xxxx" },
    { name: "all-equals", value: "====" },
  ];
  const decodeEdgeCases = decodeInputs.map(({ name, value }) => {
    const decoded = decodeWireBase64(value);
    const bytes = decoded === null ? null : Array.from(decoded);
    return { name, value, decoded: bytes };
  });

  return serialize("binary-vectors", {
    source: "packages/shared/src/zcode-protocol-v4/wire-binary.ts",
    cases,
    decodeEdgeCases,
  });
}

// ---------------------------------------------------------------------------
// envelope-measurements.json — measureTopicNotificationEnvelopeBytes ground truth.
// Sizes chosen to cover 2-byte (small) and 3-byte (20KB) VQL wire regimes.
// ---------------------------------------------------------------------------
function buildEnvelopeFixture(): string {
  const wires: Array<{ name: string; wire: Record<string, unknown> }> = [
    {
      name: "minimal-complete",
      wire: {
        wireVersion: V4_WIRE_PROTOCOL_VERSION,
        kind: "complete",
        deliveryKind: "primary",
        logicalFrameId: "lf-1",
        logicalFrameOrdinal: 0,
        topic: "conversation",
        subscriptionId: "sub-1",
        frame: { text: "hi" },
      },
    },
    {
      name: "fragment-with-checksum",
      wire: {
        wireVersion: V4_WIRE_PROTOCOL_VERSION,
        kind: "fragment",
        deliveryKind: "primary",
        logicalFrameId: "lf-2",
        logicalFrameOrdinal: 7,
        topic: "conversation",
        subscriptionId: "sub-1",
        fragmentIndex: 2,
        fragmentCount: 5,
        logicalBytes: 123_456,
        checksum: { algorithm: "crc32", value: "deadbeef" },
        dataBase64: "A".repeat(4 * Math.ceil(600 / 3)),
      },
    },
    {
      name: "unicode-payload",
      wire: {
        wireVersion: V4_WIRE_PROTOCOL_VERSION,
        kind: "complete",
        deliveryKind: "primary",
        logicalFrameId: "lf-3",
        logicalFrameOrdinal: 1,
        topic: "conversation",
        subscriptionId: "sub-2",
        frame: {
          text: '中文 🚀 "quoted"\nline end',
          emoji: "👩‍💻🌍",
        },
      },
    },
    {
      name: "large-20k-payload",
      wire: {
        wireVersion: V4_WIRE_PROTOCOL_VERSION,
        kind: "complete",
        deliveryKind: "primary",
        logicalFrameId: "lf-4",
        logicalFrameOrdinal: 2,
        topic: "conversation",
        subscriptionId: "sub-2",
        frame: { content: "x".repeat(20_000) },
      },
    },
  ];

  const cases = wires.map(({ name, wire }) => {
    const candidate = wire as unknown as TopicWireFrameCandidate;
    const wireJson = JSON.stringify(wire);
    const measurement = measureTopicNotificationEnvelopeBytes(candidate);
    assert(
      measurement.maxBytes ===
        Math.max(
          measurement.cliNdjsonBytes,
          measurement.channelSocketBytes,
          measurement.mobileRelayBytes,
        ),
      `${name}: TS maxBytes must be the max of the three carriers`,
    );
    return {
      name,
      wireJson,
      wireJsonBytes: utf8JsonByteLength(wire),
      measurement: {
        cliNdjsonBytes: measurement.cliNdjsonBytes,
        channelSocketBytes: measurement.channelSocketBytes,
        mobileRelayBytes: measurement.mobileRelayBytes,
        maxBytes: measurement.maxBytes,
      },
    };
  });

  return serialize("envelope-measurements", {
    source: "packages/shared/src/zcode-protocol-v4/wire-codec.ts",
    cases,
  });
}

function main(): void {
  const files = new Map<string, string>([
    ["protocol-constants.json", buildConstantsFixture()],
    ["binary-vectors.json", buildBinaryFixture()],
    ["envelope-measurements.json", buildEnvelopeFixture()],
  ]);

  const checkMode = process.argv.includes("--check");
  if (checkMode) {
    const drifted: string[] = [];
    for (const [name, expected] of files) {
      const path = join(fixturesDir, name);
      let actual: string | null = null;
      try {
        actual = readFileSync(path, "utf8");
      } catch {
        actual = null;
      }
      if (actual !== expected) drifted.push(name);
    }
    if (drifted.length > 0) {
      console.error(
        `[golden-vectors] DRIFT: fixtures out of sync with TS source: ${drifted.join(", ")}`,
      );
      console.error("[golden-vectors] run `pnpm golden:vectors` and review the diff");
      process.exitCode = 1;
      return;
    }
    console.log(`[golden-vectors] OK: ${files.size} fixtures match TS source`);
    return;
  }

  mkdirSync(fixturesDir, { recursive: true });
  for (const [name, content] of files) {
    writeFileSync(join(fixturesDir, name), content, "utf8");
    console.log(`[golden-vectors] wrote native/fixtures/v4-wire/${name}`);
  }
}

main();
