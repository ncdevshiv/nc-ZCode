import type { Logger } from "@zcode/contracts";
import {
  createMemorySampleWriteGate,
  memoryUsageToSampleFields,
  zcodeProtocolNotifications,
  type MemorySample,
  type ZCodeProtocolNotification,
} from "@zcode/shared";
import {
  createZCodeProcessResourceSampler,
  type ZCodeProcessResourceSampler,
} from "../process-resource-sampler.js";
import type { ZCodeProtocolAgentServer } from "./server.js";

export function startProtocolResourceSampler(
  server: ZCodeProtocolAgentServer,
  send: (message: ZCodeProtocolNotification) => void,
  logger: Logger,
): ZCodeProcessResourceSampler | undefined {
  try {
// Local memory diagnostic log: reuse the same 60s beat,
//Write one line after change/heartbeat gate control to avoid flushing the disk at the same time as the message flow.
    const memoryDiagnosticsGate = createMemorySampleWriteGate();
    const sampler = createZCodeProcessResourceSampler({
      onSample: (sample, memoryUsage) => {
        send({
          method: zcodeProtocolNotifications.processResourceSample,
          params: sample,
        });
// resident session TTL / The water level convergence borrows the resource sampling beat (60s) as a back-up, and does not add a new timer;
// The sampler has an exception for onSample, and a single rebalance failure will not affect telemetry reporting.
        server.rebalanceResidentSessions();
        try {
// In the same beat, the transient event time is eliminated first, and then sampled. The eventRows in the log reflect the dwell amount after elimination.
          server.pruneSessionEventStores();
          server.pruneDetachedChildPublishers();
        } catch {
// Failure to eliminate all resources will not affect resource reporting and diagnosis logs.
        }
        try {
          const memorySample: MemorySample = {
            role: "agent_node",
            ...memoryUsageToSampleFields(memoryUsage),
            counters: server.collectMemoryDiagnostics(),
          };
          const reason = memoryDiagnosticsGate.evaluate(memorySample, Date.now());
          if (reason) {
            const { role: _role, counters, ...memoryFields } = memorySample;
            logger.info("Process memory sample", {
              event: "zcode_protocol.process.memory_sample",
              reason,
              ...memoryFields,
              counters,
            });
          }
        } catch {
// If the diagnostic log fails, only the current sample will be lost, and resource reporting and rebalance will not be affected.
        }
      },
    });
    sampler.start();
    return sampler;
  } catch {
// Resource telemetry is best effort, and initialization failure cannot change the Agent startup result.
    return undefined;
  }
}
