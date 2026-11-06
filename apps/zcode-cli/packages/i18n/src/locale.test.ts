import { describe, expect, it } from "bun:test";
import { detectLocale, isSupportedLocale, isUiLocale, resolveLocale, DEFAULT_LOCALE } from "./locale.js";

describe("i18n locale", () => {
  it("isSupportedLocale works", () => {
    expect(isSupportedLocale("en-US")).toBe(true);
    expect(isSupportedLocale("zh-CN")).toBe(true);
    expect(isSupportedLocale("fr-FR")).toBe(false);
  });

  it("isUiLocale works", () => {
    expect(isUiLocale("auto")).toBe(true);
    expect(isUiLocale("en-US")).toBe(true);
    expect(isUiLocale("invalid")).toBe(false);
  });

  it("resolveLocale with auto uses detected", () => {
    expect(resolveLocale("auto", "zh-CN")).toBe("zh-CN");
    expect(resolveLocale("auto", "en-US")).toBe("en-US");
  });

  it("resolveLocale with explicit locale", () => {
    expect(resolveLocale("zh-CN", "en-US")).toBe("zh-CN");
  });

  it("resolveLocale defaults when invalid", () => {
    expect(resolveLocale(undefined, undefined)).toBe(DEFAULT_LOCALE);
    expect(resolveLocale("auto", undefined)).toBe(DEFAULT_LOCALE);
  });

  it("detectLocale from env", () => {
    expect(detectLocale({ env: { LANG: "zh_CN.UTF-8" } })).toBe("zh-CN");
    expect(detectLocale({ env: { LC_ALL: "en_US.UTF-8" } })).toBe("en-US");
  });

  it("detectLocale from intl", () => {
    expect(detectLocale({ intlLocale: "zh-CN" })).toBe("zh-CN");
  });

  it("detectLocale handles LANGUAGE with colons", () => {
    expect(detectLocale({ env: { LANGUAGE: "zh_CN:en_US" } })).toBe("zh-CN");
  });
});
