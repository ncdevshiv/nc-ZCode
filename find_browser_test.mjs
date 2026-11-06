import fs from 'fs';
import path from 'path';
function walk(dir) {
  const out=[];
  for (const f of fs.readdirSync(dir)) {
    const p = path.join(dir,f);
    const st = fs.statSync(p);
    if (st.isDirectory()) out.push(...walk(p));
    else if (f.endsWith('.rs')) out.push(p);
  }
  return out;
}
for (const f of walk('native/crates/nc-code-host/tests')) {
  const c = fs.readFileSync(f).toString();
  if (c.includes('Browser') || c.includes('"Browser"')) {
    console.log(f);
    process.exit(0);
  }
}
console.log('not found');
process.exit(1);
