import subprocess
result = subprocess.run(['node', r'G:\ZCode-main\native\crates\nc-code-rpc\tests\generate_golden.mjs'], 
                        cwd=r'G:\ZCode-main', 
                        stdout=subprocess.PIPE)
with open(r'G:\ZCode-main\native\crates\nc-code-rpc\tests\golden.json', 'wb') as f:
    f.write(result.stdout)
