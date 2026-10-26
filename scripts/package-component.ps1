# Package reef or burrow for Windows as <name>-<version>-<target>.zip:
# the binary, its install guide as INSTALL.md, and the example files from
# packaging/<component>/ in examples/.
#
#   ./scripts/package-component.ps1 -Component reef|burrow -Binary <path/to/exe> [-OutDir dist] [-Target x86_64-pc-windows-msvc]
param(
    [Parameter(Mandatory = $true)][ValidateSet("reef", "burrow")][string]$Component,
    [Parameter(Mandatory = $true)][string]$Binary,
    [string]$OutDir,
    [string]$Target = "x86_64-pc-windows-msvc"
)
$ErrorActionPreference = "Stop"

$Root = Split-Path -Parent $PSScriptRoot
if (-not $OutDir) { $OutDir = Join-Path $Root "dist" }
if (-not (Test-Path $Binary)) { throw "binary not found: $Binary" }
switch ($Component) {
    "reef" { $Exe = "reef"; $Guide = "docs/reef/install.md" }
    "burrow" { $Exe = "burrow"; $Guide = "docs/burrow/install.md" }
}

# Version from the [workspace.package] (or [package]) section of Cargo.toml.
$Version = $null
$InPackage = $false
foreach ($line in Get-Content (Join-Path $Root "Cargo.toml")) {
    if ($line -match '^\[(workspace\.)?package\]') { $InPackage = $true; continue }
    if ($line -match '^\[') { $InPackage = $false }
    if ($InPackage -and $line -match '^version\s*=\s*"([^"]+)"') { $Version = $Matches[1]; break }
}
if (-not $Version) { throw "could not read version from Cargo.toml" }

$Name = "$Exe-$Version-$Target"
$Stage = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
$Dir = Join-Path $Stage $Name
New-Item -ItemType Directory -Force -Path (Join-Path $Dir "examples") | Out-Null
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

try {
    Copy-Item $Binary (Join-Path $Dir "$Exe.exe")
    Copy-Item (Join-Path $Root $Guide) (Join-Path $Dir "INSTALL.md")
    Copy-Item (Join-Path $Root "packaging/$Component/*") (Join-Path $Dir "examples")
    foreach ($f in "LICENSE", "NOTICE", "CHANGELOG.md") {
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
