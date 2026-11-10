# NC-AG — configuration, deployment and operations

## 1. Configuration sources

Precedence: explicit process setting (env) > `NCAG_CONFIG_FILE` JSON > built-in defaults.

| Env var | Meaning | Default |
| --- | --- | --- |
| `NCAG_CONFIG_FILE` | optional JSON config file (see below) | — |
| `NCAG_WORKSPACE_ROOT` | default workspace for new projects (must be a git repository) | `.` |
| `NCAG_DATA_DIR` | data directory; the store lives at `<dir>/ncag.sqlite` | `<workspace>/.ncag` |
| `NCAG_PROVIDER_ID` | provider id to use (otherwise the stored default, then first executable) | — |
| `NCAG_MODEL_ID` | model id to use | — |
| `NCAG_API_TOKEN` | when set, every `/api/nc-ag/*` call needs `?token=` or `Authorization: Bearer` | unset |
| `NCAG_PROJECT_BUDGET_USD` | project spend ceiling in USD | `25` |
| `NCAG_CONCURRENCY` | max in-flight agents per project (1–64) | `4` |

`NCAG_CONFIG_FILE` shape (`docs/nc-ag/config.example.json`):

```jsonc
{
  "workspaceRoot": ".",
  "dataDir": "",
  "model": { "providerId": "", "modelId": "", "contextWindow": null },
  "budgets": {
    "projectUsd": 25,
    "taskTokens": 250000,
    "concurrency": 4,
    "maxAgentsPerTask": 3,
    "maxRetries": 2,
    "maxReviewCycles": 2,
    "maxTaskMinutes": 60
  },
  "policy": {
    "requirePlanApproval": false,
    "requireMaterialApproval": true,
    "stallAfterSecs": 300,
    "heartbeatTimeoutSecs": 90,
    "leaseTtlSecs": 900,
    "defaultDecisionClass": "technical_recommendation"
  },
  "apiToken": ""
}
```

Validation is fail-fast: an out-of-range `concurrency`, a negative budget or
`maxReviewCycles > 5` refuses to start with `invalid_request` naming the field.
`apiToken` is never logged; `zcode-ncag --check-config` prints the effective
configuration with the credential source, not the secret.

## 2. Credentials

NC-AG has **no credentials of its own**. The model provider is resolved from the
personal provider store (`~/.zcode/v2/provider-config-rust-v1.json`, the same file the
ZCode host uses). Configure a provider in **Settings → Providers** (or edit that file
directly) with:

```jsonc
{
  "api": { "type": "openai-chat-completions", "baseUrl": "https://…/v1" },
  "access": { "type": "api-key", "apiKey": "<key>" },
  "pricing": { "inputUsdPerMillionTokens": 0.5, "outputUsdPerMillionTokens": 1.0 }
}
```

Supported `api.type` values: `anthropic-messages` and `openai-chat-completions`
(`openai-responses` is a different protocol and is rejected by name).
`pricing` is optional; without it the dashboard reports `$0.00` rather than guessing.
Missing `baseUrl` or `apiKey` fails closed with `provider_unavailable` and the dispatch
is recorded as `provider.unavailable`; the request becomes `waiting_provider`.

**Verified in this environment:** the personal provider store on this machine contains an
executable `openai-chat-completions` entry and a live run did reach the model (events
`agent.round` / `agent.tool` with real token counts). No NC-AG-side secret is required.

## 3. Running NC-AG

### 3.1 Inside the ZCode server (web/remote)

```sh
NCAG_DATA_DIR=/data/ncag \
NCAG_WORKSPACE_ROOT=/srv/repo \
bun run dev:web          # or: zcode-server --host 0.0.0.0 --port 3030 --token "$TOKEN"
```

The router is mounted at `/api/nc-ag/*`; the dashboard is `http://host:port/nc-ag`
(alias `/cn/nc-ag`).

### 3.2 Inside the desktop shell

The Tauri shell builds the runtime from `AppPaths` on startup and logs
`NC-AG disabled: …` (a warning, not a crash) when the configuration is invalid. NC-AG
runs in the same process as the host backend; there is no second server.

### 3.3 Standalone service

```sh
cargo build --release --manifest-path native/Cargo.toml -p nc-code-orchestrator
NCAG_DATA_DIR=/data/ncag ./native/target/release/zcode-ncag --host 127.0.0.1 --port 3040
# prints: NCAG LISTENING 127.0.0.1:3040
```

Useful for operations/CI where the UI is not needed (`--check-config` prints the
effective configuration and exits).

## 4. Health checks

| Probe | Expected |
| --- | --- |
| `GET /healthz` | `{"status":"ok"}` (server process) |
| `GET /api/nc-ag/health` | `status: ok`, `auditChainVerified: true`, `provider.configured: true` |
| `GET /api/nc-ag/audit/verify` | `verified: true`, monotonically growing `entries` |
| `GET /api/nc-ag/events?limit=1` | `latestId` increases while work is running |
| `GET /api/nc-ag/dashboard` | `inFlight`, per-project counters and `auditVerified` |

A degraded `/api/nc-ag/health` (`auditChainVerified: false`) means the audit table was
edited outside NC-AG: stop the service, investigate, and restore from backup before
continuing.

## 5. Backup and restore

- **State**: `<dataDir>/ncag.sqlite` (WAL mode; back up with `sqlite3 … ".backup"` or copy
  all three files `ncag.sqlite`, `-wal`, `-shm` while the service is stopped).
- **Work**: each task's commits live on branch `ncag/<task-id>` in the project repository;
  worktrees are disposable (`git worktree prune` after a restore).
- **Restore**: stop the service, replace the database, start it. Startup recovery
  requeues orphaned tasks with their worktrees preserved; no state is invented.
- **Audit**: `/api/nc-ag/audit` exports the hash-chained rows; a hash mismatch after a
  restore is reported by `/api/nc-ag/audit/verify`.

## 6. Troubleshooting

| Symptom | Confirmed cause | Action |
| --- | --- | --- |
| `invalid_request: … is not a git worktree` | workspace is not a git repository | initialise or point `workspace` at a repo |
| `provider_unavailable: no provider is configured` | empty provider store | add a provider in Settings → Providers |
| `provider_unavailable: provider api.type … not supported` | provider uses `openai-responses` | switch the provider to `openai-chat-completions` or `anthropic-messages` |
| request stuck in `waiting_provider` | dispatch failed closed | fix the provider, then `POST /projects/{id}/resume` |
| `user_decision_required` on resolve | decision class is `material` | resolve with `decidedBy: "user"` |
| task `failed` with `max_rounds_exhausted` | the model looped without submitting | reassign the task; inspect `events` for the loop |
| `budget.exceeded` | project spend reached the ceiling | raise the budget in the project config or reduce scope |
| `lease.waiting` repeated | another task holds an exclusive write | inspect `/leases/waiters`, wait for expiry, or release manually |
| stream shows `stream down` in the UI | SSE disconnected (server restart) | the dashboard falls back to polling; reload when the server is back |

## 7. Security posture in production

- Bind to `127.0.0.1` unless you set `NCAG_API_TOKEN` and a real token on the server.
- NC-AG runs the model and shell tools with the OS account of the service; the workspace
  must be a repository you are willing to let agents modify inside worktrees.
- Reviewers/auditors are read-only at the permission ceiling level; a project owner cannot
  widen it by editing a profile.
- Secrets stay in the provider store; NC-AG redacts bearer tokens, `sk-…` keys and private
  keys from logs, events, messages and API payloads.
