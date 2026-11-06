import io

path = 'packages/ui/src/settings/model-provider-section/ProviderCardSections.tsx'
s = io.open(path, encoding='utf-8').read()

s = s.replace('''import {
  TID_MODEL_PROVIDER_ADD_MODEL_BUTTON,''','''import {
  TID_MODEL_PROVIDER_ADD_MODEL_BUTTON,
  TID_MODEL_PROVIDER_AUTO_FETCH_BUTTON,''', 1)

s = s.replace('''import { InfoIcon, LockKeyholeIcon, Plus, Pencil, Trash2, MoreHorizontal } from "lucide-react";''','''import {
  InfoIcon,
  LockKeyholeIcon,
  Plus,
  Pencil,
  Sparkles,
  Trash2,
  MoreHorizontal,
} from "lucide-react";''', 1)

old = '''        <Button
          type="button"
          variant="secondary"
          size="default"
          className="rounded-lg"
          data-testid={TID_MODEL_PROVIDER_ADD_MODEL_BUTTON}
          onClick={openAddDialog}
        >
          <Plus data-icon="inline-start" aria-hidden="true" />
          {intl.formatMessage({ id: "settings.modelProvider.addModel" })}
        </Button>
      </div>'''
new = '''        <div className="flex flex-wrap items-center gap-2">
          <Button
            type="button"
            variant="secondary"
            size="default"
            className="rounded-lg"
            data-testid={TID_MODEL_PROVIDER_ADD_MODEL_BUTTON}
            onClick={openAddDialog}
          >
            <Plus data-icon="inline-start" aria-hidden="true" />
            {intl.formatMessage({ id: "settings.modelProvider.addModel" })}
          </Button>
          <Button
            type="button"
            variant="secondary"
            size="default"
            className="rounded-lg"
            data-testid={TID_MODEL_PROVIDER_AUTO_FETCH_BUTTON}
            onClick={() => setAutoFetchOpen(true)}
          >
            <Sparkles data-icon="inline-start" aria-hidden="true" />
{intl.formatMessage({ id: "settings.modelProvider.autoFetch.button" })}
          </Button>
        </div>
      </div>'''
assert old in s
s = s.replace(old, new, 1)

s = s.replace(''' const [addDialogOpen, setAddDialogOpen] = useState(false);''',''' const [addDialogOpen, setAddDialogOpen] = useState(false);
  const [autoFetchOpen, setAutoFetchOpen] = useState(false);''', 1)

# Hang the dialog box at the end of the component: find the closing position of the return in this block (after ProviderModelMetadataDialog)
old_tail = ''' {models.length > 0 ? ('''
assert old_tail in s

io.open(path, 'w', encoding='utf-8', newline='').write(s)
print('card sections button wired')