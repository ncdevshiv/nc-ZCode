/** Communication dashboard: persisted messages, acknowledgment and broadcasting. */
import { useState } from "react";
import { formatTime } from "../format.js";
import { NcagBadge, NcagEmpty, NcagSurface } from "../primitives.js";
import type { NcagMessage } from "../types.js";

export function CommsTab({
  messages,
  onAck,
  onSend,
  canSend,
}: {
  messages: NcagMessage[];
  onAck: (id: string) => Promise<boolean>;
  onSend: (subject: string, body: string, recipient: string) => Promise<boolean>;
  canSend: boolean;
}) {
  const [subject, setSubject] = useState("");
  const [body, setBody] = useState("");
  const [recipient, setRecipient] = useState("");

  return (
    <div className="grid min-h-0 grid-cols-1 gap-3 xl:grid-cols-3">
      <NcagSurface
        title="Messages"
        subtitle={`${messages.length} persisted`}
        className="xl:col-span-2"
      >
        {messages.length === 0 ? (
          <NcagEmpty
            message="No messages yet."
            hint="Agents and the orchestrator exchange typed, persisted messages (info, help, findings, handoff, escalation…)."
          />
        ) : (
          <ul className="flex flex-col gap-1">
            {messages
              .slice()
              .reverse()
              .map((message) => (
                <li
                  key={message.id}
                  className="rounded-md border border-border bg-surface px-2 py-1.5"
                >
                  <div className="flex flex-wrap items-center gap-2">
                    <NcagBadge tone="neutral" label={message.kind} />
                    <span className="min-w-0 flex-1 truncate text-ui-sm font-medium">
                      {message.subject}
                    </span>
                    <span className="text-ui-2xs text-foreground-subtle">
                      {formatTime(message.createdAt)}
                    </span>
                  </div>
                  <p className="mt-1 line-clamp-3 text-ui-xs text-foreground-subtle">
                    {message.body}
                  </p>
                  <div className="mt-1 flex flex-wrap items-center gap-2 text-ui-2xs text-foreground-subtlest">
                    <span>
                      {message.sender} → {message.recipient}
                    </span>
                    <span>· {message.deliveryState}</span>
                    {message.requiresAck ? (
                      <button
                        type="button"
                        className="rounded-sm border border-border px-1.5 py-0.5 hover:bg-surface-hover"
                        disabled={message.ackState === "acknowledged"}
                        onClick={() => void onAck(message.id)}
                      >
                        {message.ackState === "acknowledged" ? "acknowledged" : "acknowledge"}
                      </button>
                    ) : null}
                  </div>
                </li>
              ))}
          </ul>
        )}
      </NcagSurface>

      <NcagSurface title="Broadcast" subtitle="project channel, recipient optional">
        {!canSend ? <p className="text-ui-xs text-foreground-subtle">Select a project first.</p> : null}
        <div className="flex flex-col gap-2">
          <input
            className="h-7 rounded-md border border-input-border bg-input px-2 text-ui-sm"
            placeholder="Recipient agent id (blank = project channel)"
            value={recipient}
            onChange={(event) => setRecipient(event.target.value)}
          />
          <input
            className="h-7 rounded-md border border-input-border bg-input px-2 text-ui-sm"
            placeholder="Subject"
            value={subject}
            onChange={(event) => setSubject(event.target.value)}
          />
          <textarea
            className="min-h-24 rounded-md border border-input-border bg-input p-2 text-ui-sm"
            placeholder="Body — delivered to the chosen recipient or the whole project."
            value={body}
            onChange={(event) => setBody(event.target.value)}
          />
          <button
            type="button"
            className="h-7 rounded-md bg-primary px-3 text-ui-sm text-primary-foreground disabled:opacity-50"
            disabled={!canSend || subject.trim().length === 0 || body.trim().length === 0}
            onClick={() => {
              void onSend(subject.trim(), body.trim(), recipient.trim()).then((ok) => {
                if (ok) {
                  setSubject("");
                  setBody("");
                }
              });
            }}
          >
            Send message
          </button>
        </div>
      </NcagSurface>
    </div>
  );
}
