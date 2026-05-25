# AtomCode uninstaller — PowerShell
#
#   irm https://atomgit.com/atomgit_atomcode/atomcode/raw/main/uninstall.ps1 | iex
#
# Flags (pass via param):
#   -Yes              skip prompts; use defaults (G1=yes, G2=no, G3=yes)
#   -Purge            delete everything including %USERPROFILE%\.atomcode
#   -KeepData         only delete binary + PATH entry
#   -DryRun           print plan, do nothing
#   -PrintManifest    emit manifest used for parity tests, exit
param(
    [switch]$Yes,
    [switch]$Purge,
    [switch]$KeepData,
    [switch]$DryRun,
    [switch]$PrintManifest
)

$ErrorActionPreference = "Stop"

$Group2Files = @("auth.toml","mcp.json","config.toml","ATOMCODE.md")
$Group3Files = @("history","input_history.txt","recent_dirs.txt","codingplan_sync.json","device_id")
$Group3Dirs  = @("staged","telemetry","plugins","commands","skills")
$Group3Prefixes = @("notice.")

if ($PrintManifest) {
    Write-Output ("group2_files=" + ($Group2Files -join " "))
    Write-Output ("group3_files=" + ($Group3Files -join " "))
    Write-Output ("group3_dirs="  + ($Group3Dirs  -join " "))
    Write-Output ("group3_prefixes=" + ($Group3Prefixes -join " "))
    exit 0
}

if ($Purge -and $KeepData) { Write-Error "-Purge conflicts with -KeepData"; exit 2 }

# locate install dir
$InstallDir = if ($env:ATOMCODE_PREFIX) { $env:ATOMCODE_PREFIX } else { Join-Path $env:LOCALAPPDATA "AtomCode" }
$Binary = Join-Path $InstallDir "atomcode.exe"

$DataDir = if ($env:ATOMCODE_HOME) { $env:ATOMCODE_HOME } else { Join-Path $env:USERPROFILE ".atomcode" }

# plan
Write-Host "Will remove (Group 1):"
if (Test-Path $Binary) { Write-Host "  $Binary" }
foreach ($f in @("atomcode.exe.bak",".atomcode.rolling",".atomcode.download",".atomcode.writable-probe")) {
    $p = Join-Path $InstallDir $f
    if (Test-Path $p) { Write-Host "  $p" }
}

if (-not $KeepData) {
    Write-Host ""
    Write-Host "Will consider (Group 2 — credentials):"
    foreach ($f in $Group2Files) { $p = Join-Path $DataDir $f; if (Test-Path $p) { Write-Host "  $p" } }
    Write-Host ""
    Write-Host "Will remove (Group 3 — state):"
    foreach ($f in $Group3Files) { $p = Join-Path $DataDir $f; if (Test-Path $p) { Write-Host "  $p" } }
    foreach ($d in $Group3Dirs)  { $p = Join-Path $DataDir $d; if (Test-Path $p) { Write-Host "  $p" } }
    foreach ($pre in $Group3Prefixes) {
        Get-ChildItem -Path $DataDir -Filter "$pre*" -ErrorAction SilentlyContinue |
            ForEach-Object { Write-Host "  $($_.FullName)" }
    }
}

if ($DryRun) { exit 0 }

# resolve decisions
$DoG2 = $false; $DoG3 = $true
if ($Purge)        { $DoG2 = $true;  $DoG3 = $true }
elseif ($KeepData) { $DoG2 = $false; $DoG3 = $false }
elseif (-not $Yes) {
    if (-not [Environment]::UserInteractive) {
        Write-Error "refusing to run interactively without an interactive session; pass -Yes / -Purge / -KeepData / -DryRun"
        exit 2
    }
    $a = Read-Host "[Group 1] Remove binary and PATH edit? [Y/n]"
    if ($a -match '^(n|no)$') { Write-Host "aborted"; exit 1 }
    $a = Read-Host "[Group 2] Remove credentials and global config? [y/N]"
    if ($a -match '^(y|yes)$') { $DoG2 = $true }
    $a = Read-Host "[Group 3] Remove local state and extensions? [Y/n]"
    if ($a -match '^(n|no)$') { $DoG3 = $false }
    $a = Read-Host "Continue? [y/N]"
    if (-not ($a -match '^(y|yes)$')) { Write-Host "aborted"; exit 1 }
}

# execute (state → credentials → rc → binary)
if ($DoG3) {
    foreach ($f in $Group3Files) { Remove-Item -Force -ErrorAction SilentlyContinue (Join-Path $DataDir $f) }
    foreach ($d in $Group3Dirs)  { Remove-Item -Recurse -Force -ErrorAction SilentlyContinue (Join-Path $DataDir $d) }
    foreach ($pre in $Group3Prefixes) {
        Get-ChildItem -Path $DataDir -Filter "$pre*" -ErrorAction SilentlyContinue |
            ForEach-Object { Remove-Item -Force -ErrorAction SilentlyContinue $_.FullName }
    }
}
if ($DoG2) { foreach ($f in $Group2Files) { Remove-Item -Force -ErrorAction SilentlyContinue (Join-Path $DataDir $f) } }
Remove-Item -Force -ErrorAction SilentlyContinue $DataDir   # only succeeds if empty

# Windows User PATH cleanup
$cur = [Environment]::GetEnvironmentVariable("Path", "User")
if ($cur) {
    $entries = $cur -split ';'
    $kept = $entries | Where-Object {
        $_.TrimEnd('\').ToLower() -ne $InstallDir.TrimEnd('\').ToLower() -and
        $_.TrimEnd('\').ToLower() -ne ($env:LOCALAPPDATA + '\AtomCode').ToLower()
    }
    if ($kept.Count -ne $entries.Count) {
        [Environment]::SetEnvironmentVariable("Path", ($kept -join ';'), "User")
        Write-Host "removed $InstallDir from User PATH"
    }
}

# binary + install dir
if (Test-Path $Binary) {
    try {
        Remove-Item -Force $Binary
        Remove-Item -Recurse -Force $InstallDir -ErrorAction SilentlyContinue
    } catch {
        Write-Error "could not remove $Binary — close any running atomcode.exe and re-run."
        exit 4
    }
}

Write-Host "uninstall complete."
