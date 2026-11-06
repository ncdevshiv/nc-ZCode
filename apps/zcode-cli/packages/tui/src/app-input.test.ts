import { describe, expect, it } from "bun:test";
import { filterSlashCommands, reconcileSlashSelection, modelCommandQuery, filterModelOptions } from "./app-input.js";

describe("app-input", () => {
  it("filterSlashCommands filters by name", () => {
    const commands = [
      { name: "help", description: "show help" },
      { name: "clear", description: "clear screen" },
    ];
    const result = filterSlashCommands("/he", commands as any);
    expect(result.length).toBe(1);
    expect(result[0].name).toBe("help");
  });

  it("reconcileSlashSelection returns state when commands match", () => {
    const commands = [{ name: "help", description: "show help" }];
    const result = reconcileSlashSelection("/he", commands as any);
    expect(result).toBeDefined();
    expect(result?.selectedIndex).toBe(0);
  });

  it("modelCommandQuery parses /model", () => {
    expect(modelCommandQuery("/model")).toBe("");
    expect(modelCommandQuery("/model gpt")).toBe("gpt");
  });

  it("filterModelOptions filters models", () => {
    const models = [
      { id: "gpt-4", label: "GPT-4", providerLabel: "OpenAI" },
      { id: "claude-3", label: "Claude 3", providerLabel: "Anthropic" },
    ];
    const result = filterModelOptions("/model gpt", models as any);
    expect(result.length).toBe(1);
  });
});
