import os, re, time, json, urllib.request, urllib.parse

SKIP_DIRS = {'node_modules', '.git', 'dist', 'target', 'build', 'out', '.next', '.nc-tools'}
SKIP_FILES = {'pnpm-lock.yaml', 'package-lock.json', 'Cargo.lock', 'THIRD-PARTY-NOTICES.md'}
SKIP_SUFFIX = ('.baseline.json', '.lock')
SKIP_PATH_PARTS = ('_tr_',)
TEXT_EXT = {'.ts','.tsx','.rs','.mjs','.md','.json','.py','.toml','.html','.css','.sh','.yaml','.yml','.js','.mts','.cjs','.swift','.cs','.txt','.svg','.patch','.jsonl','.env','.development','.example','.production'}
HAN = re.compile(r'[\u4e00-\u9fff]')

def has_han(s): return bool(HAN.search(s))

def google_translate(text):
    for attempt in range(5):
        try:
            url = "https://translate.googleapis.com/translate_a/single?client=gtx&sl=zh-CN&tl=en&dt=t&q=" + urllib.parse.quote(text)
            with urllib.request.urlopen(url, timeout=60) as r:
                data = json.loads(r.read().decode('utf-8'))
            return ''.join(seg[0] for seg in data[0] if seg and isinstance(seg, list) and seg[0])
        except Exception as e:
            time.sleep(2 ** attempt)
    raise RuntimeError('translate failed')

def chunk_text(text, limit=1800):
    lines = text.splitlines(keepends=True)
    chunks = []
    cur = ''
    for line in lines:
        if len(line) > limit:
            # split long line by chars
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

def translate_file(path):
    try:
        with open(path, encoding='utf-8') as f:
            content = f.read()
    except UnicodeDecodeError:
        return False
    if not has_han(content):
        return False
    chunks = chunk_text(content)
    out = []
    for c in chunks:
        if not has_han(c):
            out.append(c)
        else:
            out.append(google_translate(c))
            time.sleep(0.25)
    new_content = ''.join(out)
    with open(path, 'w', encoding='utf-8', newline='') as f:
        f.write(new_content)
    return True

count = 0
for root, dirs, files in os.walk('.'):
    dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
    for fn in files:
        if fn in SKIP_FILES or fn.endswith(SKIP_SUFFIX):
            continue
        if any(part in fn for part in SKIP_PATH_PARTS):
            continue
        ext = os.path.splitext(fn)[1].lower()
        if ext not in TEXT_EXT:
            # also handle no-extension env files
            if fn not in ('.env','.env.development','.env.example','.env.production'):
                continue
        path = os.path.join(root, fn)
        try:
            if translate_file(path):
                count += 1
                print('translated', path)
        except Exception as e:
            print('ERROR', path, e)
print('done', count)