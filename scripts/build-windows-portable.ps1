param(
    [string]$GStreamerRoot = $env:GSTREAMER_1_0_ROOT_MSVC_X86_64
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
$dist = Join-Path $repo "dist"
$stage = Join-Path $dist "sharescreen-windows"
$configPath = Join-Path $repo "frontend\src-tauri\tauri.conf.json"
$tauriConfig = Get-Content -LiteralPath $configPath -Raw | ConvertFrom-Json
$archive = Join-Path $dist "sharescreen-$($tauriConfig.version)-windows-x86_64.zip"
$exe = Join-Path $repo "target\release\sharescreen-tauri.exe"

if (-not $GStreamerRoot -or -not (Test-Path (Join-Path $GStreamerRoot "bin"))) {
    throw "Set GSTREAMER_1_0_ROOT_MSVC_X86_64 to the GStreamer MSVC x86_64 install directory."
}

Push-Location $repo
try {
    pnpm --dir frontend install --frozen-lockfile
    if ($LASTEXITCODE -ne 0) { throw "pnpm install failed ($LASTEXITCODE)." }
    pnpm --dir frontend tauri build --no-bundle
    if ($LASTEXITCODE -ne 0) { throw "Tauri build failed ($LASTEXITCODE)." }
} finally {
    Pop-Location
}

if (-not (Test-Path $exe)) { throw "Expected app executable not found: $exe" }

if (Test-Path $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
if (Test-Path $archive) { Remove-Item -LiteralPath $archive -Force }
New-Item -ItemType Directory -Path $stage -Force | Out-Null
Copy-Item -LiteralPath $exe -Destination $stage
Get-ChildItem -LiteralPath (Join-Path $GStreamerRoot "bin") -Filter "*.dll" |
    Copy-Item -Destination $stage

$pluginSource = Join-Path $GStreamerRoot "lib\gstreamer-1.0"
$pluginDestination = Join-Path $stage "gstreamer\lib\gstreamer-1.0"
New-Item -ItemType Directory -Path $pluginDestination -Force | Out-Null
# Static .a archives are developer files; the app only needs plugin DLLs.
Get-ChildItem -LiteralPath $pluginSource -Filter "*.dll" |
    Copy-Item -Destination $pluginDestination

$gstConfig = Join-Path $GStreamerRoot "etc\gstreamer-1.0"
if (Test-Path $gstConfig) {
    Copy-Item -LiteralPath $gstConfig -Destination (Join-Path $stage "gstreamer\etc") -Recurse
}

$scanner = Join-Path $GStreamerRoot "libexec\gstreamer-1.0\gst-plugin-scanner.exe"
if (-not (Test-Path $scanner)) { throw "GStreamer plugin scanner not found: $scanner" }
Copy-Item -LiteralPath $scanner -Destination $stage

@'
Sharescreen portable build

Extract this folder and run sharescreen-tauri.exe. Keep the gstreamer folder
and DLL files beside it. This build targets 64-bit Windows. WebView2 Runtime
is required; Windows 11 usually includes it. No Rust, Node.js, or source
checkout is needed.
'@ | Set-Content -LiteralPath (Join-Path $stage "README.txt") -Encoding utf8

Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $archive -CompressionLevel Optimal
Write-Host "Portable build: $stage"
Write-Host "Shareable archive: $archive"
