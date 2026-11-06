with open(r'G:\ZCode-main\native\crates\nc-code-node-repl\src\driver\kernel_driver.mjs') as f:
    content = f.read()
import re
matches = re.finditer(r'\bID\b', content)
results = []
for m in matches:
    line_start = content.rfind('\n', 0, m.start()) + 1
    line_end = content.find('\n', m.end())
    if line_end == -1: line_end = len(content)
    line = content[line_start:line_end]
    results.append(line)
for r in results:
    print(r)
