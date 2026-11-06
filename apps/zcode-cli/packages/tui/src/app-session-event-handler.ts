import type { SessionEvent } from "@zcode/contracts";
import type { TuiCopy } from "@zcode/i18n";
import React from "react";
import { applySessionEventToState } from "./app-events.js";
import { describeSessionEvent } from "./state.js";
import { isSubagentToolMirror } from "./app-subagent-events.js";

type SessionEventHandlers = Parameters<typeof applySessionEventToState>[1];

/**
* Memory window for applied event id.
 *
* Why it is needed: Cross-turn resident subscription and per-turn `onEvent` will cast the same event at the same time. The mirror itself
* Duplicate delivery immunity (the shared reducer returns null for the same event that has just been reduced), but the translation is not——
* `assistant_message` and the like are unconditional appends, and double-casting will cause the reply to appear twice on the screen.
 *
* Bounded is enough: the two sinks are triggered successively on the same emit, and the repetitions are always close to each other, and there is no need to remember the entire session.
 */
const MAX_REMEMBERED_EVENT_IDS = 2_048;

/**
* Only events from the **main session** are transcribed.
 *
* Why filtering is necessary: ​​The original event of actor/child session will be thrown through `notifyExternalChildSessionEvent`
* **The same external sink set** of the parent runtime, and **retains the child's own sessionId** (the protocol layer accordingly
* detached live session routed to the desktop). TUI's sink is hung on that collection, as is per-turn onEvent.
* Therefore, without filtering, the actor's streaming increment, tool call, submit_result, and turn_complete will all be drawn into the main transcription.
 *
* The dwf progress event itself is a **parent session** event (`session.events.ts:134` "Append to parent session"), so the tool is stuck
* Completion rounds with post-settlement are not affected - they are still the only two surfaces for workflow on TUI.
 *
* Release when the main sessionId cannot be obtained: It is better to render a little more than to let the whole page be transcribed just because an id is missing.
 */
function isMainSessionEvent(event: SessionEvent, mainSessionId: string | undefined): boolean {
  // Tool mirrors deliberately carry the parent's sessionId. They are activity
  // metadata, not parent transcript/tool/usage facts.
  if (isSubagentToolMirror(event)) return false;
  if (!mainSessionId) return true;
  const eventSessionId = typeof event.sessionId === "string" ? event.sessionId : undefined;
  if (eventSessionId === undefined) return true;
  return eventSessionId === mainSessionId;
}

type SessionEventApplierInput = SessionEventHandlers & {
  subscribeSessionEvents?: (sink: (event: SessionEvent) => void) => () => void;
  observeSessionEvent?: (event: SessionEvent) => void;
  copy: TuiCopy;
  getMainSessionId?: () => string | undefined;
  setLastEvent: (event: string) => void;
};

/**
* Complete ingress pipeline: session gate → deduplication → apply to state.
 *
* Exporting it as a pure function (seen-set is held by the caller) instead of just leaving it in the hook is to make the **effect** measurable:
* "The actor's turn_complete will not be appended to the main transcription" This kind of regression can only be proven at the pipeline level.
* Cannot be proven at the predicate level. Returns whether it was actually applied.
 *
* Gates are ranked before deduplication: external events should not occupy the deduplication window (the window is bounded and is crowded out by actor events
* will allow duplicate submissions from the main session to be missed).
 */
function applyMainSessionEvent(
  event: SessionEvent,
  applied: Set<string>,
  input: SessionEventApplierInput,
): boolean {
  if (!isMainSessionEvent(event, input.getMainSessionId?.())) return false;
  if (!rememberSessionEvent(applied, event)) return false;
  input.setLastEvent(describeSessionEvent(event));
  applySessionEventToState(event, input, input.copy);
  return true;
}

export function useSessionEventApplier(
  input: SessionEventApplierInput,
): (event: SessionEvent) => void {
  const appliedEventIdsRef = React.useRef<Set<string>>(new Set());
  const applyEvent = React.useCallback(
    (event: SessionEvent) => {
      input.observeSessionEvent?.(event);
      applyMainSessionEvent(event, appliedEventIdsRef.current, input);
    },
    [input],
  );
  useSessionEventSubscription(input.subscribeSessionEvents, applyEvent);
  return applyEvent;
}

/** A stable subscription survives renders; both event sources share the applier. */
function useSessionEventSubscription(
  subscribe: ((sink: (event: SessionEvent) => void) => () => void) | undefined,
  applyEvent: (event: SessionEvent) => void,
): void {
  const latest = React.useRef(applyEvent);
  latest.current = applyEvent;
  React.useEffect(() => subscribe?.((event) => latest.current(event)), [subscribe]);
}

/**
* Note this event; return false to indicate it has already been applied (callers should skip it entirely).
 *
* Events without id are all allowed: It is better to render it again than to swallow the entire event just because a key is missing.
 */
function rememberSessionEvent(applied: Set<string>, event: SessionEvent): boolean {
  const eventId = typeof event.id === "string" && event.id.length > 0 ? event.id : undefined;
  if (eventId === undefined) return true;
  if (applied.has(eventId)) return false;
  applied.add(eventId);
  if (applied.size > MAX_REMEMBERED_EVENT_IDS) {
// Set maintains insertion order, with the oldest batch coming out first.
    const excess = applied.size - MAX_REMEMBERED_EVENT_IDS;
    let removed = 0;
    for (const id of applied) {
      applied.delete(id);
      removed += 1;
      if (removed >= excess) break;
    }
  }
  return true;
}
