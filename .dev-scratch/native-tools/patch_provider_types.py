import io

path = 'packages/provider/src/modelCatalog.ts'
s = io.open(path, encoding='utf-8').read()
s = s.replace('''import type { ProviderId, ModelId } from "./ids.js";

''', '')
s = s.replace('ProviderId', 'string').replace('ModelId', 'string')
io.open(path, 'w', encoding='utf-8', newline='').write(s)

path = 'packages/provider/src/index.ts'
s = io.open(path, encoding='utf-8').read()
if 'modelCatalog.js' not in s:
    s = s.replace('export * from "./model-selection-config.js";',
                  'export * from "./model-selection-config.js";\nexport * from "./modelCatalog.js";')
io.open(path, 'w', encoding='utf-8', newline='').write(s)
print('provider types ok')
