from pathlib import Path
import re
p = Path('G:/ZCode-main/packages/shared/src/channels.ts')
c = p.read_text()
start = c.find('export const ServiceChannels')
if start >= 0:
    end = c.find('} as const', start)
    if end >= 0:
        block = c[start:end+10]
        channels = []
        for m in re.finditer(r'([A-Za-z][A-Za-z0-9]*)\s*:\s*"([^"]+)"', block):
            key = m.group(1)
            name = m.group(2)
            channels.append((key,name))
        for k,n in channels:
            print(f'{k}: {n}')
        print(f'\nTOTAL: {len(channels)}')
