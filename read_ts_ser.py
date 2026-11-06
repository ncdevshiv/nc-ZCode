with open(r'G:\ZCode-main\packages\rpc\src\serialization.ts') as f:
    for i,l in enumerate(f,1):
        print(str(i) + ': ' + l.rstrip())
        if i>=220: break
