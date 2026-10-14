# Run with: powershell -File scripts/tests/install-version.ps1 (or pwsh).
# Execute the installer's version resolver with mocked HTTP; never install a binary.
$ErrorActionPreference = 'Stop'
$Root = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
$Source = Get-Content (Join-Path $Root 'scripts/install.ps1') -Raw
$Tokens = $null
$ParseErrors = $null
[System.Management.Automation.Language.Parser]::ParseInput($Source, [ref]$Tokens, [ref]$ParseErrors) | Out-Null
if ($ParseErrors.Count -gt 0) { throw "Installer syntax errors: $ParseErrors" }
$Resolver = $Source.Substring(0, $Source.IndexOf('# --- pick install dir ---'))
$OriginalVersion = $env:ATOMCODE_VERSION
$OriginalArch = $env:PROCESSOR_ARCHITECTURE
$OriginalArch64 = $env:PROCESSOR_ARCHITEW6432
function Invoke-RestMethod {
    param($Uri, [switch]$UseBasicParsing, $TimeoutSec)
    $script:Requests.Add($Uri)
    if ($Uri -like '*/releases/latest') {
        if ($script:ApiFails) { throw 'mock API failure' }
        return $script:ApiBody
    }
    if ($script:ManifestFails) { throw 'mock manifest failure' }
    return $script:ManifestBody
}
try {
    $env:PROCESSOR_ARCHITECTURE = 'AMD64'
    $env:PROCESSOR_ARCHITEW6432 = 'AMD64'
    $Cases = @(
        @{ Override='v9.8.7'; Api=@{}; Manifest=@{}; Expected='v9.8.7'; Calls=0 },
        @{ Api=@{tag_name='v9.8.6'}; Manifest=@{}; Expected='v9.8.6'; Calls=1 },
        @{ ApiFails=$true; Manifest=@{version='v9.8.5'}; Expected='v9.8.5'; Calls=2 },
        @{ Api=@{}; Manifest=@{version='v9.8.5-beta.1'}; Expected='v9.8.5-beta.1'; Calls=2 },
        @{ Api=@{tag_name='../../bad'}; Manifest=@{version='v9.8.5'}; Expected='v9.8.5'; Calls=2 },
        @{ ApiFails=$true; ManifestFails=$true; Fails=$true; Calls=2 },
        @{ Api=@{}; Manifest=@{version='broken'}; Fails=$true; Calls=2 }
    )
    foreach ($Case in $Cases) {
        $env:ATOMCODE_VERSION = $Case.Override
        $script:Requests = New-Object 'System.Collections.Generic.List[string]'
        $script:ApiBody = $Case.Api
        $script:ManifestBody = $Case.Manifest
        $script:ApiFails = $Case.ApiFails
        $script:ManifestFails = $Case.ManifestFails
        $Failure = $null
        try { $Actual = & ([scriptblock]::Create($Resolver + "`nreturn `$Version")) }
        catch { $Failure = $_ }
        if ($Case.Fails) {
            if (-not $Failure -or "$Failure" -notmatch 'ATOMCODE_VERSION') { throw 'Expected failure with override guidance' }
        } elseif ($Failure -or $Actual -ne $Case.Expected) {
            throw "Expected $($Case.Expected), got $Actual; error: $Failure"
        }
        if ($script:Requests.Count -ne $Case.Calls) { throw 'Unexpected metadata request count' }
    }
    Write-Host '7 installer version scenarios passed.'
} finally {
    $env:ATOMCODE_VERSION = $OriginalVersion
    $env:PROCESSOR_ARCHITECTURE = $OriginalArch
    $env:PROCESSOR_ARCHITEW6432 = $OriginalArch64
}
