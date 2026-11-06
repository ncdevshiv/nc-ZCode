import { mkdirSync, readFileSync, realpathSync, statSync, unlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  gitBashPathToWindowsPath,
  type ExecutionCommand,
  type ExecutionRequest,
  type ExecutionShellDialect,
  windowsPathToGitBashPath,
} from "@zcode/contracts";

interface CwdCapturePlan {
  command: ExecutionCommand;
  cwdFilePath?: string;
}

export function createCwdCapturePlan(
  request: ExecutionRequest,
  options: {
    dialect: ExecutionShellDialect;
    platform: NodeJS.Platform;
  },
): CwdCapturePlan {
  if (request.captureCwdAfterSuccess !== true || request.command.mode !== "shell") {
    return { command: request.command };
  }

  const cwdCaptureDir = tmpdir();
  mkdirSync(cwdCaptureDir, { recursive: true });
  const cwdFilePath = join(cwdCaptureDir, `zcode-${crypto.randomUUID()}-cwd`);

// Each time Bash still starts a new shell; after success, only the final pwd -P is written back to the main process, and env/alias/function cannot be persisted.
// By default, shell, hooks, and background commands do not take this branch to avoid changing other execution surfaces.
  const wrappedCommand =
    options.dialect === "cmd"
      ? createWindowsCmdCwdCaptureCommand(request.command.command, cwdFilePath)
      : createPosixCwdCaptureCommand(
          request.command.command,
          options.dialect === "git-bash" ? windowsPathToGitBashPath(cwdFilePath) : cwdFilePath,
        );

  return {
    command: {
      ...request.command,
      command: wrappedCommand,
    },
    cwdFilePath,
  };
}

function createPosixCwdCaptureCommand(command: string, cwdFilePath: string): string {
  return [
    command,
    "__zcode_status=$?",
    `if [ "$__zcode_status" -eq 0 ]; then pwd -P > ${shellQuote(cwdFilePath)}; fi`,
    'exit "$__zcode_status"',
  ].join("\n");
}

function createWindowsCmdCwdCaptureCommand(command: string, cwdFilePath: string): string {
  return [
    command,
    'set "__zcode_status=%ERRORLEVEL%"',
    `if "%__zcode_status%"=="0" cd > ${cmdQuote(cwdFilePath)}`,
    "exit /b %__zcode_status%",
  ].join("\r\n");
}

export function readCapturedCwd(
  cwdFilePath: string | undefined,
  options: { dialect: ExecutionShellDialect },
): string | undefined {
  if (!cwdFilePath) return undefined;
  try {
    const value = readFileSync(cwdFilePath, "utf8").replace(/\r?\n$/u, "");
    if (!value) return undefined;
    const hostValue = normalizeCapturedCwdForHost(value, options.dialect);
    const stats = statSync(hostValue);
    if (!stats.isDirectory()) return undefined;
    return realpathSync(hostValue);
  } catch {
    return undefined;
  } finally {
    try {
      unlinkSync(cwdFilePath);
    } catch {
// cwd capture only affects the internal session state, and failure to clean up cannot affect the tool results.
    }
  }
}

function normalizeCapturedCwdForHost(value: string, dialect: ExecutionShellDialect): string {
  return dialect === "git-bash" ? gitBashPathToWindowsPath(value) : value;
}

function shellQuote(value: string): string {
  return `'${value.replaceAll("'", "'\\''")}'`;
}

function cmdQuote(value: string): string {
  return `"${value.replaceAll('"', '""')}"`;
}
