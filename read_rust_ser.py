with open(r'G:\ZCode-main\native\crates\nc-code-rpc\src\serialization.rs') as f:
    for i,l in enumerate(f,1):
        print(str(i) + ': ' + l.rstrip())
        if i>=200: break
