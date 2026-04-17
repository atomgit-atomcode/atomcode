# AtomCode installer for Windows — PowerShell
#
#   irm https://atomgit.com/atomgit_atomcode/atomcode-release/raw/main/install.ps1 | iex
#
# Env overrides:
#   $env:ATOMCODE_VERSION   release tag to install (default: v4.15.3)
#   $env:ATOMCODE_PREFIX    install dir (default: %LOCALAPPDATA%\AtomCode)

$ErrorActionPreference = "Stop"

$Version = if ($env:ATOMCODE_VERSION) { $env:ATOMCODE_VERSION } else { "v4.15.3" }
$RepoBase = "https://atomgit.com/atomgit_atomcode/atomcode-release/releases/download"

# --- detect arch ---
$Arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
switch ("$Arch") {
    "X64"   { $ArchTag = "x64" }
    "Arm64" { $ArchTag = "arm64" }
    default {
        Write-Host "Unsupported architecture: $Arch (supported: x64, arm64)" -ForegroundColor Red
        exit 1
    }
}

$BinName = "atomcode-$Version-windows-$ArchTag.exe"
$Url = "$RepoBase/$Version/$BinName"

# --- pick install dir ---
$Prefix = if ($env:ATOMCODE_PREFIX) {
    $env:ATOMCODE_PREFIX
} else {
    Join-Path $env:LOCALAPPDATA "AtomCode"
}

if (-not (Test-Path $Prefix)) {
    New-Item -ItemType Directory -Path $Prefix -Force | Out-Null
}

# --- download ---
$Dest = Join-Path $Prefix "atomcode.exe"
$TmpFile = Join-Path $env:TEMP "atomcode-download.exe"

Write-Host "==> Downloading $BinName"
Write-Host "    from $Url"

try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $ProgressPreference = 'SilentlyContinue'
    Invoke-WebRequest -Uri $Url -OutFile $TmpFile -UseBasicParsing
} catch {
    Write-Host "Error: download failed." -ForegroundColor Red
    Write-Host "       $_" -ForegroundColor Red
    Write-Host "       URL: $Url" -ForegroundColor Red
    exit 1
}

# Sanity check: must not be an HTML page
$Header = [System.IO.File]::ReadAllBytes($TmpFile)[0..3]
if ([char]$Header[0] -eq '<') {
    Write-Host "Error: download looks like an HTML page, not a binary." -ForegroundColor Red
    Write-Host "       The release may not exist, or the URL is wrong." -ForegroundColor Red
    Write-Host "       URL: $Url" -ForegroundColor Red
    Remove-Item $TmpFile -Force -ErrorAction SilentlyContinue
    exit 1
}

# --- install ---
Write-Host "==> Installing to $Dest"
Move-Item -Path $TmpFile -Destination $Dest -Force

# --- add to PATH ---
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($UserPath -notlike "*$Prefix*") {
    $NewPath = "$Prefix;$UserPath"
    [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
    # Also update current session so user can use it immediately
    $env:Path = "$Prefix;$env:Path"
    Write-Host ""
    Write-Host "Added $Prefix to user PATH." -ForegroundColor Green
    Write-Host "New terminal windows will pick it up automatically."
}

# --- done ---
Write-Host ""
Write-Host "Installed: $Dest" -ForegroundColor Green
try {
    & $Dest --version
} catch {
    # ignore
}

Write-Host ""
Write-Host "Run 'atomcode' to get started." -ForegroundColor Cyan
