const fs = require('fs');
const d = JSON.parse(fs.readFileSync('G:/ZCode-main/native/tools/tool-parity.table.json'));
const b = d.tools.filter(t=>t.status==='boundary');
fs.writeFileSync('G:/ZCode-main/boundary_list.txt', b.map(x=>x.name).join('\n'));
console.log('done', b.length);
