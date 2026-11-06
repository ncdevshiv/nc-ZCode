with open(r'G:\ZCode-main\native\crates\nc-code-rpc\tests\golden.json','rb') as f:
    data = f.read()
if data.startswith(b'\xef\xbb\xbf'):
    data = data[3:]
with open(r'G:\ZCode-main\native\crates\nc-code-rpc\tests\golden.json','wb') as f:
    f.write(data)
