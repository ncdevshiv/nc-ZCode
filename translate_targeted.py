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

SKIP_DIRS = {'node_modules', '.git', 'dist', 'target', 'build', 'out', '.next', '.nc-tools'}
TEXT_EXT = {'.ts','.tsx','.rs','.mjs','.md','.json','.py','.toml','.html','.css','.sh','.yaml','.yml','.js','.mts','.cjs','.swift','.cs','.txt','.svg','.patch'}
STR_RE = re.compile(r'(["\'`])((?:\\.|(?!\1).)*[\u4e00-\u9fff](?:\\.|(?!\1).)*)\1')

count = 0
for root, dirs, files in os.walk('.'):
    dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
    for fn in files:
        if os.path.splitext(fn)[1].lower() not in TEXT_EXT:
            continue
        p = os.path.abspath(os.path.join(root, fn))
        try:
            with open(p, encoding='utf-8') as f:
                lines = f.readlines()
        except Exception:
            continue
        if not any(has_han(l) for l in lines):
            continue
        new_lines = []
        changed = False
        in_block = False
        for line in lines:
            stripped = line.lstrip()
            if not has_han(line):
                new_lines.append(line)
                # track block comments
                if '/*' in line and '*/' not in line:
                    in_block = True
                if in_block and '*/' in line:
                    in_block = False
                continue
            # determine if comment line
            is_comment = in_block or stripped.startswith('//') or stripped.startswith('#') or stripped.startswith('*') or stripped.startswith('/*') or stripped.startswith('<!--')
            if is_comment:
                try:
                    tr = google_translate(line)
                    if line.endswith('\n') and not tr.endswith('\n'):
                        tr += '\n'
                    new_lines.append(tr)
                    changed = True
                    time.sleep(0.2)
                except Exception as e:
                    new_lines.append(line)
                    print('FAIL', p, e, flush=True)
            else:
                # translate string literals containing Han
                def repl(m):
                    quote = m.group(1)
                    inner = m.group(2)
                    if not has_han(inner):
                        return m.group(0)
                    try:
                        tr_inner = google_translate(inner)
                        # remove any newlines
                        tr_inner = tr_inner.replace('\n',' ')
                        global changed
                        changed = True
                        time.sleep(0.2)
                        return quote + tr_inner + quote
                    except Exception as e:
                        print('FAILSTR', p, e, flush=True)
                        return m.group(0)
                new_line = STR_RE.sub(repl, line)
                new_lines.append(new_line)
            # track block comments
            if '/*' in line and '*/' not in line:
                in_block = True
            if in_block and '*/' in line:
                in_block = False
        if changed:
            content = ''.join(new_lines)
            tmp = p + '.tmp_translate'
            with open(tmp, 'wb') as f:
                f.write(content.encode('utf-8'))
            os.replace(tmp, p)
            count += 1
            print('translated', p, flush=True)
print('done', count, flush=True)