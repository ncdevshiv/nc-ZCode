# Change and removal log

Required by §8 of the repository's Rust engineering rules. Every removal records
the two-condition justification, the evidence, and the verification. Entries are
appended, never rewritten.

Condition 1 = retention causes greater harm (evidence mandatory).
Condition 2 = a demonstrably better alternative exists, implemented and verified
before the old item is removed.

---

## R-2026-11-08-01 — `native/target/` build cache (188 GB)

- **Date:** 2026-11-08
- **Affected:** `native/target/` (gitignored via `native/.gitignore:1`)
- **Removed item:** the entire cargo build cache — 135 GB `debug/deps` (797
  `.pdb` = 63.7 GB, 70.7 GB binaries/rlibs), 51 GB `debug/incremental`, 1.7 GB
  `debug/build`, 497 MB `debug/_up_/_up_/packages/web`
- **Original purpose:** reproducible compiler output cache for the native
  workspace
- **Reason for removal:** 97% of the 194 GB working tree was build output. The
  artifacts were produced under unoptimized defaults (`debug = 2`, unbounded
  incremental) and included sessions from at least three superseded workspace
  rustc hashes plus a recursively mis-copied `packages/web` tree.
- **Condition:** [x] Condition 1 (uncontrolled resource consumption — 188 GB
  against a 3.0 GB source tree) **and** [x] Condition 2 (a better alternative now
  exists: tuned `[profile.*]` in `native/Cargo.toml`, `.cargo/config.toml`, and
  `scripts/native-cache.mjs` retention tooling)
- **Evidence:** `du -sh` breakdown in `docs/engineering/resource-budget.md` §1;
  `git check-ignore native/target` → `native/.gitignore:1:/target/`;
  `git ls-files native/target | wc -l` → `0`
- **Reference/dependency trace:** `scripts/rust-backend.mjs:18` resolves
  `native/target/<profile>/zcode-server.exe`; `scripts/prepare-native-host.mjs:56`
  resolves `native/target/<profile>/zcode-host.exe`. Both tolerate a missing
  binary (they rebuild), so removing the cache breaks no caller.
- **Potential consequences:** the next `cargo build`/`cargo test` is a full
  rebuild. Mitigated by the new profiles (no full-rebuild type info, dependencies
  without debug info) and by the pinned `rust-toolchain.toml`.
- **Replacement:** `native/Cargo.toml` profiles + `.cargo/config.toml` +
  `scripts/native-cache.mjs` (this repository)
- **Integration status:** complete
- **Tests executed:** `cargo metadata` clean (no config warnings);
  `cargo check --workspace --all-targets` from the cleaned state → exit 0 in
  4m 03s; `cargo test --workspace --no-run` → exit 0 (all test targets build);
  `cargo clippy --workspace --all-targets -- -D warnings` → **fails** on
  pre-existing `nc-code-workflow` dead code (see P-2026-11-08-05);
  `oxlint scripts/native-cache.mjs` → 0 warnings / 0 errors
- **Advantages:** 188 GB reclaimed; future growth bounded by the retention
  policy; debug info sized for its actual use (backtraces)
- **Disadvantages:** first build after the clean is slower
- **Compatibility impact:** none on source or on the produced binaries' behaviour
- **Migration requirements:** none
- **Why retention was rejected:** the cache was not bounded, not garbage
  collected, and was the dominant cost of the working tree
- **Final decision:** removed

---

## R-2026-11-08-02 — `.bun-cache/` download cache

- **Date:** 2026-11-08
- **Affected:** `.bun-cache/` (untracked, 153 MB)
- **Removed item:** cached platform-specific bun binaries
  (`bun-darwin-aarch64-v1.4.3`, `bun-linux-x64-v1.4.3`)
- **Original purpose:** bun's per-platform binary download cache
- **Reason for removal:** regenerable on demand; not part of the repository
- **Condition:** [x] Condition 2 (bun re-downloads when needed; `.gitignore` now
  excludes the directory)
