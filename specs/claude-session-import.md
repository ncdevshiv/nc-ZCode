# Spec: Claude Code native session import

Status: implemented (Rust). Owner: `nc-code-services-agent::claude_import` (source policy) +
`nc-code-host::session_import` (transcript facts).

## 1. Product rule

A user who has been working in Claude Code can migrate that history into ZCode and keep
chatting in the same conversation. The imported task is a **real ZCode session**: the model
receives its history on the next turn, the UI renders its rows, and edit/retry/fork work on it.
It is not a read-only archive and not a second transcript format.

Migration is explicit and local. Nothing is uploaded, no network call is made, and the source
files under `~/.claude/projects` are never modified.

## 2. Ownership

| Concern | Single owner |
| --- | --- |
| Discovering source files, deciding what text is user-visible | `nc-code-services-agent/src/claude_import.rs` |
| Row shapes, model history, `turn_history_starts`, durability | `nc-code-host` (`session/import` → `open_session_with_id`) |
| Task membership (pin/archive/delete/order) and `migrationSource` | `TaskIndexStore` |
| Runtime model/provider selection after import | the workspace's current config, **never** the source |

The task channel must not write transcript rows itself, and the host must not know Claude's
JSONL format. The channel parses; the host persists.

## 3. Interfaces

### 3.1 `zcode-task.scanImportableClaudeSessions`

Params `{workspacePath?, workspaceIdentity?, modifiedSince?, limit?}` → array of
`{provider:"claude", sessionId, workspacePath, sourcePath, updatedAt, createdAt?, previewTitle?}`,
sorted by `updatedAt` descending, truncated to `limit` when `limit > 0`.

`workspaceIdentity` does not affect discovery (the source has no ZCode identity); it is only
used at import time to place the task row.

### 3.2 `zcode-task.importClaudeSessions`

Params `{workspacePath?, workspaceIdentity?, sessionIds: string[]}` →
`{imported: [{provider, sessionId, taskId, workspacePath}],
  skipped: [{provider, sessionId, reason, workspacePath?}],
  failed: [{provider, sessionId, reason, workspacePath?}]}`.

Every requested id lands in exactly one bucket. Ids are trimmed and de-duplicated, order
preserved. One bad session never aborts the batch.

Skip reasons (closed set):

- `session_not_found_or_workspace_mismatch` — no source file, sidechain transcript, temporary
  worktree workspace, or `cwd` does not match the requested `workspacePath`.
- `workspace_path_missing` — the source's workspace directory no longer exists on disk.

`workspacePath` is included on a skip when it is known (the filter for the first reason, the
source `cwd` for the second).

### 3.3 Host `session/import`

Params `{sessionId?, title?, createdAt?, updatedAt?, messages:[{role, content, timestamp?}]}`
→ `{sessionId, title, createdAt}`.

`role` is `user` or `assistant`; anything else is `invalidParams`. Empty content is skipped.
An assistant message before the first user message is skipped (it has no turn to belong to).
A colliding `sessionId` is refused — re-import never overwrites an existing transcript.

## 4. Source parsing rules

Discovery walks `<home>/.claude/projects/**/*.jsonl`, skipping directories named `subagents`,
`worktree`, `worktrees` (case-insensitive). `sessionId` is the file stem.

A transcript is rejected when any head record carries `isSidechain: true` (top level,
`message`, or `request`), or when its workspace path contains a `.claude/worktree` or
`.claude/worktrees` segment. Those are Claude's own temporary execution surfaces, not user
projects.

The workspace path is the `cwd` of the **first visible business message**, not of an arbitrary
head record: queue/progress entries can carry a deeper temporary cwd.

Visible text extraction:

- `type: "user"` with `isMeta: true` (entry or message) is not visible.
- `tool_result` content parts are protocol noise and are dropped.
- `<ide_opened_file>…</ide_opened_file>` and `<local-command-…>…</local-command-…>` /
  `<command-…>…</command-…>` blocks are replaced with a space; CRLF is normalized; the result
  is trimmed.
