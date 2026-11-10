/** Request state and the delivery pack (implementation evidence, reviews, limitations). */
import { formatCost } from "../format.js";
import { NcagBadge, NcagEmpty, NcagMetric, NcagSurface } from "../primitives.js";
import type { NcagDelivery, NcagRequest } from "../types.js";

export function DeliveryTab({
  delivery,
  requests,
  hasOpenDecisions,
}: {
  delivery: NcagDelivery | null;
  requests: NcagRequest[];
  hasOpenDecisions: boolean;
}) {
  const request = requests.at(-1) ?? null;
  return (
    <div className="grid min-h-0 grid-cols-1 gap-3 xl:grid-cols-2">
      <NcagSurface title="Request" subtitle={request?.id ?? "no request"}>
        {request ? (
          <div className="flex flex-col gap-2">
            <NcagBadge
              tone={
                request.status === "completed"
                  ? "success"
                  : request.status === "failed"
                    ? "danger"
                    : "warning"
              }
              label={request.status}
            />
            <p className="text-ui-sm text-foreground">{request.instruction}</p>
            <p className="text-ui-2xs text-foreground-subtle">
              planner: {request.planner ?? "not run"} · trace {request.traceId}
            </p>
            {request.plan ? (
              <div>
                <p className="text-ui-2xs uppercase text-foreground-subtlest">Plan</p>
                <p className="text-ui-xs text-foreground-subtle">{request.plan.summary}</p>
                {request.plan.contradictions.length > 0 ? (
                  <p className="mt-1 text-ui-xs text-warning">
                    contradictions: {request.plan.contradictions.join("; ")}
                  </p>
                ) : null}
                {request.plan.assumptions.length > 0 ? (
                  <p className="mt-1 text-ui-2xs text-foreground-subtlest">
                    assumptions: {request.plan.assumptions.join("; ")}
                  </p>
                ) : null}
              </div>
            ) : null}
            {hasOpenDecisions ? (
              <p className="rounded-md border border-warning/40 bg-warning/10 px-2 py-1 text-ui-xs text-warning">
                Execution is waiting on your decision — see the Decisions tab.
              </p>
            ) : null}
          </div>
        ) : (
          <NcagEmpty message="Submit a request to see its delivery pack." />
        )}
      </NcagSurface>

      <NcagSurface title="Delivery pack" subtitle={delivery?.status ?? "not built yet"}>
        {delivery ? (
          <div className="flex flex-col gap-2">
            <div className="grid grid-cols-2 gap-2">
              <NcagMetric label="Evidence rows" value={delivery.evidence.length} />
              <NcagMetric label="Review rows" value={delivery.reviews.length} />
              <NcagMetric label="Integrations" value={delivery.integrations.length} />
              <NcagMetric label="Cost" value={formatCost(delivery.usage.costUsd)} />
            </div>
            <p className="text-ui-xs text-foreground-subtle">{delivery.summary}</p>
            <div>
              <p className="text-ui-2xs uppercase text-foreground-subtlest">Files changed</p>
              <p className="text-ui-xs text-foreground-subtle">
                {delivery.filesChanged.join(", ") || "none recorded"}
              </p>
            </div>
            <div>
              <p className="text-ui-2xs uppercase text-foreground-subtlest">Limitations</p>
              {delivery.limitations.length === 0 ? (
                <p className="text-ui-xs text-foreground-subtle">none reported</p>
              ) : (
                <ul className="list-disc pl-4 text-ui-xs text-warning">
                  {delivery.limitations.map((limitation) => (
                    <li key={limitation}>{limitation}</li>
                  ))}
                </ul>
              )}
            </div>
            {delivery.decisions.length > 0 ? (
              <div>
                <p className="text-ui-2xs uppercase text-foreground-subtlest">Decisions</p>
                <ul className="list-disc pl-4 text-ui-xs text-foreground-subtle">
                  {delivery.decisions.map((decision) => (
                    <li key={decision.id}>
                      {decision.topic} → {decision.resolution ?? "open"}
                    </li>
                  ))}
                </ul>
              </div>
            ) : null}
          </div>
        ) : (
          <NcagEmpty
            message="No delivery yet."
            hint="The pack is produced from task results, verification evidence, reviews and integrations."
          />
        )}
      </NcagSurface>
    </div>
  );
}
