# Spec: ZCode services boundary (removed)

> **WARNING**: Never trust docs/logs without fully auditing code. Always verify against actual implementation.
Status: updated 2026-11-05 (user directive). All ZCode services have been removed.
Related: `specs/rust-backend-replacement.md`, `specs/typescript-frontend-only.md`.

## 1. Product rule

- The application is **fully provider-agnostic and open source**: any provider (OpenAI/Anthropic-compatible,
  API-key or custom endpoint) keeps working exactly as before. No provider is removed
  or hardcoded by this spec.
- **All ZCode services (auth, quota, cloud sending data) are removed.** No connections to ZCode/z.ai infrastructure.
- No Z.ai-specific behaviour is present: no OAuth login, no free-credit claim, no billing/balance reads,
  no coding-plan purchase/team/enterprise flows, no Z.ai usage-quota provider, no conversation telemetry,
  no share or feedback upload, no model-data forwarding, no marketing previews.
- All authentication, quota tracking, and cloud data transmission are disabled/removed. Users must provide
  their own API keys/endpoints for any provider they wish to use.

## 2. Data policy

**No data leaves the device to ZCode services.** Prompts, model context, conversation history, tool output,
file contents, workspace paths, telemetry, usage statistics, analytics identifiers must never be sent to
ZCode infrastructure. All traffic is solely between the user and the provider endpoint they configure.

## 3. Removed surfaces

All of the following are removed/not ported:
- Z.ai OAuth and authentication flows
- Z.ai Start Plan claim and billing/balance
- `coding-plan-subscription` purchase, team and enterprise flows
- Z.ai usage-stats quota provider
- Conversation telemetry, share/feedback uploads
- Start Plan marketing previews
- Any analytics/telemetry emitters tied to ZCode services
- Settings/skill/MCP/plugin remote sync
- Prompt-attachment transfer through ZCode infrastructure
- Remote feature flags, remote content catalogs, remote scheduling

## 4. Completion gate

The application must function fully with only user-configured providers. No ZCode service endpoints are contacted.
All provider interactions go directly from the user to their chosen provider endpoint. The rest of the application
remains provider-agnostic as tracked in `specs/rust-backend-replacement.md`.
