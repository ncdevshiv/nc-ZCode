/** Resource leases, waiters and integration outcomes. */
import { NcagBadge, NcagEmpty, NcagSurface } from "../primitives.js";
import type { NcagIntegration, NcagLease } from "../types.js";

export function ResourcesTab({
  leases,
  integrations,
  onRelease,
}: {
  leases: NcagLease[];
  integrations: NcagIntegration[];
  onRelease: (id: string) => Promise<boolean>;
}) {
  return (
    <div className="grid min-h-0 grid-cols-1 gap-3 xl:grid-cols-2">
      <NcagSurface title="Resource leases" subtitle="an exclusive write blocks other tasks">
        {leases.length === 0 ? (
          <NcagEmpty
            message="No active leases."
            hint="A task acquires a lease before it writes a declared resource; leases carry fencing tokens and expire."
          />
        ) : (
          <ul className="flex flex-col gap-1">
            {leases.map((lease) => (
              <li
                key={lease.id}
                className="flex items-center gap-2 rounded-md border border-border bg-surface px-2 py-1.5"
              >
                <NcagBadge
                  tone={lease.mode === "exclusive_write" ? "warning" : "info"}
                  label={lease.mode}
                />
                <span className="min-w-0 flex-1 truncate text-ui-sm">{lease.resource}</span>
                <span className="shrink-0 text-ui-2xs text-foreground-subtle">
                  {lease.ownerTask.slice(0, 14)} . fencing #{lease.fencingToken}
                </span>
                <button
                  type="button"
                  className="shrink-0 rounded-sm border border-border px-1.5 py-0.5 text-ui-2xs"
                  onClick={() => void onRelease(lease.id)}
                >
                  release
                </button>
              </li>
            ))}
          </ul>
        )}
      </NcagSurface>

      <NcagSurface title="Integration" subtitle="merges are evidence-gated">
        {integrations.length === 0 ? (
          <NcagEmpty
            message="Nothing integrated yet."
            hint="Integration runs only after verification evidence and review approval are green."
          />
        ) : (
          <ul className="flex flex-col gap-1">
            {integrations.map((integration) => (
              <li
                key={integration.id}
                className="rounded-md border border-border bg-surface px-2 py-1.5"
              >
                <div className="flex items-center gap-2">
                  <NcagBadge
                    tone={
                      integration.outcome === "merged"
                        ? "success"
                        : integration.outcome === "conflict"
                          ? "danger"
                          : "warning"
                    }
                    label={integration.outcome}
                  />
                  <span className="min-w-0 flex-1 truncate text-ui-sm">
                    {integration.taskId.slice(0, 18)}
                  </span>
                </div>
                <p className="truncate text-ui-2xs text-foreground-subtle">
                  {integration.commitSha ?? "no commit"} . {integration.changed.length} file(s)
                  {integration.conflicts.length > 0
                    ? ` . conflicts: ${integration.conflicts.join(", ")}`
                    : ""}
                </p>
                {integration.detail ? (
                  <p className="truncate text-ui-2xs text-foreground-subtlest">
                    {integration.detail}
                  </p>
                ) : null}
              </li>
            ))}
          </ul>
        )}
      </NcagSurface>
    </div>
  );
}
