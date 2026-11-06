import io

# ------------------------------------------- ProviderCardSections: mount dialog
path = 'packages/ui/src/settings/model-provider-section/ProviderCardSections.tsx'
s = io.open(path, encoding='utf-8').read()

s = s.replace('''import { PresetProviderApiKeyBanner } from "./PresetProviderApiKeyBanner.js";''','''import { PresetProviderApiKeyBanner } from "./PresetProviderApiKeyBanner.js";
import { ProviderModelAutoFetchDialog } from "./ProviderModelAutoFetchDialog.js";''', 1)

# mount the dialog right after the models header block (inside same component return)
anchor = '''      {models.length > 0 ? (
        <div className="overflow-hidden rounded-lg border border-input-border bg-input">'''
assert anchor in s
mount = '''      <ProviderModelAutoFetchDialog
        open={autoFetchOpen}
        onOpenChange={setAutoFetchOpen}
        providerId={providerId}
      />
'''
s = s.replace(anchor, mount + anchor, 1)
io.open(path, 'w', encoding='utf-8', newline='').write(s)
print('dialog mounted')

# ------------------------------------------------------------ i18n (en + zh)
EN = [
    '  "settings.modelProvider.autoFetch.button": "Auto fetch",
    '  "settings.modelProvider.autoFetch.title": "Models on this endpoint',
    '  "settings.modelProvider.autoFetch.description": "Fetch the provider model list, review inferred capabilities, verify a model with a real request, then add or remove in one go.",
    '  "settings.modelProvider.autoFetch.search": "Filter models",
    '  "settings.modelProvider.autoFetch.loading": "Reading the remote model list...",
    '  "settings.modelProvider.autoFetch.empty": "No models match this filter.",
    '  "settings.modelProvider.autoFetch.refresh": "Refetch from the endpoint",
    '  "settings.modelProvider.autoFetch.selectAll": "Select all",
    '  "settings.modelProvider.autoFetch.added": "Added",
    '  "settings.modelProvider.autoFetch.source.catalog": "Provider metadata",
    '  "settings.modelProvider.autoFetch.source.heuristic": "Inferred",
    '  "settings.modelProvider.autoFetch.context": "{tokens, number} context",
    '  "settings.modelProvider.autoFetch.thinking": "Thinking",
    '  "settings.modelProvider.autoFetch.thinking.measured": "Thinking verified",
    '  "settings.modelProvider.autoFetch.image": "Image input",
    '  "settings.modelProvider.autoFetch.tools.measured": "Tools verified",
    '  "settings.modelProvider.autoFetch.tools.measuredNo": "Tools not returned",
    '  "settings.modelProvider.autoFetch.tools.inferred": "Tools likely",
    '  "settings.modelProvider.autoFetch.tools.inferredNo": "Tools unlikely",
    '  "settings.modelProvider.autoFetch.tools.unknown": "Tools unknown",
    '  "settings.modelProvider.autoFetch.unavailable": "Not reachable",
    '  "settings.modelProvider.autoFetch.verify": "Verify",
    '  "settings.modelProvider.autoFetch.removeOne": "Remove this model",
    '  "settings.modelProvider.autoFetch.counts": "{total, number} listed · {added, number} added",
    '  "settings.modelProvider.autoFetch.addAll": "Add all ({count, number})",
    '  "settings.modelProvider.autoFetch.addSelected": "Add selected ({count, number})",
    '  "settings.modelProvider.autoFetch.removeSelected": "Remove ({count, number})",
    '  "settings.modelProvider.autoFetch.purpose.chat": "Chat",
    '  "settings.modelProvider.autoFetch.purpose.code": "Code",
    '  "settings.modelProvider.autoFetch.purpose.vision": "Vision",
    '  "settings.modelProvider.autoFetch.purpose.reasoning": "Reasoning",
' "settings.modelProvider.autoFetch.purpose.embedding": "Embedding",
    ' "settings.modelProvider.autoFetch.purpose.rerank": "Rerank",
    ' "settings.modelProvider.autoFetch.purpose.guard": "Guard",
    ' "settings.modelProvider.autoFetch.purpose.image-generation": "Image generation",
    ' "settings.modelProvider.autoFetch.purpose.chart-generation": "Chart generation",
    ' "settings.modelProvider.autoFetch.purpose.speech": "Speech",
    ' "settings.modelProvider.autoFetch.purpose.audio": "Audio",
    ' "settings.modelProvider.autoFetch.purpose.audio-transcription": "Transcription",
    ' "settings.modelProvider.autoFetch.purpose.code-completion": "Completion",
    ' "settings.modelProvider.autoFetch.purpose.unknown": "Unclassified",
}
ZH = [
    ' "settings.modelProvider.autoFetch.button": "Smart Fetch",
    ' "settings.modelProvider.autoFetch.title": "List of models for this endpoint",
    ' "settings.modelProvider.autoFetch.description": "Read the server model list, view capability judgments, verify individual models with real requests, and then add or remove them with one click.",
    ' "settings.modelProvider.autoFetch.search": "Filter model",
    ' "settings.modelProvider.autoFetch.loading": "Reading the remote model list...",
    ' "settings.modelProvider.autoFetch.empty": "No matching model.",
    ' "settings.modelProvider.autoFetch.refresh": "Refetch the endpoint list",
    ' "settings.modelProvider.autoFetch.selectAll": "Select all",
    ' "settings.modelProvider.autoFetch.added": "Added",
    ' "settings.modelProvider.autoFetch.source.catalog": "Endpoint metadata",
    ' "settings.modelProvider.autoFetch.source.heuristic": "Inference",
    ' "settings.modelProvider.autoFetch.context": "Context {tokens, number}",
' "settings.modelProvider.autoFetch.thinking": "Thinking gear",
    ' "settings.modelProvider.autoFetch.thinking.measured": "Verified thinking output",
    ' "settings.modelProvider.autoFetch.image": "Image input",' "settings.modelProvider.autoFetch.tools.measured": "Verified tool call",
    ' "settings.modelProvider.autoFetch.tools.measuredNo": "Tool call not returned",
    ' "settings.modelProvider.autoFetch.tools.inferred": "Speculation support tools",
    ' "settings.modelProvider.autoFetch.tools.inferredNo": "It is speculated that tools are not supported",
    ' "settings.modelProvider.autoFetch.tools.unknown": "Tool capabilities are unknown",
    ' "settings.modelProvider.autoFetch.unavailable": "unavailable",
    ' "settings.modelProvider.autoFetch.verify": "Actual measurement",
    ' "settings.modelProvider.autoFetch.removeOne": "Remove the model",
    ' "settings.modelProvider.autoFetch.counts": "Total {total, number} · {added, number} added",
    ' "settings.modelProvider.autoFetch.addAll": "Add all ({count, number})",
    ' "settings.modelProvider.autoFetch.addSelected": "Add selected ({count, number})",
    ' "settings.modelProvider.autoFetch.removeSelected": "Remove ({count, number})",
    ' "settings.modelProvider.autoFetch.purpose.chat": "Conversation",
    ' "settings.modelProvider.autoFetch.purpose.code": "code",
    ' "settings.modelProvider.autoFetch.purpose.vision": "Vision",
    ' "settings.modelProvider.autoFetch.purpose.reasoning": "reasoning",
    ' "settings.modelProvider.autoFetch.purpose.embedding": "vector",
    ' "settings.modelProvider.autoFetch.purpose.rerank": "Rerank",
    ' "settings.modelProvider.autoFetch.purpose.guard": "security",
' "settings.modelProvider.autoFetch.purpose.image-generation": "Chart generation", ' "settings.modelProvider.autoFetch.purpose.chart-generation": "Chart generation",
    ' "settings.modelProvider.autoFetch.purpose.speech": "Speech synthesis",
    ' "settings.modelProvider.autoFetch.purpose.audio": "audio",
    ' "settings.modelProvider.autoFetch.purpose.audio-transcription": "Speech recognition",
    ' "settings.modelProvider.autoFetch.purpose.code-completion": "code completion",' "settings.modelProvider.autoFetch.purpose.unknown": "Uncategorized",
}

for path, block in (
    ('packages/ui/src/i18n/locales/en-US.ts', EN),
    ('packages/ui/src/i18n/locales/zh-CN.ts', ZH),
):
    s = io.open(path, encoding='utf-8').read()
    anchor = ' "settings.modelProvider.addModel":'
    idx = s.find(anchor)
    assert idx > 0, path
    line_start = s.rindex('\n', 0, idx) + 1
    insertion = '\n'.join(block.values())
    # Remove the leading space difference in key: just insert it line by line (the key is already indented with two spaces)
    s = s[:line_start] + insertion + '\n' + s[line_start:]
    io.open(path, 'w', encoding='utf-8', newline='').write(s)
    print('i18n keys added', path)