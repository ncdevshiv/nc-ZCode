import { LOCAL_TTFT_MAX_PENDING, localTtftNow, type LocalTtftDetail } from "@zcode/shared";

type PreparationStage =
  | Exclude<LocalTtftDetail["stage"], "attempt" | "retry_wait" | "user_confirmation">
  | "execution";
export interface LocalTurnPreparationFact {
  sessionId: string;
  turnId: string;
  id: string;
  stage: PreparationStage;
  start: number;
  end?: number;
  outcome?: "completed" | "failed" | "cancelled";
}
type Subscription = { publish: (fact: LocalTurnPreparationFact) => void; sequence: number };
//Only save observation callbacks, not business inputs or stage facts; CLI recorder exclusive facts and subscription life cycle.
const subscriptions = new Map<string, Subscription>();
const noop = () => {};
export function observeLocalTurnPreparation(
  inputId: string,
  publish: Subscription["publish"],
): () => void {
  if (subscriptions.size >= LOCAL_TTFT_MAX_PENDING || subscriptions.has(inputId)) return noop;
  const subscription = { publish, sequence: 0 };
  subscriptions.set(inputId, subscription);
  return () => {
    if (subscriptions.get(inputId) === subscription) subscriptions.delete(inputId);
  };
}
export function beginLocalTurnPreparation(
  trace: { queryId?: string; sessionId?: string; turnId?: string },
  stage: PreparationStage,
): (outcome?: LocalTurnPreparationFact["outcome"]) => void {
  const subscription = trace.queryId ? subscriptions.get(trace.queryId) : undefined;
  if (!subscription || !trace.sessionId || !trace.turnId) return noop;
  const fact: LocalTurnPreparationFact = {
    sessionId: trace.sessionId,
    turnId: trace.turnId,
    id: `prepare:${subscription.sequence++}`,
    stage,
    start: localTtftNow(),
  };
  const publish = (value: LocalTurnPreparationFact) => {
// Failure to observe the callback must not change Core's return value or exception; no additional acquisition tool loop will be required after the first output is unbound.
    if (subscriptions.get(trace.queryId!) !== subscription) return;
    try {
      subscription.publish(value);
    } catch {
/* Telemetry does not participate in business decisions. */
    }
  };
  publish(fact);
  let ended = false;
  return (outcome = "completed") => {
    if (ended) return;
    ended = true;
    publish({ ...fact, end: localTtftNow(), outcome });
  };
}
