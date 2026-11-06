import { describe, expect, it } from "bun:test";
import { NodeReplSession } from "./node-repl-session.js";

describe("NodeReplSession", () => {
  it("creates a session", () => {
    const session = new NodeReplSession();
    expect(session).toBeDefined();
  });

  it("runs simple code", async () => {
    const session = new NodeReplSession();
    const result = await session.run("1 + 2");
    expect(result.result).toBe("3");
  });

  it("handles syntax errors", async () => {
    const session = new NodeReplSession();
    const result = await session.run("1 +");
    expect(result.error).toBeDefined();
  });

  it("disposes session", () => {
    const session = new NodeReplSession();
    session.dispose();
    expect(session).toBeDefined();
  });
});
