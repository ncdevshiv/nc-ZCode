import { getCapturedZCodeAgentTelemetryEnv } from "@zcode/shared";
import {
  prepareModelTelemetryEnv,
  shutdownPreparedModelTelemetry,
  type PrepareModelTelemetryOptions,
} from "@zcode/telemetry";

/**
* The official CLI asynchronous entry is called before creating the synchronous App; only the prepared device MID is returned to the business env.
* Private configurations such as OTLP Header are still retained in the in-process capture area and do not enter the Tool/MCP sub-process environment.
 */
export async function prepareZCodeTelemetryEnv(
  env: NodeJS.ProcessEnv = process.env,
  options: PrepareModelTelemetryOptions = {},
): Promise<NodeJS.ProcessEnv> {
  const prepared = await prepareModelTelemetryEnv({
    ...getCapturedZCodeAgentTelemetryEnv(),
    ...env,
  }, {
    ...options,
    productVersion: options.productVersion ?? env.ZCODE_APP_VERSION,
  });
  const deviceMid = prepared.ZCODE_TELEMETRY_DEVICE_MID;
  return deviceMid ? { ...env, ZCODE_TELEMETRY_DEVICE_MID: deviceMid } : env;
}

/**
* Close the Telemetry Owner held by the current process symmetrically with prepareZCodeTelemetryEnv.
* Only flush is allowed for a single App/Session; only the outermost executable entry can call this function.
 */
export async function shutdownZCodeTelemetry(): Promise<void> {
  await shutdownPreparedModelTelemetry();
}
