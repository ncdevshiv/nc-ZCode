// v4 interactive response registration form (native, resolveInteraction).
//
// Background: The permissions/AskUserQuestion of conversation-product-protocol is a "reverse request" - CLI
// context.requestClient pushes the request to the client and awaits the response. Old clients respond with RPC RESPONSE
// (resolveClientRequest is closed by server-N id). v4 client does not reply to RPC response, but sends one
// Forward `resolveInteraction` COMMAND (first come, first served, late noop).
//
// This registration table is the convergence point of two response paths: interaction-broker presses the business requestId when initiating a reverse request.
// (= interactionId of v4) Register a deferred and let requestClient race with the deferred;
// The v4 command surface (handlers/interaction-background.ts) gets resolveInteraction and passes through this table
// Resolve corresponds to deferred. Promise.race on the broker side immediately uses v4 response to close and abort the dangling ones.
// Reverse RPC.
//
// Layering: This table only does "find the deferred by id and deliver the response", and does not understand the schema of permission/userInput
// Difference - the broker comes with its own resolve callback when registering (the closure is known to kind), and this table is transparent to the response body.
// Ownership: This file is v4 native infrastructure (put in the v4 directory). Import this file in the old directory (broker/server)
// Legal (the dependency direction only allows old directory → v4 directory).
import { ASK_USER_QUESTION_E2E_CLOCK_SCALE_ENV } from "@zcode/shared";

export type V4InteractionAnswer = {
  optionId?: string;
  freeText?: string;
// (elicitation receipt convergence, with shared command.ts resolveInteraction
// answer synchronization): When the action exists, the broker is accurately mapped according to accept/decline/cancel.
// content directly transfers the content semantics of the old userInput response (multiple questions answers/annotations are lossless).
  action?: "accept" | "decline" | "cancel";
  content?: Record<string, unknown>;
};

const ASK_USER_QUESTION_HIDDEN_GRACE_MS = 60_000;
const ASK_USER_QUESTION_AUTO_RESOLUTION_MS = 300_000;
interface V4InteractionRegistryOptions {
  hiddenGraceMs?: number;
  autoResolutionMs?: number;
  now?: () => number;
}

export function resolveV4InteractionRegistryOptionsFromEnv(
  env: NodeJS.ProcessEnv,
): V4InteractionRegistryOptions | undefined {
  if (env.ZCODE_ENV !== "test") return undefined;
  const rawScale = env[ASK_USER_QUESTION_E2E_CLOCK_SCALE_ENV]?.trim();
  if (!rawScale) return undefined;
  const scale = Number(rawScale);
  if (!Number.isFinite(scale) || scale < 1 || scale > 1_000) {
    throw new Error(`${ASK_USER_QUESTION_E2E_CLOCK_SCALE_ENV} must be between 1 and 1000`);
  }
  return {
    hiddenGraceMs: Math.max(1, Math.round(ASK_USER_QUESTION_HIDDEN_GRACE_MS / scale)),
    autoResolutionMs: Math.max(1, Math.round(ASK_USER_QUESTION_AUTO_RESOLUTION_MS / scale)),
  };
}

export type V4InteractionAutoResolution =
  | {
      state: "hiddenGrace" | "visibleCountdown";
      startedAt: number;
      visibleAt: number;
      deadlineAt: number;
    }
  | {
      state: "snoozed";
      startedAt: number;
      snoozedAt: number;
    };

export interface V4InteractionRegistrationOptions {
  sessionId: string;
  kind: "askUserQuestion" | "other";
  fullAccess?: () => Promise<void>;
  initialAutoResolution?: V4InteractionAutoResolution;
  onAutoResolutionUpdated?: (state: V4InteractionAutoResolution) => void | Promise<void>;
}

interface RegisteredInteraction {
/** Broker-side callback: Map v4 answer to the response corresponding to the schema and resolve the reverse request. */
  resolve: (answer: V4InteractionAnswer) => void;
  options?: V4InteractionRegistrationOptions;
  autoResolution?: V4InteractionAutoResolution;
  /**
* AskUserQuestion timing qualification solidified when registering. It is permanently set to false after being closed, and old issues will not be traced back when reopened.
   */
  fullAccessPending?: Promise<void>;
  autoResolutionEligible: boolean;
  visibleTimer?: ReturnType<typeof setTimeout>;
  deadlineTimer?: ReturnType<typeof setTimeout>;
  token: symbol;
}

export class V4InteractionRegistry {
  private readonly pending = new Map<string, RegisteredInteraction>();
  private readonly queuesBySession = new Map<string, string[]>();
  private readonly hiddenGraceMs: number;
  private readonly autoResolutionMs: number;
  private readonly now: () => number;
  private askUserQuestionAutoResolutionEnabled = true;
  private interactionPreferenceCommandApplied = false;

  constructor(options: V4InteractionRegistryOptions = {}) {
    this.hiddenGraceMs = options.hiddenGraceMs ?? ASK_USER_QUESTION_HIDDEN_GRACE_MS;
    this.autoResolutionMs = options.autoResolutionMs ?? ASK_USER_QUESTION_AUTO_RESOLUTION_MS;
    this.now = options.now ?? Date.now;
  }