- User parts join with `\n\n`; assistant `type: "text"` parts join with `""`.
- `model: "<synthetic>"` assistant entries are SDK control placeholders and are dropped,
  **unless** `isApiErrorMessage: true` — an API/login error is a bubble the user actually saw.
- A trailing `No response requested.` line is stripped from assistant text.
- Assistant events split across one turn are aggregated into a single message, flushed at the
  next visible user turn or at end of file.

Timestamps accept epoch seconds, epoch millis, numeric strings and RFC3339. `createdAt` is the
first visible **user** timestamp (file birthtime is only a fallback); `updatedAt` is the last
visible message timestamp.

The title is derived from the first visible user message.

## 5. Identity and re-import

`taskId = "claude-import-" + sha256("claude:" + workspacePath + ":" + sessionId)[..24]`.
The id is deterministic, so importing the same source twice targets the same task.

The host refuses a colliding id, so "already imported" is a successful re-import of the
existing task: `AgentService::import_session` cold-resumes the session from disk (the host's
`load_session` registers it again after `closeSession`/`deleteTask`, which only closes the
session and keeps the transcript), then the membership row is restored (`archived = false`,
`deletedAt`/`archivedAt` cleared) and `migrationSource = "claudeCode"` is (re)asserted. A user
who deleted an imported task and imports it again must see it in the active list, still
opening the real conversation.

`workspaceIdentity` is written to the task row only when the import was filtered to a
workspace; otherwise the row keeps the source path as its key.

## 6. What is NOT imported

Claude's tool calls, tool results, file changes, usage/token counts and runtime
mode/model/provider are not migrated. Inventing them would fabricate facts the user never
saw, and carrying the source model/provider into the session would pollute the workspace's
current runtime selection on resume. Only visible user/assistant text crosses the boundary.

## 7. Acceptance scenarios

1. Scan finds a real transcript, reports its `cwd`, stem id, mtime and a preview title.
2. Scan excludes sidechain transcripts, `subagents`/`worktree(s)` directories and
   `.claude/worktrees` workspaces.
3. Import produces a session whose rows are `turnHeader`/`userInput`/`assistantText`, whose
   history the next turn receives, and whose `turn_history_starts` anchor branch editing.
4. `isMeta`, `tool_result`, IDE/command tags, `<synthetic>` placeholders and
   `No response requested.` never reach the transcript; an `isApiErrorMessage` bubble does.
5. Split assistant events aggregate into one message per turn.
6. A missing workspace directory is skipped with `workspace_path_missing`; an unknown id with
   `session_not_found_or_workspace_mismatch`; a workspace mismatch is skipped, not imported.
7. Re-import after archive/delete restores the row to the active list, keeps
   `migrationSource = "claudeCode"`, re-registers the cold session and still opens its real
   conversation without appending a second copy of the transcript.
8. The imported task appears in `listTasks` for its workspace with a settled status
   (never `running`, which is what a `draft` phase would project) and emits
   `workspace_task_list_changed` with `reason: "task_created"`.
9. A batch with one good and one bad id reports both, in their own buckets; a corrupt
   source fails loudly instead of importing half a history.

## 8. Verification

- `native/crates/nc-code-services-agent/src/claude_import.rs` unit tests (parsing, filters,
  identity, timestamps).
- `native/crates/nc-code-services-agent/tests/claude_import_e2e.rs` — real channel runtime,
  real files on disk, real host session; asserts rows, history, task list and events.
- `native/crates/nc-code-host/src/session_import.rs` unit tests (rows, history, collision,
  orphan assistant, id safety).
- `native/crates/nc-code-host/src/store.rs` unit tests pin the `meta.json` key contract the
  task channel reads for cold sessions (`sessionId`/`createdAt`/`updatedAt`/
  `workspaceIdentity`/`collaborationMode`/…) and legacy snake-case files still loading —
  without it a closed or restarted session is invisible to `listTasks`.
- Gates: `cargo test -p nc-code-host -p nc-code-services-agent`,
  `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`,
  `bun run native:coverage`.