- **Evidence:** `git status --porcelain` shows both entries as `??`;
  `git check-ignore .bun-cache` → not ignored (gap closed in `.gitignore`)
- **Reference/dependency trace:** bun runtime; no script references
  `.bun-cache/` by path
- **Potential consequences:** first bun invocation after cleanup re-downloads the
  platform binary
- **Replacement:** none needed
- **Integration status:** complete
- **Tests executed:** `bun --version` after removal
- **Advantages:** 153 MB reclaimed; `.gitignore` now matches reality
- **Disadvantages:** one re-download
- **Compatibility impact:** none
- **Migration requirements:** none
- **Why retention was rejected:** pure download cache with a regeneration path
- **Final decision:** removed

---

## Proposed removals — pending approval (tracked files)

These items are **tracked in git**, so removing them changes repository content
rather than local state. They are recorded here as violations and proposed, not
executed.

### P-2026-11-08-01 — one-off debug scripts and logs at the repository root

- **Items:** 74 tracked scratch files matching `*.py`, `*.js`, `*.mjs`, `*.cjs`,
  `*.txt`, `*.ps1` at the root (e.g. `add_more.py`, `check_golden.py`,
  `parity.txt`, `native_test_output.txt`, `remaining_chinese.txt`)
- **Original purpose:** one-off migration/translation/debug helpers
- **Proposed reason:** none are imported by any package or script; they are
  executable clutter in the repository root
- **Condition:** [ ] Condition 2 (the work they performed is captured in
  committed scripts under `scripts/` and `specs/`)
- **Evidence:** `git ls-files` confirms tracked status; no `require`/`import`
  reference outside each file (to be confirmed before removal)
- **Migration requirements:** none if unreferenced
- **Status:** **not removed** — awaiting approval

### P-2026-11-08-02 — build/test log dumps inside `native/`

- **Items:** `native/build_out.txt`, `check2..6.txt`, `check_out.txt`,
  `clippy.txt`, `fmt.txt`, `fmt2..5.txt`, `lin.txt`, `test_nc.txt`,
  `test_out.txt`, `wp.txt`, `edit.txt`, `build_nc.txt`, `native/*.txt`
- **Original purpose:** captured compiler/test output from debugging sessions
- **Proposed reason:** reproducible by re-running the tool; they are not source
- **Condition:** [ ] Condition 2
- **Status:** **not removed** — awaiting approval

### P-2026-11-08-03 — `native/crates/nc-code-host/src/session_assistant_row.txt`

- **Item:** a `.txt` file inside a crate's `src/` directory
- **Original purpose:** unknown; not a Rust module, not referenced by `mod`
- **Proposed reason:** non-source file in the source tree breaks the crate's
  module inventory and any per-extension tooling
- **Condition:** [ ] Condition 1 (defective source-tree layout) — evidence of the
  original purpose still required
- **Status:** **not removed** — awaiting approval and purpose investigation

### P-2026-11-08-04 — untracked scratch under `.dev-scratch/` and `.ncag/`

- **Items:** 135 untracked entries under `.dev-scratch/` (host logs,
  `ncag-*.sqlite*`, demo repos) and 3 under `.ncag/`
- **Original purpose:** local debugging scratch
- **Proposed reason:** regenerable; `.dev-scratch/` also contains 26 **tracked**
  files, so a wholesale delete would remove repository content
- **Condition:** [ ] Condition 2
- **Status:** **not removed** — `.gitignore` updated so untracked scratch is
  excluded from enumeration; the 26 tracked files stay

---

## Violations found by the build-cache remediation (not removed)

These were surfaced by running `cargo check` / `cargo clippy` against the cleaned
workspace. They are **pre-existing**, not introduced by the remediation, and are
recorded here because the rules require warnings to be investigated rather than
hidden.

