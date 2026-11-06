import * as fs from 'fs';
import * as path from 'path';

const tsServicesDir = 'G:/ZCode-main/packages/services/src';
const rustCratesDir = 'G:/ZCode-main/native/crates';

// Get TS service modules (directories and top-level files)
const entries = fs.readdirSync(tsServicesDir, { withFileTypes: true });
const tsServices = [];
for (const e of entries) {
  if (e.isDirectory()) {
    tsServices.push({ type: 'dir', name: e.name });
  } else if (e.isFile() && e.name.endsWith('.ts') && !e.name.endsWith('.d.ts')) {
    tsServices.push({ type: 'file', name: e.name });
  }
}
tsServices.sort((a,b) => a.name.localeCompare(b.name));
console.log('=== TS Services in packages/services/src ===');
for (const s of tsServices) {
  console.log(s.type + ': ' + s.name);
}
console.log(`\nTotal: ${tsServices.length}`);

// Get Rust service crates
const rustCrates = fs.readdirSync(rustCratesDir).filter(f => f.startsWith('nc-code-services-'));
rustCrates.sort();
console.log('\n=== Rust service crates ===');
for (const c of rustCrates) {
  console.log(c);
}
console.log(`\nTotal: ${rustCrates.length}`);
