## Core Principles

- Before adding or modifying a behavior, update the corresponding spec first; create the directory as needed if it does not exist. First clarify the product rules, status owners, interfaces and acceptance scenarios, and then implement the code.
- Based on the currently checked out source code, `package.json` and architectural policy. Only the functions, commands and files provided by the current warehouse are retained in the instructions; when deleting functions, the references in instructions and skills are simultaneously cleaned up.
- When locating a problem, investigate the cause first without explicitly asking to modify the code. Combine source code, logs, and runtime evidence to distinguish confirmed causes from hypotheses to be verified.
- Retain local changes that are not related to the task, and do not restore removed modules or internal dependencies on their own.

## Language boundaries: TypeScript only exists on the front end

- **New or expanded** TypeScript in the repository is only allowed to occur in the front-end and development tools; everything outside the front-end
TypeScript is all migration debt (see `specs/typescript-frontend-only.md`).
- Allowed list: `packages/ui`, `packages/web`, `packages/desktop/src/renderer`,
  `packages/desktop/src/preload`、`.agents/skills`、`scripts`、`harness`、
  `packages/desktop/scripts`、`packages/server/build-remote.ts`、
  `**/*.config.ts`、`**/*.d.ts`、`**/*.d.mts`。
- Machine enforcement: `languageBoundary` section of `architecture-policy.yaml` +
`.architecture-ts-baseline.json` Ratchet baseline, executed by `bun run architecture:check`
(Run on every call, `--changed` will also run; not just look at changed files). The baseline only decreases but does not increase.
Extending the baseline requires `bun run architecture:ts-baseline:update --accept-growth --reason … --approved-by …`.
- When you need to write TS in non-front-end (such as tests compared with frozen fixtures), do not bypass the gate: in
`languageBoundary.escapeHatches` adds a strip `reason` / `approvedBy` / `expires`
The entry makes it reviewable and expirable.
- **Mandatory scope**: `bun run architecture:check` and `bun run verify:pre-push`. This warehouse is currently
**There is no CI to run this step** (there is no architecture access control in `.github/`), so you must run it yourself before submitting.
- This boundary **does not delete any code**, nor does it judge whether the Rust transplant is completed; whether it can be deleted
The four conditions of `old.back/README.md` are determined by `RUST-PORT-MATRIX.md`.

## Command and warehouse structure

Run `node scripts/check-workspace-freshness.mjs` to check the baseline before starting work. The Node version is subject to `mise.toml`.

The following command is executed from the repository root directory:

| Purpose | Command |
| ----------------- | --------------------------------------------------------------- |
| Type check | `bun run typecheck` |
| Lint              | `bun run lint` / `bun run lint:fix`                                   |
| Format check | `bun run fmt:check` |
| Desktop development | `bun run dev:desktop` |
| Web Development | `bun run dev:web` |
| Pre-commit check | `bun run verify:pre-push` (Lint and schema check) |
| Architecture check | `bun run architecture:check --changed` |
| TS language boundary baseline | `bun run architecture:ts-baseline:update` (default only decreases but not increases, see above) |
| Architecture access control single test | `bun run architecture:test` (included in `verify:pre-push`) |
| Module reading package | `bun run architecture:context <module-id>` |
| Unused dependencies and exports | `bun run knip` |
| Export reference query | `bun run dep:refs --list-exports <file>` |
| Golden vector generation/checking | `bun run golden:vectors` / `bun run golden:check` |
| Rust workspace testing | `bun run native:test` (`native/`, see specs/rust-native-crates.md) |
| Cargo third-party license | `node scripts/third-party-cargo.mjs [--write]` |

The test entry is based on the current `package.json` of the target package and the actual test file, and does not assume the existence of a unified single test or E2E command.

- `packages/desktop`：Electron main、host、renderer。
- `packages/web`, `packages/server`: Web client and server.
- `packages/ui`: shared React components, hooks and Zustand store.
- `packages/services`: business services; `packages/rpc`: RPC framework.
- `packages/shared`: shared protocol and type; `packages/client`: Agent client SDK.
- `apps/zcode-cli`: Agent CLI and runtime.
- `CONTEXT.md`: plugin store domain vocabulary; read before modifying related UI.
- `DESIGN.md`: UI design specifications; read before modifying the UI.

