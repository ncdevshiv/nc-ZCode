// ============================================================
// Two things for actor session transcription: boundary counting and seed truncation replication (amend-resume)
// ============================================================
//
// These two things must live in the same module because they share a **loadability** invariant:
//
// Boundary N ⇒ Copy the first N messages of the source conversation, just to get the complete transcription "up to the ask".
//
// The only reason why it is established is: counting messages and copying messages go to the same access plane ({@link ActorTranscriptStore.messages}),
// It is also the one read when core's `resumeFromStore` is rehydrated (core/src/runtime/methods/resume.ts).
// As long as one of the three readers changes to another caliber (such as counting only active branches, or counting by parts), the boundary is in the eyes of the two readers
// It’s just two lengths. The truncated transcription will have one more or one less text - and that’s exactly the boundary between “the text seen by the model” and “journal note”
// The look of being quietly misplaced. Therefore: **The counting caliber needs to be changed, and the three places must be changed at the same time**.
//
// The key property of count offset (rather than message id range) is that the **prefix remains unchanged after copying**: the id will all change after copying into a new session.
// The count remains unchanged, so the boundary value of the copied ask remains valid in the new session - chain revision (B copies from A, C then copies from B)
// The basis of this is this.

import {
  createMessageId,
  createPartId,
  type MessageId,
  type MessageInfo,
  type MessagePart,
  type MessageWithParts,
  type PartId,
  type SessionId,
} from "@zcode/contracts";
import { cloneMessageForFork, clonePartForFork } from "@zcode/core";
import { WorkflowError, type ActorSessionSeed } from "@zcode/dynamic-workflow";

/**
* Session transcription access interface required on the driver side: read all messages of a session, write one message/part.
 *
* Structurally, it is a true subset of {@link import("@zcode/contracts").SessionStorePort}, so production can directly
* Just pass in the session store. The reason for narrowing is the same as the journal / task-link port of run service: driver
* With only these three methods, declaring the entire store will make "the driver relies on the full capabilities of the session store" true.
 */
export interface ActorTranscriptStore {
  messages(input: { sessionID: SessionId }): Promise<MessageWithParts[]>;
  saveMessage(input: MessageInfo): Promise<void>;
  savePart(input: MessagePart): Promise<void>;
}

/**
* The number of messages that are currently persisted in an actor session (the value of ask boundary accounting).
 *
* Only count those that fall into the library: the driver asks this number at the end of an exchange, and the runtime's message persistence is awaited within the turn
* Completed (core's persistMessage/persistPart), so the amount dropped at this moment is the entire output of this exchange.
 */
export async function countActorTranscript(
  store: ActorTranscriptStore,
  sessionId: SessionId,
): Promise<number> {
  return (await store.messages({ sessionID: sessionId })).length;
}

/**
* Copy the first `seed.messageCount` messages (along with their respective parts) from the source session into the target session.
 *
*Three properties:
 *
* 1. **Precursor read-only**. Each message and part copied out has a new ID - `message.id` / `part.id` is the primary key of the whole database.
* And `saveMessage`’s upsert will change `session_id` to a new value when ids conflict (adapters’
* messages.ts). Moving the ID as it is is not "copying", it is moving the predecessor's transcription. id recasting then requires `parentID`
* With part inline anchors followed by remapping, this is exactly what core's fork cloner {@link cloneMessageForFork} /
* {@link clonePartForFork} has done the right thing (fork is the same action as this function: copy one paragraph by value to another
* session, child continues with local id), so reuse them here instead of writing a second copy.
* 2. **Impotent**. The target session already has ≥ messageCount messages, that is, the entire section is skipped: that is the case of resume after the crash of revised run.
* ——The session id is cast purely deterministically from (runId, actorRef), and the one obtained by re-hanging is the one that has been loaded with "copy + new one"
* Conversation of content, copying it again is equivalent to doubling the above text. If insufficient, copy again (half prefix left by a crash during copying): id press
* (Target session, subscript) Purely determined, re-copying is an upsert of existing lines, and no duplication will occur.
* 3. **Lack of material means loud failure**. The source session does not exist (read back empty) or is shorter than bounds, indicating that the service was constructed from the journal fact
* The seed store cannot be redeemed - corruption level, not downgradeable (the half that can be downgraded is in the door on the service side:
* Candidates with missing sessions on the chain should be discarded there).
 *
* @returns The actual number of copied items; `undefined` means idempotent skipping.
 */
export async function seedActorTranscript(input: {
  seed: ActorSessionSeed;
  store: ActorTranscriptStore;
  targetSessionId: SessionId;
}): Promise<number | undefined> {
  const { seed, store, targetSessionId } = input;
  const existing = await store.messages({ sessionID: targetSessionId });
  if (existing.length >= seed.messageCount) return undefined;

  const source = await store.messages({ sessionID: seed.sourceSessionId as SessionId });
  if (source.length < seed.messageCount) {
    throw new WorkflowError(
      "DriverError",
      `Cannot seed the subagent transcript: source session ${seed.sourceSessionId} has only ` +
        `${source.length} messages, but the boundary requires the first ${seed.messageCount}.`,
      { mismatch: { expected: String(seed.messageCount), got: String(source.length) } },
    );
  }

//Old id → New id: The parentID of assistant and the embedded anchor point of part are remapped according to it. The prefixes are consecutive,
//So the earlier message referenced by each message must already be in the table (the subscript is strictly increasing).
  const messageIds = new Map<MessageId, MessageId>();
  for (let index = 0; index < seed.messageCount; index++) {
    const message = source[index]!;
    const nextMessageId = seededMessageId(targetSessionId, index);
    const cloned = cloneMessageForFork(message.info, {
      forkedSessionId: targetSessionId,
      messageIdMap: messageIds,
      nextMessageId,
    });
    messageIds.set(message.info.id, nextMessageId);
    await store.saveMessage(cloned);
    for (const [partIndex, part] of message.parts.entries()) {
      await store.savePart(
        clonePartForFork(part, {
          forkedSessionId: targetSessionId,
          messageIdMap: messageIds,
          nextMessageId,
          nextPartId: seededPartId(targetSessionId, index, partIndex),
        }),
      );
    }
  }
  return seed.messageCount;
}

/**
* The id of the seed copy: by (target session, prefix_subscript) pure OK.
 *
* What you buy with certainty is the **repairability of half-copy**: the copy crashes halfway, and the existing rows will be upserted back with the same id when re-hanging.
* Instead of growing another one next to it. The session id already contains runId and actorRef, so copies of two different target sessions will naturally not collide.
 */
function seededMessageId(targetSessionId: SessionId, index: number): MessageId {
  return createMessageId(`${targetSessionId}-seed-${index}`);
}

function seededPartId(targetSessionId: SessionId, index: number, partIndex: number): PartId {
  return createPartId(`${targetSessionId}-seed-${index}-${partIndex}`);
}
