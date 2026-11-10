/** Execution timeline: every persisted event, newest first. */
import { formatTime } from "../format.js";
import { NcagEmpty, NcagSurface } from "../primitives.js";
import type { NcagEvent } from "../types.js";

export function TimelineTab({ events }: { events: NcagEvent[] }) {
  return (
    <NcagSurface title="Execution timeline" subtitle={`${events.length} event(s) persisted`}>
      {events.length === 0 ? (
        <NcagEmpty
          message="No events yet."
          hint="Every plan, assignment, tool call, review, decision and recovery is appended here."
        />
      ) : (
        <ol className="flex flex-col gap-1">
          {events
            .slice()
            .reverse()
            .map((event) => (
              <li
                key={event.id}
                className="flex items-start gap-2 border-b border-border py-1 last:border-b-0"
              >
                <span className="w-20 shrink-0 text-ui-2xs text-foreground-subtlest">
                  {formatTime(event.createdAt)}
                </span>
                <span className="w-40 shrink-0 truncate text-ui-2xs text-foreground">
                  {event.kind}
                </span>
                <span className="min-w-0 flex-1 truncate text-ui-2xs text-foreground-subtle">
                  {JSON.stringify(event.payload)}
                </span>
              </li>
            ))}
        </ol>
      )}
    </NcagSurface>
  );
}
