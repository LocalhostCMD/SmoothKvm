# Signs files with the SmoothKvm code-signing certificate from the current user's certificate store.
# Usage: sign.ps1 file1.exe [file2.exe ...]
# The private key is non-exportable and never leaves this machine. Signatures are timestamped so they stay
# valid after the certificate expires.
param([Parameter(Mandatory, ValueFromRemainingArguments)][string[]]$Files)
$ErrorActionPreference = 'Stop'
$cert = Get-ChildItem Cert:\CurrentUser\My -CodeSigningCert | Where-Object { $_.Subject -like 'CN=SmoothKvm Release Signing*' } | Sort-Object NotAfter -Descending | Select-Object -First 1
if (-not $cert) { throw 'SmoothKvm code-signing certificate not found in Cert:\CurrentUser\My' }
$signtool = Get-ChildItem "C:\Program Files (x86)\Windows Kits\10\bin" -Recurse -Filter signtool.exe | Where-Object { $_.FullName -like '*\x64\*' } | Sort-Object FullName -Descending | Select-Object -First 1
if (-not $signtool) { throw 'signtool.exe (Windows SDK) not found' }
foreach ($f in $Files) {
  & $signtool.FullName sign /sha1 $cert.Thumbprint /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 /d 'SmoothKvm' $f
  if ($LASTEXITCODE -ne 0) { throw "signing failed for $f" }
}
