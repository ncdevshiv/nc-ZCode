# native/ — pure-native (Rust) workspace

Target: `specs/rust-pure-native-roadmap.md` (Rust core + Tauri shell, no JS runtime
ships). This workspace is built alongside — not instead of — the TS stack until the
S3 cutover.

## Layout

- `crates/zcode-v4-wire/` — Rust mirror of the V4 wire binary pure functions
  (`packages/shared/src/zcode-protocol-v4/{wire-binary,wire-codec}.ts`): CRC-32,
  wire base64 encode/decode (TS-lenient semantics), VQL byte lengths, topic
  notification envelope measurement.
- `fixtures/v4-wire/` — **shared golden vectors**, generated from the TS source.
  Both sides consume the same files; they are never derived per side.

## Source-of-truth rule (coexistence period)

`packages/shared` (zod schemas + TS wire functions) is the source of truth until
S3 exit of the roadmap. The Rust crate must reproduce these fixtures exactly.
Regeneration flow:

```sh
bun run golden:vectors        # regenerate fixtures from TS source
bun run golden:check          # fail if fixtures drift from TS source
bun run native:test           # cargo test (includes golden-vector conformance)
# or directly: cargo test --manifest-path native/Cargo.toml
```

## Fixture scope & known divergence boundaries

- `protocol-constants.json` mirrors only the limits this crate uses
  (`maxFrameBytes`, `logicalFrameAssemblyMaxFragments`,
  `transportEnvelopeIdMaxChars`); extending the crate requires extending the
  fixture in the same change.
- Envelope fixtures cover wire JSON in the 1-, 2-, and 3-byte VQL regimes and the
  escaping surface v4 frames actually produce (ASCII, CJK, emoji, quotes, `\n`,
  U+2028). Deliberately out of scope until v4 can emit them: lone surrogates
  (`JSON.stringify` escapes, serde refuses) and non-integer floats (number
  formatting differs between JS and serde). If a schema change starts emitting
  either, add fixtures first — fail closed on divergence, never silently accept.
- Base64 decode mirrors the **lenient** TS semantics (e.g. `AB==` is accepted even
  though it is non-canonical); strict-decoder crates like `base64` would reject it,
  which is why the crate hand-rolls decode instead of depending on a crate.

## Dependencies

Zero runtime dependencies so far. Any future crate dependency must be registered
in the third-party notice pipeline (see `scripts/generate-third-party-notices.mjs`
and `third-party/native-search/sources.json` for the existing model) in the same
change that introduces it.
