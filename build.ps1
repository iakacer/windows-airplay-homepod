# Build HomePod Cast (release) and copy it to dist\HomePod Cast.exe
# Requires: rustup GNU toolchain + MinGW-w64 (gcc, dlltool) on PATH.

$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
$mingw = "$env:USERPROFILE\mingw64\bin"
$env:PATH = "$env:USERPROFILE\.cargo\bin;$mingw;$env:PATH"

Write-Host "Building homepod-cast (release)..."
Push-Location $root
try {
    cargo build -p homepod-cast --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
} finally {
    Pop-Location
}

$dist = Join-Path $root "dist"
New-Item -ItemType Directory -Force -Path $dist | Out-Null
$dest = Join-Path $dist "HomePod Cast.exe"
Copy-Item (Join-Path $root "target\release\homepod-cast.exe") $dest -Force
Write-Host "Done -> $dest ($([math]::Round((Get-Item $dest).Length/1MB,2)) MB)"
