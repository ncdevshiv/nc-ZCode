import json
with open(r'G:\ZCode-main\native\crates\nc-code-rpc\tests\golden.json', encoding='utf-8') as f:
    content = f.read()
idx = content.find('"中文')
if idx >= 0:
    end = content.find('"', idx + 10)
    print(repr(content[idx:end+1]))
