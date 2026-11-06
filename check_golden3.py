import json
with open(r'G:\ZCode-main\native\crates\nc-code-rpc\tests\golden.json') as f:
    data = json.load(f)
v = data['serialization'][3]
print(json.dumps(v, ensure_ascii=False))
