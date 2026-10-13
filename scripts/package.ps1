# Builds Galley in release mode and creates dist\galley-<version>-windows-x64.zip to share.
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
Push-Location $root
try {
    cargo build --release -p review_app
    if ($LASTEXITCODE -ne 0) { throw 'Build failed.' }

    $cargoToml = Get-Content (Join-Path $root 'Cargo.toml') -Raw
    if ($cargoToml -notmatch '(?m)^version\s*=\s*"([^"]+)"') { throw 'No version found in Cargo.toml.' }
    $name = "galley-$($Matches[1])-windows-x64"

    $dist = Join-Path $root 'dist'
    $staging = Join-Path $dist $name
    $zip = Join-Path $dist "$name.zip"
    if (Test-Path $staging) { Remove-Item $staging -Recurse -Force }
    New-Item -ItemType Directory -Force $staging | Out-Null

    Copy-Item (Join-Path $root 'target\release\galley.exe') $staging
    Copy-Item (Join-Path $root 'packaging\*') $staging
    Compress-Archive -Path (Join-Path $staging '*') -DestinationPath $zip -Force
    Remove-Item $staging -Recurse -Force

    $mb = [math]::Round((Get-Item $zip).Length / 1MB, 1)
    Write-Host "Done: $zip ($mb MB)" -ForegroundColor Green
}
finally {
    Pop-Location
}
