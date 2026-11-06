import io

# --------------------------------------------- ConversationRowView wiring
path = 'packages/ui/src/v4/ConversationRowView.tsx'
s = io.open(path, encoding='utf-8').read()

s = s.replace('''' TID_V4_FORK,'''','''' TID_V4_FORK,
  TID_V4_MESSAGE_THROUGHPUT,'''', 1)

# helpers right before the actions component
old = '''export const ConversationAssistantTextActions = memo(function ConversationAssistantTextActions({'''
new = '''/** tok/s display: >100 does not retain decimals, >10 has one digit, and the remaining two digits - avoid the numbers in narrow columns from being long or short. */
export function formatTokensPerSecond(value: number): string {
  if (!Number.isFinite(value) || value <= 0) return "0";
  if (value >= 100) return Math.round(value).toString();
  if (value >= 10) return value.toFixed(1);
  return value.toFixed(2);
}

function throughputTitle(
  intl: { formatMessage(descriptor: { id: string }, values?: Record<string, unknown>): string },
  throughput: NonNullable<AssistantTextRow["throughput"]>,
): string {
  return intl.formatMessage(
    { id: "chat.message.throughput.tooltip" },
    {
      tokens: throughput.completionTokens,
      elapsedMs: throughput.elapsedMs,
      chunks: throughput.chunks ?? 0,
      window: throughput.windowSource,
      estimated: throughput.estimated === true,
    },
  );
}

export const ConversationAssistantTextActions = memo(function ConversationAssistantTextActions({'''
assert old in s
s = s.replace(old, new, 1)

# row-level call site passes measured throughput
old = ''' <ConversationAssistantTextActions
          rowId={row.rowId}
          entityId={row.entityId}
          text={copyText ?? row.text}
          createdAt={row.createdAt}'''
new = ''' <ConversationAssistantTextActions
          rowId={row.rowId}
          entityId={row.entityId}
          text={copyText ?? row.text}createdAt={row.createdAt}
          throughput={row.throughput}'''
assert old in s
s = s.replace(old, new, 1)
io.open(path, 'w', encoding='utf-8', newline='').write(s)
print('ConversationRowView wired')

# --------------------------------------------- TurnGroup (Turning Toolbar) wiring
path = 'packages/ui/src/v4/ConversationTurnGroup.tsx'
s = io.open(path, encoding='utf-8').read()
old = ''' <ConversationAssistantTextActions
              rowId={latestAssistantTextRow.rowId}
              entityId={latestAssistantTextRow.entityId}
              text={assistantCopyText}
              createdAt={latestAssistantTextRow.createdAt}'''
new = ''' <ConversationAssistantTextActions
              rowId={latestAssistantTextRow.rowId}
              entityId={latestAssistantTextRow.entityId}
              text={assistantCopyText}
              createdAt={latestAssistantTextRow.createdAt}
              throughput={latestAssistantTextRow.throughput}'''
assert old in s
s = s.replace(old, new, 1)
io.open(path, 'w', encoding='utf-8', newline='').write(s)
print('ConversationTurnGroup wired')

#-------------------------------------------------------------i18n messages
for path, entries in (
    ('packages/ui/src/i18n/locales/en-US.ts', ''' "chat.message.throughput": "{tps} tok/s",
  "chat.message.throughput.tooltip": "Measured output throughput: {tokens} tokens in {elapsedMs} ms ({chunks} stream chunks, window={window}).{estimated, select, true { Token count is estimated from characters because the provider returned no usage.} other {}}",'''),
    ('packages/ui/src/i18n/locales/zh-CN.ts', ''' "chat.message.throughput": "{tps} tok/s","chat.message.throughput.tooltip": "Measured output throughput: {tokens} tokens / {elapsedMs} ms ({chunks} streaming chunks, window size = {window}). {estimated, select, true { provider did not respond to usage, the number of tokens is estimated by characters.} other {}}",'''),
):
    s = io.open(path, encoding='utf-8').read()
    anchor = ' "chat.message.copy":'
    idx = s.find(anchor)
    assert idx > 0, path
    line_end = s.index('\n', idx) + 1
    s = s[:line_end] + entries + '\n' + s[line_end:]
    io.open(path, 'w', encoding='utf-8', newline='').write(s)
    print('i18n patched', path)