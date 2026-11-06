// sessions-index topic publisher: done on top of SessionsIndexProjection
// seq interval accounting + snapshot/delta frame construction + single subscription per connection (resubscription replaces the old generation).
//Simpler than ConversationTopicPublisher: index delta does not do profile filtering (list summary is consistent for all clients),
// conflation has been completed within projection (same as sessionId override); replay buffer is bounded,
// Overflow/interruption degenerates into snapshot (equivalent to resume transmission under conflated semantics).
import type {
  ConversationSnapshot,
  SessionsIndexDelta,
  SessionsIndexTopicFrame,
  TopicFrameDeliveryKind,
} from "@zcode/shared/zcode-protocol-v4";
import { sessionsIndexTopic } from "@zcode/shared/zcode-protocol-v4";
import {
  SessionsIndexProjection,
  type SessionSummaryDeriveExtra,
} from "./sessions-index-projection.js";
import type { TopicFrameReservation } from "./topic-frame-reservation.js";

interface IndexSubscription {
  subscriptionId: string;
  connectionId: string;
/** Next frame fromSeq ((fromSeq, toSeq] semantics). */
  sentSeq: number;
  inFlight: TopicFrameReservation<SessionsIndexTopicFrame> | null;
  nextLogicalFrameOrdinal: number;
}

interface SessionsIndexSubscribeResult {
  subscriptionId: string;
  mode: "snapshot" | "resume";
  frame: SessionsIndexTopicFrame | null;
  reservation: TopicFrameReservation<SessionsIndexTopicFrame> | null;
  rollback(): boolean;
}

interface SessionsIndexResyncRequest {
  base: { logEpoch: string; seq: number } | null;
  forceSnapshot?: boolean;
}

export class SessionsIndexPublisher {
  private readonly projection: SessionsIndexProjection;
  private currentSeq = 0;
/** (seq, delta) Bounded replay buffer: only retain the most recent frames for resume; overflow degrades to snapshot. */
  private readonly deltaLog: Array<{ seq: number; delta: SessionsIndexDelta }> = [];
  private readonly maxDeltaLog = 512;
  private readonly subscriptions = new Map<string, IndexSubscription>();
  private readonly subscriptionIdByConnection = new Map<string, string>();
  private nextSubscriptionSerial = 1;
  private nextLogicalFrameSerial = 1;

  constructor(
    readonly workspaceId: string,
    readonly logEpoch: string,
    private readonly now: () => number = Date.now,
  ) {
    this.projection = new SessionsIndexProjection(workspaceId, logEpoch);
  }

  private get topic(): string {
    return sessionsIndexTopic(this.workspaceId);
  }

/** Cold start seed: directly put a known summary (store session, no live projection). */
  seed(summary: Parameters<SessionsIndexProjection["seed"]>[0]): void {
    this.projection.seed(summary);
  }

/** Migration reread: Complete the missing store digest for the first cold seed and generate normal delta for online subscriptions. */
  mergeMissingStoredSummaries(
    summaries: readonly Parameters<SessionsIndexProjection["seed"]>[0][],
  ): boolean {
    return this.record(
      summaries.flatMap((summary) => this.projection.insertSeedIfMissing(summary)),
    );
  }

/** The latest snapshot of a session is entered → update summary, the generated delta is accounted for and advanced to seq. Returns whether there are any changes. */
  ingestConversation(
    snapshot: ConversationSnapshot,
    extra: Omit<SessionSummaryDeriveExtra, "workspaceId">,
  ): boolean {
    return this.record(this.projection.upsertFromConversation(snapshot, extra));
  }

/** Session removal → remove delta accounting. Returns whether there are any changes. */
  removeSession(sessionId: string): boolean {
    return this.record(this.projection.remove(sessionId));
  }

  private record(deltas: SessionsIndexDelta[]): boolean {
    if (deltas.length === 0) return false;
    for (const delta of deltas) {
      this.currentSeq += 1;
      this.deltaLog.push({ seq: this.currentSeq, delta });
    }
    while (this.deltaLog.length > this.maxDeltaLog) this.deltaLog.shift();
    return true;
  }

  get seq(): number {
    return this.currentSeq;
  }

/** Subscription: resume if base is valid and resumable, otherwise snapshot. Single subscription per connection (replaces old generation). */
  subscribe(
    connectionId: string,
    base?: { logEpoch: string; seq: number },
  ): SessionsIndexSubscribeResult {
    const result = this.subscribeReserved(connectionId, base);
    result.reservation?.commit();
    return result;
  }