### P-2026-11-08-05 — dead code in `nc-code-workflow` and `nc-code-node-repl`

- **Items:**
  - `nc-code-workflow/src/compile/parser.rs:5` — `parse_workflow_script` never
    used in non-test builds (it *is* used by the `mod tests` directly below it)
  - `nc-code-workflow/src/compile/ts_workflow.rs:48` — field `kind` never read
  - `nc-code-workflow/src/compile/ts_workflow.rs:68` — field `name` never read
  - `nc-code-workflow/src/compile/ts_workflow.rs:81` — field `artifact_id` never read
  - `nc-code-workflow/src/compile/ts_workflow.rs:59` — variant `SiteExtra::None`
    never constructed
  - `nc-code-workflow/src/compile/kernel.rs:662,670` — `node_available` /
    `dist_entry_if_present` never used, **inside `#[cfg(test)] mod tests`**
  - `nc-code-node-repl` — field `config` never read
  - `nc-code-workflow/src/compile/ts_workflow.rs:1120` — `clippy::unnecessary_sort_by`
- **Evidence:** `cargo clippy --workspace --all-targets -- -D warnings` exits
  non-zero with 6 errors in `nc-code-workflow`; `cargo check` reports the same
  items as warnings. `specs/rust-style-audit.md:24` claims clippy `-D warnings`
  passes — **that claim is stale**.
- **Original purpose:** `parse_workflow_script` is a test-only helper (the fix is
  `#[cfg(test)]`, not deletion). The `SiteExtra` fields/variants and the kernel
  helpers are port-in-progress scaffolding whose intended use is not recorded.
- **Proposed reason:** dead code hides real regressions and blocks the `-D
  warnings` gate
- **Condition:** [ ] Condition 2 — but the correct action differs per item:
  `#[cfg(test)]` for the parser helper, deletion for the two test helpers, and a
  product decision for the `SiteExtra` model fields
- **Status:** **not removed** — deleting port scaffolding without knowing whether
  it is pending wiring would violate §7 (removal requires understanding the
  original purpose and tracing references). Recorded for a product decision.
- **Recommended next step:** decide per item — gate the parser helper with
  `#[cfg(test)]`, delete the two genuinely unused test helpers, and either read
  or remove the `SiteExtra` fields in the same change that wires them.

---

## R-2026-11-09-01 â€” repo-root Python patch scripts (`add_*.py`, `update_*.py`)

- **Date:** 2026-11-09
- **Affected (removed):** `add_more.py`, `add_more2.py`, `add_system.py`,
  `add_convshare.py`, `update_workspace.py`, `update_config.py` (repo root)
- **Removed item:** six one-shot Python scripts that rewrote Rust sources by
  string replacement.
- **Original purpose:** they injected channel registrations into
  `native/crates/nc-code-app/src/lib.rs` and
  `native/crates/nc-code-services-workspace/src/lib.rs` during the port
  (`commands`, `hooks`, `memory`, `subagents`, `system`, `conversation-share`,
  `media-preview`). Example: `add_more.py:12-14` inserts
  ``registry.register("commands", CommandsService::new())``.
- **Reference/dependency trace:** no build, CI, package.json script, or Rust
  crate references them (`grep` over `package.json`, `scripts/`, `native/tools/`
  returns nothing). They are not imported by any module.
- **Condition:** [x] Condition 1 â€” retention causes greater harm. Re-running any
  of them re-injects the standalone `commands`/`hooks`/`memory`/`subagents`/
  `system`/`media-preview` registrations that are now **owned** by
  `nc-code-services-extensions` and `nc-code-services-workspace`. That produces a
  duplicate-channel registration and breaks composition at startup
  (`nc-code-app/src/lib.rs:174-193` records the earlier breakage).
  [x] Condition 2 â€” the better alternative already exists: channels are declared
  once in the composition root and in the owners' `register()` methods.
