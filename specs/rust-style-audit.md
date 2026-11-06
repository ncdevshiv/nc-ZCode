# Rust style audit against the Google Rust Style Guide

Status: **audit complete for the rules below; deviations recorded, not silently accepted**
Date: 2026-10-31
Guide: <https://google.github.io/styleguide/rustguide.html>
Scope: `native/crates/**` (30 crates, ~122k LOC) plus the new provider-handoff module.

This is a style audit, not a completion claim for the port. Functional status lives in
`specs/rust-backend-replacement.md` and `specs/disabled-features.md`.

## Method

1. Read the guide's enforceable rules and grouped them: formatting, imports, naming,
   documentation, errors, ownership, modules, tests, unsafe, clippy.
2. Ran workspace-wide automated checks (below) and a keyword scan for `unsafe`,
   glob imports, and `SAFETY:` coverage.
3. Fixed violations in code owned by this project. External path crates
   (`G:/nc-sdk/nc-tools`) are out of scope and were not modified.

## Automated evidence

| Check | Command | Result |
| --- | --- | --- |
| Clippy, all targets, warnings denied | `cargo clippy --workspace --all-targets -- -D warnings` | **pass** (all 30 crates) |
| Formatting, owned workspace | `cargo fmt --all -- --check` | **pass** for `native/crates/**`; the only diffs are external `G:/nc-sdk/nc-tools` files |
| Unsafe sites carry `SAFETY:` | keyword scan of `crates/**/*.rs` | **27/27 sites covered** (all in `zcode-proc` Windows FFI) |
| Production glob imports | `use …::*;` scan | **0**; the 244 globs are test-module preludes (`use super::*;` 233, plus `use common::*;`/`use crate::types::*;` inside `#[cfg(test)]`) |
| Provider handoff | `cargo test -p zcode-host --test provider_handoff_e2e` | **1/1** (real host process, real model round) |
| Provider handoff units | `cargo test -p zcode-host --lib -- provider_handoff` | **5/5** |

## Rule-by-rule status

| Guide rule | Status | Evidence / action |
| --- | --- | --- |
| Formatting via `git cl format` | **Pass** for owned crates | `cargo fmt` is the repo's canonical formatter; `tool_bench.rs` was the one owned file out of format and is fixed. External nc-tools diffs are intentionally untouched. |
| No glob imports | **Deviation, accepted** | Production code has none. Test modules use `use super::*;` and `use common::*;` as test preludes; this is deliberate test ergonomics, not production API leakage. |
| Types unqualified, functions qualified | **Partial** | Functions are qualified at call sites (`rows::tool_output`, `crate::shell::shell()`); types are imported. Enforcing the full rule would churn every module without a behavioral benefit; recorded, not scheduled. |
| Descriptive lifetimes, short generics, avoid abbreviations | **Pass** for reviewed/new code | `provider_handoff.rs` uses descriptive names (`ProviderHandoff`, `ModelEntry`, `selection_candidates`); no one-letter generics beyond closures/indexes. |
| Public APIs documented; panics/invariants documented | **Partial** | New modules and public items are documented. The workspace does not enable `missing_docs`; older modules have sparse docs. Enabling it now would be a large documentation-only change. |
| Errors: `Option`/`Result`/`panic!` appropriately | **Pass** | New code returns `Result`/named outcomes and never panics on external input; tests use `expect` with messages. |
| Ownership: avoid `Rc`/`Arc`, prefer borrows | **Deviation, justified** | 659 `Arc` uses are structural: sessions/hosts/registries are shared across async tasks and threads by design. New code takes borrows and clones only owned `String`/`Value` fields. |
| Small crates, flat layout, leaf modules named after items | **Partial** | All 30 crates are flat under `crates/`; `zcode-host` is the large integration crate (composition target), and its modules are named after their subjects. No split planned without a functional reason. |
| Tests use the codebase's framework, not raw `#[test]` | **Deviation, accepted** | The workspace uses cargo's built-in `#[test]`/`tokio::test` and integration-test binaries. This is the project's framework; introducing a second harness would be churn. |
| Unsafe: minimize, encapsulate, `SAFETY:` comments | **Fixed** | All 27 Windows FFI sites in `zcode-proc` now carry `SAFETY:` comments; the over-broad `unsafe` around `Command::pre_exec` was narrowed to the three libc calls. No unsafe exists outside `zcode-proc`. |
| Clippy: follow lints (borrowed_box, should_implement_trait, …) | **Pass** | Workspace clippy with `-D warnings` is clean. |

## Fixes landed by this audit

1. `crates/zcode-services-agent/src/bin/tool_bench.rs`: rustfmt (owned file; bench only).
2. `crates/zcode-host/src/provider_handoff.rs`: `doc_lazy_continuation` clippy fix while adding
   the new module.
3. `crates/zcode-proc/src/job.rs` / `process.rs`: 27 `SAFETY:` comments added; the
   `pre_exec` unsafe scope narrowed to the libc calls it actually protects.

## Accepted deviations (reasons, not defects)

- Test-only glob imports and the built-in test framework: cargo-standard for this workspace.
- `Arc`-based sharing: required by the host/session ownership model; reviewed call sites keep
  clones out of hot loops where practical.
- Mixed qualified-type/function imports: the guide's rule is not enforced by rustfmt/clippy and
  a mass rewrite would obscure review history.
- `missing_docs` not enabled: public API documentation is added with each ported module; a
  workspace-wide documentation pass is a separate, documentation-only change.