  subscribeReserved(
    connectionId: string,
    base?: { logEpoch: string; seq: number },
  ): SessionsIndexSubscribeResult {
    const previousId = this.subscriptionIdByConnection.get(connectionId);
    const previousSubscription = previousId ? this.subscriptions.get(previousId) : undefined;
    if (previousId) this.subscriptions.delete(previousId);
    const subscriptionId = `six-${this.logEpoch}-${this.nextSubscriptionSerial++}`;
    const subscription: IndexSubscription = {
      subscriptionId,
      connectionId,
      sentSeq: 0,
      inFlight: null,
      nextLogicalFrameOrdinal: 1,
    };
    this.subscriptions.set(subscriptionId, subscription);
    this.subscriptionIdByConnection.set(connectionId, subscriptionId);
    const rollback = (): boolean => {
      if (
        subscription.inFlight === null ||
        this.subscriptions.get(subscriptionId) !== subscription ||
        this.subscriptionIdByConnection.get(connectionId) !== subscriptionId
      ) {
        return false;
      }
      this.subscriptions.delete(subscriptionId);
      if (previousId && previousSubscription) {
        this.subscriptions.set(previousId, previousSubscription);
        this.subscriptionIdByConnection.set(connectionId, previousId);
      } else {
        this.subscriptionIdByConnection.delete(connectionId);
      }
      return true;
    };

// resume: base generation + seq falls within the replay buffer interval.
    const canResume =
      base !== undefined && base.logEpoch === this.logEpoch && this.canResumeFrom(base.seq);
    if (canResume) {
      subscription.sentSeq = base.seq;
      const reservation =
        base.seq === this.currentSeq ? null : this.reserveDeltaFrame(subscription, "initial");
      return this.subscribeResult(
        subscriptionId,
        "resume",
        reservation,
        reservation ? rollback : () => false,
      );
    }
    const reservation = this.reserveFrame(
      subscription,
      this.snapshotFrame(subscriptionId),
      "initial",
    );
    return this.subscribeResult(subscriptionId, "snapshot", reservation, rollback);
  }

/** same-sub recovery: Rebuild from client base, do not trust sentSeq. */
  resyncReserved(
    subscriptionId: string,
    request: SessionsIndexResyncRequest,
  ): SessionsIndexSubscribeResult | null {
    const subscription = this.subscriptions.get(subscriptionId);
    if (!subscription) return null;
    const previous = { sentSeq: subscription.sentSeq, inFlight: subscription.inFlight };
    subscription.inFlight = null;
    const base = request.base;
    const canResume =
      !request.forceSnapshot &&
      base !== null &&
      base.logEpoch === this.logEpoch &&
      this.canResumeFrom(base.seq);
    if (canResume) {
      subscription.sentSeq = base.seq;
      const reservation =
        base.seq === this.currentSeq
          ? this.reserveFrame(
              subscription,
              {
                topic: this.topic,
                subscriptionId,
                fromSeq: base.seq,
                toSeq: base.seq,
                sentAt: this.now(),
                payload: { kind: "deltas", deltas: [] },
              },
              "recovery",
            )
          : this.reserveDeltaFrame(subscription, "recovery");
      return this.subscribeResult(
        subscriptionId,
        "resume",
        reservation,
        this.resyncRollback(subscription, reservation, previous),
      );
    }
    subscription.sentSeq = 0;
    const reservation = this.reserveFrame(
      subscription,
      this.snapshotFrame(subscriptionId),
      "recovery",
    );
    return this.subscribeResult(
      subscriptionId,
      "snapshot",
      reservation,
      this.resyncRollback(subscription, reservation, previous),
    );
  }

  private resyncRollback(
    subscription: IndexSubscription,
    reservation: TopicFrameReservation<SessionsIndexTopicFrame>,
    previous: Pick<IndexSubscription, "sentSeq" | "inFlight">,
  ): () => boolean {
    return (): boolean => {
      if (
        this.subscriptions.get(subscription.subscriptionId) !== subscription ||
        subscription.inFlight !== reservation
      ) {
        return false;
      }
      subscription.sentSeq = previous.sentSeq;
      subscription.inFlight = previous.inFlight;
      return true;
    };
  }

  private canResumeFrom(seq: number): boolean {
    if (seq < 0 || seq > this.currentSeq) return false;
    if (seq === this.currentSeq) return true;
    const firstPending = this.deltaLog.find((entry) => entry.seq > seq);
    return firstPending?.seq === seq + 1;
  }

