import re
with open(r'G:\ZCode-main\native\crates\nc-code-node-repl\src\driver\kernel_driver.mjs') as f:
    for i,l in enumerate(f,1):
        if re.search(r'\bID\b', l):
            print(i, repr(l))
            break
