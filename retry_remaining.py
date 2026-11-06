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

SKIP_DIRS = {'node_modules', '.git', 'dist', 'target', 'build', 'out', '.next', '.nc-tools'}
TEXT_EXT = {'.ts','.tsx','.rs','.mjs','.md','.json','.py','.toml','.html','.css','.sh','.yaml','.yml','.js','.mts','.cjs','.swift','.cs','.txt','.svg','.patch'}
count = 0
for root, dirs, files in os.walk('.'):
    dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
    for fn in files:
        if os.path.splitext(fn)[1].lower() not in TEXT_EXT:
            continue
        p = os.path.abspath(os.path.join(root, fn))
        try:
            with open(p, encoding='utf-8') as f:
                content = f.read()
        except Exception:
            continue
        if not has_han(content):
            continue
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
                    print('FAIL', p, e, flush=True)
                time.sleep(0.4)
        new_content = ''.join(out)
        tmp = p + '.tmp_translate'
        with open(tmp, 'wb') as f:
            f.write(new_content.encode('utf-8'))
        os.replace(tmp, p)
        count += 1
        print('translated', p, flush=True)
print('done', count, flush=True)