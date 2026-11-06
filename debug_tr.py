import os, re, time, json, urllib.request, urllib.parse

HAN = re.compile(r'[\u4e00-\u9fff]')
def has_han(s): return bool(HAN.search(s))

# find first small file
for root, dirs, files in os.walk('packages/ui'):
    dirs[:] = [d for d in dirs if d not in ('dist','node_modules')]
    for fn in files:
        if os.path.splitext(fn)[1] not in ('.ts','.tsx','.md','.json'):
            continue
        p = os.path.join(root, fn)
        try:
            with open(p, encoding='utf-8') as f:
                content = f.read()
        except Exception as e:
            continue
        if not has_han(content):
            continue
        if len(content) > 2000:
            continue
        print('FILE', p, len(content), flush=True)
        # translate whole content
        url = "https://translate.googleapis.com/translate_a/single?client=gtx&sl=zh-CN&tl=en&dt=t&q=" + urllib.parse.quote(content)
        print('requesting', flush=True)
        with urllib.request.urlopen(url, timeout=30) as r:
            data = json.loads(r.read().decode('utf-8'))
        print('done', ''.join(seg[0] for seg in data[0] if seg and isinstance(seg, list) and seg[0])[:500], flush=True)
        break