import io

EN = [
    '  "settings.modelProvider.autoFetch.button": "Auto fetch",',
    '  "settings.modelProvider.autoFetch.title": "Models on this endpoint",',
    '  "settings.modelProvider.autoFetch.description": "Read the provider model list, review capability hints, verify a model with a real request, then add or remove in bulk.",',
    '  "settings.modelProvider.autoFetch.search": "Filter models",',
    '  "settings.modelProvider.autoFetch.loading": "Reading the remote model list...",',
    '  "settings.modelProvider.autoFetch.empty": "No model matches this filter.",',
    '  "settings.modelProvider.autoFetch.refresh": "Refetch from the endpoint",',
    '  "settings.modelProvider.autoFetch.selectAll": "Select all",',
    '  "settings.modelProvider.autoFetch.added": "Added",',
    '  "settings.modelProvider.autoFetch.source.catalog": "Endpoint metadata",',
    '  "settings.modelProvider.autoFetch.source.heuristic": "Inferred",',
    '  "settings.modelProvider.autoFetch.context": "context {tokens, number}",',
    '  "settings.modelProvider.autoFetch.thinking": "Thinking",',
    '  "settings.modelProvider.autoFetch.thinking.measured": "Thinking verified",',
    '  "settings.modelProvider.autoFetch.image": "Image input",',
    '  "settings.modelProvider.autoFetch.tools.measured": "Tools verified",',
    '  "settings.modelProvider.autoFetch.tools.measuredNo": "No tool call returned",',
    '  "settings.modelProvider.autoFetch.tools.inferred": "Tools likely",',
    '  "settings.modelProvider.autoFetch.tools.inferredNo": "Tools unlikely",',
    '  "settings.modelProvider.autoFetch.tools.unknown": "Tools unknown",',
    '  "settings.modelProvider.autoFetch.unavailable": "Not reachable",',
    '  "settings.modelProvider.autoFetch.verify": "Verify",',
' "settings.modelProvider.autoFetch.removeOne": "Remove this model",',
    ' "settings.modelProvider.autoFetch.counts": "{total, number} listed - {added, number} added",',
    ' "settings.modelProvider.autoFetch.addAll": "Add all ({count, number})",',
    ' "settings.modelProvider.autoFetch.addSelected": "Add selected ({count, number})",',
    ' "settings.modelProvider.autoFetch.removeSelected": "Remove ({count, number})",',
    ' "settings.modelProvider.autoFetch.purpose.chat": "Chat",',
    ' "settings.modelProvider.autoFetch.purpose.code": "Code",',
    ' "settings.modelProvider.autoFetch.purpose.vision": "Vision",',
    ' "settings.modelProvider.autoFetch.purpose.reasoning": "Reasoning",',
    ' "settings.modelProvider.autoFetch.purpose.embedding": "Embedding",',
    ' "settings.modelProvider.autoFetch.purpose.rerank": "Rerank",',
    ' "settings.modelProvider.autoFetch.purpose.guard": "Guard",',
    ' "settings.modelProvider.autoFetch.purpose.image-generation": "Image generation",',
    ' "settings.modelProvider.autoFetch.purpose.chart-generation": "Chart generation",',
    ' "settings.modelProvider.autoFetch.purpose.speech": "Speech",',
    ' "settings.modelProvider.autoFetch.purpose.audio": "Audio",',
    ' "settings.modelProvider.autoFetch.purpose.audio-transcription": "Transcription",',
    ' "settings.modelProvider.autoFetch.purpose.code-completion": "Completion",',
    ' "settings.modelProvider.autoFetch.purpose.unknown": "Unclassified",',
]