  /**
* The broker is registered when initiating a reverse request. Return the logout function (broker calls it in finally, no matter v4 or
* RPC-response closing ports are cleaned to prevent leakage). Same interactionId re-registration (reannounce re-send) coverage
* Old callback - the old reverse request has been invalidated by the broker's race/abort, pointing to the latest wait.
   */
  register(
    interactionId: string,
    resolve: (answer: V4InteractionAnswer) => void,
    options?: V4InteractionRegistrationOptions,
  ): () => void {
    const previous = this.pending.get(interactionId);
    const token = Symbol(interactionId);
    if (previous) {
      this.clearTimers(previous);
    }
    const entry: RegisteredInteraction = {
      resolve,
      options,
      token,
      autoResolutionEligible:
        previous?.autoResolutionEligible ??
        (options?.kind === "askUserQuestion" ? this.askUserQuestionAutoResolutionEnabled : false),
      ...(previous?.autoResolution || options?.initialAutoResolution
        ? { autoResolution: previous?.autoResolution ?? options?.initialAutoResolution }
        : {}),
    };
    this.pending.set(interactionId, entry);
    if (options && !previous) {
      const queue = this.queuesBySession.get(options.sessionId) ?? [];
      queue.push(interactionId);
      this.queuesBySession.set(options.sessionId, queue);
    }
    if (options) {
      if (entry.autoResolution) {
        if (options.kind === "askUserQuestion" && !entry.autoResolutionEligible) {
          void this.convertToSnoozed(entry);
        } else {
          this.resumeAutoResolution(interactionId, entry);
        }
      } else {
        this.activateHead(options.sessionId);
      }
    }
    return () => {
      const current = this.pending.get(interactionId);
      if (!current || current.token !== token) return;
      this.remove(interactionId, current);
    };
  }

  /**
* v4 resolveInteraction command end: deliver the response to the waiting broker deferred.
* Returns whether it is a hit - if it misses (answered/logged out/unknown id), the command interface will be closed idempotently.
* (proto.alreadyResolved semantics, multi-end first come first served, late response is harmless).
   */
  resolve(interactionId: string, answer: V4InteractionAnswer): boolean {
    const entry = this.pending.get(interactionId);
    if (!entry || entry.fullAccessPending) return false;
// Delete first and then resolve: resolve may trigger the logout of broker finally synchronously to avoid repeated delivery under re-entry.
    this.remove(interactionId, entry);
    entry.resolve(answer);
    return true;
  }

/** Responses with permission side effects are only open to registered same-session capabilities; failed requests are retained for retry. */
  async resolveFullAccess(interactionId: string, sessionId: string): Promise<boolean> {
    const entry = this.pending.get(interactionId);
    if (!entry) return false;
    if (entry.options?.sessionId !== sessionId || !entry.options.fullAccess) {
      throw new Error("Full access is not supported for this interaction");
    }
    if (entry.fullAccessPending) {
      await entry.fullAccessPending;
      return true;
    }
    const operation = entry.options.fullAccess();
    entry.fullAccessPending = operation;
    try {
      await operation;
      if (this.pending.get(interactionId) !== entry) return false;
      this.remove(interactionId, entry);
      entry.resolve({ optionId: "allowOnce" });
      return true;
    } finally {
      delete entry.fullAccessPending;
    }
  }

/** The first valid operation permanently suspends the automatic ending of the current AskUserQuestion; repeated/late calls are noops. */
  async snoozeAutoResolution(interactionId: string): Promise<boolean> {
    const entry = this.pending.get(interactionId);
    if (
      !entry?.options ||
      entry.options.kind !== "askUserQuestion" ||
      !entry.autoResolution ||
      entry.autoResolution.state === "snoozed"
    ) {
      return false;
    }
    await this.convertToSnoozed(entry);
    return true;
  }

  /**
* Update global gate. Shutdown will cancel all timers synchronously and wait for the active countdown to persist to snoozed before ACK;
* Restart the problem timing that only allows new registrations in the future.
   */
  async setAskUserQuestionAutoResolutionEnabled(enabled: boolean): Promise<number> {
    this.interactionPreferenceCommandApplied = true;
    return this.applyAskUserQuestionAutoResolutionEnabled(enabled);
  }

  /**
* The session startup handshake is only responsible for old Host compatibility and the initial value of the first runtime. Once the explicit workspace command
* Upon arrival, a late start handshake must not override a later setup commit.
   */
  async initializeAskUserQuestionAutoResolutionEnabled(enabled: boolean): Promise<number> {
    if (this.interactionPreferenceCommandApplied) return 0;
    return this.applyAskUserQuestionAutoResolutionEnabled(enabled);
  }

