import { spawn, type ChildProcess } from "node:child_process";
import { BashFileOutput } from "./bash-file-output.js";
import { readCapturedCwd } from "./cwd-capture.js";
import { defaultCwdDialect } from "./execution-command.js";
import { NodeExecutionAdapterProcess } from "./node-execution-adapter-process.js";
import { resolveLegacyExecutionOutputEncoding } from "./outputEncoding.js";
import {
  DEFAULT_TIMEOUT_MS,
  FORCE_EXIT_AFTER_KILL_MS,
  abortSignalReason,
  isBashMergedOutputRequest,
  isExpectedChildStdinClosureError,
} from "./execution-utils.js";
import type {
  ExitState,
  InternalExecutionRunOptions,
  StopReason,
} from "./execution-adapter-types.js";
import type {
  ExecutionRequest,
  ExecutionResult,
  ExecutionRunOptions,
  ExecutionShellDialect,
} from "@zcode/contracts";

export class NodeExecutionAdapterRun extends NodeExecutionAdapterProcess {
  async run(
    request: ExecutionRequest,
    options: ExecutionRunOptions = {},
  ): Promise<ExecutionResult> {
    const startedAt = new Date();
    const useBashMergedOutput = isBashMergedOutputRequest(request);
    let requestStop: (reason: StopReason) => void = () => undefined;
    let outputLimitExceeded = false;
    const internalOptions = options as InternalExecutionRunOptions;
    const aggregatePersistedBudget = internalOptions.sharePersistedOutputLimitAcrossStreams
      ? {
          bytes: 0,
          maxBytes: Math.max(0, this.persistedOutputLimit(request)),
        }
      : undefined;

    if (this.closePromise) {
      return this.createStoppedResult(startedAt, "cancelled", "Execution adapter is shutting down");
    }

    if (options.signal?.aborted) {
      return this.createStoppedResult(startedAt, "cancelled", "Execution cancelled before spawn");
    }

    let persistedLimitNotified = false;
    const onPersistedLimit =
      request.outputLimit?.killProcessOnPersistedLimit === true ||
      internalOptions.onPersistedLimit ||
      internalOptions.shouldStopOnPersistedLimit
        ? () => {
            if (persistedLimitNotified) return;
            persistedLimitNotified = true;
            internalOptions.onPersistedLimit?.();
            if (
              request.outputLimit?.killProcessOnPersistedLimit === true ||
              internalOptions.shouldStopOnPersistedLimit?.() === true
            ) {
              requestStop("output_limit");
            }
          }
        : undefined;
    const legacyOutputEncoding = resolveLegacyExecutionOutputEncoding({
      platform: this.platform,
      processEnv: this.processEnv,
    });
// Separate parsing of task records will repeatedly and synchronously execute Windows chcp; reuse the encoding value during execution.
    internalOptions.onOutputEncodingResolved?.(legacyOutputEncoding);
    const file = useBashMergedOutput
      ? new BashFileOutput(
          this.outputPathForRequest(request, "stdout")!,
          this.platform,
          legacyOutputEncoding,
        )
      : undefined;
    const stdout = file
      ? undefined
      : this.createOutputCollector(
          request,
          "stdout",
          legacyOutputEncoding,
          onPersistedLimit,
          aggregatePersistedBudget,
        );
    const stderr = file
      ? undefined
      : this.createOutputCollector(
          request,
          "stderr",
          legacyOutputEncoding,
          onPersistedLimit,
          aggregatePersistedBudget,
        );
    let child: ChildProcess | undefined;
    let cwdDialect: ExecutionShellDialect = defaultCwdDialect(this.platform);
    let cwdFilePath: string | undefined;
    let timedOut = false;
    let cancelled = false;
    let exited = false;
    let childClosed = false;
    let executionSettled = false;
    let childReadyForTermination = false;
    let terminationRequested = false;
    let stopRequested = false;
    let forceExitTimer: NodeJS.Timeout | undefined;
    let progressTimer: NodeJS.Timeout | undefined;
    let timeoutTimer: NodeJS.Timeout | undefined;
    const timeoutMs = request.timeoutMs ?? DEFAULT_TIMEOUT_MS;
    let finishExit: (state: ExitState) => void = () => undefined;
    let finishResourceTelemetry: (state: ExitState) => void = () => undefined;
// The timeout timer starts after spawn (see below). Stopping before spawn can only come from cancellation or adapter shutdown.
    const createPreSpawnStoppedResult = () =>
      this.createStoppedResult(
        startedAt,
        "cancelled",
        this.closePromise
          ? "Execution adapter is shutting down"
          : "Execution cancelled before spawn",
      );
    const stoppedBeforeSpawn = (): ExecutionResult | undefined => {
      if (!stopRequested && !this.closePromise && !options.signal?.aborted) return undefined;
      return createPreSpawnStoppedResult();
    };

    const terminateStartedChild = () => {
      if (!stopRequested || !child || !childReadyForTermination || terminationRequested) {
        return;
      }
      const startedChild = child;
      terminationRequested = true;
// Root shell exit does not mean that its process group and descendants inheriting pipe have exited.
// cancel/close must be able to clean up the entire execution after the team leader exits to avoid orphans.
      this.terminateProcessTree(startedChild, useBashMergedOutput);
      if (file) {
        finishExit({ code: timedOut ? 143 : 137 });
        return;
      }
      forceExitTimer = setTimeout(() => {
        if (!exited) {
          finishExit({
            signal: "SIGKILL",
          });
        }
// Neither Windows' taskkill nor POSIX's PGID may no longer be able to address detached descendants;
// Finally release the read end held by ZCode, and cannot continue to keep the unknown process alive CLI.
        this.destroyChildOutputStreams(startedChild);
      }, FORCE_EXIT_AFTER_KILL_MS);
      forceExitTimer.unref?.();
    };

    requestStop = (reason: StopReason) => {
      if (executionSettled || stopRequested || (file && exited)) return;
      stopRequested = true;
      if (reason === "timeout") {
        timedOut = true;
      } else if (reason === "cancelled") {
        cancelled = true;
      } else {
        outputLimitExceeded = true;
      }
      terminateStartedChild();
    };

    const executionId = this.registerActiveExecution(requestStop);
    const abortHandler = () => {
      const reason = abortSignalReason(options.signal);
      requestStop(reason === "output_limit" ? "output_limit" : "cancelled");
    };
    if (options.signal?.aborted) {
      requestStop("cancelled");
    } else {
      options.signal?.addEventListener("abort", abortHandler, { once: true });
    }

    try {
      const prepared = await this.prepareChildSpawn(request);

// Shell init snapshot creation is asynchronous; the user may cancel or close the adapter while waiting.

      const stoppedAfterCommandPreparation = stoppedBeforeSpawn();
      if (stoppedAfterCommandPreparation) {
        return stoppedAfterCommandPreparation;
      }

      if (file) {
        await file.prepare();
        const stoppedAfterOutputPreparation = stoppedBeforeSpawn();
        if (stoppedAfterOutputPreparation) {
          await file.discard();
          return stoppedAfterOutputPreparation;
        }
        prepared.spawnOptions.stdio = [
          request.stdin === undefined ? "ignore" : "pipe",
          file.fd!,
          file.fd!,
        ];
      }

      const spawnedChild = spawn(
        prepared.command.file,
        prepared.command.args,
        prepared.spawnOptions,
      );
      child = spawnedChild;
      finishResourceTelemetry = this.trackBashResources(spawnedChild, useBashMergedOutput, () => ({
        timedOut,
        killed: cancelled || outputLimitExceeded,
      }));
      cwdDialect = prepared.cwdDialect;
      cwdFilePath = prepared.cwdFilePath;

      const exitPromise = new Promise<ExitState>((resolve) => {
        finishExit = (state) => {
          if (exited) return;
          exited = true;
          finishResourceTelemetry(state);
          file?.stopWatching();
          if (file) internalOptions.bashLifecycle?.onExit?.();
          resolve(state);
        };
        spawnedChild.once("error", (error) => finishExit({ error }));
        spawnedChild.once("exit", (code, signal) =>
          finishExit({
            code: code ?? undefined,
            signal: signal ?? undefined,
          }),
        );
      });

      const closePromise = new Promise<void>((resolve) => {
        spawnedChild.once("close", () => {
          childClosed = true;
          resolve();
        });
      });

      await file?.close();
      const watchBashLimit = () => {
        if (!file || exited || stopRequested) return;
        file.watchLimit(this.persistedOutputLimit(request), () => requestStop("output_limit"));
      };
      if (file) {
        watchBashLimit();
        if (internalOptions.bashLifecycle)
          internalOptions.bashLifecycle.onBackgrounded = watchBashLimit;
        if (options.onEvent && !exited && !internalOptions.bashLifecycle?.isBackgrounded()) {
          file.watchProgress(
            this.progressTailBytes,
            this.progressThresholdMs,
            this.progressIntervalMs,
            (output, outputPreview) => {
              this.emit(options, {
                type: "progress",
                elapsedMs: Date.now() - startedAt.getTime(),
                pid: spawnedChild.pid,
                stdoutBytes: output.bytes,
                stderrBytes: 0,
                stdoutTail: output.text,
                outputPreview,
                timestamp: new Date(),
              });
            },
          );
        }
      }
      childReadyForTermination = true;
      terminateStartedChild();

      this.emit(options, { type: "started", pid: spawnedChild.pid, timestamp: startedAt });

// Reason for recovery: The timer is advanced to the preparation stage because it is a behavior of the protected-resource sandbox era - capability
// The probe may hang indefinitely, and the total timeout must cover the preparation period. After the sandbox is removed, only the shell snapshot / is left in the preparation phase.
// For existing asynchronous steps such as artifacts, timeoutMs is restored to only constrain the running of the child process to avoid slow shell initialization or
// When timeout is short, the command will be judged as timed_out before it is started; cancellation and shutdown during the preparation period are still handled by stoppedBeforeSpawn.
      if (timeoutMs > 0) {
        timeoutTimer = setTimeout(() => requestStop("timeout"), timeoutMs);
      }

      if (stdout && stderr) {
        if (options.onEvent) {
          progressTimer = setInterval(() => {
            if (
              childClosed ||
              (exited && internalOptions.shouldRetainExecutionAfterRootExit?.() !== true)
            )
              return;
            const elapsedMs = Date.now() - startedAt.getTime();
            if (elapsedMs < this.progressThresholdMs) return;
            this.emit(options, {
              type: "progress",
              elapsedMs,
              pid: spawnedChild.pid,
              stdoutBytes: stdout.bytes,
              stderrBytes: stderr.bytes,
              stdoutTail: stdout.tailText(),
              stderrTail: stderr.tailText(),
              timestamp: new Date(),
            });
          }, this.progressIntervalMs);
          progressTimer.unref?.();
        }
        this.attachPipedOutput(spawnedChild, stdout, stderr, legacyOutputEncoding, options);
      }
      const inputFailure = this.writeChildInput(spawnedChild, request.stdin);

      const exitState = await exitPromise;
      if (!file) {
        await this.drainChildOutput(
          spawnedChild,
          closePromise,
          internalOptions,
          terminationRequested,
        );
        await Promise.all([stdout!.close(), stderr!.close()]);
      } else if (exitState.error) {
        await file.discard();
      }
      const stdoutResult = file
        ? await this.readBashResult(file, request, internalOptions, exitState, outputLimitExceeded)
        : stdout!.result();
      const stderrResult = useBashMergedOutput
        ? { text: "", bytes: 0, truncated: false }
        : stderr!.result();
      const completedAt = new Date();
      if (file && outputLimitExceeded) cancelled = true;
      const baseStatus = this.statusFromExit(exitState, timedOut, cancelled, outputLimitExceeded);
      const stdinWriteError = inputFailure();
      const unexpectedStdinFailure =
        baseStatus === "completed" &&
        stdinWriteError &&
        !isExpectedChildStdinClosureError(stdinWriteError)
          ? this.toFailure("unknown", stdinWriteError)
          : undefined;
      const failure = exitState.error
        ? this.toFailure("spawn_error", exitState.error)
        : file && outputLimitExceeded
          ? { type: "output_limit" as const, message: "Command killed: output file exceeded 5GB" }
          : (this.statusFailure(timedOut, cancelled, outputLimitExceeded, timeoutMs) ??
            unexpectedStdinFailure);
      const status = unexpectedStdinFailure ? "failed" : baseStatus;
      const resolvedCwd =
        status === "completed" && exitState.code === 0
          ? readCapturedCwd(cwdFilePath, { dialect: cwdDialect })
          : undefined;
      const result = this.createResult({
        status,
        startedAt,
        completedAt,
        pid: spawnedChild.pid,
        exitCode: exitState.code,
        signal: exitState.signal,
        stdout: stdoutResult,
        stderr: stderrResult,
        timedOut,
        cancelled,
        error: failure,
        resolvedCwd,
      });

      this.emitResult(options, result);
      return result;
    } catch (error) {
      if (!child) await file?.discard();
      if (stopRequested || this.closePromise || options.signal?.aborted) {
        return createPreSpawnStoppedResult();
      }
      const failure = this.toFailure("spawn_error", error);
      const result = this.createResult({
        status: "spawn_error",
        startedAt,
        completedAt: new Date(),
        stdout: file ? await file.result(this.bashInlineLimit(request)) : stdout!.result(),
        stderr: useBashMergedOutput ? { text: "", bytes: 0, truncated: false } : stderr!.result(),
        timedOut: false,
        cancelled: false,
        error: failure,
      });
      this.emitResult(options, result);
      return result;
    } finally {
      if (!exited)
        finishResourceTelemetry({ error: new Error("Execution ended before child exit") });
      executionSettled = true;
      file?.stopWatching();
      if (internalOptions.bashLifecycle) internalOptions.bashLifecycle.onBackgrounded = undefined;
      await file?.close();
      if (timeoutTimer) clearTimeout(timeoutTimer);
      if (progressTimer) clearInterval(progressTimer);
      if (forceExitTimer) clearTimeout(forceExitTimer);
      options.signal?.removeEventListener("abort", abortHandler);
      this.completeActiveExecution(executionId);
    }
  }
}
