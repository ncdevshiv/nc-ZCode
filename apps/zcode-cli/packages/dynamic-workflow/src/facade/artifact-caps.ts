// @ts-nocheck
/**
* The upper limit constant of the artifact.
 *
* ⚠ Terminology: The artifact here is a **user interface product** - a file published by a script to the user via `artifact.*` /
* markdown / preset Kanban, not the artifact inside the engine (`RunSettlement.artifact`,
* The typed output value of `analysis/artifact-types.ts`, which is the top-level return value shown to the model).
 *
* It is also a module of its own as `report-caps.ts` / `world-read-caps.ts`, for the same reason: this batch of numbers
* **The execution side is split** - the id number and version number are executed by the engine core (it only knows how many versions of this id there are in the journal),
* The number of bytes and text length are performed by the driver (only it can read the file and write to the store). Modules placed on either side will cause
* The reader assumes that they are enforced by the same side; and the numbers are contracted to be the same on both sides, so they must live in pure packs, and have names,
* Let the engine test and driver test assert the same constant instead of copying each one.
 *
* Overflow strategy **Split by member family** (error code): content member (`file` / `markdown`) returned
* promise, the script has somewhere to catch, so the limit exceedance is **Node-level rejection** (`ArtifactTooLarge` /
* `ArtifactVersionCapExceeded` / `ArtifactCapExceeded`); the preset member returns void, there is no rejection channel,
* So the same upper limit for that family is failRun. The same argument as the world-read / report demarcation.
 */

/** The quantity, volume and text limit of the product. Numbers are contracts (see top of this module). */
export const ARTIFACT_CAPS = {
/** There can be up to several different product IDs in one run. */
  maxArtifactsPerRun: 32,
/** The maximum number of versions a single ID can publish (each successful publication by a content member = one version). */
  maxVersionsPerArtifact: 16,
/** Maximum number of bytes for `artifact.file` (same value as PROTOCOL_V4_LIMITS.attachmentMaxBytes). */
  maxFileBytes: 20 * 1024 * 1024,
/** Maximum number of bytes (UTF-8) for `artifact.markdown`. */
  maxMarkdownBytes: 256 * 1024,
/** Maximum number of characters for `opts.title`. */
  maxTitleLength: 120,
/** Maximum number of characters for `opts.description`. */
  maxDescriptionLength: 500,
/** The maximum number of bytes after presetting spec normalized JSON. */
  maxSpecSerializedBytes: 8 * 1024,
/** Maximum number of characters for product id. */
  maxIdLength: 64,
} as const;

/**
* Character set of legal product id: `[A-Za-z0-9_.-]`, not empty.
 *
* Why is it so narrow: id is **cross-face identity**——journal's `dwf_node.artifact_id` column, store's
* `toolCallId`, v4 query params, and side panel tab keys all use it as a key. Slash, space, percent sign in any of them
* will become an escape dispute, and an escape dispute resolved once in each place is four answers.
 *
* Without `g` flag: Regular expressions with `g` retain `lastIndex` between `test`, and the same id will be tested twice in a row to get different answers.
 */
export const ARTIFACT_ID_PATTERN = /^[A-Za-z0-9_.-]+$/;
