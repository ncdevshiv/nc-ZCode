/**
 * Renderer-side browser recording port.
 *
 * The Tauri child webview is a native layer, so the renderer cannot use
 * `captureStream` on it directly. Instead it polls `Page.captureScreenshot`
 * through the shell's CDP bridge, draws each frame into an offscreen canvas and
 * records the canvas with `MediaRecorder` (WebM/VP8 or VP9). The resulting bytes
 * are written to the workspace by `saveBrowserRecording`.
 *
 * Unsupported inputs fail explicitly:
 * - remote workspaces are rejected (no local write target);
 * - platforms without `browserViewCdp`/`saveBrowserRecording`/`MediaRecorder`
 *   return `null`, so the lifecycle controller reports `capability_unsupported`.
 */
import type { BrowserRecordingOptions } from "@zcode/shared";
import {
  cursorCleanupScript,
  cursorOverlayScript,
  executeRecordingActions,
  RecordingCancelledError,
} from "@/browser-use/browserUseRecordingActions.js";

export interface BrowserUseRecordingPlatform {
  browserViewCdp?: (payload: {
    key: string;
    method: string;
    params?: Record<string, unknown>;
  }) => Promise<unknown>;
  saveBrowserRecording?: (payload: {
    workspacePath: string;
    relativePath: string;
    data: ArrayBuffer;
  }) => Promise<string>;
}

export interface BrowserUseRecordingPort {
  start(input: {
    jobId: string;
    tabId: string;
    options: BrowserRecordingOptions;
    onPhase: (phase: "capturing" | "finalizing", progress: number) => void;
    onComplete: (result: {
      data: Uint8Array;
      width: number;
      height: number;
      fps: number;
      durationMs: number;
      frameCount: number;
    }) => void;
    onError: (error: string) => void;
  }): void;
  cancel(jobId: string): void;
  write(input: {
    jobId: string;
    relativePath: string;
    data: Uint8Array;
  }): Promise<string>;
}

interface ActiveCapture {
  cancelled: boolean;
  stopping: boolean;
  stop: () => void;
}

const captures = new Map<string, ActiveCapture>();

function decodeBase64(value: string): Uint8Array {
  const binary = atob(value);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index);
  }
  return bytes;
}

function toArrayBuffer(bytes: Uint8Array): ArrayBuffer {
  return bytes.buffer.slice(
    bytes.byteOffset,
    bytes.byteOffset + bytes.byteLength,
  ) as ArrayBuffer;
}

function pickMimeType(): string {
  const candidates = [
    "video/webm;codecs=vp9",
    "video/webm;codecs=vp8",
    "video/webm",
  ];
  return (
    candidates.find((candidate) => MediaRecorder.isTypeSupported(candidate)) ??
    "video/webm"
  );
}

function sleep(milliseconds: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, milliseconds);
  });
}

