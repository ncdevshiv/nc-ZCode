$files = Get-ChildItem -Path "G:\ZCode-main\native\crates" -Recurse -Include "*.rs" | Where-Object { $_.FullName -notmatch "\\tests\\" -and $_.FullName -notmatch "\\bin\\" };
foreach($f in $files) {
  $content = Get-Content $f.FullName -Raw;
  if($content -notmatch '#\[cfg\(test\)\]') {
    Write-Output $f.FullName.Replace("G:\ZCode-main\native\crates\", "")
  }
} | Select-Object -First 40
