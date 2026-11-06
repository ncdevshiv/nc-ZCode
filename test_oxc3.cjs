const parser = require('oxc-parser');
const result = parser.parseSync('test.ts', 'async function f() { return 1; }');
console.log(result.program.body[0].type);
console.log(result.errors.length);
