const parser = require('oxc-parser');
const result = parser.parseSync('test.ts', 'const x = 1;');
console.log(result.program.body[0].type);
