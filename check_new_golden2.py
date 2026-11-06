import json
with open(r'G:\ZCode-main\native\crates\nc-code-rpc\tests\golden.json.new', encoding='utf-16') as f:
    data = json.load(f)
for i,v in enumerate(data['serialization']):
    if v.get('kind') == 'string':
        print(i, json.dumps(v, ensure_ascii=False))
