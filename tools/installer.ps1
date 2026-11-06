# Builds dist\backseat-setup-<version>.exe from the bundled folder (tools\bundle.ps1).
#   tools\installer.ps1 [-NoBundle] [-Server wss://host/ws]
# -Server fixes the signaling server in the app (no server field).
param([switch]$NoBundle, [string]$Server = '')
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
Set-Location $root
if (-not $NoBundle) {
  & "$PSScriptRoot\bundle.ps1" -Server $Server
  if ($LASTEXITCODE) { throw 'bundle failed' }
}
$version = (Select-String -Path "$root\Cargo.toml" -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$src = Join-Path $root 'dist\backseat'
$wv2 = Join-Path $root '.toolchain\downloads\MicrosoftEdgeWebview2Setup.exe'
$out = Join-Path $root "dist\backseat-setup-$version.exe"
$sizeKb = [int]((Get-ChildItem $src -Recurse -File | Measure-Object Length -Sum).Sum / 1KB)
& "$root\.toolchain\nsis\makensis.exe" /V2 "/DVERSION=$version" "/DSRC=$src" "/DWEBVIEW2=$wv2" "/DOUT=$out" "/DSIZE_KB=$sizeKb" "$root\installer\backseat.nsi"
if ($LASTEXITCODE) { throw 'makensis failed' }
'installer {0}: {1:N1} MB (folder {2:N1} MB)' -f $out, ((Get-Item $out).Length / 1MB), ($sizeKb / 1KB)