  unsubscribe(subscriptionId: string, connectionId?: string): void {
    const subscription = this.subscriptions.get(subscriptionId);
    if (!subscription) return;
    if (connectionId !== undefined && subscription.connectionId !== connectionId) {
      return;
    }
    this.subscriptions.delete(subscriptionId);
    if (this.subscriptionIdByConnection.get(subscription.connectionId) === subscriptionId) {
      this.subscriptionIdByConnection.delete(subscription.connectionId);
    }
  }

  hasSubscription(subscriptionId: string, connectionId?: string): boolean {
    const subscription = this.subscriptions.get(subscriptionId);
    return Boolean(
      subscription && (connectionId === undefined || subscription.connectionId === connectionId),
    );
  }

  private snapshotFrame(subscriptionId: string): SessionsIndexTopicFrame {
    return {
      topic: this.topic,
      subscriptionId,
      fromSeq: 0,
      toSeq: this.currentSeq,
      sentAt: this.now(),
      payload: { kind: "snapshot", snapshot: this.projection.getSnapshot() },
    };
  }

/** Discharge unsent incremental frames of a subscription; return null if no increment. */
  flush(subscriptionId: string): SessionsIndexTopicFrame | null {
    const reservation = this.reserveFlush(subscriptionId);
    if (!reservation || !reservation.commit()) return null;
    return reservation.frame;
  }

  reserveFlush(subscriptionId: string): TopicFrameReservation<SessionsIndexTopicFrame> | null {
    const subscription = this.subscriptions.get(subscriptionId);
    if (!subscription) return null;
    if (subscription.inFlight) return subscription.inFlight;
    if (subscription.sentSeq >= this.currentSeq) return null;
    return this.reserveDeltaFrame(subscription);
  }

  private reserveDeltaFrame(
    subscription: IndexSubscription,
    deliveryKind: TopicFrameDeliveryKind = "online",
  ): TopicFrameReservation<SessionsIndexTopicFrame> {
    const pending = this.deltaLog.filter((entry) => entry.seq > subscription.sentSeq);
// The replay buffer has discarded some intervals (seq breaks) → degenerated into snapshot (equivalent under conflated semantics).
    if (pending.length === 0 || pending[0]!.seq !== subscription.sentSeq + 1) {
      return this.reserveFrame(
        subscription,
        this.snapshotFrame(subscription.subscriptionId),
        deliveryKind,
      );
    }
    const fromSeq = subscription.sentSeq;
    return this.reserveFrame(
      subscription,
      {
        topic: this.topic,
        subscriptionId: subscription.subscriptionId,
        fromSeq,
        toSeq: this.currentSeq,
        sentAt: this.now(),
        payload: { kind: "deltas", deltas: pending.map((entry) => entry.delta) },
      },
      deliveryKind,
    );
  }

  private reserveFrame(
    subscription: IndexSubscription,
    frame: SessionsIndexTopicFrame,
    deliveryKind: TopicFrameDeliveryKind,
  ): TopicFrameReservation<SessionsIndexTopicFrame> {
    let committed = false;
    const reservation: TopicFrameReservation<SessionsIndexTopicFrame> = {
      deliveryKind,
      logicalFrameId: `${subscription.subscriptionId}-lf-${this.nextLogicalFrameSerial++}`,
      logicalFrameOrdinal: subscription.nextLogicalFrameOrdinal++,
      frame,
      commit: () => {
        if (committed) return true;
        if (
          this.subscriptions.get(subscription.subscriptionId) !== subscription ||
          subscription.inFlight !== reservation
        ) {
          return false;
        }
        subscription.sentSeq = frame.toSeq;
        subscription.inFlight = null;
        committed = true;
        return true;
      },
    };
    subscription.inFlight = reservation;
    return reservation;
  }

  private subscribeResult(
    subscriptionId: string,
    mode: "snapshot" | "resume",
    reservation: TopicFrameReservation<SessionsIndexTopicFrame> | null,
    rollback: () => boolean,
  ): SessionsIndexSubscribeResult {
    return {
      subscriptionId,
      mode,
      reservation,
      rollback,
      get frame() {
        reservation?.commit();
        return reservation?.frame ?? null;
      },
    };
  }

  hasSubscribers(): boolean {
    return this.subscriptions.size > 0;
  }

  subscriptionIds(): string[] {
    return [...this.subscriptions.keys()];
  }

  connectionIdForSubscription(subscriptionId: string): string | null {
    return this.subscriptions.get(subscriptionId)?.connectionId ?? null;
  }
}
