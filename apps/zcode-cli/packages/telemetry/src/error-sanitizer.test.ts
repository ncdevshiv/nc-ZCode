import { describe, expect, it } from "bun:test";
import { sanitizeTelemetryError, sanitizeErrorMessage, claimSanitizedTelemetryError } from "./error-sanitizer.js";

describe("error-sanitizer", () => {
  it("sanitizes basic error", () => {
    const error = new Error("test error");
    const result = sanitizeTelemetryError(error);
    expect(result.type).toBe("Error");
    expect(result.message).toBe("test error");
  });

  it("sanitizes error with code", () => {
    const error = { name: "CustomError", message: "fail", code: "ERR_123" };
    const result = sanitizeTelemetryError(error);
    expect(result.type).toBe("CustomError");
    expect(result.code).toBe("ERR_123");
    expect(result.message).toBe("fail");
  });

  it("redacts URLs in messages", () => {
    const msg = "Failed to connect to https://example.com/api/secret";
    const sanitized = sanitizeErrorMessage(msg);
    expect(sanitized).not.toContain("example.com/api/secret");
  });

  it("redacts authorization headers", () => {
    const msg = "Request failed with Authorization: Bearer secret123token";
    const sanitized = sanitizeErrorMessage(msg);
    expect(sanitized).not.toContain("secret123token");
  });

  it("redacts API keys", () => {
    const msg = "api_key=sk-1234567890abcdef";
    const sanitized = sanitizeErrorMessage(msg);
    expect(sanitized).toContain("{redacted}");
  });

  it("redacts emails", () => {
    const msg = "User test@example.com failed";
    const sanitized = sanitizeErrorMessage(msg);
    expect(sanitized).toContain("{email}");
  });

  it("claimSanitizedTelemetryError claims errors", () => {
    const error = new Error("test");
    const first = claimSanitizedTelemetryError(error);
    expect(first).toBeDefined();
    const second = claimSanitizedTelemetryError(error);
    expect(second).toBeUndefined();
  });
});
