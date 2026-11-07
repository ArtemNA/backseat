# Builds dist\backseat-setup-<version>.exe from the bundled folder (tools\bundle.ps1).
#   tools\installer.ps1 [-NoBundle] [-Server wss://host/ws] [-WebView2 <bootstrapper.exe>]
# -Server fixes the signaling server in the app (no server field).
# -WebView2 is the Microsoft Edge WebView2 bootstrapper; by default the one in
# .toolchain\downloads. makensis: .toolchain\nsis, else PATH, else Program Files.
param([switch]$NoBundle, [string]$Server = '', [string]$WebView2 = '')
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
Set-Location $root
if (-not $NoBundle) {
  & "$PSScriptRoot\bundle.ps1" -Server $Server
  if ($LASTEXITCODE) { throw 'bundle failed' }
}
$version = (Select-String -Path "$root\Cargo.toml" -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$src = Join-Path $root 'dist\backseat'
$wv2 = if ($WebView2) { (Resolve-Path $WebView2).Path } else { Join-Path $root '.toolchain\downloads\MicrosoftEdgeWebview2Setup.exe' }
$out = Join-Path $root "dist\backseat-setup-$version.exe"
$sizeKb = [int]((Get-ChildItem $src -Recurse -File | Measure-Object Length -Sum).Sum / 1KB)
$makensis = "$root\.toolchain\nsis\makensis.exe"
if (-not (Test-Path $makensis)) {
  $makensis = (Get-Command makensis.exe -ErrorAction SilentlyContinue).Source
  if (-not $makensis) { $makensis = "${env:ProgramFiles(x86)}\NSIS\makensis.exe" }
  if (-not (Test-Path $makensis)) { throw 'makensis.exe not found (NSIS)' }
}
& $makensis /V2 "/DVERSION=$version" "/DSRC=$src" "/DWEBVIEW2=$wv2" "/DOUT=$out" "/DSIZE_KB=$sizeKb" "$root\installer\backseat.nsi"
if ($LASTEXITCODE) { throw 'makensis failed' }
'installer {0}: {1:N1} MB (folder {2:N1} MB)' -f $out, ((Get-Item $out).Length / 1MB), ($sizeKb / 1KB)
