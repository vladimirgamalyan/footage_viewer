#Requires -Version 7
<#
.SYNOPSIS
  Build a release and publish the full bundle to GitHub Releases.

.DESCRIPTION
  Builds footage_viewer in release mode and packages the executable together with
  the FFmpeg runtime DLLs as footage_viewer-<version>-win64-full.zip, then creates
  or updates the GitHub release tagged v<version>. The tester downloads this bundle
  by hand from the Releases page. The version comes from app/Cargo.toml, so bump it
  there first.

.EXAMPLE
  ./scripts/publish.ps1
#>

$ErrorActionPreference = 'Stop'
# Handle native-command (cargo/gh) failures via $LASTEXITCODE ourselves, so
# `gh release view` returning non-zero (release absent) is a branch, not a throw.
$PSNativeCommandUseErrorActionPreference = $false

$root = Split-Path -Parent $PSScriptRoot
$dist = Join-Path $root 'dist'
$relDir = Join-Path $root 'target/release'

# Version from the crate manifest -- the single source of truth for the tag.
$manifest = Join-Path $root 'app/Cargo.toml'
$m = Select-String -Path $manifest -Pattern '^version = "(.+)"' | Select-Object -First 1
if (-not $m) { throw "Could not read version from $manifest" }
$version = $m.Matches[0].Groups[1].Value
$tag = "v$version"

Write-Host "Publishing $tag..."

# 1. Build. build.rs copies the FFmpeg DLLs next to the exe.
& cargo build --release -p footage_viewer
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$exe = Join-Path $relDir 'footage_viewer.exe'
if (-not (Test-Path $exe)) { throw "missing $exe" }

$dlls = Get-ChildItem -Path $relDir -Filter '*.dll' | ForEach-Object { $_.FullName }
if (-not $dlls) { throw "no DLLs in $relDir; build with FFMPEG_DIR set" }

New-Item -ItemType Directory -Force -Path $dist | Out-Null

# 2. Full bundle (exe + DLLs) -- what the tester downloads and extracts by hand.
$fullZip = Join-Path $dist "footage_viewer-$version-win64-full.zip"
Compress-Archive -Path (@($exe) + $dlls) -DestinationPath $fullZip -Force

# 3. Create or update the GitHub release.
& gh release view $tag *> $null
if ($LASTEXITCODE -eq 0) {
    Write-Host "Release $tag exists; uploading asset (clobber)..."
    & gh release upload $tag $fullZip --clobber
} else {
    Write-Host "Creating release $tag..."
    & gh release create $tag $fullZip --title $tag --notes "footage_viewer $tag"
}
if ($LASTEXITCODE -ne 0) { throw "gh release step failed" }

Write-Host "Done: $tag"
Write-Host ("Asset: " + (Split-Path $fullZip -Leaf))
