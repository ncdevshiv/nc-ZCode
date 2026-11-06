import urllib.request, urllib.parse
q = '''// First line of comments
const x = 1; // second line
function foo() { return "Hello"; }
'''
url = "https://translate.googleapis.com/translate_a/single?client=gtx&sl=zh-CN&tl=en&dt=t&q=" + urllib.parse.quote(q)
resp = urllib.request.urlopen(url, timeout=20).read().decode()
import json
data = json.loads(resp)
print(''.join(seg[0] for seg in data[0]))