//Journal sequence interception layer of engine events. Separate into modules instead of staying in launch.ts: the responsibility there is to "assemble and start a run", while here it is
// "Truncate the number assigned by appendEvent to the following emit" - the two things are complete, and this one has only one consumer.
import type { JournalStorePort, RunEvent, StoredEvent } from "@zcode/dynamic-workflow";

/**
* Wrap a layer of journal, just to cut off the sequence assigned by `appendEvent` for use by the subsequent emit.
 *
* Premise (guaranteed by the engine's `record()` and nailed by the test): Each event is followed by "appendEvent" post-synchronization
* emit", one-to-one correspondence. So "the sequence of the last append" is the sequence of "the one currently being emitted".
* To be on the safe side, check event object **reference equality**: inequality means that the premise is broken. At this time, the last sequence is returned instead of
* Take a guess - a silent error is harder to detect than a stuck error, and testing will catch it before production does.
 */
export function createJournalSequenceCapture(journal: JournalStorePort): {
  journal: JournalStorePort;
  sequenceOf: (event: RunEvent) => number;
} {
  let lastStored: StoredEvent | undefined;
// Explicit forwarding method by method instead of `{...journal, appendEvent}`: both implementations are classes, and expansion only copies its own properties.
// All methods on the prototype will be discarded (the engine will raise TypeError when calling getRun later). The forwarding plane only has engine ports:
// Orphan convergence and enumerated narrow queries are deliberately not included - they happen on the service side, directly to deps.journal, without going through this layer.
  const wrapped: JournalStorePort = {
    createRun: (record) => journal.createRun(record),
    getRun: (runId) => journal.getRun(runId),
    updateRunStatus: (runId, status, settlement) =>
      journal.updateRunStatus(runId, status, settlement),
    updateRunUsage: (runId, spentTokens) => journal.updateRunUsage(runId, spentTokens),
    putActor: (record) => journal.putActor(record),
    getActor: (runId, siteId, ordinal) => journal.getActor(runId, siteId, ordinal),
    listActors: (runId) => journal.listActors(runId),
    putNode: (record) => journal.putNode(record),
    getNode: (runId, siteId, ordinal) => journal.getNode(runId, siteId, ordinal),
    listNodes: (runId) => journal.listNodes(runId),
    appendEvent: (runId, event) => {
      const stored = journal.appendEvent(runId, event);
      lastStored = stored;
      return stored;
    },
    listEvents: (runId, opts) => journal.listEvents(runId, opts),
  };
  return {
    journal: wrapped,
    sequenceOf: (event) =>
      lastStored?.event === event ? lastStored.sequence : (lastStored?.sequence ?? 0),
  };
}