## Implementation and verification

- Code changes use `.agents/skills/architecture-governance/SKILL.md`, first run the architecture check, and then read the controlled context of the target module.
- Avoid duplicate states and multiple write paths. Clarify the unique owner, interface, dependency direction, event order and idempotent boundaries, and do not use timeouts to cover up synchronization problems.
- When there are behavioral changes, add corresponding tests first; interactive changes require E2E scenarios. Check that the tests are consistent with the implementation and actually perform the available verification. Explain truthfully when it is not implemented or the environment is restricted.
- When fixing bugs, use Chinese comments to explain the reasons and basis for repair. When discovering design flaws, align them with users first, and don’t continue to add covert branches.
- Solutions involving status, timing, remote or asynchronous synchronization, using diagrams to show owners and sequence of events.
- `bun run typecheck` and `bun run lint` must be executed to report real results and do not write existing failures as passes.
- Use asynchronous file and network IO; cross-package imports use public entries and respect existing path aliases.
- Prohibit the UI from directly calling Repo and Service to reference the specific implementation of Runtime, cross-domain import implementation details and circular dependencies.

## UI and platform boundaries

- Comply with `DESIGN.md`, reuse existing components, and take into account the layout, interaction, theme and internationalization of desktop and mobile web.
- Components access services through `packages/ui/src/hooks/`; platform operations pass `IPlatformService` (`packages/shared/src/platform.ts`) and do not call `window.zcode` directly.
- Handle differences in desktop, web, local and remote environments via dependency injection, taking into account Windows, macOS and Linux.
- Zustand status is located in `packages/ui/src/store/`. The subject, language and other fields of broadcast synchronization need to prevent loopbacks; the local state of the UI should not be mistaken for server-side facts.
- Files containing JSX in hooks use `.tsx`.

## Processes, protocols and remote control

- Desktop app communicates with Agent via stdio. Protocol changes are updated simultaneously in `packages/shared/src/zcode-protocol/index.ts`, providing strict type and runtime verification.
- Main is responsible for windows, native operations, process scheduling and message forwarding, and does not carry task/session business status.
- Use one window-scoped Local Host per window; the local workspace shares the Host. The remote workspace is managed by the connection registry in the window, and no additional Desktop Remote Host is created.
- When the mobile phone remote control is connected to the desktop, there is already a Host attachment, and the reused session is running; no new Agent, Local Host or remote session is created for the mobile phone.
- Desktop's `desktop-continuous` live link and mobile's `web-remote-replayable` recovery link must be clearly distinguished. When modifying stream, snapshot, queue or reconnecting, both semantics are verified at the same time.
- The external relay and Main only perform authentication, pairing, heartbeat, forwarding and attachment scheduling, and do not save business status such as task queues and snapshots.
- Accepted busy/running input is serially admitted by CLI/runtime `CommandInbox`; Renderer only retains uncommitted drafts and pending optimistic overlay, and Host owner/lease is responsible for routing.
- Retain owner/lease, cross-host routing and stale run protection, and cannot delete boundary judgment based on only a single path.

## Workspace Identity

- `workspaceIdentity` is used for identity isolation, `workspacePath` is used for file operations, commands cwd, Git and path display.
- The identity key is unified into `workspaceIdentity?.trim() || workspacePath`, which is suitable for deduplication, binding, caching, queuing, persistence and request correlation.
- The remote link passes `workspaceIdentity` and `remoteSessionId` through and must not be matched by path alone.
- The new interface retains the local path fallback; the remote identity reuses the existing construction and parsing tools and does not require handwritten formats in the business code.

## log

- UI uses `packages/ui/src/logger.ts` and does not use `console.log` or `window.zcode?.log` directly.
- Agent/session/runtime related service logs use `createServiceLogger(scope)` (`packages/services/src/logger/serviceLogger.ts`).
- `debug` is used for high-frequency diagnosis of protocol raw data, streaming chunks, and item-by-item tool updates, without leaving the production environment on disk.
- `info` is used for production-usable events such as process and session lifecycle, permission results, one-time initialization, etc.
- `warn` is used for recoverable exceptions; `error` is used for unrecoverable errors such as crash, handshake failure, authentication loss, etc.
- Do not write credentials, real user data, and internal service addresses in logs, samples, or commits.
