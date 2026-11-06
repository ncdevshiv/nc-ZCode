/**
* Shape verification of preset product spec.
 *
* ⚠ Terminology: artifact = **User interface product** (the board that the script publishes to the user), not `RunSettlement.artifact`
* That top-level return value.
 *
* Why is the verification in the **engine core** instead of the driver: the preset declaration is not the driver at all (it has no executable effect),
* A spec with an illegal shape must be stopped before being dropped into the journal - the records in the journal are projected and cold recovered
* The only truth is that if a spec that cannot be drawn is dropped into the library, it will explode again on every reading surface.
 *
* Why is it handwritten instead of zod: This package has zero runtime dependencies (facade/compilation/engine all rely on typescript),
* And the shape of spec is as small as one screen. Adding a schema library to verify four objects requires changing the dependency premise of the entire pure package.
 *
* Return a human-readable sentence instead of a boolean: this message will fall into run via `ArtifactSpecInvalid`
* failure_json, the reader (model or author) needs to modify the script according to it, "invalid spec" cannot help.
 */

import { ARTIFACT_CAPS } from "../facade/artifact-caps.js";
import type { ArtifactPresetOp } from "../facade/registry.js";
import { canonicalJson } from "./hash.js";

/** An illegal reason; `undefined` means passed. */
type ArtifactSpecProblem = string | undefined;

/**
* Verify a preset spec. `op` determines the required fields, and all four types of public parts (length of title / description) are checked.
* Passed spec guarantees: it is a normal object, required fields are present and have the correct shape, and it does not exceed 8KB after normalization.
 */
export function validateArtifactSpec(op: ArtifactPresetOp, spec: unknown): ArtifactSpecProblem {
  if (!isPlainObject(spec)) return `${op} spec must be an object literal, got ${describe(spec)}`;

  const common = checkOptions(spec);
  if (common !== undefined) return common;

  const shape = checkShape(op, spec);
  if (shape !== undefined) return shape;

// Final check on the upper limit of volume: first give the specific errors in shape, and then talk about the size - a spec that is both deformed and too large, the author must first
// Know where the deformity is.
  const bytes = utf8ByteLength(canonicalJson(spec));
  if (bytes > ARTIFACT_CAPS.maxSpecSerializedBytes) {
    return `${op} spec is ${bytes} bytes serialized, over the ${ARTIFACT_CAPS.maxSpecSerializedBytes}-byte limit`;
  }
  return undefined;
}

function checkShape(op: ArtifactPresetOp, spec: Record<string, unknown>): ArtifactSpecProblem {
  if (op === "chart") {
    const x = checkField(spec.x, "x");
    if (x !== undefined) return x;
    const y = spec.y;
    if (Array.isArray(y)) {
      if (y.length === 0) return "chart spec y is an empty array; give it at least one series";
      for (const [index, entry] of y.entries()) {
        const problem = checkField(entry, `y[${index}]`);
        if (problem !== undefined) return problem;
      }
    } else {
      const problem = checkField(y, "y");
      if (problem !== undefined) return problem;
    }
    if (spec.type !== undefined && !["line", "bar", "scatter"].includes(String(spec.type))) {
      return `chart spec type must be "line", "bar" or "scatter", got ${describe(spec.type)}`;
    }
    if (spec.scale !== undefined && !["linear", "log"].includes(String(spec.scale))) {
      return `chart spec scale must be "linear" or "log", got ${describe(spec.scale)}`;
    }
    if (spec.baseline !== undefined) return checkField(spec.baseline, "baseline");
    return undefined;
  }
  if (op === "table") {
    const columns = checkFieldList(spec.columns, "columns");
    if (columns !== undefined) return columns;
    if (spec.key !== undefined && !isNonEmptyString(spec.key)) {
      return "table spec key must be a non-empty string (the field that identifies a row)";
    }
    return undefined;
  }
  if (op === "metrics") return checkFieldList(spec.metrics, "metrics");
  // board
  if (!isNonEmptyString(spec.key)) return "board spec key must be a non-empty string (the field that identifies a card)";
  if (!isNonEmptyString(spec.status)) {
    return "board spec status must be a non-empty string (the field that picks a card's column)";
  }
  if (!Array.isArray(spec.columns) || spec.columns.length === 0) {
    return "board spec columns must be a non-empty array of strings (the column order)";
  }
  for (const [index, column] of spec.columns.entries()) {
    if (!isNonEmptyString(column)) return `board spec columns[${index}] must be a non-empty string`;
  }
// cardTitle is the **field name** (which field is the card title), and ArtifactOptions.title (the board's own title)
// are two different things - they hit the same key before, and the name was changed precisely so that one board could say both.
  if (spec.cardTitle !== undefined && !isNonEmptyString(spec.cardTitle)) {
    return "board spec cardTitle must be a non-empty string (the field used as the card title)";
  }
  if (spec.detail !== undefined) return checkFieldList(spec.detail, "detail");
  return undefined;
}

