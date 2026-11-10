# native/ — pure-native (Rust) workspace

Target: `specs/rust-pure-native-roadmap.md` (Rust core + Tauri shell, no JS runtime
ships). This workspace is built alongside — not instead of — the TS stack until the
S3 cutover.

## Layout (updated 2026-11-06; see trace below)

<!-- trace:2026-11-06 docs sync — replaces stale zcode-v4-wire-only layout. Workspace = `members = ["crates/nc-code-*"]` (native/Cargo.toml:7); glob-counted 54 nc-code-* crates (incl. 35 nc-code-services-*). Old 12-crate zcode-* map superseded; per-crate map now in specs/rust-native-crates.md. -->

- Core libs: `nc-code-v4-wire` (V4 wire binary: CRC-32, lenient base64, VQL lengths),
  `nc-code-core` (CommandInbox admission, lease, identity), `nc-code-rpc` +
  `nc-code-channels` (channel RPC wire + registry), `nc-code-cdp`.
- S1 islands: `nc-code-sidecar-proto` + `nc-code-watch` / `nc-code-scan` /
  `nc-code-proc` / `nc-code-image` / `nc-code-diff`.
- Agent path: `nc-code-provider` (model executor), `nc-code-workflow`,
  `nc-code-host` (198 `.rs` files / 83,922 lines, glob-counted 2026-11-06;
  43 tools: 32 sync + 11 async, `src/tools/mod.rs:258-369`; V4 all 31 wired,
  `src/rpc.rs:56`). Agentic kernels `nct-mcp` / `nct-core` pulled from
  `G:/nc-sdk/nc-tools` (`nc-code-host/Cargo.toml:59-60`).
- App/server/shell: `nc-code-app` (composition root — real channel registry),
  `nc-code-server` (`channel_router` 8 live routes: 4 base `lib.rs:283-286`
  + 4 `/api/remote/*` via `remote_api.rs:53-56`), `nc-code-shell` (32 `.rs`
  files; `src/backend.rs:66` `build_with_ports` + `:93` `serve` = real embedded
  server on `127.0.0.1:0`).
- Services: 35 `nc-code-services-*` crates + `nc-code-service-core` (one per TS
  service domain; see `nc-code-app/src/lib.rs` `build_with_ports` registry).
- `fixtures/v4-wire/` — **shared golden vectors**, generated from the TS source.
  Both sides consume the same files; they are never derived per side.

## Source-of-truth rule (coexistence period)

`packages/shared` (zod schemas + TS wire functions) is the source of truth until
S3 exit of the roadmap. The Rust crates must reproduce these fixtures exactly.
Regeneration flow:

```sh
bun run golden:vectors        # regenerate fixtures from TS source
bun run golden:check          # fail if fixtures drift from TS source
bun run native:test           # cargo test (includes golden-vector conformance)
# or directly: cargo test --manifest-path native/Cargo.toml
bun run native:smoke          # boot the real zcode-server binary and drive its HTTP+WS surface
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

Workspace deps: `serde`, `serde_json`, `tokio` (`native/Cargo.toml:10-15`); crate
edges live in each crate's `Cargo.toml` (host: `nc-code-host/Cargo.toml:50-60`;
shell: `nc-code-shell/Cargo.toml:33-44`). Any new crate dependency must be registered
in the third-party notice pipeline (see `scripts/generate-third-party-notices.mjs`
and `third-party/native-search/sources.json` for the existing model) in the same
change that introduces it.
