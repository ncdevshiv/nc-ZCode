import { useMemo, useState } from "react";
import { isApiKeyAccess, resolveProviderTemplateName } from "@zcode/provider";
import { ArrowLeftIcon, Loader2Icon, PlusIcon } from "lucide-react";
import { Button } from "@/components/ui/button.js";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog.js";
import { Input } from "@/components/ui/input.js";
import { useProviderSettingsServiceView } from "@/hooks/useProviderSettingsView.js";
import { useServices } from "@/hooks/useServices.js";
import { useZCodeIntl } from "@/i18n/IntlProvider.js";
import { logger } from "@/logger.js";
import { NC_CODE_PRODUCT_NAME, ZCODE_CODING_PLAN_AVAILABLE } from "@/lib/productCapabilities.js";
import { ProviderLogo } from "@/settings/model-provider-section/ProviderLogo.js";
import { useZCodeStore } from "@/store/StoreProvider.js";

/**
 * First-run provider setup. Deliberately optional: the Rust backend does not register the
 * ZCode account/OAuth channels, so first launch must not block on a login screen. The user
 * either adds an API-key provider here or closes the dialog and keeps using the workspace.
 */
export function ProviderSetupDialog({
  open,
  onOpenChange,
  onOpenModelSettings,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onOpenModelSettings: () => void;
}) {
  const { intl, locale } = useZCodeIntl();
  const { providerSettingsService, modelSelectionService } = useServices();
  const { state, reload, commit } = useProviderSettingsServiceView(providerSettingsService);
  const markApiKeyLoginSuccess = useZCodeStore((store) => store.markApiKeyLoginSuccess);
  const [selectedTemplateId, setSelectedTemplateId] = useState<string | null>(null);
  const [apiKey, setApiKey] = useState("");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const templates = useMemo(() => {
    if (state.status !== "ready") return [];
    return state.view.providerTemplates.filter((template) => {
      if (!isApiKeyAccess(template.config.access)) return false;
      // Coding-plan account templates are out of the Rust product surface.
      return (
        ZCODE_CODING_PLAN_AVAILABLE || template.config.access.type !== "zhipu-coding-plan-api-key"
      );
    });
  }, [state]);

  const selectedTemplate =
    state.status === "ready"
      ? state.view.providerTemplates.find((template) => template.templateId === selectedTemplateId)
      : undefined;

  const close = () => {
    if (saving) return;
    onOpenChange(false);
  };

  const save = async () => {
    if (!selectedTemplate || !isApiKeyAccess(selectedTemplate.config.access)) return;
    const key = apiKey.trim();
    if (!key) {
      setError(intl.formatMessage({ id: "login.apiKey.emptyError" }));
      return;
    }
    setSaving(true);
    setError(null);
    try {
      const created = await providerSettingsService.createPersonalProvider({
        templateId: selectedTemplate.templateId,
        initialConfig: {
          access: { type: selectedTemplate.config.access.type, apiKey: key },
        },
      });
      const view = await modelSelectionService.getView();
      const preferred = view.providers.find(
        (provider) => provider.providerId === created.providerId,
      )?.models[0]?.modelId;
      markApiKeyLoginSuccess(preferred ?? null);
      commit(await providerSettingsService.getView());
      setApiKey("");
      setSelectedTemplateId(null);
      onOpenChange(false);
    } catch (saveError) {
      logger.error("[ProviderSetup] create provider failed", {
        templateId: selectedTemplate.templateId,
        error: saveError,
      });
      setError(
        intl.formatMessage(
          { id: "login.apiKey.saveError" },
          { error: saveError instanceof Error ? saveError.message : String(saveError) },
        ),
      );
    } finally {
      setSaving(false);
    }
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next) close();
        else onOpenChange(true);
      }}
    >
      <DialogContent className="max-w-md" data-testid="provider-setup-dialog" showCloseButton>
        <DialogHeader>
          <DialogTitle>
            {intl.formatMessage({ id: "providerSetup.title" }, { product: NC_CODE_PRODUCT_NAME })}
          </DialogTitle>
          <DialogDescription>
            {intl.formatMessage(
              { id: "providerSetup.description" },
              { product: NC_CODE_PRODUCT_NAME },
            )}
          </DialogDescription>
        </DialogHeader>

        {selectedTemplate ? (
          <div className="space-y-3">
            <button
              type="button"
              className="flex items-center gap-1.5 text-ui-base text-foreground-subtle hover:text-foreground"
              onClick={() => {
                setSelectedTemplateId(null);
                setError(null);
              }}
              disabled={saving}
            >
              <ArrowLeftIcon className="size-4" />
              {intl.formatMessage({ id: "providerSetup.back" })}
            </button>
            <div className="flex items-center gap-3 rounded-lg border border-border bg-surface px-4 py-3">
              <ProviderLogo logo={selectedTemplate.config.logo} className="size-8" />
              <div className="min-w-0 flex-1">
                <div className="truncate text-ui-base font-medium">
                  {resolveProviderTemplateName(
                    selectedTemplate.templateId,
                    selectedTemplate,
                    locale,
                  )}
                </div>
                <div className="truncate text-ui-sm text-foreground-subtle">
                  {intl.formatMessage({ id: "login.apiKey.providerLabel" })}
                </div>
              </div>
            </div>
            <Input
              autoFocus
              type="password"
              value={apiKey}
              onChange={(event) => setApiKey(event.target.value)}
              placeholder={intl.formatMessage({ id: "login.apiKey.placeholder" })}
              aria-label={intl.formatMessage({ id: "login.apiKey.placeholder" })}
              disabled={saving}
              onKeyDown={(event) => {
                if (event.key === "Enter") void save();
              }}
            />
            {error ? <p className="text-ui-base text-destructive">{error}</p> : null}
            <Button className="w-full" onClick={() => void save()} disabled={saving}>
              {saving ? <Loader2Icon className="size-4 animate-spin" /> : null}
              {intl.formatMessage({ id: "providerSetup.addProvider" })}
            </Button>
          </div>
        ) : state.status === "loading" ? (
          <div className="flex items-center justify-center gap-2 py-6 text-ui-base text-foreground-subtle">
            <Loader2Icon className="size-4 animate-spin" />
            {intl.formatMessage({ id: "common.loading" })}
          </div>
        ) : state.status === "error" ? (
          <div className="space-y-3">
            <p className="break-all text-ui-base text-destructive">{state.error.message}</p>
            <Button variant="outline" className="w-full" onClick={reload}>
              {intl.formatMessage({ id: "common.retry" })}
            </Button>
          </div>
        ) : (
          <div className="max-h-[50vh] space-y-2 overflow-y-auto">
            {templates.map((template) => (
              <button
                key={template.templateId}
                type="button"
                className="flex w-full items-center gap-3 rounded-lg border border-border bg-surface px-4 py-3 text-left transition-colors hover:border-border-hover hover:bg-hover"
                onClick={() => {
                  setSelectedTemplateId(template.templateId);
                  setError(null);
                }}
              >
                <ProviderLogo logo={template.config.logo} className="size-8" />
                <span className="min-w-0 flex-1 truncate text-ui-base font-medium">
                  {resolveProviderTemplateName(template.templateId, template, locale)}
                </span>
                <PlusIcon className="size-4 shrink-0 text-foreground-subtle" />
              </button>
            ))}
            {templates.length === 0 ? (
              <p className="py-4 text-center text-ui-base text-foreground-subtle">
                {intl.formatMessage({ id: "providerSetup.noTemplates" })}
              </p>
            ) : null}
          </div>
        )}

        <DialogFooter className="gap-2 sm:justify-between">
          <Button variant="ghost" onClick={close} disabled={saving}>
            {intl.formatMessage({ id: "providerSetup.skip" })}
          </Button>
          <Button variant="outline" onClick={onOpenModelSettings} disabled={saving}>
            {intl.formatMessage({ id: "providerSetup.openSettings" })}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
