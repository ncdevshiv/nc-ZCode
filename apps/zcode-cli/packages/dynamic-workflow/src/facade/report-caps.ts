// @ts-nocheck
/**
* Upper bound constant for `report`.
 *
* and `world-read-caps.ts` are divided into two modules because the **execution sides of the two are different**: the upper limit of world-read is given by
* driver execution (only it can "not produce" - let ripgrep stop at 2000 entries), the upper limit of report is determined by **engine core**
* Execution (report is just the driver, it ends when the journal is dropped in the core). The same module will lead readers to think that they are composed of
* The same side is compulsory; since the numbers are all contracts, the two places are the same.
 *
* The strategy for overflow is to **fail the entire run** (`ReportCapExceeded`) instead of rejecting the node like world-read does.
* This is not a judgment of severity but of the fact that **channel denied**: `report` returns `void`, there is no place for the script to `catch`.
* Precisely because the script author cannot write the recovery path, these two numbers must be so wide that a reasonable script will never touch them.
 */

/** The number of reports per run and the upper limit of the number of serialized bytes in a single run. Numbers are contracts (see top of this module). */
export const REPORT_CAPS = {
/** The maximum number of `report` items in one run. */
  maxItemsPerRun: 256,
/** The maximum number of bytes after serialization of a single item. */
  maxItemSerializedBytes: 32 * 1024,
} as const;
