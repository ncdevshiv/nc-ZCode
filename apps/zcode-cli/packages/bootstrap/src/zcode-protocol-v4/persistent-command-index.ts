// session-scoped lazy index for commands/query.
// The loader is executed only once per session; new transcript/marker/child/discarded facts are incorporated using record increments.
import type { CommandAck, CommandKey } from "@zcode/shared/zcode-protocol-v4";

export type PersistentCommandFactSource = "transcript" | "timeline" | "child" | "discarded";

interface PersistentCommandIndexSeed {
  workspacePath: string;
  workspaceIdentity?: string;
  facts?: Partial<Record<PersistentCommandFactSource, readonly CommandAck[]>>;
}

interface PersistentCommandIndexTarget {
  workspacePath: string;
  workspaceIdentity?: string;
}

interface PersistentCommandIndexHost {
  loadSession(sessionId: string): Promise<PersistentCommandIndexSeed | null>;
}

interface SessionIndex {
  workspaceKey: string;
  facts: Record<PersistentCommandFactSource, Map<string, CommandAck>>;
}

function workspaceKey(target: PersistentCommandIndexTarget): string {
  return target.workspaceIdentity?.trim() || target.workspacePath;
}

function emptyFacts(): SessionIndex["facts"] {
  return {
    transcript: new Map(),
    timeline: new Map(),
    child: new Map(),
    discarded: new Map(),
  };
}

export class PersistentCommandIndex {
  private readonly sessions = new Map<string, Promise<SessionIndex | null>>();

  constructor(private readonly host: PersistentCommandIndexHost) {}

  async lookup(source: PersistentCommandFactSource, key: CommandKey): Promise<CommandAck | null> {
    if (key.sessionId === null) return null;
    const index = await this.ensureSession(key.sessionId);
    return index?.facts[source].get(key.commandId) ?? null;
  }

  /**
* Transcript append / marker settle / child create / discarded ledger is called after writing; the entire volume is not rescanned.
* Explicitly reject when the expected target is inconsistent with the workspaceKey obtained by the first load to prevent remote cross-talk on the same path.
   */
  async record(
    target: PersistentCommandIndexTarget,
    sessionId: string,
    source: PersistentCommandFactSource,
    ack: CommandAck,
  ): Promise<void> {
    const index = await this.ensureSession(sessionId);
    if (!index) throw new Error("fault.command.querySessionNotFound");
    if (index.workspaceKey !== workspaceKey(target)) {
      throw new Error("fault.command.queryForeignWorkspace");
    }
    index.facts[source].set(ack.commandId, ack);
  }

  invalidate(sessionId: string): void {
    this.sessions.delete(sessionId);
  }

  private ensureSession(sessionId: string): Promise<SessionIndex | null> {
    let pending = this.sessions.get(sessionId);
    if (!pending) {
      pending = this.host.loadSession(sessionId).then((seed) => {
        if (!seed) return null;
        const facts = emptyFacts();
        for (const source of ["transcript", "timeline", "child", "discarded"] as const) {
          for (const ack of seed.facts?.[source] ?? []) facts[source].set(ack.commandId, ack);
        }
        return { workspaceKey: workspaceKey(seed), facts };
      });
// If the read fails, the rejected Promise will not be cached; the next query can be retried after the store is restored.
      pending.catch(() => {
        if (this.sessions.get(sessionId) === pending) this.sessions.delete(sessionId);
      });
      pending.then(
        (index) => {
// unknown/session-not-found is not a fact and cannot be cached; subsequent transcripts must be visible after being placed.
          if (index === null && this.sessions.get(sessionId) === pending) {
            this.sessions.delete(sessionId);
          }
        },
        () => {},
      );
      this.sessions.set(sessionId, pending);
    }
    return pending;
  }
}
