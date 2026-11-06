import re

path = r'G:\ZCode-main\native\crates\nc-code-rpc\tests\generate_golden.mjs'
with open(path, 'r', encoding='utf-8') as f:
    content = f.read()

# Replace any Chinese string line with correct one
new_line = '{ kind: "string", value: "Chinese \u2713" },'
content2 = re.sub(r'\{ kind: "string", value: "Chinese [^"]*" \},', new_line, content)
if content2 != content:
    content = content2
    print('replaced')
else:
    print('no change')
    # look for it
    for i, line in enumerate(content.splitlines()):
        if 'Chinese' in line:
            print(f'LINE{i}: {line}')
            break

with open(path, 'w', encoding='utf-8') as f:
    f.write(content)