- **Potential consequences:** none at build time (dead files). Losing them costs
  nothing: the wiring they produced is committed in
  `nc-code-app/src/lib.rs` and the owning crates.
- **Replacement:** direct edits in the composition root, guarded by
  `nc-code-app/tests/basic.rs` (channel-family assertion).
- **Verification:** `cargo test -p nc-code-app` â†’ 4 + 1 + 4 tests pass after
  removal.

## R-2026-11-09-02 â€” empty `native/crates/zcode-host/` directory

- **Date:** 2026-11-09
- **Affected (removed):** `native/crates/zcode-host/` (empty directory, 0 files)
- **Original purpose:** pre-rename crate directory for the S3 host; the crate was
  renamed to `nc-code-host` (package name, `native/crates/nc-code-host/Cargo.toml:2`).
- **Reference/dependency trace:** not a workspace member â€” `native/Cargo.toml:7`
  globs `crates/nc-code-*`, so Cargo never sees it. No source file references
  the path. Docs (`specs/disabled-features.md`, `RUST-PORT-MATRIX.md`) mention the
  *crate* name `zcode-host`, which is now the retained **binary** name
  (`nc-code-host/Cargo.toml:15`), not this directory.
- **Condition:** [x] Condition 1 â€” an empty directory that looks like a crate is a
  false signal for audits (two independent audits flagged it).
- **Potential consequences:** none; the binary `zcode-host` keeps working.
- **Verification:** `cargo test -p nc-code-host --lib` â†’ 612 tests pass, 0 fail.


## R-2026-11-09-03 â€” `apps/zcode-cli/packages/swift-bridge` (placeholder package)

- **Date:** 2026-11-09
- **Affected (moved):** `apps/zcode-cli/packages/swift-bridge/` â†’ `old.back/apps/zcode-cli/packages/swift-bridge/`
- **Removed item:** the whole package: `src/index.ts` (20 lines: `isSwiftAvailable` â†’ always
  `false`, `detectSwiftVersion` â†’ always `null`, two `TODO` markers), `package.json`
  (`"description": "Swift interop layer (placeholder)"`), `tsconfig.json`.
- **Original purpose:** a placeholder to be filled "when Swift integration is needed"
  (its own doc comment). Every branch returned a constant; nothing was ever implemented.
- **Reference/dependency trace:** repo-wide search for `swift-bridge`,
  `@zcode/swift-bridge`, `isSwiftAvailable`, `detectSwiftVersion` outside
  `node_modules`/`dist` finds **only the package's own files** plus documentation
  (`RUST-PORT-MATRIX.md` Â§2.4 already lists it as a zero-importer stub), the architecture
  baseline and the third-party inventory. No TS/JS module, build script, or workspace
  `package.json` imports it.
- **Condition:** [x] Condition 1 â€” retention causes greater harm: the repo's own P-1 gate
  (`specs/disabled-features.md` Â§5) rejects `TODO`/placeholder modules, so this package made
  every "no stubs" claim false and failed audits; [x] Condition 2 â€” the better alternative
  already exists: the macOS-native capabilities it gestured at live in Rust in
  `nc-code-shell` (keychain/Chrome credential import, print-to-pdf, window control), so no
  Rust counterpart is needed for a module nothing consumes.
- **Potential consequences:** none. The four `old.back/` conditions hold vacuously for
  (1)â€“(3) (no product surface, no runtime, no traffic) and are proven for (4) (zero
  importers).
- **Replacement:** none required (see above). The original source is preserved under
  `old.back/` at its original relative path, so a future decision can restore it with one
  `git mv`.
- **Verification:** `bun run architecture:ts-baseline:update` â†’ debt 510,956 â†’ 509,090 lines
  (Î”âˆ’1,866); `bun scripts/architecture/architecture-check.mjs check` â†’ `architecture: OK`,
  `violations: 0`, `ts-boundary OK (2293 files / 509,090 lines)`.

