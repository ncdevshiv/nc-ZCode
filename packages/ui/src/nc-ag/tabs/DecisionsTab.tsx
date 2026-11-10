/** Debate & decision centre: requested approach, recommendation, evidence, pros/cons/risks. */
import { useState } from "react";
import { decisionTone } from "../format.js";
import { NcagBadge, NcagEmpty, NcagSurface } from "../primitives.js";
import type { NcagDecision } from "../types.js";

export function DecisionsTab({
  decisions,
  onResolve,
}: {
  decisions: NcagDecision[];
  onResolve: (decisionId: string, resolution: string, rationale: string) => Promise<boolean>;
}) {
  const [rationale, setRationale] = useState("");
  return (
    <NcagSurface
      title="Debate & decision centre"
      subtitle="material decisions belong to the user; NC-AG recommends with evidence"
    >
      {decisions.length === 0 ? (
        <NcagEmpty
          message="No disagreements recorded."
          hint="When NC-AG disagrees with a request it opens a decision with evidence instead of silently choosing one interpretation."
        />
      ) : (
        <ul className="flex flex-col gap-3">
          {decisions.map((decision) => (
            <li key={decision.id} className="rounded-md border border-border bg-surface p-3">
              <div className="flex flex-wrap items-center gap-2">
                <NcagBadge tone={decisionTone(decision.status)} label={decision.status} />
                <NcagBadge tone="neutral" label={decision.class} />
                <span className="text-ui-sm font-semibold">{decision.topic}</span>
                {decision.class !== "autonomous" && decision.status === "open" ? (
                  <NcagBadge tone="warning" label="your decision is required" />
                ) : null}
              </div>

              <div className="mt-2 grid gap-2 sm:grid-cols-2">
                <div>
                  <p className="text-ui-2xs uppercase text-foreground-subtlest">You asked for</p>
                  <p className="text-ui-xs text-foreground-subtle">{decision.requested}</p>
                </div>
                <div>
                  <p className="text-ui-2xs uppercase text-foreground-subtlest">
                    NC-AG recommends
                  </p>
                  <p className="text-ui-xs text-foreground-subtle">{decision.recommendation}</p>
                </div>
              </div>

              {decision.evidence.length > 0 ? (
                <div className="mt-2">
                  <p className="text-ui-2xs uppercase text-foreground-subtlest">Evidence</p>
                  <ul className="list-disc pl-4 text-ui-xs text-foreground-subtle">
                    {decision.evidence.map((item) => (
                      <li key={item}>{item}</li>
                    ))}
                  </ul>
                </div>
              ) : null}

              <div className="mt-2 grid gap-2 sm:grid-cols-2">
                {decision.options.map((option) => (
                  <div key={option.id} className="rounded-md border border-border bg-card p-2">
                    <p className="text-ui-xs font-medium">{option.label}</p>
                    <p className="text-ui-2xs text-success">pro: {option.pros.join("; ") || "—"}</p>
                    <p className="text-ui-2xs text-warning">con: {option.cons.join("; ") || "—"}</p>
                    <p className="text-ui-2xs text-destructive">
                      risk: {option.risks.join("; ") || "—"}
                    </p>
                  </div>
                ))}
              </div>

              {decision.status === "open" ? (
                <div className="mt-2 flex flex-col gap-2">
                  <input
                    className="h-7 rounded-md border border-input-border bg-input px-2 text-ui-sm"
                    placeholder="Rationale (stored permanently with the decision)"
                    value={rationale}
                    onChange={(event) => setRationale(event.target.value)}
                  />
                  <div className="flex flex-wrap gap-2">
                    <button
                      type="button"
                      className="h-7 rounded-md bg-primary px-3 text-ui-xs text-primary-foreground"
                      onClick={() => void onResolve(decision.id, "recommended", rationale)}
                    >
                      Adopt the recommendation
                    </button>
                    <button
                      type="button"
                      className="h-7 rounded-md border border-border px-3 text-ui-xs"
                      onClick={() => void onResolve(decision.id, "requested", rationale)}
                    >
                      Keep my approach
                    </button>
                  </div>
                </div>
              ) : (
                <p className="mt-2 text-ui-2xs text-foreground-subtle">
                  settled as <span className="text-foreground">{decision.resolution}</span> by{" "}
                  {decision.decidedBy} · {decision.rationale ?? "no rationale"}
                </p>
              )}
            </li>
          ))}
        </ul>
      )}
    </NcagSurface>
  );
}
