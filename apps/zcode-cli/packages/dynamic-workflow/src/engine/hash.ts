/**
* Deterministic hashing included in the package: FNV-1a (32-bit) on normalized JSON.
* No need for node:crypto - src/ remains pure and portable, the hash is only used as a defensive consistency check during replay
* (Not for cryptography purposes, the anti-collision requirements are low, determinism and portability are the key points).
 */

/**
* Normalized JSON serialization: Object keys are ordered by code point, giving stable endianness for semantically identical values.
* Only override the JSON value (string/number/boolean/null/array/plain object) used by the host call.
* Non-JSON values ​​such as undefined / functions should not appear on this path; serialized to "null" when encountered to maintain full functionality.
 */
export function canonicalJson(value: unknown): string {
  if (value === null || typeof value !== "object") {
// Basic types: handed to JSON.stringify (number/boolean/string), undefined/function falls back to null.
    const s = JSON.stringify(value);
    return s === undefined ? "null" : s;
  }
  if (Array.isArray(value)) {
    return `[${value.map((v) => canonicalJson(v)).join(",")}]`;
  }
  const obj = value as Record<string, unknown>;
  const keys = Object.keys(obj).sort();
  const parts: string[] = [];
  for (const key of keys) {
    const v = obj[key];
// Skip undefined members, consistent with JSON.stringify object semantics.
    if (v === undefined) continue;
    parts.push(`${JSON.stringify(key)}:${canonicalJson(v)}`);
  }
  return `{${parts.join(",")}}`;
}

/** FNV-1a 32-bit hash, outputs an 8-digit hexadecimal string. */
export function fnv1a(input: string): string {
// FNV offset basis/prime (32 bits). Guaranteed 32-bit multiplication wrapping with Math.imul.
  let hash = 0x811c9dc5;
  for (let i = 0; i < input.length; i++) {
    hash ^= input.charCodeAt(i) & 0xff;
// The high-order bytes are also included to avoid hashing only the low-order bytes and losing the distinction of multi-byte characters.
    hash ^= (input.charCodeAt(i) >> 8) & 0xff;
    hash = Math.imul(hash, 0x01000193);
  }
// Convert to unsigned and zero-padd to 8 hexadecimal digits.
  return (hash >>> 0).toString(16).padStart(8, "0");
}

/** Computes a defensive hash of a host call input: FNV-1a for normalized JSON. */
export function inputHash(value: unknown): string {
  return fnv1a(canonicalJson(value));
}
