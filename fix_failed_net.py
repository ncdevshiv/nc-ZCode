import os, re, time, json, urllib.request, urllib.parse
from pathlib import Path

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

# parse failed paths from translate3_log.txt
paths = []
for line in open('translate3_log.txt', encoding='utf-8', errors='ignore'):
    if line.startswith('ERROR'):
        parts = line.split(' ', 2)
        if len(parts) >= 2:
            paths.append(parts[1].strip())

print('failed count', len(paths))
for p in paths:
    ap = os.path.abspath(p)
    try:
        content = open(ap, encoding='utf-8').read()
    except Exception as e:
        print('read err', ap, e); continue
    if not has_han(content):
        print('no han', ap); continue
    chunks = chunk_text(content)
    out = []
    for c in chunks:
        if not has_han(c):
            out.append(c)
        else:
            try:
                out.append(google_translate(c))
            except Exception as e:
                out.append(c)
                print('FAIL translate', ap, e)
            time.sleep(0.4)
    new_content = ''.join(out)
    # write via .NET-like: try pathlib, then fallback to os.replace temp
    try:
        Path(ap).write_bytes(new_content.encode('utf-8'))
        print('translated', ap)
    except Exception as e:
        # fallback: write temp then replace
        tmp = ap + '.tmp'
        with open(tmp, 'wb') as f:
            f.write(new_content.encode('utf-8'))
        os.replace(tmp, ap)
        print('translated via replace', ap)