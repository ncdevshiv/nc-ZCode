import { useCallback } from "react";
import type { ModelSelectionView } from "@zcode/provider";
import type { ModelSelection } from "@zcode/shared";

/** Start plan recommendation disabled - ZCode services removed */
export function useStartPlanRecommendation(
  _view: ModelSelectionView | null | undefined,
  _surface?: "subagent",
) {
  return useCallback(async (selection: ModelSelection): Promise<ModelSelection | null> => {
    return selection;
  }, []);
}
