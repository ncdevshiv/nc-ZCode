s1 = '中文✓'
s2 = '中文 ✓'
print('s1:', repr(s1), len(s1))
for c in s1:
    print(c, hex(ord(c)))
print('\ns2:', repr(s2), len(s2))
for c in s2:
    print(c, hex(ord(c)))
