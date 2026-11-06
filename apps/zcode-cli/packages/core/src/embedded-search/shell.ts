import type { ExecutionShellSelection } from "@zcode/contracts";

// provider-visible embedded branch is enabled by default; the execution layer only explicitly supports POSIX
// Inject find()/grep() alias into the session shell of shell function.
const ENABLE_EMBEDDED_SEARCH_BASH_PRELUDE = true;

export function shouldInjectEmbeddedSearchBashPrelude(): boolean {
  return ENABLE_EMBEDDED_SEARCH_BASH_PRELUDE;
}

export function supportsEmbeddedSearchShellSelection(
  selection: ExecutionShellSelection | undefined,
): boolean {
  return selection?.dialect === "posix" || selection?.dialect === "git-bash";
}
