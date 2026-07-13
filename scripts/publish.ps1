#Requires -Version 7
<#
.SYNOPSIS
  Build a release and publish it to GitHub Releases for the in-app self-updater.

.DESCRIPTION
  Builds footage_viewer in release mode and packages the executable as
  footage_viewer-<version>-<target>.zip -- the asset the running app downloads
  to update itself -- then creates or updates the GitHub release tagged
  v<version>. The version comes from app/Cargo.toml, so bump it there first.

  Run with -Full to also attach footage_viewer-<version>-win64-full.zip, the
  one-time bundle (exe + FFmpeg DLLs) a brand-new tester downloads by hand. Use
  it for the first release and whenever the bundled DLLs change; a plain feature
  release only needs the ~14 MB exe zip, since self-update swaps just the exe.

.EXAMPLE
  ./scripts/publish.ps1          # feature release: exe-only asset
  ./scripts/publish.ps1 -Full    # first release / DLLs changed: also full bundle
#>
param([switch]$Full)

$ErrorActionPreference = 'Stop'
# Handle native-command (cargo/rustc/gh) failures via $LASTEXITCODE ourselves, so
# `gh release view` returning non-zero (release absent) is a branch, not a throw.
$PSNativeCommandUseErrorActionPreference = $false

$root = Split-Path -Parent $PSScriptRoot
$dist = Join-Path $root 'dist'
$relDir = Join-Path $root 'target/release'

# Version from the crate manifest -- the single source of truth the app compares against.
$manifest = Join-Path $root 'app/Cargo.toml'
$m = Select-String -Path $manifest -Pattern '^version = "(.+)"' | Select-Object -First 1
if (-not $m) { throw "Could not read version from $manifest" }
$version = $m.Matches[0].Groups[1].Value

# Host triple must match TARGET in app/src/update.rs so the app picks this asset.
$target = (& rustc -vV | Select-String '^host: (.+)$').Matches[0].Groups[1].Value
$tag = "v$version"

Write-Host "Publishing $tag ($target)..."

# 1. Build. build.rs copies the FFmpeg DLLs next to the exe.
& cargo build --release -p footage_viewer
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$exe = Join-Path $relDir 'footage_viewer.exe'
if (-not (Test-Path $exe)) { throw "missing $exe" }

New-Item -ItemType Directory -Force -Path $dist | Out-Null

# 2. exe-only update asset -- what the self-updater downloads and unpacks.
$exeZip = Join-Path $dist "footage_viewer-$version-$target.zip"
Compress-Archive -Path $exe -DestinationPath $exeZip -Force
$assets = @($exeZip)

# 3. Optional one-time full bundle (exe + DLLs). Its name omits the target triple
#    so the self-updater never picks it instead of the exe-only asset.
if ($Full) {
    $dlls = Get-ChildItem -Path $relDir -Filter '*.dll' | ForEach-Object { $_.FullName }
    if (-not $dlls) { throw "no DLLs in $relDir; build with FFMPEG_DIR set" }
    $fullZip = Join-Path $dist "footage_viewer-$version-win64-full.zip"
    Compress-Archive -Path (@($exe) + $dlls) -DestinationPath $fullZip -Force
    $assets += $fullZip
}

# 4. Create or update the GitHub release.
& gh release view $tag *> $null
if ($LASTEXITCODE -eq 0) {
    Write-Host "Release $tag exists; uploading assets (clobber)..."
    & gh release upload $tag @assets --clobber
} else {
    Write-Host "Creating release $tag..."
    & gh release create $tag @assets --title $tag --notes "footage_viewer $tag"
}
if ($LASTEXITCODE -ne 0) { throw "gh release step failed" }

Write-Host "Done: $tag"
Write-Host ("Assets: " + (($assets | ForEach-Object { Split-Path $_ -Leaf }) -join ', '))
