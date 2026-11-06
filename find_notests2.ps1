\ = Get-ChildItem -Path 'G:\ZCode-main\native\crates' -Recurse -Include '*.rs' | Where-Object { \.FullName -notmatch '\\tests\\' -and \.FullName -notmatch '\\bin\\' };
\ = 0;
foreach(\ in \) {
  \ = Get-Content \.FullName -Raw;
  if(\ -notmatch '#\[cfg\(test\)\]') {
    Write-Output \.FullName.Replace('G:\ZCode-main\native\crates\','');
    \++;
    if(\ -ge 40) { break; }
  }
}
