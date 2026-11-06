/**
 * Contract type of remote model directory (Auto fetch) and capability detection.
 *
 * Source of fact in Rust `provider-settings` channel (`native/crates/zcode-services-provider/src/catalog.rs`):
 * `catalog` = self-reported endpoint, `heuristic` = name rule inference, `measured` = use the product's own
 *provider harness actual test. The three sources must be distinguishable in the UI (`specs/model-catalog-and-tps.md`).
 */
import type { ModelId, ProviderId } from "@zcode/provider";

export type ModelCatalogConfidence = "catalog" | "heuristic";

export interface ModelCatalogInputFormats {
  readonly text: boolean;
  readonly image: boolean;
  readonly video: boolean;
  readonly audio: boolean;
  readonly pdf: boolean;
}

export interface RemoteModelCatalogEntry {
  readonly modelId: ModelId;
  readonly displayName?: string;
  readonly ownedBy?: string;
  readonly created?: number;
  /** Usage tags (chat/code/vision/reasoning/embedding/…). */
  readonly purpose: readonly string[];
  readonly input: ModelCatalogInputFormats;
  readonly contextWindow?: number;
  readonly maxOutputTokens?: number;
  readonly supportsToolCall?: boolean;
  readonly reasoningLevels?: readonly string[];
  readonly confidence: ModelCatalogConfidence;
  /** This model already exists in this provider (the panel displays Remove instead of Add accordingly). */
  readonly alreadyAdded: boolean;
  readonly enabled: boolean;
}

export interface RemoteModelCatalog {
  readonly baseUrl: string;
  readonly apiType: string;
  readonly fetchedAt: number;
  readonly total: number;
  readonly models: readonly RemoteModelCatalogEntry[];
}

export interface RemoteModelCatalogInput {
  readonly providerId: ProviderId;
  /** Ignore in-process TTL cache re-fetching. */
  readonly force?: boolean;
}

export type RemoteModelProbeCheck = "toolCall" | "reasoning" | "throughput";

export interface RemoteModelProbeResult {
  readonly modelId: ModelId;
  readonly connectivity: {
    readonly ok: boolean;
    readonly latencyMs: number;
    readonly generationMs?: number;
    readonly chunks?: number;
    readonly completionTokens?: number;
    readonly textChars?: number;
    readonly error?: {
      readonly code?: string;
      readonly message?: string;
      readonly statusCode?: number;
      readonly reason?: string;
    } | null;
  };
  readonly toolCall?: {
    readonly ok: boolean;
    readonly toolName?: string | null;
    readonly error?: unknown;
  };
  readonly reasoning?: {
    readonly ok: boolean;
    readonly channel?: string | null;
    readonly error?: unknown;
  };
  readonly throughput?: {
    readonly ok: boolean;
    readonly completionTokens: number;
    readonly estimated?: boolean;
    readonly elapsedMs: number;
    readonly windowSource?: "generation" | "request";
    readonly tokensPerSecond: number;
    readonly error?: unknown;
  };
  /** Probe items to skip when connectivity fails (does not falsify subsequent facts). */
  readonly skippedChecks?: readonly string[];
  readonly probeError?: string;
}

export interface RemoteModelProbeInput {
  readonly providerId: ProviderId;
  readonly modelId: ModelId;
  readonly checks?: readonly RemoteModelProbeCheck[];
}

export interface RemoteModelSelectionInput {
  readonly providerId: ProviderId;
  readonly modelIds: readonly ModelId[];
}
