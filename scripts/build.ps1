<#
.SYNOPSIS
    Build the portable Sharescreen desktop app for Windows.

.DESCRIPTION
    Compiles `sharescreen-ui`, stages the executable (and the FFmpeg DLLs) in
    dist\<name>, and writes a .zip unless -NoArchive is passed.

    FFmpeg is reused from -FfmpegDir / $env:FFMPEG_DIR when available.
    Otherwise the pinned shared build is downloaded to %TEMP% and unpacked
    there. Pass -NoFfmpeg to build without bundling FFmpeg.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\build.ps1

.EXAMPLE
    ./scripts/build.ps1 -Profile debug -NoArchive
#>
[CmdletBinding()]
param(
    [ValidateSet("debug", "release")]
    [string]$Profile = "release",

    [string]$OutDir = "dist",

    # Directory containing FFmpeg's include/, lib/, and bin/ directories.
    # Also accepts an extraction directory with one FFmpeg folder inside.
    [string]$FfmpegDir = $env:FFMPEG_DIR,

    [string]$FfmpegUrl = "https://github.com/FFmpeg-Builds/FFmpeg-Builds/releases/download/autobuild-2026-11-11-12-39/ffmpeg-N-128790-g9bee1e603c-win64-lgpl-shared.zip",

    # Optional SHA-256 of the downloaded FFmpeg zip.
    [string]$FfmpegSha256 = "",

    [switch]$NoFfmpeg,
    [switch]$NoArchive,
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"

$repo = Split-Path -Parent $PSScriptRoot
$outRoot = if ([System.IO.Path]::IsPathRooted($OutDir)) { $OutDir } else { Join-Path $repo $OutDir }
$stageName = if ($Profile -eq "release") { "sharescreen-windows" } else { "sharescreen-windows-$Profile" }
$stage = Join-Path $outRoot $stageName
$targetDir = Join-Path (Join-Path $repo "target") $Profile
$exe = Join-Path $targetDir "sharescreen.exe"

function Find-FfmpegRoot {
    param([string]$Path)

    if (-not $Path) { return $null }
    if (-not (Test-Path -LiteralPath $Path)) {
        throw "FFmpeg directory does not exist: $Path"
    }

    $resolved = (Resolve-Path -LiteralPath $Path).Path
    if (Test-Path -LiteralPath (Join-Path $resolved "bin")) {
        return $resolved
    }

    $child = Get-ChildItem -LiteralPath $resolved -Directory -ErrorAction SilentlyContinue |
        Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName "bin") } |
        Select-Object -First 1
    if ($child) { return $child.FullName }

    throw "Could not find FFmpeg bin/ under: $Path"
}

function Get-FfmpegRoot {
    if ($NoFfmpeg) { return $null }

    if ($FfmpegDir) {
        return Find-FfmpegRoot -Path $FfmpegDir
    }

    $temp = if ($env:TEMP) { $env:TEMP } else { [System.IO.Path]::GetTempPath() }
    $zipName = Split-Path -Leaf $FfmpegUrl
    $archive = Join-Path $temp $zipName
    $extract = Join-Path $temp ([System.IO.Path]::GetFileNameWithoutExtension($zipName))

    if (-not (Test-Path -LiteralPath $extract)) {
        if (-not (Test-Path -LiteralPath $archive)) {
            Write-Host "Downloading FFmpeg shared build..."
            Invoke-WebRequest -Uri $FfmpegUrl -OutFile $archive
        }

        if ($FfmpegSha256) {
            $actual = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash
            if ($actual -ne $FfmpegSha256.ToUpperInvariant()) {
                throw "FFmpeg checksum mismatch: expected $FfmpegSha256, got $actual"
            }
        }

        Write-Host "Extracting FFmpeg..."
        Expand-Archive -LiteralPath $archive -DestinationPath $extract -Force
    }

    return Find-FfmpegRoot -Path $extract
}

$ffmpegRoot = Get-FfmpegRoot
if ($ffmpegRoot) {
    $env:FFMPEG_DIR = $ffmpegRoot
    Write-Host "Using FFmpeg: $ffmpegRoot"
}

if (-not $SkipBuild) {
    Push-Location $repo
    try {
        $cargoArgs = @("build", "-p", "sharescreen-ui")
        if ($Profile -eq "release") { $cargoArgs += "--release" }
        & cargo @cargoArgs
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build failed with exit code $LASTEXITCODE"
        }
    } finally {
        Pop-Location
    }
} else {
    Write-Host "Skipping build (-SkipBuild)"
}

if (-not (Test-Path -LiteralPath $exe)) {
    throw "Expected executable not found: $exe"
}

if (Test-Path -LiteralPath $stage) {
    Remove-Item -LiteralPath $stage -Recurse -Force
}
New-Item -ItemType Directory -Path $stage -Force | Out-Null
Copy-Item -LiteralPath $exe -Destination $stage

if ($ffmpegRoot) {
    $bin = Join-Path $ffmpegRoot "bin"
    $dlls = @(Get-ChildItem -LiteralPath $bin -Filter "*.dll")
    if ($dlls.Count -eq 0) {
        throw "No FFmpeg DLLs found in: $bin"
    }
    $dlls | Copy-Item -Destination $stage
    # Keep the raw target/release binary runnable as well.
    $dlls | Copy-Item -Destination $targetDir
}

@'
Sharescreen portable build

Extract this folder and run sharescreen.exe. Keep the FFmpeg DLL files beside
the executable. This build targets 64-bit Windows. No Rust, Node.js, FFmpeg,
or source checkout is needed on the machine that runs the app.
'@ | Set-Content -LiteralPath (Join-Path $stage "README.txt") -Encoding utf8

if (-not $NoArchive) {
    $zip = Join-Path $outRoot "$stageName.zip"
    if (Test-Path -LiteralPath $zip) {
        Remove-Item -LiteralPath $zip -Force
    }
    Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zip -Force
    Write-Host "Archive: $zip"
}

Write-Host "Portable build: $stage"
