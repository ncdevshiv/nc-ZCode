import json
with open(r'G:\ZCode-main\native\crates\nc-code-rpc\tests\golden.json') as f:
    data = json.load(f)
v = data['serialization'][3]
print(repr(v['value']))
ser = v['serialized']
bytes_ser = bytes.fromhex(ser)
payload = bytes_ser[2:2+10]
print(payload)
print(payload.decode('utf-8'))
