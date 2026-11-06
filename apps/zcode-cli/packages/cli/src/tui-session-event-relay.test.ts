import { describe, expect, it, vi } from "bun:test";
import { createTuiSessionEventRelay } from "./tui-session-event-relay.js";
import type { SessionEvent } from "@zcode/contracts";

describe("createTuiSessionEventRelay", () => {
  it("attaches when first sink is added and detaches when last sink removed", () => {
    const subscribe = vi.fn(() => () => {});
    const relay = createTuiSessionEventRelay({
      currentRuntime: () => ({ id: 1 }),
      readSubscriber: () => subscribe,
    });

    expect(relay.isAttached()).toBe(false);
    const unsubscribe = relay.addSink(() => {});
    expect(subscribe).toHaveBeenCalledTimes(1);
    expect(relay.isAttached()).toBe(true);

    unsubscribe();
    expect(relay.isAttached()).toBe(false);
  });

  it("fans out events to all sinks and handles sink removal during traversal", () => {
    const sinks: Array<(event: SessionEvent) => void> = [];
    const unsubscribeSubscribe = vi.fn();
    const subscribe = vi.fn((sink: { onSessionEvent: (event: SessionEvent) => void }) => {
      sinks.push(sink.onSessionEvent);
      return unsubscribeSubscribe;
    });

    const relay = createTuiSessionEventRelay({
      currentRuntime: () => ({ id: 1 }),
      readSubscriber: () => subscribe,
    });

    const events: SessionEvent[] = [];
    const unsubscribe1 = relay.addSink((event) => events.push(event));
    const unsubscribe2 = relay.addSink((event) => {
      events.push(event);
      unsubscribe1(); // remove during traversal
    });

    const event = { type: "test" } as unknown as SessionEvent;
    sinks[0]?.(event);
    expect(events).toHaveLength(2);
    expect(unsubscribeSubscribe).not.toHaveBeenCalled();

    // After removal, only remaining sink should receive
    events.length = 0;
    sinks[0]?.(event);
    expect(events).toHaveLength(1);
  });

  it("reattaches on replaceApp and reuses current runtime", () => {
    const subscribe1 = vi.fn(() => () => {});
    const subscribe2 = vi.fn(() => () => {});
    let runtime = { id: 1 };
    const relay = createTuiSessionEventRelay({
      currentRuntime: () => runtime,
      readSubscriber: (r) => (r === runtime ? subscribe1 : subscribe2),
    });

    relay.addSink(() => {});
    expect(subscribe1).toHaveBeenCalledTimes(1);
    expect(subscribe2).not.toHaveBeenCalled();

    runtime = { id: 2 };
    relay.reattach();
    expect(subscribe1).toHaveBeenCalledTimes(1); // old detached
    expect(subscribe2).toHaveBeenCalledTimes(1);
  });

  it("does not attach when no sinks", () => {
    const subscribe = vi.fn(() => () => {});
    const relay = createTuiSessionEventRelay({
      currentRuntime: () => ({}),
      readSubscriber: () => subscribe,
    });
    relay.reattach();
    expect(subscribe).not.toHaveBeenCalled();
    expect(relay.isAttached()).toBe(false);
  });
});
