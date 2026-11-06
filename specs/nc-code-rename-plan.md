# nc-code rename plan — what "change zcode to nc-code" actually contains

The directive is to rename the product from `zcode` to `nc-code`. That looks like one search and
replace and is not: some of these names are internal, and some are already written to users' disks,
already read by processes we do not control, or already spoken on the wire. This document splits
the surface into classes, states the decision per class, and says what a change in each class
costs. It exists so that no part of the rename is done silently.

Audited 2026-10-31 from the checked-out source, not from memory.

## Class A — internal identifiers (rename in one sweep, no user impact)

| surface | today | note |
| --- | --- | --- |
| Rust crate names and package ids | 32 crates `zcode-app`, `zcode-cdp`, `zcode-channels`, `zcode-core`, `zcode-diff`, `zcode-host`, `zcode-image`, `zcode-node-repl`, `zcode-proc`, `zcode-provider`, `zcode-rpc`, `zcode-scan`, `zcode-server`, `zcode-service-core`, `zcode-services-*`, `zcode-shell`, `zcode-sidecar-proto`, `zcode-v4-wire`, `zcode-watch`, `zcode-workflow` | `native/crates/*`; become `nc-code-*` |
| Rust import paths | `use zcode_workflow::…`, `zcode_host::…` | mechanically follow the crate rename |
| crate-internal constants | `WORKFLOW_JOURNAL_FILE = "workflow-journal.sqlite"` is **Class B** (on disk), `SNIPPET_RUN_ID_PREFIX = "dwfeval"` is Class C (wire-visible) | check each constant, do not blanket-replace |
| test and bench scratch paths | `zcode-wf-bench-*`, `zcode-nws-bench-*`, `zcode-compile-fixture-*` under the OS temp dir | free to rename; nothing reads them back |
| doc comments and specs prose | `zcode-workflow`, `zcode-host` used as names for the crates | rename together with the crates, in the same commit, or the docs point at things that do not exist |

Verification for Class A: per-crate `cargo build`, `cargo clippy --all-targets -- -D warnings`,
`cargo fmt -p <crate> --check`, `cargo test -p <crate>` for all 32 crates (workspace-wide gates are
unusable here — see the note on external path deps in `native/Cargo.toml`), plus `pnpm typecheck`
and `pnpm lint` because the frontend references the host binary by name.

## Class B — on-disk state (rename = migration, needs a decision)

| surface | where it lives | consequence of renaming |
| --- | --- | --- |
| `<workspace>/.zcode/` | workspace config, `config.json` hook toggles, `workflow-runs/<runId>.mjs` sandbox entries, saved definitions | every existing user's workspace configuration becomes invisible; needs a read-old/write-new shim plus a cleanup step |
| the host `data-dir` tree | session stores (two SQLite databases + transcript JSONL), `workflow-journal.sqlite`, `<data-dir>/security/` workspace-hook trust store | all history, every resumable workflow run and every trust grant; renaming the root without a shim is data loss |
| provider store config files | paths injected by the frontend (`ZCODE_BUILTIN_PROVIDER_CONFIG_FILE`, `ZCODE_PERSONAL_PROVIDER_CONFIG_FILE`) and read by the native handoff (`provider_handoff.rs`) | credentials and model selection become unreadable; the native path fails closed with a named reason, which is correct behaviour but still "no model" for the user |
| binary file name | `zcode-host(.exe)` | the desktop spawns it by name (`zcodeAgentProcessManager.ts`) and packaging/SEA paths, updater manifests and any installed shortcuts refer to it |

Decision rule used here: **no Class-B rename happens in an identifier sweep.** Each one needs (1) a
compatibility read of the old location, (2) an atomic move or a documented one-time migration, (3) a
rollback story, and (4) a test that an existing directory from the previous name still opens. That is
its own piece of work per surface, not a `sed`.

Why this is stated instead of done quietly: silently renaming the data dir is the kind of change that
looks like a cosmetic win in the diff and is a lost-history bug in the product.

## Class C — wire and cross-process names (rename only with both sides)

| surface | value | who else speaks it |
| --- | --- | --- |
| compile kernel handshake protocol | `zcode-workflow-compile/1` | the Node kernel script in this repo; both ends change together, so it is safe as long as an old host never meets a new kernel (it cannot: they ship together) |
| v4 protocol package and method names | `zcode-protocol`, `zcode-protocol-v4`, method strings on the wire | the mobile web client and any relay in the field: renaming a method string breaks every older client silently, because JSON-RPC just answers "method not found" |
| artifact URI scheme | `zcode-artifact://…` | stored in journal rows and delivered to clients; old rows keep the old scheme forever |
| workspace-hook / skill / plugin env vars | `ZCODE_SKILL_DIR`, `ZCODE_MANIFEST_PATH`, `ZCODE_PLUGIN_ROOT`, `ZCODE_PLUGIN_ID_ENV_KEY`, `ZCODE_GIT_BINARY`, `ZCODE_NODE_PATH`, `ZCODE_WORKFLOW_DIST`, `ZCODE_WEB_DIST`, `ZCODE_NATIVE_HOST`, `ZCODE_DYNAMIC_WORKFLOW_MODE` | the desktop main process, plugin/skill authors' shells, and CI launchers. Plugins read these at run time, so a rename must accept both names for a transition window |
| auth and relay env | `ZCODE_JWT_TOKEN_KEY`, `ZCODE_SERVER_AUTH_TOKEN`, `ZCODE_ENDPOINT_ORIGIN`, `ZCODE_ENV` | deployment environments and the relay; these are set by operators, not by us |

Rule: for Class C, accept both spellings for one release, emit the new one, and retire the old only
after the field has had a chance to move. Method strings on the wire stay as they are until the
protocol version bumps, because the protocol contract is the thing clients are pinned to.

## Sequencing

1. **English-only pass first** (in flight as of 2026-10-31). Comments, doc-comments and specs
   become English before any identifier changes, because the rename touches files whose Chinese
   comments quote identifiers by name; doing both at once makes either diff unreadable and makes a
   bad merge invisible.
2. **Class A sweep** in one commit: crate dirs, `Cargo.toml` names, `use` paths, binary name inside
   the Rust workspace, then the frontend reference to the spawned binary in the same commit so no
   tree state ever has a host that looks for a binary nothing builds.
3. **Class C dual-name window** for env vars and the plugin/skill surface; wire method strings wait
   for a protocol version bump.
4. **Class B migrations**, one per surface, each with its own spec section, test and rollback. The
   workspace directory and the data-dir root are the two that matter; the rest follow.

If any step cannot be completed with its gates green, it stays undone and recorded — a half-done
rename is worse than a slow one, because it produces a tree where the docs, the binaries and the
on-disk state name three different products.
