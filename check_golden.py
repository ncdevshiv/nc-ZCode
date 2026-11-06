import json
with open(r'G:\ZCode-main\native\crates\nc-code-rpc\tests\golden.json') as f:
    data = json.load(f)
for i,v in enumerate(data['serialization']):
    if v.get('kind') == 'string':
        print(i, v)
        break
