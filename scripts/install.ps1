# AtomCode installer for Windows — PowerShell
#
#   irm https://raw.atomgit.com/atomgit_atomcode/atomcode/raw/main/scripts/install.ps1 | iex
#
# Env overrides:
#   $env:ATOMCODE_VERSION   release tag to install (default: latest release,
#                             auto-detected from the AtomGit API)
#   $env:ATOMCODE_PREFIX    install dir (default: %LOCALAPPDATA%\AtomCode)
# IMPORTANT: when changing install paths, registry edits, or filenames here,
# also update scripts/uninstall.ps1 AND
# crates/atomcode-core/src/uninstall/paths.rs. The CI parity test guards
# the manifest, but binary path / PATH edit are not checked.

param(
  [string]$Invite = ""
)

$ErrorActionPreference = "Stop"

# --- referral invite argument fallback ---
if (-not $Invite) {
  $Invite = $env:ATOMCODE_INVITE
}

# Fallback version used only when $env:ATOMCODE_VERSION is unset and the API lookup fails.
$DefaultVersion = "v5.0.1"
$RepoBase = "https://atomgit.com/atomgit_atomcode/atomcode/releases/download"
$RepoLatestApi = "https://api.atomgit.com/api/v5/repos/atomgit_atomcode/atomcode/releases/latest"

# --- detect arch ---
# Prefer PROCESSOR_ARCHITEW6432 (set only when a 32-bit process runs on a 64-bit
# OS — it holds the real OS arch). Fall back to PROCESSOR_ARCHITECTURE.
# Avoids RuntimeInformation::OSArchitecture which is empty on older PS 5.1/.NET.
$RealArch = if ($env:PROCESSOR_ARCHITEW6432) {
    $env:PROCESSOR_ARCHITEW6432
} else {
    $env:PROCESSOR_ARCHITECTURE
}

switch ($RealArch) {
    "AMD64" { $ArchTag = "x64" }
    "ARM64" { $ArchTag = "arm64" }
    default {
        Write-Host "Unsupported architecture: $RealArch (supported: AMD64, ARM64)" -ForegroundColor Red
        exit 1
    }
}

# --- resolve version ---
# Honor $env:ATOMCODE_VERSION if set; otherwise auto-detect the latest release
# tag from the API, falling back to $DefaultVersion if the lookup yields nothing.
if ($env:ATOMCODE_VERSION) {
    $Version = $env:ATOMCODE_VERSION
} else {
    Write-Host "==> Detecting latest version"
    try {
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
        $ProgressPreference = 'SilentlyContinue'
        $Latest = Invoke-RestMethod -Uri $RepoLatestApi -UseBasicParsing -TimeoutSec 10
        $Version = $Latest.tag_name
    } catch {
        $Version = $null
    }
    if (-not $Version) { $Version = $DefaultVersion }
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

# --- referral invite code handling ---
if ($Invite) {
  if ($Invite -match '^[A-Za-z0-9]{8}$') {
    $AtomcodeDir = if ($env:ATOMCODE_HOME) {
      $env:ATOMCODE_HOME
    } else {
      Join-Path $env:USERPROFILE ".atomcode"
    }

    New-Item -ItemType Directory -Force -Path $AtomcodeDir | Out-Null

    $InstallUuid = [guid]::NewGuid().ToString()

    $pendingInvite = @"
invite_code=$Invite
install_uuid=$InstallUuid
attempted_at=$([DateTimeOffset]::UtcNow.ToUnixTimeSeconds())
"@

    Set-Content -Path (Join-Path $AtomcodeDir "pending_invite") -Value $pendingInvite
  } else {
    Write-Warning "Invalid invite code format, skipping referral"
  }
}
# --- end referral handling ---

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
# Move-Item -Force is unreliable on Windows PowerShell 5.1 when the destination
# already exists (see: fails with "当文件已存在时，无法创建该文件"). Do an explicit
# Remove-Item first, and surface a clear message if the old binary is locked
# (atomcode.exe still running in another terminal).
Write-Host "==> Installing to $Dest"
if (Test-Path $Dest) {
    try {
        Remove-Item $Dest -Force -ErrorAction Stop
    } catch {
        Write-Host "Error: cannot replace existing $Dest" -ForegroundColor Red
        Write-Host "       It may be in use. Close any running atomcode.exe and re-run this installer." -ForegroundColor Red
        Write-Host "       $_" -ForegroundColor Red
        Remove-Item $TmpFile -Force -ErrorAction SilentlyContinue
        exit 1
    }
}
Move-Item -Path $TmpFile -Destination $Dest

# --- add to PATH ---
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
# Compare COMPLETE, normalized PATH entries — never a substring of the raw string.
# A raw `-like "*$Prefix*"` false-positives when another entry merely CONTAINS the
# prefix (e.g. "...\AtomCodeBackup" for prefix "...\AtomCode"), silently skipping the
# real add so atomcode isn't on PATH in a new terminal. Split on ';', trim trailing
# '\' + whitespace, match case-insensitively (Windows paths are case-insensitive).
# Also avoids `-like` treating the prefix as a wildcard pattern (e.g. a literal '[').
$PrefixNorm = $Prefix.TrimEnd('\').Trim()
$InPath = $false
if ($UserPath) {
    foreach ($entry in ($UserPath -split ';')) {
        if ($entry.TrimEnd('\').Trim() -ieq $PrefixNorm) { $InPath = $true; break }
    }
}
if (-not $InPath) {
    $NewPath = if ($UserPath) { "$Prefix;$UserPath" } else { $Prefix }
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
