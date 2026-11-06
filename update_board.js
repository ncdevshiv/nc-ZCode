const fs = require('fs');
const path = 'G:/ZCode-main/.nc-tools/board.md';
const content = fs.readFileSync(path, 'utf8');
const now = new Date().toISOString();
// Add agent
const agent = {
  "id": "SVC-PORT-001",
  "name": "Services Porting Agent",
  "role": "Backend Developer",
  "parent": "SD-001",
  "status": "working",
  "registered_at": now
};
// Insert into JSON array
const treeStart = content.indexOf('```json');
const treeEnd = content.indexOf('```', treeStart + 7);
const treeContent = content.substring(treeStart + 7, treeEnd).trim();
const agents = JSON.parse(treeContent);
agents.push(agent);
const newTree = JSON.stringify(agents, null, 2);
let newContent = content.substring(0, treeStart + 7) + '\n' + newTree + '\n' + content.substring(treeEnd);
const msg = '- [' + now + '] SVC-PORT-001 (Backend Developer): Registered as Services Porting Agent under SD-001. Status: working. Task: Port all TS-only service modules (596 out of 610). Start with packages/services/src/core shared code, then work systematically. Each service needs full Rust implementation + unit tests + integration tests. Follow dependency order. Update channel implementations in Rust. No mocks/stubs. Keep plugins/skills intact. Report progress to board.';
newContent = newContent.replace('## Message Log\n', '## Message Log\n' + msg + '\n\n');
fs.writeFileSync(path, newContent);
console.log('done');
