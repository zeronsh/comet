param(
    [Parameter(Mandatory)][string]$BaselineZip,
    [Parameter(Mandatory)][string]$ExpectedVersion,
    [Parameter(Mandatory)][string]$ExpectedSha256
)
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false # update --check intentionally exits 1.
$root = Join-Path ([IO.Path]::GetTempPath()) ("zeron updater O'Brien 日本語 " + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $root | Out-Null
Expand-Archive -LiteralPath $BaselineZip -DestinationPath $root
$exe = Join-Path $root 'zeron.exe'
$env:ZERON_DATA_DIR = Join-Path $root 'profile'
$env:ZERON_IPC_PORT = '0'
$env:ZERON_RELEASES_URL = $null # Use the feed shipped inside the real package.
$baseline = & $exe --version
if ($LASTEXITCODE -ne 0 -or $baseline -eq "zeron $ExpectedVersion") { throw 'Invalid baseline version' }
& $exe update --check
if ($LASTEXITCODE -ne 1) { throw 'Baseline did not discover an update' }
& $exe update
if ($LASTEXITCODE -ne 0) { throw 'Update failed' }
$updated = & $exe --version
if ($LASTEXITCODE -ne 0 -or $updated -ne "zeron $ExpectedVersion") { throw "Wrong updated version: $updated" }
if ((Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash -ne $ExpectedSha256) { throw 'Installed binary differs from published binary' }
& $exe update --check
if ($LASTEXITCODE -ne 0) { throw 'Updated app still reports an available update' }
& $exe status
if ($LASTEXITCODE -ne 0) { throw 'Updated app cannot start' }
Write-Host "PASS: $baseline -> $updated using the packaged GitHub feed; SHA-256 and startup verified."
Write-Host "Test installation: $root"
