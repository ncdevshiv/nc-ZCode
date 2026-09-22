# Spec: Native crate map, sidecar protocol, stage acceptance

Owner: pending (architecture)
Status: S0–S5 executed 2026-09-22 (crate layer built, gates green — evidence in RUST-PORT-LOG.md ENTRY 005); S3 host integration executing 2026-09-22 (zcode-host increment, acceptance above); S5 host wiring and S4 R4 BrowserView gate remain OPEN; S6 not started
Date: 2026-09-22

Governing specs: `specs/rust-pure-native-roadmap.md` (stage definitions, §7 R1–R4),
`specs/rust-cli-provider-boundary.md` (S2 seam/event order), `specs/rust-serialization-ffi-boundary.md`
(encoding/process-boundary rules). This spec only defines the crate-level map, the
sidecar stdio protocol, cross-crate dependency direction, and per-stage acceptance.

## Crate map (stage → crate)

| Stage | Crate (`native/crates/`) | Kind        | Mirrors / replaces                                                         |
| ----- | ------------------------ | ----------- | -------------------------------------------------------------------------- |
| S1    | `zcode-sidecar-proto`    | lib         | NDJSON sidecar framing shared by all islands                               |
| S1    | `zcode-watch`            | lib + bin   | `packages/services/src/fileWatcher/fileWatcherService.ts`                  |
| S1    | `zcode-scan`             | lib + bin   | `packages/services/src/storage/adapters/fsWalker.ts`                       |
| S1    | `zcode-proc`             | lib + bin   | `taskkill /T /F` + scraped process lists (Job Objects)                     |
| S1    | `zcode-image`            | lib + bin   | `jimp` hot path in `apps/zcode-cli/.../adapters`                           |
| S1    | `zcode-diff`             | lib + bin   | npm `diff@9` (jsdiff) usage in `core` + `bootstrap`                        |
| S2    | `zcode-provider`         | lib         | `adapters/src/model/**` (`ModelExecutor` semantics)                        |
| S3    | `zcode-core`             | lib         | `command-inbox.ts` admission + owner/lease + identity key                  |
| S3    | `zcode-host`             | lib + bin   | TS Agent Host: stdio v4 RPC surface + session/projection/delivery profiles |
| S4    | `zcode-shell`            | bin (Tauri) | Electron main → Tauri shell, `IPlatformService` adapter                    |
| S5    | `zcode-server`           | lib + bin   | `packages/server` → axum (static UI + host port)                           |
| S0    | `zcode-v4-wire`          | lib         | v4 wire binary pure functions (already delivered)                          |

Dependency direction (enforced by review; no exceptions without updating this spec):

- Island bins (`watch`/`scan`/`proc`/`image`/`diff`) → `zcode-sidecar-proto` only.
- `zcode-provider`, `zcode-server`, `zcode-shell`, `zcode-v4-wire`:
  **no cross-crate edges yet** — integration edges arrive with the S3/S5 strangler
  cutover and must be added here first. No cycles possible until then.
- `zcode-host` (S3, first integration crate) may depend **only** on
  `zcode-v4-wire`, `zcode-core`, and `zcode-provider`. No other crate may depend on
  `zcode-host`. All later host edges (storage, terminal, MCP, frame routing) must be
  added here first.
- Workspace deps (`native/Cargo.toml`): `serde`, `serde_json`, `tokio`. Crate-specific
  deps (notify, image, windows, axum, reqwest, tauri, …) live in the crate's own
  `Cargo.toml`.

## Sidecar stdio protocol (S1 islands)

Transport: child process stdin/stdout, **NDJSON** (one JSON object per `\n`-terminated
line), stderr reserved for diagnostics. Same process-boundary encoding family as the
existing v4 NDJSON links (see serialization spec); no new binary framing.

Frame shapes (`zcode-sidecar-proto`, protocol version `1`):

```jsonc
{"v":1,"id":7,"method":"watch","params":{...}}      // request  (client → sidecar)
{"v":1,"id":7,"ok":true,"result":{...}}             // response success
{"v":1,"id":7,"ok":false,"error":{"code":404,"message":"..."}} // response failure
{"v":1,"event":"fs_event","data":{...}}             // async event (sidecar → client)
{"v":1,"event":"ready","data":{"sidecar":"zcode-watch","proto":1}} // first line on start
```

Rules:

1. Every frame carries `v`. Version mismatch → error frame
   (`code: 400 PROTOCOL_VERSION_MISMATCH`) then the sidecar exits(1). Fail closed.
2. Malformed line → `event:"proto_error"` (raw line truncated to 512 bytes) and the
   sidecar **keeps serving**; it never panics on client input.
3. `method:"shutdown"` → success response, then flush, then exit(0). EOF on stdin →
   same graceful path. Double shutdown is idempotent.
