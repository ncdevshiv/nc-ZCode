// tui-prompt-handler.ts reaches the upper limit of oxlint max-lines (400 lines), and puts it in createApp
// "Read dotenv → locate the session to be restored → install the bootstrap module → start Provider Registry
// Resident runtime → Read default model selection" This process level is ready to be split into this file;
// The public side is still exported from tui-prompt-handler.ts.
import { loadBootstrapModule } from "./bootstrap-loader.js";
import { loadCliDotenv } from "./env.js";
import { createCliProviderRefreshReporter } from "./provider-runtime-env.js";
import { resolveResumeSession } from "./resume.js";
import type { CliResumeRequest, RunDependencies } from "./cli-types.js";

type ProviderRegistryRuntime = Awaited<
  ReturnType<NonNullable<RunDependencies["startProcessProviderRegistryRuntime"]>>
>;

// Process-level handles reused across App replacements (/new, /resume, /fork): only one copy is used during the entire Prompt Handler life cycle.
// Only symmetric shutdown in final state close. Previously there were three let closure variables in createTuiSubmitPrompt.
interface TuiProcessRuntimeState {
  providerRegistryRuntimePromise: Promise<ProviderRegistryRuntime> | undefined;
  shutdownTelemetry: (() => Promise<void>) | undefined;
}

export const createTuiProcessRuntimeState = (): TuiProcessRuntimeState => ({
  providerRegistryRuntimePromise: undefined,
  shutdownTelemetry: undefined,
});

// The return value type is left to inference: these are inferred local variables in the original createApp. Instead, the handwritten interface will
// The joint signature of brand type (SessionId) and createZCodeApp is narrowed wrongly.
export async function prepareTuiAppRuntime(
  deps: RunDependencies,
  version: string,
  request: CliResumeRequest,
  state: TuiProcessRuntimeState,
) {
  const env = deps.env ?? process.env;
  const workingDirectory = (deps.cwd ?? process.cwd)();
  const dotenvResult = (deps.loadDotenv ?? loadCliDotenv)({
    cwd: workingDirectory,
    env,
  });

  if (dotenvResult.error) {
    throw new Error(`Failed to load environment file: ${dotenvResult.path}`, {
      cause: dotenvResult.error,
    });
  }

  const sessionId = await resolveResumeSession(request, workingDirectory, env, deps);
  const bootstrapModule = deps.createZCodeApp ? undefined : await loadBootstrapModule();
  const createAppFactory = deps.createZCodeApp ?? bootstrapModule?.createZCodeApp;
  if (!createAppFactory) throw new Error("ZCode app factory is unavailable.");
  const prepareTelemetry =
    deps.prepareZCodeTelemetryEnv ?? bootstrapModule?.prepareZCodeTelemetryEnv;
  if (prepareTelemetry) {
    state.shutdownTelemetry =
      deps.shutdownZCodeTelemetry ?? bootstrapModule?.shutdownZCodeTelemetry;
  }
  const appEnv = prepareTelemetry
    ? await prepareTelemetry(env, {
        cliVersion: version,
        productVersion: env.ZCODE_APP_VERSION,
      })
    : env;
  const startProviderRegistryRuntime =
    deps.startProcessProviderRegistryRuntime ??
    bootstrapModule?.startProcessProviderRegistryRuntime;
  if (!startProviderRegistryRuntime) {
    throw new Error("Provider Registry runtime is unavailable.");
  }
  state.providerRegistryRuntimePromise ??= startProviderRegistryRuntime(
    appEnv,
    deps.skipUserConfig
      ? {}
      : {
          standalone: {
            ...createCliProviderRefreshReporter(),
            ...(deps.userConfigPath ? { legacyCliUserConfigFilePath: deps.userConfigPath } : {}),
          },
        },
  );
  const providerRegistryRuntime = await state.providerRegistryRuntimePromise;
  const configuredDefaultModelSelection = providerRegistryRuntime?.modelSelectionConfigRepository
    ? await providerRegistryRuntime.modelSelectionConfigRepository.read()
    : providerRegistryRuntime?.configuredDefaultModelSelection;

  return {
    appEnv,
    configuredDefaultModelSelection,
    createAppFactory,
    providerRegistryRuntime,
    sessionId,
    workingDirectory,
  };
}
