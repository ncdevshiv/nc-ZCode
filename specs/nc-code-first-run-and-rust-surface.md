# Spec: NC-Code first-run provider setup and Rust-only product surface

Status: accepted 2026-11-06
Owner: desktop + UI
Related: `specs/rust-backend-replacement.md`, `specs/tauri-migration-plan.md`,
`specs/zai-claim-boundary.md`, `specs/disabled-features.md`

## 1. Product rule

The Tauri desktop app runs the embedded Rust backend (`nc-code-app` + `nc-code-server`).
ZCode account / OAuth / Coding Plan / Start Plan / upgrade services are not registered by
that backend and must not be presented as available in the UI.

`NC-Code` is the visible product name. Internal package names, protocol identifiers and
the `zcode://` deep-link scheme stay unchanged.

## 2. First run

1. First launch does **not** open the OAuth account screen.
2. It shows one dismissible provider-setup prompt:
   - pick a provider template (API-key providers, including OpenAI-compatible/custom),
   - enter the API key and save, or
   - close / "Skip for now" and continue to the workspace without a model.
3. Dismissal is persisted locally; the prompt is shown again only when no usable provider
   exists and the user has not dismissed it.
4. Without a provider the app remains usable: the workspace opens, and model-dependent
   actions fail with a named "no model configured" message that points to
   Settings → Model settings, never to an upgrade page.

## 3. Removed surfaces

When account services are unavailable (`ZCODE_ACCOUNT_SERVICES_AVAILABLE = false`):

- no forced login guard, no "Connect account" menu item, no sidebar plan badge;
- no Z.ai / BigModel / Start Plan / Coding Plan preset navigation entries;
- no upgrade CTA in the chat error banner, composer, footer or settings;
- usage settings shows only app usage, not Coding Plan tabs;
- the model provider list contains only providers the Rust `provider-settings` channel
  actually returns (personal/API-key/custom providers).

Legacy components may remain in the tree, but they must be unreachable and safe to render
(no throw when the coding-plan dialog provider is absent).

## 4. Plugin channel argument contract

`plugin-management` is a channel, so clients send positional arguments
(`ProxyChannel.toService` → `call(method, [params])`). The params object is flat:

```json
{ "workspacePath": "C:\\work", "configScope": "user" }
```

The CLI protocol shape (`{ "workspace": { "workspacePath": ... } }`) remains accepted for
compatibility, but the channel shape is the primary contract and is pinned by a test.

## 5. Acceptance

- `cargo test -p nc-code-services-plugins` covers both argument shapes.
- Tauri boot log contains no `Unknown channel: oauth` and no plugin
  `workspacePath must be a non-empty string` errors.
- First launch shows the provider prompt; skip/close reaches the workspace and does not
  re-open the prompt in the same install.
- Model settings shows an empty personal-provider list, not Z.ai/Start Plan cards.
- Window title, tray/menu, welcome copy and page titles show `NC-Code`.
- `bun run typecheck`, `bun run lint`, `cargo clippy` for touched crates pass.

## 6. Runtime defects fixed alongside the surface change

The Tauri run surfaced defects that were not visible in the stale (Nov 4) web build:

| Symptom | Cause | Fix |
| --- | --- | --- |
| Blank window after dismissing the startup login/provider screen | `Root` initialised `isRestoringOAuthSession=true`; the OAuth restore hook is no longer mounted, so the startup gate never released | Store initial value is `ZCODE_ACCOUNT_SERVICES_AVAILABLE`; the onboarding shell renders children immediately when `autoTrigger=false` |
| `Cannot read properties of undefined (reading 'length')` on every boot | `useZCodeSessionService`, `useZCodeAgentService`, `useZCodeTaskService` called a workspace hook conditionally; the workspace path appears after first render, changing hook order | Always call `useWorkspaceServices` (it already falls back to base services) |
| Provider prompt reappeared on every restart | Backend bound `127.0.0.1:0`; every launch had a new origin and therefore a new localStorage | Shell binds a stable default port `43117` (`ZCODE_SHELL_PORT` override), falling back to ephemeral only when the port is owned |
| `ZCodeWorkspaceRuntimeNotFound` warning on boot | `subscribeSessionsIndexV4` ignored `runtimePolicy: "existing-only"` and tried to start a runtime, which requires a provider | `sessions_index_request` uses `existing_runtime` for existing-only; server logs the expected unavailable case at debug |
| Model settings stuck on “Loading...” with no providers | The detail panel treats a null selected item as loading | Explicit empty state when the navigation list is empty |
| CORS error for `zcode.z.ai/api/v1/client/configs` | The web Help-config fetch runs from the loopback origin inside WebView2 | Tauri mode skips the remote hop and uses the bundled/local Help config |