  private async applyAskUserQuestionAutoResolutionEnabled(enabled: boolean): Promise<number> {
    this.askUserQuestionAutoResolutionEnabled = enabled;
    if (enabled) return 0;

    const persistence: Promise<void>[] = [];
    let snoozedInteractionCount = 0;
    for (const entry of this.pending.values()) {
      if (entry.options?.kind !== "askUserQuestion") continue;
      entry.autoResolutionEligible = false;
      if (entry.autoResolution && entry.autoResolution.state !== "snoozed") {
        snoozedInteractionCount += 1;
        persistence.push(this.convertToSnoozed(entry));
      }
    }
    await Promise.all(persistence);
    return snoozedInteractionCount;
  }

  has(interactionId: string): boolean {
    return this.pending.has(interactionId);
  }

/** Resident recycling determination: whether the session still has interactions waiting for user response (deactivation will cause the response to fail). */
  hasPendingForSession(sessionId: string): boolean {
    for (const entry of this.pending.values()) {
      if (entry.options?.sessionId === sessionId) return true;
    }
    return false;
  }

  private activateHead(sessionId: string): void {
    const queue = this.queuesBySession.get(sessionId);
    const interactionId = queue?.[0];
    if (!interactionId) return;
    const entry = this.pending.get(interactionId);
    if (
      !entry?.options ||
      entry.options.kind !== "askUserQuestion" ||
      !entry.autoResolutionEligible ||
      entry.autoResolution
    ) {
      return;
    }
    const startedAt = this.now();
    const visibleAt = startedAt + this.hiddenGraceMs;
    const deadlineAt = startedAt + this.autoResolutionMs;
    entry.autoResolution = {
      state: "hiddenGrace",
      startedAt,
      visibleAt,
      deadlineAt,
    };
    this.notifyAutoResolution(entry);
    this.scheduleActiveAutoResolution(interactionId, entry);
  }

  private resumeAutoResolution(interactionId: string, entry: RegisteredInteraction): void {
    const sessionId = entry.options?.sessionId;
    if (!sessionId || this.queuesBySession.get(sessionId)?.[0] !== interactionId) {
      return;
    }
    const autoResolution = entry.autoResolution;
    if (!autoResolution) return;
    if (autoResolution.state === "snoozed") {
      this.notifyAutoResolution(entry);
      return;
    }
    const now = this.now();
    if (now >= autoResolution.deadlineAt) {
      queueMicrotask(() => {
        this.resolve(interactionId, {
          action: "accept",
          content: { answers: {} },
        });
      });
      return;
    }
    if (autoResolution.state === "hiddenGrace" && now >= autoResolution.visibleAt) {
      entry.autoResolution = {
        ...autoResolution,
        state: "visibleCountdown",
      };
    }
    this.notifyAutoResolution(entry);
    this.scheduleActiveAutoResolution(interactionId, entry);
  }

  private scheduleActiveAutoResolution(interactionId: string, entry: RegisteredInteraction): void {
    const autoResolution = entry.autoResolution;
    if (!autoResolution || autoResolution.state === "snoozed") return;
    const now = this.now();
    if (autoResolution.state === "hiddenGrace") {
      entry.visibleTimer = setTimeout(
        () => {
          const current = this.pending.get(interactionId);
          if (current !== entry || current.autoResolution?.state !== "hiddenGrace") {
            return;
          }
          current.autoResolution = {
            state: "visibleCountdown",
            startedAt: autoResolution.startedAt,
            visibleAt: autoResolution.visibleAt,
            deadlineAt: autoResolution.deadlineAt,
          };
          this.notifyAutoResolution(current);
        },
        Math.max(0, autoResolution.visibleAt - now),
      );
    }
    entry.deadlineTimer = setTimeout(
      () => {
        this.resolve(interactionId, {
          action: "accept",
          content: { answers: {} },
        });
      },
      Math.max(0, autoResolution.deadlineAt - now),
    );
  }

  private notifyAutoResolution(entry: RegisteredInteraction): void {
    if (!entry.autoResolution) return;
    void entry.options?.onAutoResolutionUpdated?.(entry.autoResolution);
  }

  private async convertToSnoozed(entry: RegisteredInteraction): Promise<void> {
    const autoResolution = entry.autoResolution;
    if (!autoResolution || autoResolution.state === "snoozed") return;
    this.clearTimers(entry);
    entry.autoResolution = {
      state: "snoozed",
      startedAt: autoResolution.startedAt,
      snoozedAt: this.now(),
    };
    await entry.options?.onAutoResolutionUpdated?.(entry.autoResolution);
  }

  private clearTimers(entry: RegisteredInteraction): void {
    if (entry.visibleTimer) clearTimeout(entry.visibleTimer);
    if (entry.deadlineTimer) clearTimeout(entry.deadlineTimer);
    delete entry.visibleTimer;
    delete entry.deadlineTimer;
  }

  private remove(interactionId: string, entry: RegisteredInteraction): void {
    this.pending.delete(interactionId);
    this.clearTimers(entry);
    const sessionId = entry.options?.sessionId;
    if (!sessionId) return;
    const queue = this.queuesBySession.get(sessionId);
    if (!queue) return;
    const index = queue.indexOf(interactionId);
    if (index >= 0) queue.splice(index, 1);
    if (queue.length === 0) {
      this.queuesBySession.delete(sessionId);
      return;
    }
    this.activateHead(sessionId);
  }
}
