// @ts-nocheck
/**
* Upper bound constant for world-read.
 *
* **Execution in the driver, constants in the pure package**, there is a reason why the two things are separated: only the driver side can "not produce"——
* Letting ripgrep stop at 2000 entries is better than materializing a million and going back. But the constant belongs to the contract and not the driver
* Private: Use cases on the engine and compilation sides must assert based on them, and a number written only in the driver can only be copied for testing.
* So one day the two portions are not equal.
 *
* The overflow strategy is **Reject Node** (`WorldReadCapExceeded`, the script can `catch`), never truncate and add a flag.
* Truncation gives a quietly incomplete world view to the script, which will then use it to fan out - fan out is the expensive step.
 */

/** Upper limit of grep/git.diff/git.log. Numbers are contracts (see top of this module). */
export const WORLD_READ_CAPS = {
  /**
* The maximum number of matching files for `files.glob`. Previously, glob did not have its own cap, so the file system
* The default value of the port for UI tools (100, mtime descending order) takes effect silently - exactly what this registry wants to prohibit
* "Silent clip to upper limit". All world-read caps must have names here.
   */
  globMaxFiles: 2000,
/** The maximum number of hits in `files.grep`. */
  grepMaxMatches: 2000,
/** `files.grep` The maximum number of bytes after serialization of the result (and the upper limit of the number of entries **first come first rejected**). */
  grepMaxSerializedBytes: 256 * 1024,
/** The maximum number of bytes output by `git.diff`. */
  gitDiffMaxBytes: 512 * 1024,
/** `git.log` The maximum number of items that can be requested; if it exceeds the limit, it will be rejected in a structured manner instead of silently reaching the upper limit. */
  gitLogMaxCount: 100,
/** `git.log` The number of entries when count is not specified. */
  gitLogDefaultCount: 20,
/** Maximum number of bytes for `world.run` stdout (reject without truncation, cap+1 probe). */
  runStdoutMaxBytes: 256 * 1024,
/** Maximum number of bytes for `world.run` stderr (same as above). */
  runStderrMaxBytes: 256 * 1024,
/** `world.run` Wall clock (ms) when timeoutMs is not specified. **Unlimited Clamp**: Designed for real long-distance testing. */
  runDefaultTimeoutMs: 300_000,
} as const;
