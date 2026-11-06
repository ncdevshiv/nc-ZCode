import os, re, time, json, urllib.request, urllib.parse

HAN = re.compile(r'[\u4e00-\u9fff]')
def has_han(s): return bool(HAN.search(s))

def google_translate(text):
    for attempt in range(8):
        try:
            url = "https://translate.googleapis.com/translate_a/single?client=gtx&sl=zh-CN&tl=en&dt=t&q=" + urllib.parse.quote(text)
            with urllib.request.urlopen(url, timeout=90) as r:
                data = json.loads(r.read().decode('utf-8'))
            return ''.join(seg[0] for seg in data[0] if seg and isinstance(seg, list) and seg[0])
        except Exception as e:
            time.sleep(3 ** min(attempt,3))
    raise RuntimeError('translate failed')

def chunk_text(text, limit=1200):
    lines = text.splitlines(keepends=True)
    chunks = []
    cur = ''
    for line in lines:
        if len(line) > limit:
            if cur:
                chunks.append(cur); cur=''
            for i in range(0, len(line), limit):
                chunks.append(line[i:i+limit])
            continue
        if len(cur) + len(line) > limit:
            chunks.append(cur); cur = line
        else:
            cur += line
    if cur:
        chunks.append(cur)
    return chunks

# parse failed paths
paths = []
for line in open('translate_log.txt', encoding='utf-8'):
    if line.startswith('ERROR'):
        # ERROR path translate failed
        parts = line.split(' ', 2)
        if len(parts) >= 2:
            paths.append(parts[1])

print('failed count', len(paths))
for p in paths:
    p = p.strip()
    if not os.path.exists(p):
        print('missing', p)
        continue
    try:
        with open(p, encoding='utf-8') as f:
            content = f.read()
    except Exception as e:
        print('read error', p, e)
        continue
    if not has_han(content):
        print('no han', p)
        continue
    chunks = chunk_text(content)
    out = []
    for c in chunks:
        if not has_han(c):
            out.append(c)
        else:
            out.append(google_translate(c))
            time.sleep(0.4)
    new_content = ''.join(out)
    with open(p, 'w', encoding='utf-8', newline='') as f:
        f.write(new_content)
    print('translated', p)