export function createBrowserUseRecordingPort(input: {
  platform: BrowserUseRecordingPlatform;
  workspacePath: string;
  remoteSessionId?: string | null;
}): BrowserUseRecordingPort | null {
  const { platform, workspacePath, remoteSessionId } = input;
  if (
    !workspacePath.trim() ||
    remoteSessionId ||
    !platform.browserViewCdp ||
    !platform.saveBrowserRecording ||
    typeof document === "undefined" ||
    typeof MediaRecorder === "undefined" ||
    typeof HTMLCanvasElement === "undefined"
  ) {
    return null;
  }
  const browserViewCdp = platform.browserViewCdp;
  const saveBrowserRecording = platform.saveBrowserRecording;

  return {
    start({ jobId, tabId, options, onPhase, onComplete, onError }) {
      const fps = Math.min(60, Math.max(1, options.fps ?? 25));
      const maxDurationMs = Math.min(
        90_000,
        Math.max(1_000, options.maxDurationMs ?? 60_000),
      );
      const settleMs = Math.max(0, options.settleMs ?? 300);
      const jpegQuality = Math.min(100, Math.max(1, options.jpegQuality ?? 80));
      const requestedViewport = options.viewport;
      const actions = options.actions ?? [];
      const showCursor = options.showCursor !== false;
      const cdp = (method: string, params?: Record<string, unknown>) =>
        browserViewCdp({ key: tabId, method, params });
      let cancelled = false;
      let stopping = false;
      let interval: ReturnType<typeof setInterval> | undefined;
      let timeout: ReturnType<typeof setTimeout> | undefined;
      let captureInFlight = false;
      let frameCount = 0;
      let canvas: HTMLCanvasElement | undefined;
      let stream: MediaStream | undefined;
      let recorder: MediaRecorder | undefined;
      let viewportOverride = false;
      let cursorInstalled = false;
      let width = requestedViewport?.width ?? 0;
      let height = requestedViewport?.height ?? 0;
      let startedAt = Date.now();

      const removeCursorOverlay = () => {
        if (!cursorInstalled) return;
        cursorInstalled = false;
        void cdp("Runtime.evaluate", {
          expression: cursorCleanupScript(),
          returnByValue: true,
        }).catch(() => undefined);
      };

      const stop = () => {
        if (stopping) return;
        stopping = true;
        if (interval) clearInterval(interval);
        if (timeout) clearTimeout(timeout);
        interval = undefined;
        timeout = undefined;
        removeCursorOverlay();
        if (recorder && recorder.state !== "inactive") {
          recorder.stop();
        }
        for (const track of stream?.getTracks() ?? []) track.stop();
        if (viewportOverride) {
          viewportOverride = false;
          void browserViewCdp({
            key: tabId,
            method: "Emulation.clearDeviceMetricsOverride",
          }).catch(() => undefined);
        }
      };
      const fail = (error: unknown) => {
        if (cancelled) return;
        cancelled = true;
        stop();
        captures.delete(jobId);
        onError(error instanceof Error ? error.message : String(error));
      };
      const active: ActiveCapture = {
        cancelled: false,
        stopping: false,
        stop: () => {
          cancelled = true;
          active.cancelled = true;
          stop();
        },
      };
      captures.set(jobId, active);

      void (async () => {
        try {
          if (requestedViewport) {
            if (
              !Number.isInteger(requestedViewport.width) ||
              !Number.isInteger(requestedViewport.height) ||
              requestedViewport.width <= 0 ||
              requestedViewport.height <= 0
            ) {
              throw new Error("recording viewport must be positive integers");
            }
            await browserViewCdp({
              key: tabId,
              method: "Emulation.setDeviceMetricsOverride",
              params: {
                width: requestedViewport.width,
                height: requestedViewport.height,
                deviceScaleFactor: 1,
                mobile: false,
              },
            });
            viewportOverride = true;
          }
          if (settleMs > 0) await sleep(settleMs);
          if (cancelled) return;

          const first = (await browserViewCdp({
            key: tabId,
            method: "Page.captureScreenshot",
            params: { format: "jpeg", quality: jpegQuality, fromSurface: true },
          })) as { data?: unknown };
          if (typeof first?.data !== "string") {
            throw new Error("recording capture returned no image data");
          }
          const firstBytes = decodeBase64(first.data);
          const bitmap = await createImageBitmap(
            new Blob([toArrayBuffer(firstBytes)], { type: "image/jpeg" }),
          );
          width = requestedViewport?.width ?? bitmap.width;
          height = requestedViewport?.height ?? bitmap.height;
          canvas = document.createElement("canvas");
          canvas.width = width;
          canvas.height = height;
          const context = canvas.getContext("2d");
          if (!context) throw new Error("recording canvas 2d context is unavailable");
          context.drawImage(bitmap, 0, 0, width, height);
          bitmap.close();
          frameCount = 1;

          stream = canvas.captureStream(fps);
          const mimeType = pickMimeType();
          const chunks: BlobPart[] = [];
          recorder = new MediaRecorder(stream, {
            mimeType,
            videoBitsPerSecond: 2_500_000,
          });
          recorder.ondataavailable = (event) => {
            if (event.data.size > 0) chunks.push(event.data);
          };
          recorder.onstop = () => {
            removeCursorOverlay();
            if (cancelled) return;
            onPhase("finalizing", 0.9);
            const blob = new Blob(chunks, { type: mimeType });
            void blob
              .arrayBuffer()
              .then((buffer) => {
                if (cancelled) return;
                captures.delete(jobId);
                onComplete({
                  data: new Uint8Array(buffer),
                  width,
                  height,
                  fps,
                  durationMs: Math.max(0, Date.now() - startedAt),
                  frameCount,
                });
              })
              .catch(fail);
          };
          recorder.start();
          startedAt = Date.now();
          if (showCursor) {
            const result = (await cdp("Runtime.evaluate", {
              expression: cursorOverlayScript(),
              returnByValue: true,
            })) as {
              exceptionDetails?: { text?: string; exception?: { description?: string } };
            };
            if (result?.exceptionDetails) {
              throw new Error(
                result.exceptionDetails.exception?.description ??
                  result.exceptionDetails.text ??
                  "recording cursor overlay failed",
              );
            }
            cursorInstalled = true;
          }
          onPhase("capturing", 0.1);

          const captureFrame = async () => {
            if (cancelled || stopping || captureInFlight) return;
            captureInFlight = true;
            try {
              const frame = (await browserViewCdp({
                key: tabId,
                method: "Page.captureScreenshot",
                params: {
                  format: "jpeg",
                  quality: jpegQuality,
                  fromSurface: true,
                },
              })) as { data?: unknown };
              if (typeof frame?.data !== "string") {
                throw new Error("recording capture returned no image data");
              }
              const frameBitmap = await createImageBitmap(
                new Blob([toArrayBuffer(decodeBase64(frame.data))], { type: "image/jpeg" }),
              );
              context.drawImage(frameBitmap, 0, 0, width, height);
              frameBitmap.close();
              frameCount += 1;
            } catch (error) {
              fail(error);
            } finally {
              captureInFlight = false;
            }
          };
          interval = setInterval(
            () => void captureFrame(),
            Math.max(20, Math.floor(1000 / fps)),
          );
          if (actions.length > 0) {
            void executeRecordingActions(actions, {
              cdp,
              isCancelled: () => cancelled || stopping,
              sleep,
              viewport: { width, height },
            })
              .then(async () => {
                await sleep(300);
                if (!cancelled && !stopping) stop();
              })
              .catch((error) => {
                if (error instanceof RecordingCancelledError || cancelled) return;
                fail(error);
              });
            timeout = setTimeout(
              () => fail(new Error("browser recording timed out")),
              maxDurationMs,
            );
          } else {
            timeout = setTimeout(stop, maxDurationMs);
          }
        } catch (error) {
          fail(error);
        }
      })();
    },
    cancel(jobId) {
      const capture = captures.get(jobId);
      if (!capture) return;
      capture.cancelled = true;
      capture.stop();
      captures.delete(jobId);
    },
    async write({ relativePath, data }) {
      return saveBrowserRecording({
        workspacePath,
        relativePath,
        data: data.buffer.slice(
          data.byteOffset,
          data.byteOffset + data.byteLength,
        ) as ArrayBuffer,
      });
    },
  };
}
