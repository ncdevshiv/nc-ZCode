import urllib.request, urllib.parse
q = 'const msg = "Error: file does not exist"; // Prompt user'
url = "https://translate.googleapis.com/translate_a/single?client=gtx&sl=zh-CN&tl=en&dt=t&q=" + urllib.parse.quote(q)
print(urllib.request.urlopen(url, timeout=20).read().decode()[:1000])