/**
 * Coding Plan composer surface.
 *
 * The Rust product build does not register `coding-plan-subscription` or account services;
 * the usage panel and its reset/celebration machinery stay out of the UI. Only the public
 * types and predicates are kept so existing composer wiring compiles and fails closed.
 */
import type { CodingPlanResetType } from "@zcode/shared";
import type {
  CodingPlanUsageAvailableProvider,
  CodingPlanUsageRemainingEntitlement,
} from "@/CodingPlanUsageRemainingPanel.js";
import type { useZCodeIntl } from "@/i18n/IntlProvider.js";
import type { SidebarUsageCodingPlanSourceId } from "@/lib/sidebarUsageCodingPlanProviderPreference.js";

export type ChatCodingPlanUsageRemainingConfig = {
  availableProviders: CodingPlanUsageAvailableProvider[];
  entitlements: CodingPlanUsageRemainingEntitlement[];
  onAccess?: () => Promise<void> | void;
  refreshing?: boolean;
  modelProvidersLoading: boolean;
  onEntitlementRefresh?: () => void | Promise<void>;
  onProviderChange?: (providerId: SidebarUsageCodingPlanSourceId) => void;
  onUsageClick?: () => void;
  selectedProviderId?: SidebarUsageCodingPlanSourceId;
};

/** Composer 触发器 hover 展开面板后要求补播撒花的自动完成 used_at,按重置类型定位到对应额度条。 */
export type CodingPlanQuotaResetAutoConfettiArms = Record<CodingPlanResetType, number | null>;

export function hasChatCodingPlanUsageRemaining(
  _config: ChatCodingPlanUsageRemainingConfig,
): boolean {
  return false;
}

export function ChatCodingPlanUsageRemainingPanel(_props: {
  config: ChatCodingPlanUsageRemainingConfig;
  intl: ReturnType<typeof useZCodeIntl>["intl"];
  locale: string;
  quotaResetDialogOpen?: boolean;
  separated?: boolean;
  autoCelebrateArm?: CodingPlanQuotaResetAutoConfettiArms | null;
  onAutoCelebrated?: (completedAt: number) => void;
  onQuotaResetDialogOpenChange?: (open: boolean) => void;
}): null {
  return null;
}
