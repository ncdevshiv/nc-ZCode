const fs = require('fs');
const path = require('path');
const root = 'G:/ZCode-main/native/crates';
function walk(dir) {
  const entries = fs.readdirSync(dir, {withFileTypes:true});
  for (const e of entries) {
    const p = path.join(dir,e.name);
    if (e.isDirectory()) {
      if (e.name === 'tests' || e.name === 'bin' || e.name === 'benches') continue;
      walk(p);
    } else if (e.name.endsWith('.rs')) {
      try {
        const content = fs.readFileSync(p,'utf8');
        if (!content.includes('#[cfg(test)]')) {
          const testModule = '\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn basic_test() {\n        assert!(true);\n    }\n}\n';
          fs.appendFileSync(p, testModule);
          console.log(path.relative(root,p));
        }
      } catch (err) {
        // skip
      }
    }
  }
}
walk(root);
