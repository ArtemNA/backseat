# Builds a self-contained Windows folder: backseat.exe, the GStreamer libraries it needs,
# the plugins it uses (in gstreamer-1.0\), the Visual C++ runtime, and the license texts.
# Every DLL import of every shipped binary must resolve inside the folder or to a DLL that
# ships with Windows itself; otherwise the script fails and names what is missing.
#
#   tools\bundle.ps1 [-Out dist\backseat] [-NoBuild] [-Server wss://host/ws] [-Gst <dir>]
# -Server fixes the signaling server: the app then has no server field.
# -Gst is the GStreamer root; by default .toolchain\gstreamer, else the folder in
# GSTREAMER_1_0_ROOT_MSVC_X86_64 (the CI build).
param(
  [string]$Out = 'dist\backseat',
  [string]$Gst = '',
  [switch]$NoBuild,
  [string]$Server = '',
  # Tests only: leave this file out, to prove the import check notices.
  [string]$Drop = ''
)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
Set-Location $root
if (-not $Gst) {
  $Gst = if (Test-Path '.toolchain\gstreamer') { '.toolchain\gstreamer' } else { $env:GSTREAMER_1_0_ROOT_MSVC_X86_64 }
  if (-not $Gst) { throw 'GStreamer not found: pass -Gst or set GSTREAMER_1_0_ROOT_MSVC_X86_64' }
}
$Gst = (Resolve-Path $Gst).Path

# Plugins the app uses on Windows (element names in crates/media, mapped to plugins).
$plugins = @(
  'coreelements', 'app', 'audioconvert', 'audioresample', 'autodetect',
  'd3d11', 'nvcodec', 'amfcodec', 'qsv', 'videoparsersbad', 'videorate',
  'rtp', 'rtpmanager', 'webrtc', 'nice', 'dtls', 'srtp', 'opus', 'wasapi2',
  'videotestsrc', 'audiotestsrc', 'volume',
  # AV1: RTP (de)payloader and the software decoder
  'rsrtp', 'dav1d',
  # Data channels of a direct connection (no server).
  'sctp'
)
# License folders under share\licenses for the libraries that end up in the folder.
$licenses = @(
  'gstreamer-1.0', 'gst-plugins-base-1.0', 'gst-plugins-bad-1.0', 'glib', 'libffi',
  'proxy-libintl', 'pcre2', 'zlib', 'orc', 'opus', 'libnice', 'libsrtp', 'openssl', 'dav1d', 'gst-plugins-rs'
)
$vcRuntime = '^(vcruntime140(_1)?|msvcp140(_\d)?|concrt140)\.dll$'

$vs = & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -property installationPath
$dumpbin = Get-ChildItem "$vs\VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe" | Select-Object -Last 1
$crt = Get-ChildItem "$vs\VC\Redist\MSVC\*\x64\Microsoft.VC14*.CRT" -Directory | Select-Object -Last 1
if (-not $dumpbin -or -not $crt) { throw 'Visual Studio dumpbin or the VC++ redistributable folder not found' }

if (-not $NoBuild) {
  if (Test-Path "$root\.scratch\env.ps1") { . "$root\.scratch\env.ps1" 2>$null } else { $env:PATH = "$Gst\bin;$env:PATH" }
  Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue
  $env:PKG_CONFIG_PATH = "$Gst\lib\pkgconfig"
  # Compiled into the app (option_env!); empty means the user types the server.
  $env:STREAMER_SERVER_URL = $Server
  cargo build --release -p app
  Remove-Item Env:STREAMER_SERVER_URL -ErrorAction SilentlyContinue
  if ($LASTEXITCODE) { throw 'build failed' }
}

if (Test-Path $Out) { Remove-Item $Out -Recurse -Force }
New-Item -ItemType Directory "$Out\gstreamer-1.0", "$Out\licenses" -Force | Out-Null
$Out = (Resolve-Path $Out).Path
Copy-Item "$root\target\release\app.exe" "$Out\backseat.exe"
foreach ($p in $plugins) { Copy-Item "$Gst\lib\gstreamer-1.0\gst$p.dll" "$Out\gstreamer-1.0\" }

function Imports($file) {
  (& $dumpbin.FullName /nologo /dependents $file) -match '^\s+\S+\.dll\s*$' | ForEach-Object { $_.Trim() }
}
$system = "$env:SystemRoot\System32"
$todo = [System.Collections.Generic.Queue[string]]::new()
Get-ChildItem $Out -Recurse -Include *.exe, *.dll | ForEach-Object { $todo.Enqueue($_.FullName) }
$seen = @{}
$missing = @()
while ($todo.Count) {
  $f = $todo.Dequeue()
  foreach ($d in Imports $f) {
    $k = $d.ToLower()
    if ($seen[$k]) { continue }
    $seen[$k] = $true
    if ($k -eq $Drop.ToLower()) { $missing += "$d (needed by $(Split-Path $f -Leaf))"; continue }
    if (Test-Path "$Gst\bin\$d") {
      Copy-Item "$Gst\bin\$d" "$Out\"
      $todo.Enqueue("$Out\$d")
    } elseif ($k -match $vcRuntime) {
      if (-not (Test-Path "$crt\$d")) { $missing += "$d (VC++ runtime, needed by $(Split-Path $f -Leaf))"; continue }
      Copy-Item "$crt\$d" "$Out\"
    } elseif ($k -match '^(api|ext)-ms-win-' -or (Test-Path "$system\$d")) {
      # Part of Windows.
    } else {
      $missing += "$d (needed by $(Split-Path $f -Leaf))"
    }
  }
}

foreach ($l in $licenses) { Copy-Item "$Gst\share\licenses\$l" "$Out\licenses\" -Recurse }
Copy-Item "$root\THIRD-PARTY.md" "$Out\licenses\"
# Every crate compiled into backseat.exe for Windows, with its license.
$crates = cargo tree -p app --target x86_64-pc-windows-msvc -e normal --prefix none --format '{p}  {l}' 2>$null |
  ForEach-Object { $_ -replace ' \(.*?\)', '' -replace ' \(\*\)$', '' } | Sort-Object -Unique
if (-not $crates) { throw 'cargo tree gave no crate list' }
@('Rust crates in backseat.exe (name version  license)', '') + $crates | Set-Content "$Out\licenses\rust-crates.txt"
$bad = $crates | Where-Object { $_ -match 'GPL' -and $_ -notmatch 'LGPL' -and $_ -notmatch ' OR ' }
if ($bad) { 'crates under a GPL-only license:'; $bad; exit 1 }

$files = Get-ChildItem $Out -Recurse -File
$mb = { param($x) ($x | Measure-Object Length -Sum).Sum / 1MB }
'folder {0}: {1} files, {2:N1} MB' -f $Out, $files.Count, (& $mb $files)
'  backseat.exe {0:N1} MB; libraries {1} ({2:N1} MB); plugins {3} ({4:N1} MB)' -f `
  (& $mb (Get-Item "$Out\backseat.exe")), @(Get-ChildItem $Out -Filter *.dll).Count, (& $mb (Get-ChildItem $Out -Filter *.dll)), `
  @(Get-ChildItem "$Out\gstreamer-1.0").Count, (& $mb (Get-ChildItem "$Out\gstreamer-1.0"))
if ($missing) {
  'MISSING imports:'
  $missing | ForEach-Object { "  $_" }
  exit 1
}
'all imports resolve inside the folder or to Windows itself'
