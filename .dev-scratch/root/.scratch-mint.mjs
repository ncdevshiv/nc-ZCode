import { z } from 'zod';
import * as fs from 'node:fs';

const mod = await import('./packages/shared/dist/zcode-protocol/index.js');
const names = [
  'zcodeSessionStateSnapshotSchema',
  'zcodeSessionInfoSchema',
  'zcodeSessionSettingsStateSchema',
  'zcodeSessionProjectionSchema',
  'zcodeSessionRuntimeStateSchema',
  'zcodeMessageWithPartsSchema',
];
const out = {};
for (const n of names) {
  const s = mod[n];
  if (!s) {
    out[n] = 'MISSING';
    continue;
  }
  try {
    out[n] = z.toJSONSchema(s, { io: 'output', unrepresentable: 'any' });
  } catch (error) {
    out[n] = 'ERR ' + error.message;
  }
}
fs.writeFileSync('.scratch-snapshot-schema.json', JSON.stringify(out, null, 2));
console.log('wrote', JSON.stringify(out).length, 'bytes');
