# Spec: Remote model-catalog auto fetch + capability probing + realtime TPS

Status: accepted 2026-10-28
Owner: native backend (fetch / probing / harness) + packages/ui (Auto fetch panel, TPS display)
Governing specs: `specs/rust-backend-replacement.md` (Rust is the source of truth for the
channel), `specs/typescript-frontend-only.md` (new logic lives in the Rust service, the
frontend only renders), `DESIGN.md` (the Auto fetch panel reuses existing components and
tokens; no new visual language is invented).

## 1. Product rules

1. **Any provider works** (as today). Auto fetch is not tied to a specific vendor: given
   `{api.type, api.baseUrl, access.apiKey}` it lists the models of that endpoint.
   NVIDIA (`https://integrate.api.nvidia.com/v1`) is the first real verification target.
2. **Automatic + smart**: one click fetches the full `/models` set and, for every model,
   gives a **source-labelled** judgement of "purpose / modality / context / output ceiling /
   tool calling / thinking levels":
   - `catalog`: structured metadata returned by the endpoint itself (when present it has the
     highest priority).
   - `heuristic`: derived from the model name and family rules, labelled with its
     confidence; the UI must display it as "inferred".
   - `measured`: obtained by this harness's real measurements (connectivity, whether it
     actually produces tool_calls, whether it produces a reasoning/thinking channel,
     throughput). Measured facts always override the previous two.
   Never present an inference as a fact: heuristic and measured must be visually
   distinguishable in the UI.
3. **One-click actions**: select all / multi-select / single-select → Add all / Add
   selected; for models that already exist → Remove (multi/single select). Add reuses the
   existing `addPersonalModel`/`savePersonalModelDraft` write path; no second persistence
   channel is added.
4. **Failure is a product fact**: unavailable, rate-limited, timeout and 5xx each return a
   per-entry status and reason code, and the panel shows them as they are; nothing is
   silently skipped and nothing is written up as "supported". Concurrency and timeouts are
   capped (default concurrency 4, connectivity probe 20s, generation probe 60s) so a whole
   batch cannot stall.
5. **Credentials**: apiKey appears only in request headers and is never written to logs,
   examples, specs, commit history or snapshot files.

## 2. Interface (channel RPC, `provider-settings`)

Two methods are added (`ProxyChannel` positional argument = a single params object):

```ts
listRemoteModels(input: {
  providerId: string;            // an existing personal provider
  force?: boolean;               // ignore the in-process TTL cache
}): Promise<{
  baseUrl: string;
  apiType: "openai-chat-completions" | "openai-responses" | "anthropic-messages";
  fetchedAt: number;             // ms epoch
  total: number;
  models: Array<{
    modelId: string;
    displayName?: string;
    ownedBy?: string;
    created?: number;
    purpose: string[];           // chat | code | vision | reasoning | embed | rerank | image | audio | guard
    input: { text: boolean; image: boolean; video: boolean; audio: boolean; pdf: boolean };
    contextWindow?: number;
    maxOutputTokens?: number;
    supportsToolCall?: boolean;
    reasoningLevels?: string[];  // e.g. ["low","medium","high"] or ["enabled"]
    confidence: "catalog" | "heuristic";
    alreadyAdded: boolean;
    enabled: boolean;            // whether this model is currently enabled in the provider
  }>;
}>

probeRemoteModel(input: {
  providerId: string;
  modelId: string;
  checks?: ("connectivity" | "toolCall" | "reasoning" | "throughput")[];
}): Promise<{
  modelId: string;
  connectivity: { ok: boolean; status?: number; latencyMs: number; reasonCode?: string; message?: string };
  toolCall?: { ok: boolean; reasonCode?: string };
  reasoning?: { ok: boolean; channel?: "reasoning_content" | "reasoning" | "thinking"; reasonCode?: string };
  throughput?: { ok: boolean; completionTokens: number; elapsedMs: number; tokensPerSecond: number; streamed: boolean; reasonCode?: string };
}>
```

## 3. Realtime TPS

1. The host is the source of truth: when an assistant turn closes, `completionTokens` and
   that turn's first-token→end duration are already known, so TPS is computed by the Rust
   host, not guessed by the renderer.
2. Carrier: an additive optional field `throughput` on the `assistantText` row
   (`{ completionTokens, elapsedMs, tokensPerSecond }`), declared in the protocol at
   `packages/shared/src/zcode-protocol-v4/rows.ts` in the same change (additive, optional,
   older clients simply ignore it).
3. Display: the message footer after the response ends (the same row as copy and the other
   actions) shows `xx.x tok/s`; no fake data while streaming is in progress; with no
   measured value the chip is not shown at all.

## 4. Acceptance scenarios

1. Auto fetch: against the NVIDIA endpoint it returns a list (measured 85 entries), each
   model carrying its purpose/modality/context/thinking judgement and a confidence label;
   after Add all, provider.models matches the list, and reopening the panel gives
   `alreadyAdded=true`.
2. Selection and removal: single/multi-select Add, single/multi-select Remove; the result
   matches `getView()`.
3. Harness measurement: run connectivity + tool + reasoning + throughput probes over all
   models; unavailable/timeout/rate-limited each land a per-entry status, and a report is
   produced (model count, available count, failure-reason distribution, TPS distribution).
4. TPS: after a real NVIDIA model answers, tok/s appears in the footer, using the same
   source and definition as `probeRemoteModel.throughput` (token counts come from usage;
   when missing, streamed chunk counting is used and marked `streamed:true`).
5. Visuals: the Auto fetch button and panel reuse the existing Button/Dialog/Checkbox/Badge
   and tokens; light/dark themes and mobile web do not break layout; all copy goes through
   i18n keys (completed for every existing locale).
