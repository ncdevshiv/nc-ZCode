import io

# ------------------------------------------------------------- test ids
path = 'packages/shared/src/test-ids.ts'
s = io.open(path, encoding='utf-8').read()
anchor = '''/** v4 assistant row retry button (dynamic suffix is rowId) */
export const TID_V4_RETRY = "v4-retry";'''
addition = '''/** v4 assistant row measured throughput chip (dynamic suffix is rowId) */
export const TID_V4_MESSAGE_THROUGHPUT = "v4-message-throughput";
/** v4 assistant row retry button (dynamic suffix is rowId) */
export const TID_V4_RETRY = "v4-retry";'''
assert anchor in s
s = s.replace(anchor, addition, 1)
marker = '''export const TID_MODEL_PROVIDER_ADD_MODEL_BUTTON'''
idx = s.find(marker)
assert idx > 0
line_end = s.index('\n', idx)
addition2 = '''

/** Model Auto fetch: pull the remote directory and import/remove in batches */
export const TID_MODEL_PROVIDER_AUTO_FETCH_BUTTON = "model-provider-auto-fetch-button";
export const TID_MODEL_PROVIDER_AUTO_FETCH_DIALOG = "model-provider-auto-fetch-dialog";
export const TID_MODEL_PROVIDER_AUTO_FETCH_REFRESH = "model-provider-auto-fetch-refresh";
export const TID_MODEL_PROVIDER_AUTO_FETCH_SELECT_ALL = "model-provider-auto-fetch-select-all";
export const TID_MODEL_PROVIDER_AUTO_FETCH_ADD_ALL = "model-provider-auto-fetch-add-all";
export const TID_MODEL_PROVIDER_AUTO_FETCH_ADD_SELECTED = "model-provider-auto-fetch-add-selected";
export const TID_MODEL_PROVIDER_AUTO_FETCH_REMOVE_SELECTED = "model-provider-auto-fetch-remove-selected";
/** Single-line model selection (dynamic suffix is modelId) */
export const TID_MODEL_PROVIDER_AUTO_FETCH_ITEM = "model-provider-auto-fetch-item";
/** Actual measurement of single row capability (dynamic suffix is modelId) */
export const TID_MODEL_PROVIDER_AUTO_FETCH_PROBE = "model-provider-auto-fetch-probe";
/** Remove the added model in a single line (dynamic suffix is modelId) */
export const TID_MODEL_PROVIDER_AUTO_FETCH_REMOVE_ONE = "model-provider-auto-fetch-remove-one";'''
s = s[:line_end + 1] + addition2 + '\n' + s[line_end + 1:]io.open(path, 'w', encoding='utf-8', newline='').write(s)
print('test ids patched')

# ---------------------------------------------------------- assistant chip
path = 'packages/ui/src/v4/ConversationRowView.tsx'
s = io.open(path, encoding='utf-8').read()
s = s.replace('''export const ConversationAssistantTextActions = memo(function ConversationAssistantTextActions({
  rowId,
  entityId,
  text,
  createdAt,''',''''export const ConversationAssistantTextActions = memo(function ConversationAssistantTextActions({
  rowId,
  entityId,
  text,
  createdAt,
  throughput,'''')
s = s.replace(''' rowId: number;
  entityId?: string;
  text: string;
  createdAt: number;
  feedback?: AssistantMessageFeedback | null;''',''' rowId: number;
  entityId?: string;
  text: string;
  createdAt: number;
  /** Host measured tok/s; by default (old host / no streaming window) chip is not rendered. */
  throughput?: AssistantTextRow['throughput'];
  feedback?: AssistantMessageFeedback | null;''')
old = ''' {/* After the old conversation surface is deleted, the V4 action bar misses the message creation time;
          Time is a read-only derived display of row.createdAt, and no new renderer state is added. */}
      {timeLabel ? ('''
new = ''' {throughput ? (
        <span
          className="inline-flex select-none items-center gap-1 text-ui-sm text-foreground-subtlest"
          data-testid={testId(TID_V4_MESSAGE_THROUGHPUT, String(rowId))}
          title={throughputTitle(intl, throughput)}
        >
          <Gauge className="size-3.5" aria-hidden="true" />
          {intl.formatMessage(
            { id: "chat.message.throughput" },
            { tps: formatTokensPerSecond(throughput.tokensPerSecond) },
          )}
        </span>
      ) : null}
      {/* After the old conversation surface is deleted, the V4 action bar misses the message creation time;
          Time is a read-only derived display of row.createdAt, and no new renderer state is added. */}
      {timeLabel ? ('''
assert old in s
s = s.replace(old, new, 1)# helper functions + imports
s = s.replace('''import { useIsOfficeMode } from "@/hooks/useInterfaceMode.js";''','''import { useIsOfficeMode } from "@/hooks/useInterfaceMode.js";''')
# add Gauge to lucide import list
s = s.replace('''import {
  PencilIcon,''','''import {
  Gauge,
  PencilIcon,''', 1)
io.open(path, 'w', encoding='utf-8', newline='').write(s)
print('chip inserted')
