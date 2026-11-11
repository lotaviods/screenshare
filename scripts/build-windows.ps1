# Build a portable Sharescreen Windows app with FFmpeg bundled.
#
# The app links FFmpeg dynamically (libavcodec, libavutil, libswscale,
# libavfilter). This script downloads the FFmpeg shared build, points the build
# at it, and copies the DLLs next to the executable. No system FFmpeg is
# needed, and the machine that runs the app does not need FFmpeg installed.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1

$ErrorActionPreference = "Stop"

$repo = Split-Path -Parent $PSScriptRoot
$dist = Join-Path $repo "dist"
$stage = Join-Path $dist "sharescreen-windows"
$ffmpegVersion = "autobuild-2026-11-11-12-39"
$ffmpegDir = Join-Path $env:TEMP "ffmpeg-win64-shared-$ffmpegVersion"
$ffmpegZip = Join-Path $env:TEMP "ffmpeg-win64-shared-$ffmpegVersion.zip"
$ffmpegUrl = "https://github.com/FFmpeg-Builds/FFmpeg-Builds/releases/download/$ffmpegVersion/ffmpeg-N-128790-g9bee1e603c-win64-lgpl-shared.zip"

if (-not (Test-Path $ffmpegDir)) {
    Write-Host "Downloading FFmpeg shared build..."
    Invoke-WebRequest -Uri $ffmpegUrl -OutFile $ffmpegZip
    Expand-Archive -Path $ffmpegZip -DestinationPath $ffmpegDir -Force
    Remove-Item -LiteralPath $ffmpegZip -Force
}

$ffmpegRoot = Get-ChildItem -LiteralPath $ffmpegDir -Directory | Select-Object -First 1
if (-not $ffmpegRoot) {
    throw "FFmpeg extraction did not produce a directory."
}
$ffmpegBin = Join-Path $ffmpegRoot.FullName "bin"
if (-not (Test-Path $ffmpegBin)) {
    throw "FFmpeg bin directory not found: $ffmpegBin"
}

$env:FFMPEG_DIR = $ffmpegRoot.FullName

Push-Location $repo
try {
    cargo build --release -p sharescreen-ui
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)." }
} finally {
    Pop-Location
}

$exe = Join-Path $repo "target\release\sharescreen.exe"
if (-not (Test-Path $exe)) { throw "Expected app executable not found: $exe" }

if (Test-Path $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
New-Item -ItemType Directory -Path $stage -Force | Out-Null
Copy-Item -LiteralPath $exe -Destination $stage

# FFmpeg's shared DLLs are the only runtime dependency.
Get-ChildItem -LiteralPath $ffmpegBin -Filter "*.dll" |
    Copy-Item -Destination $stage

@'
Sharescreen portable build

Extract this folder and run sharescreen.exe. Keep the FFmpeg DLL files beside
the executable. This build targets 64-bit Windows. No Rust, Node.js, FFmpeg,
or source checkout is needed on the machine that runs the app.
'@ | Set-Content -LiteralPath (Join-Path $stage "README.txt") -Encoding utf8

Write-Host "Portable build: $stage"
