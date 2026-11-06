import {
  SessionEventType,
  createPartId,
  createToolCallId,
  type MessageId,
} from "../deps.js";
import type { AgentRuntimeInternal } from "../internal.js";
import { consumeBrowserTurnState } from "../../repl/browser-turn-state.js";
import {
  createToolResultDisplay,
  MAX_NODE_REPL_DISPLAY_IMAGE_BASE64_BYTES,
} from "../../tool/executor/result-display.js";
import { HOST_NODE_REPL_IMAGE_MAX_DIMENSION } from "../../mcp/image-normalization.js";
import type { RegularTurnLoopState } from "./turn-loop-state.js";

const BROWSER_TURN_SCREENSHOT_TOOL_NAME = "mcp__node_repl__js";
const BROWSER_TURN_SCREENSHOT_MAX_RAW_BYTES = Math.floor(
  (MAX_NODE_REPL_DISPLAY_IMAGE_BASE64_BYTES * 3) / 4,
);

function createBrowserTurnScreenshotDisplay(image: {
  base64: string;
  mimeType: string;
}) {
  const display = createToolResultDisplay(BROWSER_TURN_SCREENSHOT_TOOL_NAME, {
    images: [image],
  });
  if (display?.kind !== "node_repl_images") return undefined;
  return { ...display, source: "browser_turn_end" };
}

async function appendScreenshotEvent(
  runtime: AgentRuntimeInternal,
  state: RegularTurnLoopState,
  type: SessionEventType,
  payload: unknown,
): Promise<void> {
  const event = runtime.createEvent(type, payload, state.turnTraceContext);
  await runtime.appendEvent(event, state.turnTraceContext);
  state.events.push(event);
}

export async function appendBrowserTurnScreenshot(
  runtime: AgentRuntimeInternal,
  state: RegularTurnLoopState,
  assistantMessageId: MessageId,
): Promise<void> {
  const turnState = consumeBrowserTurnState(runtime.sessionId, state.turnId);
  if (!turnState?.candidate) return;
// Explicit emitImage only represents that the image has been generated during tool execution and cannot replace the active tab at the end of the round.
// Final state screenshot; skipping the whole round will result in "open the page and screenshot" real Browser usage without the end of the round.
  const port = runtime.browserControlPort;
  if (!port) return;

  const { browserGeneration, browserId } = turnState.candidate;
  try {
    const listed = await port.execute({
      browserGeneration,
      browserId,
      sessionId: runtime.sessionId,
      turnId: String(state.turnId),
      command: { method: "list" },
      traceContext: state.turnTraceContext,
      signal: state.turnAbortSignal,
    });
    const activeTab = listed.ok
      ? listed.tabs?.find((tab) => tab.active === true)
      : undefined;
    if (!activeTab) {
      runtime.logger?.debug(
        "Browser turn screenshot skipped because no active tab is available",
        {
          event: "browser.turn_screenshot.skipped_no_active_tab",
          module: "core.runtime",
          turnId: String(state.turnId),
        },
      );
      return;
    }

    const captured = await port.execute({
      browserGeneration,
      browserId,
      sessionId: runtime.sessionId,
      turnId: String(state.turnId),
      command: { method: "screenshot", tabId: activeTab.tabId },
      traceContext: state.turnTraceContext,
      signal: state.turnAbortSignal,
    });
    let displayImage: { base64: string; mimeType: string } | undefined = captured.image;
    if (
      captured.ok &&
      displayImage &&
      Buffer.byteLength(displayImage.base64, "utf8") >
        MAX_NODE_REPL_DISPLAY_IMAGE_BASE64_BYTES &&
      runtime.imageProcessorPort
    ) {
// Raw PNGs for complex pages often exceed the Node REPL's 200 KiB display budget; directly
// After being handed over to the display constructor, it is silently discarded, which means it can be displayed in the first round, but no screenshots are taken in subsequent rounds of complex pages.
// Here, the unified image processing side compression is reused without relaxing the persistence budget or reinjecting the image into the model context.
      const prepared = await runtime.imageProcessorPort.prepareForModel(
        {
          data: Buffer.from(displayImage.base64, "base64"),
          maxBase64Bytes: MAX_NODE_REPL_DISPLAY_IMAGE_BASE64_BYTES,
          maxDimension: HOST_NODE_REPL_IMAGE_MAX_DIMENSION,
          maxRawBytes: BROWSER_TURN_SCREENSHOT_MAX_RAW_BYTES,
          mediaType: displayImage.mimeType,
          trace: state.turnTraceContext,
        },
        { signal: state.turnAbortSignal },
      );
      displayImage = {
        base64: Buffer.from(prepared.data).toString("base64"),
        mimeType: prepared.mediaType || displayImage.mimeType,
      };
    }
    const display =
      captured.ok && displayImage
        ? createBrowserTurnScreenshotDisplay(displayImage)
        : undefined;
    if (!display) {
      runtime.logger?.warn(
        "Browser turn screenshot capture returned no displayable image",
        {
          errorCode: captured.error?.code,
          event: "browser.turn_screenshot.capture_failed",
          module: "core.runtime",
          turnId: String(state.turnId),
        },
      );
      return;
    }

    const toolCallId = createToolCallId();
    const partId = createPartId();
    const timestamp = Date.now();
    const input = { source: "browser_turn_end" };
    await runtime.persistPart(
      {
        id: partId,
        sessionID: runtime.sessionId,
        messageID: assistantMessageId,
        type: "tool",
        callID: toolCallId,
        tool: BROWSER_TURN_SCREENSHOT_TOOL_NAME,
        state: {
          status: "completed",
          input,
          output: "",
          title: BROWSER_TURN_SCREENSHOT_TOOL_NAME,
          metadata: { schemaVersion: 1, display },
          time: { start: timestamp, end: timestamp },
        },
      },
      state.turnTraceContext,
    );

    await appendScreenshotEvent(
      runtime,
      state,
      SessionEventType.ToolCallScheduled,
      {
        toolCallId,
        assistantMessageId,
        toolName: BROWSER_TURN_SCREENSHOT_TOOL_NAME,
        input,
        dependencies: [],
        canRunParallel: false,
        schedule: {
          parallelGroups: [[toolCallId]],
          executionOrder: [toolCallId],
        },
      },
    );
    await appendScreenshotEvent(
      runtime,
      state,
      SessionEventType.ToolCallStarted,
      {
        toolCallId,
        toolName: BROWSER_TURN_SCREENSHOT_TOOL_NAME,
        startedAt: new Date(timestamp),
      },
    );
    await appendScreenshotEvent(
      runtime,
      state,
      SessionEventType.ToolCallResult,
      {
        toolCallId,
        result: { success: true, content: "", display },
        duration: 0,
      },
    );
    runtime.logger?.debug("Browser turn screenshot appended", {
      event: "browser.turn_screenshot.appended",
      module: "core.runtime",
      turnId: String(state.turnId),
    });
  } catch (error) {
// Automatic screenshots are display enhancements. Failure cannot overwrite successfully completed model rounds.
    runtime.logger?.warn("Browser turn screenshot failed", {
      error: error instanceof Error ? error.message : String(error),
      event: "browser.turn_screenshot.failed",
      module: "core.runtime",
      turnId: String(state.turnId),
    });
  }
}
