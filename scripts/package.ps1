# Builds Galley in release mode and creates, in dist\:
# - galley-<version>-windows-x64.zip to share and install by hand;
# - galley-windows-x64.exe, which installed copies download to update themselves;
# - SHA256SUMS with the hashes of both, which the updater checks.
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
    Copy-Item (Join-Path $root 'LICENSE') (Join-Path $staging 'LICENSE.txt')
    Compress-Archive -Path (Join-Path $staging '*') -DestinationPath $zip -Force
    Remove-Item $staging -Recurse -Force

    # The name has no version so updates can find it in any release.
    $exe = Join-Path $dist 'galley-windows-x64.exe'
    Copy-Item (Join-Path $root 'target\release\galley.exe') $exe -Force
    # sha256sum format, with LF line endings: "<hash>  <file>".
    $sums = foreach ($file in Get-Item $zip, $exe) {
        "$((Get-FileHash $file -Algorithm SHA256).Hash.ToLower())  $($file.Name)"
    }
    [IO.File]::WriteAllText((Join-Path $dist 'SHA256SUMS'), (($sums -join "`n") + "`n"))

    $mb = [math]::Round((Get-Item $zip).Length / 1MB, 1)
    Write-Host "Done: $zip ($mb MB)" -ForegroundColor Green
}
finally {
    Pop-Location
}
