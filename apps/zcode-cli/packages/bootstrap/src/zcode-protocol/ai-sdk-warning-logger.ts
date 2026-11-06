import type { LogContext, Logger } from "@zcode/contracts";

interface AiSdkWarningLoggerOptions {
  model?: unknown;
  provider?: unknown;
  warnings?: unknown;
}

type AiSdkWarningLogger = (options: AiSdkWarningLoggerOptions) => void;

type AiSdkWarningGlobal = typeof globalThis & {
  AI_SDK_LOG_WARNINGS?: false | AiSdkWarningLogger;
};

export function installZCodeProtocolAiSdkWarningLogger(logger: Logger): void {
// The default warning logger of AI SDK will use console.info to write stdout for the first time;
// The stdout of app-server --stdio is the ZCode Protocol NDJSON frame channel. Any ordinary text will cause the host to fail to parse.
  (globalThis as AiSdkWarningGlobal).AI_SDK_LOG_WARNINGS = (
    options: AiSdkWarningLoggerOptions,
  ) => {
    try {
      const warnings = Array.isArray(options.warnings) ? options.warnings : [];
      logger.warn("AI SDK model warning", {
        event: "model.sdk.warning",
        module: "bootstrap.zcode_protocol",
        status: "completed",
        model: stringValue(options.model),
        provider: stringValue(options.provider),
        warningCount: warnings.length,
        warnings: warnings.map(summarizeWarning),
      });
    } catch {
// warning logging cannot affect real model requests; stdout purity is guaranteed by not calling console.*.
    }
  };
}

function summarizeWarning(warning: unknown): LogContext {
  if (!warning || typeof warning !== "object") {
    return { value: stringValue(warning) };
  }
  const record = warning as Record<string, unknown>;
  return {
    type: stringValue(record.type),
    feature: stringValue(record.feature),
    message: stringValue(record.message),
    details: stringValue(record.details),
  };
}

function stringValue(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}
