import { join } from "node:path";
import type { SkillRoot } from "@zcode/contracts";

/**
* Two subtractions of dynamic workflow grayscale gate in App assembly layer.
* The subtraction of tool surface is in registerBuiltInTools of core, and the subtraction of `/` directory is in zcode-protocol/slash-commands.ts;
* Here are only the two items "command expansion" and "skill discovery" that require bootstrap side constant/path derivation.
 */

/** Custom command name that does not allow expansion when grayscale is turned off; `workflow` is provided by the zcode-guide built-in plug-in. */
export const DYNAMIC_WORKFLOW_GATED_COMMAND_NAMES: readonly string[] = ["workflow"];

/** The directory name of the workflow writing guide skill in the zcode-guide plug-in (skills/<dir>/SKILL.md). */
const DYNAMIC_WORKFLOW_SKILL_DIRECTORY_NAME = "dynamic-workflows";

const SKILL_MANIFEST_FILE_NAME = "SKILL.md";

/**
* Absolute path to SKILL.md to be excluded from skill discovery when grayscale is off.
 *
* Why filter by path instead of root: NodeSkillAdapter only provides the `disabledPaths` elimination mechanism
* (config.json's `skill.<path>.enable=false` also uses this), and the other skills of zcode-guide
* (Configuration guide, various self-diagnostics) have nothing to do with grayscale and must continue to be visible - removing the entire root will connect them.
 *
* Why not match zcode-guide exactly by pluginId: the plugin id is in the form of `<name>@<marketplace>`, which is installed with
* The source changes; when the path deduced here does not exist, it is just a Set member that never hits, without side effects.
 */
export function collectDynamicWorkflowDisabledSkillPaths(
  pluginSkillRoots: readonly SkillRoot[],
): string[] {
  return pluginSkillRoots.map((root) =>
    join(root.path, DYNAMIC_WORKFLOW_SKILL_DIRECTORY_NAME, SKILL_MANIFEST_FILE_NAME),
  );
}