/** Public display metadata (`ArtifactOptions`): type and length. */
function checkOptions(spec: Record<string, unknown>): ArtifactSpecProblem {
  if (spec.title !== undefined) {
    if (typeof spec.title !== "string") return `spec title must be a string, got ${describe(spec.title)}`;
    if (spec.title.length > ARTIFACT_CAPS.maxTitleLength) {
      return `spec title is ${spec.title.length} characters, over the ${ARTIFACT_CAPS.maxTitleLength} limit`;
    }
  }
  if (spec.description !== undefined) {
    if (typeof spec.description !== "string") {
      return `spec description must be a string, got ${describe(spec.description)}`;
    }
    if (spec.description.length > ARTIFACT_CAPS.maxDescriptionLength) {
      return `spec description is ${spec.description.length} characters, over the ` +
      `${ARTIFACT_CAPS.maxDescriptionLength} limit`;
    }
  }
  if (spec.primary !== undefined && typeof spec.primary !== "boolean") {
    return `spec primary must be a boolean, got ${describe(spec.primary)}`;
  }
  return undefined;
}

/** A non-empty list of `ArtifactField`. */
function checkFieldList(value: unknown, where: string): ArtifactSpecProblem {
  if (!Array.isArray(value) || value.length === 0) {
    return `spec ${where} must be a non-empty array of fields ({ field: "…" })`;
  }
  for (const [index, entry] of value.entries()) {
    const problem = checkField(entry, `${where}[${index}]`);
    if (problem !== undefined) return problem;
  }
  return undefined;
}

/** An `ArtifactField`: `field` must be a non-empty string (dot path), label / unit is optional. */
function checkField(value: unknown, where: string): ArtifactSpecProblem {
  if (!isPlainObject(value)) return `spec ${where} must be { field: "…" }, got ${describe(value)}`;
  if (!isNonEmptyString(value.field)) {
    return `spec ${where}.field must be a non-empty string (a dot path into the item, e.g. "timing.after")`;
  }
  for (const key of ["label", "unit"] as const) {
    if (value[key] !== undefined && typeof value[key] !== "string") {
      return `spec ${where}.${key} must be a string, got ${describe(value[key])}`;
    }
  }
  return undefined;
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isNonEmptyString(value: unknown): value is string {
  return typeof value === "string" && value.trim() !== "";
}

/** The actual value description in the error text (without printing the entire object - an 8KB spec would fill up failure_json). */
function describe(value: unknown): string {
  if (value === null) return "null";
  if (Array.isArray(value)) return `array (${value.length} items)`;
  if (typeof value === "object") return "object";
  if (typeof value === "string") return JSON.stringify(value.slice(0, 40));
  return String(value);
}

/** Number of UTF-8 bytes (same as the upper limit of report). */
function utf8ByteLength(value: string): number {
  return new TextEncoder().encode(value).length;
}
