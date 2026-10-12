# Package a Windows build as crusteo-<version>-x86_64-pc-windows-msvc.zip.
#
#   ./scripts/package-windows.ps1 [-Binary target/release/crusteo.exe] [-OutDir dist] [-Target x86_64-pc-windows-msvc]
param(
    [string]$Binary,
    [string]$OutDir,
    [string]$Target = "x86_64-pc-windows-msvc"
)
$ErrorActionPreference = "Stop"

$Root = Split-Path -Parent $PSScriptRoot
if (-not $Binary) { $Binary = Join-Path $Root "target/release/crusteo.exe" }
if (-not $OutDir) { $OutDir = Join-Path $Root "dist" }
if (-not (Test-Path $Binary)) { throw "binary not found: $Binary" }

# Version from the [package] section of Cargo.toml.
$Version = $null
$InPackage = $false
foreach ($line in Get-Content (Join-Path $Root "Cargo.toml")) {
    if ($line -match '^\[package\]') { $InPackage = $true; continue }
    if ($line -match '^\[') { $InPackage = $false }
    if ($InPackage -and $line -match '^version\s*=\s*"([^"]+)"') { $Version = $Matches[1]; break }
}
if (-not $Version) { throw "could not read version from Cargo.toml" }

$Name = "crusteo-$Version-$Target"
$Stage = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
$Dir = Join-Path $Stage $Name
New-Item -ItemType Directory -Force -Path $Dir | Out-Null
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

try {
    Copy-Item $Binary (Join-Path $Dir "crusteo.exe")
    foreach ($f in "README.md", "LICENSE", "NOTICE", "CHANGELOG.md") {
        $src = Join-Path $Root $f
        if (Test-Path $src) { Copy-Item $src $Dir }
    }
    $Zip = Join-Path (Resolve-Path $OutDir) "$Name.zip"
    if (Test-Path $Zip) { Remove-Item $Zip }
    Compress-Archive -Path $Dir -DestinationPath $Zip
    Write-Output $Zip
}
finally {
    Remove-Item -Recurse -Force $Stage -ErrorAction SilentlyContinue
}