ZH = [
    ' "settings.modelProvider.autoFetch.button": "Smart Fetch",',
    ' "settings.modelProvider.autoFetch.title": "Model list for this endpoint",',
    ' ' + '"settings.modelProvider.autoFetch.description": "Read the server model list, check the capabilities, test individual models with real requests, and then add or remove them in batches.",',
    ' "settings.modelProvider.autoFetch.search": "Filter model",',' "settings.modelProvider.autoFetch.loading": "Reading the remote model list...",',
    ' "settings.modelProvider.autoFetch.empty": "No matching model.",',
    ' "settings.modelProvider.autoFetch.refresh": "Refetch the endpoint list",',
    ' "settings.modelProvider.autoFetch.selectAll": "Select All",',
    ' "settings.modelProvider.autoFetch.added": "Added",',
    ' "settings.modelProvider.autoFetch.source.catalog": "Endpoint metadata",',
    ' "settings.modelProvider.autoFetch.source.heuristic": "Inference",',
    ' "settings.modelProvider.autoFetch.context": "Context {tokens, number}",',
    ' "settings.modelProvider.autoFetch.thinking": "Thinking gear",',
    ' "settings.modelProvider.autoFetch.thinking.measured": "Thinking output has been measured",',
    ' "settings.modelProvider.autoFetch.image": "Image input",',
    ' "settings.modelProvider.autoFetch.tools.measured": "Measured tool calls",',
    ' "settings.modelProvider.autoFetch.tools.measuredNo": "Tool call not returned",',
    ' "settings.modelProvider.autoFetch.tools.inferred": "Speculation support tools",',
    ' "settings.modelProvider.autoFetch.tools.inferredNo": "It is speculated that tools are not supported",',
    ' "settings.modelProvider.autoFetch.tools.unknown": "Tool capabilities are unknown",',
    ' "settings.modelProvider.autoFetch.unavailable": "Unavailable",',
    ' "settings.modelProvider.autoFetch.verify": "Actual measurement",',
    ' "settings.modelProvider.autoFetch.removeOne": "Remove this model",',
' "settings.modelProvider.autoFetch.counts": "Total {total, number} · {added, number} added",',
    ' "settings.modelProvider.autoFetch.addAll": "Add all ({count, number})",',
    ' "settings.modelProvider.autoFetch.addSelected": "Add selected ({count, number})",',
    ' "settings.modelProvider.autoFetch.removeSelected": "Remove ({count, number})",',
    ' "settings.modelProvider.autoFetch.purpose.chat": "Conversation",',
    ' "settings.modelProvider.autoFetch.purpose.code": "code",',' "settings.modelProvider.autoFetch.purpose.vision": "Vision",',
' "settings.modelProvider.autoFetch.purpose.reasoning": "reasoning",',
    ' "settings.modelProvider.autoFetch.purpose.embedding": "vector",',
    ' "settings.modelProvider.autoFetch.purpose.rerank": "Rerank",',
    ' "settings.modelProvider.autoFetch.purpose.guard": "Safety guardrail",',
    '"settings.modelProvider.autoFetch.purpose.image-generation": "文生图",',
    ' "settings.modelProvider.autoFetch.purpose.chart-generation": "Chart generation",',
    ' "settings.modelProvider.autoFetch.purpose.speech": "Speech synthesis",',
    ' "settings.modelProvider.autoFetch.purpose.audio": "audio",',
    ' "settings.modelProvider.autoFetch.purpose.audio-transcription": "Speech recognition",',
    ' "settings.modelProvider.autoFetch.purpose.code-completion": "code completion",',
    ' "settings.modelProvider.autoFetch.purpose.unknown": "Uncategorized",',]TPS = { 'packages/ui/src/i18n/locales/en-US.ts': [ ' "chat.message.throughput": "{tps} tok/s",', ' "chat.message.throughput.tooltip": "Measured output throughput: {tokens} tokens in {elapsedMs} ms across {chunks} stream chunks (window={window}).{estimated, select, true { Token count is estimated from characters because the provider returned no usage.} other {}}",',    ],
    'packages/ui/src/i18n/locales/zh-CN.ts': [
        ' "chat.message.throughput": "{tps} tok/s",',
        ' "chat.message.throughput.tooltip": "Measured output throughput: {tokens} tokens / {elapsedMs} ms ({chunks} streaming chunks, window size = {window}). {estimated, select, true { provider did not return usage, the number of tokens is estimated by characters.} other {}}",',
    ],
}

# If TPS copy has already been inserted, it will be skipped to avoid duplicate keys.
for path, lines in (
    ('packages/ui/src/i18n/locales/en-US.ts', EN),
    ('packages/ui/src/i18n/locales/zh-CN.ts', ZH),
):
    s = io.open(path, encoding='utf-8').read()    if 'settings.modelProvider.autoFetch.button' in s:
        print('already inserted', path)
        continue
    anchor = '};\n\nexport default'
    idx = s.rindex(anchor)
    s = s[:idx] + '\n'.join(lines) + '\n' + s[idx:]
    io.open(path, 'w', encoding='utf-8', newline='').write(s)
    print('autoFetch keys inserted', path)

for path, lines in TPS.items():
    s = io.open(path, encoding='utf-8').read()
    if 'chat.message.throughput' in s:
        print('already inserted', path)
        continue
    anchor = '};\n\nexport default'
    idx = s.rindex(anchor)
    s = s[:idx] + '\n'.join(lines) + '\n' + s[idx:]
    io.open(path, 'w', encoding='utf-8', newline='').write(s)
    print('tps keys inserted', path)
