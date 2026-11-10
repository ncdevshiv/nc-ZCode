# Resource budget — native build artifacts

Owner: architecture (native workspace)
Baseline measured: 2026-11-08
Applies to: `native/target/` and the cargo caches that feed it

This document is the storage-budget record required by the repository's Rust
engineering rules (§20 *Project growth and 200+ GB resource management*). It is
the authority for `scripts/native-cache.mjs` thresholds.

## 1. Baseline (2026-11-08, before remediation)

| Category | Path | Measured |
| --- | --- | --- |
| Total working tree | `.` | **194 GB** |
| Native build cache | `native/target/` | **188 GB (97%)** |
| — compiled artifacts | `native/target/debug/deps` | 135 GB |
| — — of which `.pdb` | `native/target/debug/deps/*.pdb` | **63.7 GB across 797 files** |
| — — of which binaries/rlibs | `deps/*` excluding `.pdb` | 70.7 GB |
| — incremental sessions | `native/target/debug/incremental` | **51 GB** |
| — build-script output | `native/target/debug/build` | 1.7 GB (457 hash-suffixed dirs) |
| — stray recursive copy | `native/target/debug/_up_/_up_/packages/web` | 497 MB |
| JS dependencies | `node_modules/` | 2.5 GB |
| CLI package | `apps/zcode-cli/` | 1.8 GB |
| External tooling (gitignored) | `.nc-tools/` | 905 MB |
| Git objects | `.git/` | 200 MB on disk / 40 MB packed |
| Everything else (source, docs, scratch) | — | ~3.0 GB |

**Source-only size (excluding `target/`, `node_modules/`, `.git/`): 3.0 GB.**

## 2. Root cause

None of the following were configured anywhere in the repository:

- No `[profile]` table in `native/Cargo.toml`. Cargo's `dev` defaults are
  `debug = 2` (full type info) and unbounded incremental compilation. On
  Windows-MSVC each test binary therefore received a ~470–715 MB `.pdb`, and
  every `cargo test` of the 56-crate workspace produced one per test target.
- No `.cargo/config.toml`. No incremental policy, no pipelining, no linker
  choice, no fetch policy.
- No retention/cleanup tooling. Cargo does not garbage-collect `target/`, so
  artifacts from every toolchain hash and crate-graph change accumulated
  permanently (the `rust-toolchain.toml` comment records three measured
  workspace rustc hashes).
- `native:test` ran `cargo test --manifest-path native/Cargo.toml` with no
  package filter, building every test target in the workspace on each run.

## 3. Budgets and thresholds

| Category | Budget | Threshold | Action on breach |
| --- | --- | --- | --- |
| `native/target/` | 20 GB | 80% (16 GB) | run `native-cache.mjs gc` |
| `native/target/debug/deps` | 12 GB | 80% | run `native-cache.mjs gc` |
| `native/target/debug/incremental` | 4 GB | 80% | run `native-cache.mjs sweep` |
| `.pdb` file count | ≤ 400 | — | prune orphans |
| Incremental sessions per crate | ≤ 2 | — | `sweep --keep-per-crate 2` |

`bun run native:cache:budget` exits non-zero when a budget is exceeded, so it can
gate a pre-push or scheduled check.

## 4. Retention policy

| Artifact | Retention | Rationale |
| --- | --- | --- |
| `target/debug/` compiled artifacts | until `cargo clean` or source/toolchain change | needed for incremental dev loop |
| `target/debug/incremental/*` | newest 2 sessions per crate **and** nothing older than 7 days | a session is only reusable for the exact crate hash; old sessions are dead |
| `.pdb` files | only while their `.exe`/`.rlib` sibling exists | an orphan PDB cannot be loaded by any debugger |
| `target/release/` | until `cargo clean` | shipped builds are reproducible from `Cargo.lock` + pinned toolchain |
| `target/debug/build/` | cargo-managed | build-script output is recreated on demand |

## 5. Cleanup strategy

`scripts/native-cache.mjs` is the single owner of build-cache lifecycle:

```sh
bun run native:cache:status   # breakdown + counts
bun run native:cache:budget   # thresholds, non-zero exit when over budget
bun run native:cache:gc       # sweep incremental + prune orphan PDBs
bun run native:cache:clean    # full `cargo clean`
```

