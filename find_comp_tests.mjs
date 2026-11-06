import fs from 'fs';
const s = fs.readFileSync('G:/ZCode-main/native/crates/nc-code-host/src/tools/computer.rs').toString();
const lines = s.split('\n');
const tests = [];
for (let i=0; i<lines.length; i++) {
  if (lines[i].includes('#[test]') || lines[i].includes('#[tokio::test]')) {
    for (let j=i; j<i+5 && j<lines.length; j++) {
      if (lines[j].includes('fn ')) {
        const m = lines[j].match(/fn\s+([a-zA-Z0-9_]+)/);
        if (m) { tests.push(m[1]); break; }
      }
    }
  }
}
console.log(tests.join('\n'));
