/**
 * Shared primitives for the NC-AG dashboard. They exist so every panel renders the same
 * surface, badge and empty-state treatment (DESIGN.md tokens only, no ad-hoc colours).
 */
import type { ReactNode } from "react";
import { cn } from "@/components/lib/utils.js";
import { toneClasses, type Tone } from "./format.js";

export function NcagSurface({
  title,
  subtitle,
  actions,
  children,
  className,
}: {
  title: string;
  subtitle?: string;
  actions?: ReactNode;
  children: ReactNode;
  className?: string;
}) {
  return (
    <section
      className={cn(
        "flex min-h-0 flex-col rounded-lg border border-card-border bg-card",
        className,
      )}
    >
      <header className="flex items-start justify-between gap-3 border-b border-border px-3 py-2">
        <div className="min-w-0">
          <h2 className="truncate text-ui-base font-semibold text-foreground">{title}</h2>
          {subtitle ? (
            <p className="truncate text-ui-xs text-foreground-subtle">{subtitle}</p>
          ) : null}
        </div>
        {actions ? <div className="flex shrink-0 items-center gap-1">{actions}</div> : null}
      </header>
      <div className="min-h-0 flex-1 overflow-auto p-3">{children}</div>
    </section>
  );
}

export function NcagBadge({ tone, label }: { tone: Tone; label: string }) {
  return (
    <span
      className={cn(
        "inline-flex h-5 items-center rounded-full border px-2 text-ui-2xs font-medium whitespace-nowrap",
        toneClasses[tone],
      )}
    >
      {label}
    </span>
  );
}

export function NcagEmpty({ message, hint }: { message: string; hint?: string }) {
  return (
    <div className="flex flex-col items-center justify-center gap-1 py-8 text-center">
      <p className="text-ui-sm text-foreground-subtle">{message}</p>
      {hint ? <p className="max-w-sm text-ui-xs text-foreground-subtlest">{hint}</p> : null}
    </div>
  );
}

export function NcagErrorBanner({
  message,
  onDismiss,
}: {
  message: string;
  onDismiss?: () => void;
}) {
  return (
    <div className="mb-3 flex items-start justify-between gap-3 rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2">
      <div className="min-w-0">
        <p className="text-ui-sm font-medium text-destructive">NC-AG error</p>
        <p className="break-words text-ui-xs text-foreground-subtle">{message}</p>
      </div>
      {onDismiss ? (
        <button
          type="button"
          className="shrink-0 rounded-sm border border-border px-2 py-1 text-ui-xs text-foreground-subtle hover:bg-surface-hover"
          onClick={onDismiss}
        >
          Dismiss
        </button>
      ) : null}
    </div>
  );
}

export function NcagMetric({
  label,
  value,
  hint,
  tone,
}: {
  label: string;
  value: string | number;
  hint?: string;
  tone?: Tone;
}) {
  return (
    <div className="rounded-md border border-border bg-surface px-3 py-2">
      <p className="truncate text-ui-2xs uppercase tracking-wide text-foreground-subtlest">
        {label}
      </p>
      <p
        className={cn(
          "mt-1 text-ui-lg font-semibold",
          tone === "danger" ? "text-destructive" : "text-foreground",
        )}
      >
        {value}
      </p>
      {hint ? <p className="truncate text-ui-2xs text-foreground-subtle">{hint}</p> : null}
    </div>
  );
}
