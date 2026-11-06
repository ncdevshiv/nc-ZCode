import { formatJson } from "@zcode/core";
import type { GlobalOptions, RunContext } from "@zcode/shared-types";
import { loadBootstrapModule } from "./bootstrap-loader.js";
import { loadCliDotenv } from "./env.js";
import type { RunDependencies } from "./cli-types.js";

export async function runLoginCommand(
  ctx: RunContext,
  options: GlobalOptions,
  deps: RunDependencies,
  noBrowser: boolean,
  args: readonly string[] = [],
): Promise<number> {
  try {
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

    // Check for API key login
    if (args.length >= 2 && (args[0] === "zai-api-key" || args[0] === "bigmodel-api-key" || args[0] === "zai-coding-plan-api-key" || args[0] === "bigmodel-coding-plan-api-key")) {
      const apiKey = args.slice(1).join(" ").trim();
      if (!apiKey) {
        throw new Error("API key is required.");
      }
      const providerId = args[0].startsWith("bigmodel") ? "bigmodel" : "zai";
      const configureApiKey = deps.configureCodingPlanApiKey ?? (await loadBootstrapModule()).configureCodingPlanApiKey;
      const result = await configureApiKey({
        apiKey,
        env,
        providerId,
      });
      if (options.json) {
        ctx.stdout.write(
          formatJson({
            status: "ready",
            provider: result.providerId,
            model: result.model,
            configPath: result.configPath,
          }),
        );
        return 0;
      }
      const providerName = result.providerId === "bigmodel" ? "BigModel" : "Z.AI";
      ctx.stdout.write(
        [
          `Configured ${providerName} Coding Plan.`,
          `Model: ${result.model}`,
          `Model selection: ${result.configPath}`,
        ].join("\n") + "\n",
      );
      return 0;
    }

    // API key passed as flag or different format
    if (args.length === 2 && (args[0] === "zai" || args[0] === "bigmodel") && args[1].length > 20) {
      const apiKey = args[1];
      const providerId = args[0] as "zai" | "bigmodel";
      const configureApiKey = deps.configureCodingPlanApiKey ?? (await loadBootstrapModule()).configureCodingPlanApiKey;
      const result = await configureApiKey({
        apiKey,
        env,
        providerId,
      });
      if (options.json) {
        ctx.stdout.write(
          formatJson({
            status: "ready",
            provider: result.providerId,
            model: result.model,
            configPath: result.configPath,
          }),
        );
        return 0;
      }
      const providerName = result.providerId === "bigmodel" ? "BigModel" : "Z.AI";
      ctx.stdout.write(
        [
          `Configured ${providerName} Coding Plan.`,
          `Model: ${result.model}`,
          `Model selection: ${result.configPath}`,
        ].join("\n") + "\n",
      );
      return 0;
    }

    throw new Error(
      "ZCode OAuth login is disabled. Use API key login:\n" +
      "  zcode login zai-api-key <api-key>\n" +
      "  zcode login bigmodel-api-key <api-key>",
    );
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    ctx.stderr.write(`Error: ${message}\n`);
    if (options.verbose && error instanceof Error && error.stack) {
      ctx.stderr.write(`${error.stack}\n`);
    }
    return 1;
  }
}

export async function runLogoutCommand(
  ctx: RunContext,
  options: GlobalOptions,
  deps: RunDependencies,
): Promise<number> {
  try {
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

    const logout = deps.logoutZCodeCli ?? (await loadBootstrapModule()).logoutZCodeCli;
    const result = await logout({ env });

    if (options.json) {
      ctx.stdout.write(
        formatJson({
          status: "logged_out",
          provider: "zai",
          credentialsPath: result.credentialsPath,
        }),
      );
      return 0;
    }

    ctx.stdout.write(
      `Logged out from Coding Plan accounts. Credentials: ${result.credentialsPath}\n`,
    );
    return 0;
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    ctx.stderr.write(`Error: ${message}\n`);
    if (options.verbose && error instanceof Error && error.stack) {
      ctx.stderr.write(`${error.stack}\n`);
    }
    return 1;
  }
}

function writeAuthorizeUrl(
  ctx: RunContext,
  options: GlobalOptions,
  authorizeUrl: string,
  noBrowser: boolean,
  providerId: "zai" | "bigmodel",
): void {
  const target = options.json ? ctx.stderr : ctx.stdout;
  if (noBrowser) {
    target.write(`Open this URL to sign in:\n${authorizeUrl}\n`);
    return;
  }

  target.write(
    `Opening browser for ${providerId === "bigmodel" ? "BigModel" : "Z.AI"} authorization.\nFallback URL:\n${authorizeUrl}\n`,
  );
}

function formatUserLabel(user: { email?: string; name?: string; user_id: string }): string {
  const label = user.name || user.email || user.user_id;
  return label ? ` as ${label}` : "";
}
