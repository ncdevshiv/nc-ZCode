/**
* `@zcode/dynamic-workflow-runtime`: sandbox harness.
 *
* Expose the harness entry, line protocol type, and entry file rendering and placement to the outside world (`renderChildEntry` /
* `writeChildEntryFile`, from which tests and tools create the same entry file as production), and the child process exit `childMain`.
* SEA hidden subcommand (`__zcode-dwf-child`) no longer imports this package: the entry file comes with childMain, and the subcommand only
* `import()` file and call its `start`.
 */

export { runWorkflowScript, type RunWorkflowOptions, type DriverFactory } from "./harness.js";
export {
  childMain,
  renderChildEntry,
  type ChildMainDeps,
  type ChildReadlineInterface,
  type ChildVmModule,
} from "./child-source.js";
export {
  childEntryFileName,
  fallbackWorkflowRunsDir,
  workflowRunsDir,
  writeChildEntryFile,
  type ChildEntryFile,
  type HarnessWarning,
  type WriteChildEntryFileInput,
} from "./child-entry-file.js";
export {
  type ChildMessage,
  type ChildPayload,
  type CompleteMessage,
  type CreateActorMessage,
  type EventMessage,
  type ParentMessage,
  type RequestMessage,
  type ResponseMessage,
  type WireError,
} from "./protocol.js";
