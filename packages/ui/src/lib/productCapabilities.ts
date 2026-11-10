/**
 * Product capability switches for the Rust-backed desktop/web build.
 *
 * ZCode account, OAuth, Coding Plan, Start Plan and upgrade services are not registered by
 * `nc-code-app`, so the UI must not offer those surfaces. Keep this as a single UI-owned
 * constant instead of probing a removed channel at runtime: a failed probe produces the
 * "No account providers available" dead end seen on first launch.
 */
export const ZCODE_ACCOUNT_SERVICES_AVAILABLE = false;

/** Coding Plan / Start Plan / upgrade purchase surfaces are gone with the account services. */
export const ZCODE_CODING_PLAN_AVAILABLE = ZCODE_ACCOUNT_SERVICES_AVAILABLE;

/** Visible product name for user-facing copy added by the Rust/Tauri surface. */
export const NC_CODE_PRODUCT_NAME = "NC-Code";

/** localStorage key: the optional first-run provider prompt was closed by the user. */
export const PROVIDER_SETUP_DISMISSED_STORAGE_KEY = "nccode-provider-setup-dismissed";
