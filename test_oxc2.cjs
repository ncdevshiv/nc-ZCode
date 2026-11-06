const parser = require('oxc-parser');
const result = parser.parseSync('test.ts', 'const x: number = 1;');
console.log(JSON.stringify(Object.keys(result)));
console.log(JSON.stringify(Object.keys(result.program)));
console.log(result.program.body[0].type);
console.log(result.errors.length);
