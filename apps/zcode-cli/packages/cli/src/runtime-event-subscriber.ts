// Read the ability of "cross-turn session event subscription" on the runtime.
//
// It is a separate module because the consumer has two different venues: headless (`prompt-command.ts`) and TUI
// (`tui-prompt-handler.ts` → `tui-session-event-relay.ts`). Capability readers need to be independent of any
// venue: If placed in the headless exclusive module, the TUI side has to import one from a module named headless.
// Reader named Headless - the name lies about the behavior. It only answers "Can this runtime be subscribed across rounds?"
import type { SessionEvent } from "@zcode/contracts";

/**
* Dynamically read a function member from `unknown`.
 *
* Collect `unknown` and dynamically detect attributes, **not** to defend against the real `AgentRuntime` (these members above are
* required), but because `RunDependencies.createZCodeApp` is a public injection point (comment in cli-types.ts
* written "tests, embedders"), the runtime of the substitute can be of any shape. Writing `?.` to a required member will result in TS
* It is judged as a constant truth condition - so the boundary is here, expressed by an explicit dynamic reading.
 */
export const readRuntimeFunction = (
  source: unknown,
  key: string,
): ((...args: never[]) => unknown) | undefined => {
  if (!source || typeof source !== "object") return undefined;
  const value = (source as Record<string, unknown>)[key];
  return typeof value === "function" ? (value as (...args: never[]) => unknown) : undefined;
};

type RuntimeEventSubscriber = (sink: {
  onSessionEvent: (event: SessionEvent) => void;
}) => () => void;

/**
* Read out the runtime's cross-turn event subscription.
 *
* **No silent downgrade**: If the subscription cannot be obtained, it returns `undefined`, and the caller returns per-turn `onEvent` (visible in a single round,
* Same as before the change). This is an honest answer of "this host does not have this ability", rather than pretending to be subscribed.
 */
export const readRuntimeEventSubscriber = (
  runtime: unknown,
): RuntimeEventSubscriber | undefined => {
  const subscribe = readRuntimeFunction(runtime, "subscribeEvents");
  if (!subscribe) return undefined;
  return (sink) => {
    const detach = (subscribe as (s: unknown) => unknown).call(runtime, sink);
    return typeof detach === "function" ? (detach as () => void) : () => undefined;
  };
};