All commands are safe to re-run. `gc` and `sweep` accept `--dry-run`. Every
removal path is asserted to be inside the repository before `rmSync`.

**Never** delete `target/` while a cargo process holds a lock on it; cargo will
rebuild what it needs. `cargo clean` is the only sanctioned full-clean path.

## 6. Monitoring

- `bun run native:cache:budget` — ad-hoc or scheduled (Windows Task Scheduler /
  pre-push). Non-zero exit on breach.
- Growth is expected when the toolchain or crate graph changes; the budgets are
  ceilings, not invariants.
- `.nc-tools/`, `node_modules/`, and `.git/` are outside this budget and are
  governed by their own tooling (`bun install`, git gc).

## 7. Compile-time levers

| Lever | Where | Effect |
| --- | --- | --- |
| `debug = 1` + `split-debuginfo = "packed"` | `native/Cargo.toml` `[profile.dev]` | removes full type info from dev PDBs |
| `[profile.dev.package."*"] debug = false` | `native/Cargo.toml` | dependencies carry no debug info |
| `pipelining = true` | `.cargo/config.toml` | start dependents before dependency codegen ends |
| `CARGO_INCREMENTAL=0` | CI / release | deterministic, cache-free builds |
| `cargo check --workspace --all-targets` | `bun run native:check` | no codegen, no PDBs — the fast verification path |
| `cargo test -p <crate>` | `bun run native:test:pkg -- <crate>` | build only the touched crate's tests |
| `strip = "symbols"`, `lto = "thin"` | `[profile.release]` | smaller shipped binaries |

## 8. Known violations (2026-11-08)

- `native/crates/nc-code-host/Cargo.toml:60-61` depends on
  `G:/nc-sdk/nc-tools/crates/{nct-mcp,nct-core}` by **absolute Windows path**.
  This breaks on any other machine or OS and pulls an out-of-repository crate
  into this workspace's `target/`. Tracked as a violation; remediation options are
  in `docs/engineering/change-removal-log.md`.
- `native/crates/**` contains 80 `unsafe {` blocks and **zero** `SAFETY:`
  comments, contradicting `specs/rust-style-audit.md` (which claims 27/27
  covered). See the change log.

## 9. Adjacent storage outside the repository

Not governed by this budget, but relevant to the same problem:

| Path | Size | Note |
| --- | --- | --- |
| `~/.cargo` | 2.4 GB | shared registry cache; `~/.cargo/registry` 2.3 GB |
| `G:/nc-sdk/nc-tools/target` | 18 GB | external path dependency's own build cache |
| `G:/nc-sdk/nc-tools/target-bench` | 4.8 GB | second cache directory |
| `G:/nc-sdk/nc-tools/target2` | 4.3 GB | third cache directory |
| `G:/nc-sdk/nc-tools/crates` (actual source) | 3.4 MB | — |

The external SDK is **99.9% build cache**. It is outside this repository and may be
shared with other projects, so it is reported here rather than cleaned. The
absolute-path dependency is what couples this workspace to that storage: `nct-mcp`
and `nct-core` are compiled into *this* workspace's `target/` using *this*
workspace's profiles, while the SDK's own 27 GB sits beside it unmanaged.

## 10. Remediation outcome (2026-11-08)

| Metric | Before | After |
| --- | --- | --- |
| Working tree | 194 GB | 19 GB |
| `native/target/` | 188 GB | 12.84 GB (full workspace check + test build + clippy) |
| `.pdb` total | 63.7 GB / 797 files | 3.76 GB / 370 files |
| `.pdb` average | ~80 MB | ~10 MB |
| `debug/incremental` | 51 GB, unbounded | 2.62 GB, retention-bounded |
| `cargo check --workspace --all-targets` from clean | n/a | 4m 03s |
| Incremental sessions | unbounded | newest 2 per crate, max age 7 days |

The remaining 12.84 GB is a *live* workspace cache (all 56 crates checked, tests
built, clippy run). Re-running `bun run native:cache:gc` keeps it inside the
budgets in §3; `bun run native:cache:clean` returns the tree to ~6 GB at the cost
of a full rebuild.

