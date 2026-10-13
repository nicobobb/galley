# Installs Galley for the current user without admin rights: copies galley.exe to
# %LOCALAPPDATA%\Programs\Galley and adds that folder to the user's PATH.
# With -Uninstall it deletes the folder and removes it from the PATH.
param(
    [switch]$Uninstall,
    [string]$Destination = (Join-Path $env:LOCALAPPDATA 'Programs\Galley')
)
$ErrorActionPreference = 'Stop'

# Where versions released under the old name, Lupa, were installed.
$legacyDestination = Join-Path $env:LOCALAPPDATA 'Programs\Lupa'

# PATH is read and written unexpanded, so entries like %USERPROFILE%\bin
# don't turn into fixed paths.
function Read-UserPath {
    $key = Get-Item 'HKCU:\Environment'
    $key.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
}

function Write-UserPath([string]$value) {
    Set-ItemProperty 'HKCU:\Environment' -Name 'Path' -Value $value -Type ExpandString
    # Tell Windows about the change so new terminals pick it up.
    [Environment]::SetEnvironmentVariable('GALLEY_INSTALL_REFRESH', '1', 'User')
    [Environment]::SetEnvironmentVariable('GALLEY_INSTALL_REFRESH', $null, 'User')
}

function Test-SameDir([string]$entry, [string]$dir) {
    [Environment]::ExpandEnvironmentVariables($entry).TrimEnd('\') -ieq $dir.TrimEnd('\')
}

# Removes a folder and its PATH entry, if present.
function Remove-Install([string]$dir) {
    if (Test-Path $dir) {
        Remove-Item $dir -Recurse -Force
    }
    $entries = @((Read-UserPath) -split ';' | Where-Object { $_ })
    if ($entries | Where-Object { Test-SameDir $_ $dir }) {
        Write-UserPath ((@($entries | Where-Object { -not (Test-SameDir $_ $dir) })) -join ';')
    }
}

if (Get-Process galley, lupa -ErrorAction SilentlyContinue) {
    throw 'Galley is running. Close it and run this script again.'
}

if ($Uninstall) {
    Remove-Install $Destination
    Write-Host 'Galley was uninstalled.'
    Write-Host "Your settings (allowed commands) remain in $env:APPDATA\galley; delete that folder if you don't want them."
    return
}

$source = Join-Path $PSScriptRoot 'galley.exe'
if (-not (Test-Path $source)) {
    throw "galley.exe wasn't found next to this script ($PSScriptRoot)."
}

Remove-Install $legacyDestination

New-Item -ItemType Directory -Force $Destination | Out-Null
Copy-Item $source $Destination -Force
# Clear the "downloaded from the internet" mark left by the zip.
Unblock-File (Join-Path $Destination 'galley.exe')

$entries = @((Read-UserPath) -split ';' | Where-Object { $_ })
if (-not ($entries | Where-Object { Test-SameDir $_ $Destination })) {
    Write-UserPath ((@($entries) + $Destination) -join ';')
}

Write-Host "Galley was installed in $Destination" -ForegroundColor Green

if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
    Write-Warning 'git was not found. Galley needs it: https://git-scm.com/download/win'
}
if (-not (Get-Command claude -ErrorAction SilentlyContinue)) {
    Write-Warning ('Claude Code (claude) was not found. Everything works except agent threads; ' +
        'to use them, install it and sign in: https://claude.com/claude-code')
}

Write-Host ''
Write-Host 'Open a NEW terminal and run:'
Write-Host '  galley                  (from inside a git repository)'
Write-Host '  galley C:\path\to\repo'