4. Events may interleave any request/response order; correlation is by `id` only.
5. Error codes: 400 protocol/params, 404 unknown method, 409 state conflict
   (e.g. duplicate watch id), 500 internal. Sidecar-internal failures are reported
   as error responses or `scan_error`-style events per method spec — never silent.

## Per-stage acceptance (what "done" means here)

- **S1**: each island = lib (unit-tested) + bin (stdio loop) + `tests/e2e_stdio.rs`
  spawning the real binary via `env!("CARGO_BIN_EXE_*")`. Diff parity proven against
  **fixtures generated by running the real npm `diff@9`** from the repo's node_modules.
  Image parity is **semantic** (dimensions/format/fit math), not byte-identical —
  jimp and `image` encoders differ; recorded as an accepted divergence (this
  deviates from the roadmap's blanket "byte-identical" line for the codec only;
  watcher/scan/diff/process outputs must be byte-equal where the TS side is
  deterministic).
- **S2**: SSE parser passes chunk-boundary torture tests; streaming tool-call
  assembly, retry/budget/idle/empty-completion/off-peak classification, strict-schema
  folding, and `video/*` request shape are covered by tests derived from the TS
  sources and `patches/@ai-sdk__*`. **Live-provider golden-request corpus from
  `runner-debug` recordings is unavailable in this checkout** — that gate stays
  OPEN and is reported as not met, not simulated.
- **S3**: `CommandInbox` semantics (serial admission, monotonic `admissionSeq`,
  idempotent resubmit, cancel) and owner/lease rules (acquire/renew/steal, stale-run
  guard) pass concurrency tests with an injected clock — no wall-clock sleeps in tests.
- **S3 `zcode-host` (this increment)**: the Rust host binary speaks the existing v4
  stdio NDJSON RPC surface (method names and envelope shapes mirror
  `V4_METHODS`/`V4_NOTIFICATIONS` in `packages/shared/src/zcode-protocol-v4/transport.ts`),
  integrating `zcode-v4-wire` (frame codec), `zcode-core` (command admission via
  `CommandInbox`/`CommandInboxHost`, identity key), and `zcode-provider` (model
  roundtrip executor) **in-process** — no sidecar hop. Command surface in this
  increment: `v4/connection/flow`, `v4/conversation/subscribe|resync|unsubscribe`,
  `v4/conversation/rowsRange`, `v4/command` (`createSession`, `sendText`, `stop`,
  `renameSession`, `deleteSession`); notifications: `v4/conversation/frame`.
  Both delivery profiles (`continuous`/`replayable`) are implemented from the shared
  profile parameters, and **both profiles' terminal states must be byte-identical**
  (the golden invariant from `profiles.ts`). Cutover seam: which binary the outer
  host spawns per workspace (flag off = TS host, unchanged; flag on = `zcode-host`).
  Acceptance: conformance via golden vectors + e2e stdio tests, plus one real
  workspace session end to end (create → prompt → stop → resync identical →
  restart → resume). Explicitly **OPEN** (not simulated): tool execution, MCP,
  subagents, context building (system prompt/memory/skills), compaction, sqlite
  single-writer storage (JSONL transcript only in this increment), live provider
  corpus, Linux runtime, interactive GUI.
- **S4**: `zcode-shell` compiles (`cargo check`/`cargo build` verified) and maps an
  `IPlatformService`-shaped command surface; interactive window verification and the
  R4 BrowserView design gate remain **open** (product decision, per roadmap §7).
- **S5**: axum app serves static UI assets with SPA fallback + health endpoints,
  behind a `HostPort` trait; wiring to a real host is S3/S5 integration (open).

## Third-party notice rule for cargo

Any change to `native/**/Cargo.toml` or `native/Cargo.lock` must regenerate cargo
notices in the same change: `node scripts/third-party-cargo.mjs --write` (collects
license texts from the cargo registry into `third-party/native-cargo/`), then
`node scripts/licenses.mjs notices` (rebuilds `THIRD-PARTY-NOTICES.md` +
`third-party/inventory.json`). The generator fails if `Cargo.lock` and the recorded
inventory disagree. Scope mirrors `third-party/native-search/sources.json`: the whole
lockfile conservatively, including dev/test/build crates.

## Verification commands

```sh
cargo fmt --manifest-path native/Cargo.toml --all -- --check
cargo clippy --manifest-path native/Cargo.toml --workspace --all-targets -- -D warnings
cargo test --manifest-path native/Cargo.toml --workspace
pnpm golden:check && pnpm typecheck && pnpm lint && pnpm fmt:check
pnpm architecture:check --changed
node scripts/third-party-cargo.mjs --check   # cargo notices fresh
```
