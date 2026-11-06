import type { ZCodeApp } from "../app/types.js";
import type { ZCodeProtocolAgentDependencies } from "./server-types.js";

/** Holds app resources instead of business sessions; temporary workspace apps and unregistered apps also belong to this owner. */
export class ProtocolRuntimeResources {
  private stopping = false;
  private readonly apps = new Set<ZCodeApp>();
  private closePromise?: Promise<void>;

  constructor(private readonly factory: ZCodeProtocolAgentDependencies["createZCodeApp"]) {}

  readonly create: ZCodeProtocolAgentDependencies["createZCodeApp"] = async (options) => {
    this.assertServing();
    const app = await this.factory(options);
    const originalClose = app.close?.bind(app);
    let closing: Promise<void> | undefined;
    app.close = () => {
      closing ??= Promise.resolve().then(async () => {
        try {
          await originalClose?.();
        } finally {
          this.apps.delete(app);
        }
      });
      return closing;
    };
    this.apps.add(app);
    if (this.stopping) {
// When create starts before EOF and returns after EOF, it cannot become an available session again, and resources cannot be lost.
      await this.stopApp(app);
      this.assertServing();
    }
    return app;
  };

  assertServing(): void {
    if (this.stopping) throw new Error("ZCode Protocol runtime is stopping");
  }

  close(): Promise<void> {
    this.stopping = true;
    this.closePromise ??= Promise.resolve().then(async () => {
      const results = await Promise.allSettled([...this.apps].map((app) => this.stopApp(app)));
      const errors = results
        .filter((result) => result.status === "rejected")
        .map((result) => result.reason);
      if (errors.length) throw new AggregateError(errors, "Protocol apps shutdown failed");
    });
    return this.closePromise;
  }

  private async stopApp(app: ZCodeApp): Promise<void> {
    try {
      app.runtime.beginShutdown();
      app.runtime.stopActiveForegroundExecution?.({ reason: "Protocol runtime stopping" });
    } finally {
      await app.close?.();
    }
  }
}
