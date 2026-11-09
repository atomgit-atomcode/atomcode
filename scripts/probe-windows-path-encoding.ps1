# ASCII-only source: safe to launch with Windows PowerShell 5.1 -File.
# Creates only a unique directory under the system temp directory. Keeps the
# UTF-8/BOM JSON report and tiny fixtures for inspection. No real workspace,
# system locale, console code page, session, or existing file is modified.
$ErrorActionPreference = 'Stop'
if ($env:OS -ne 'Windows_NT') { throw 'This probe requires Windows.' }
if (-not ('AtomCodeEncodingCodePages' -as [type])) {
    Add-Type @'
using System.Runtime.InteropServices;
public static class AtomCodeEncodingCodePages {
    [DllImport("kernel32.dll")] public static extern uint GetACP();
    [DllImport("kernel32.dll")] public static extern uint GetOEMCP();
    [DllImport("kernel32.dll")] public static extern uint GetConsoleCP();
    [DllImport("kernel32.dll")] public static extern uint GetConsoleOutputCP();
}
'@
}
# cmd refuses UNC current directories and may fall back to a system directory.
$tempRoot = [IO.Path]::GetTempPath()
if ($tempRoot.StartsWith('\\')) { throw 'Use a local-drive TEMP directory for this probe; UNC cwd is not supported by cmd.' }
$probeRoot = Join-Path $tempRoot ('atomcode-encoding-' + [guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($probeRoot) | Out-Null
$first = '2026' + [char]0x589E + [char]0x808C
$second = [string][char]0x5DE5 + [char]0x4F5C + [char]0x533A + '_wb06'
$relativeTarget = Join-Path (Join-Path $first $second) '_atomcode_archive'
$utf8 = New-Object System.Text.UTF8Encoding($false)
$utf8Bom = New-Object System.Text.UTF8Encoding($true)
$systemPowerShell = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'

function Invoke-ProbeProcess($program, $arguments, $cwd) {
    $info = New-Object System.Diagnostics.ProcessStartInfo
    $info.FileName = $program
    $info.Arguments = $arguments
    $info.WorkingDirectory = $cwd
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $process = New-Object System.Diagnostics.Process
    $process.StartInfo = $info
    $outBytes = New-Object System.IO.MemoryStream
    $errBytes = New-Object System.IO.MemoryStream
    try {
        [void]$process.Start()
        # Preserve bytes: decoding the diagnostic output is a separate question.
        $outRead = $process.StandardOutput.BaseStream.CopyToAsync($outBytes)
        $errRead = $process.StandardError.BaseStream.CopyToAsync($errBytes)
        if (-not $process.WaitForExit(15000)) { $process.Kill(); throw 'Probe child timed out.' }
        $outRead.GetAwaiter().GetResult()
        $errRead.GetAwaiter().GetResult()
        return @{
            exitCode = $process.ExitCode
            stdoutBase64 = [Convert]::ToBase64String($outBytes.ToArray())
            stderrBase64 = [Convert]::ToBase64String($errBytes.ToArray())
        }
    } finally { $process.Dispose(); $outBytes.Dispose(); $errBytes.Dispose() }
}

$results = @()
foreach ($case in @('native', 'cmd-argv', 'cmd-utf8-batch', 'ps51-encoded', 'ps51-file-no-bom', 'ps51-file-bom')) {
    $caseRoot = Join-Path $probeRoot $case
    [IO.Directory]::CreateDirectory($caseRoot) | Out-Null
    $expected = Join-Path $caseRoot $relativeTarget
    # Relative targets ensure even deliberately misdecoded scripts can only
    # create test trees INSIDE caseRoot, including when the temp parent is CJK.
    $script = '$ErrorActionPreference = ''Stop''; $p = ''' + $relativeTarget.Replace("'", "''") + '''; [IO.Directory]::CreateDirectory($p) | Out-Null; [IO.File]::WriteAllText([IO.Path]::Combine($p, ''probe.txt''), ''ok'')'
    $command = 'mkdir "' + $relativeTarget + '" && echo ok > "' + (Join-Path $relativeTarget 'probe.txt') + '"'
    $processResult = $null
    try {
        if ($case -eq 'native') {
            [IO.Directory]::CreateDirectory($expected) | Out-Null
            [IO.File]::WriteAllText((Join-Path $expected 'probe.txt'), 'ok')
        } elseif ($case -eq 'cmd-argv') {
            $processResult = Invoke-ProbeProcess $env:ComSpec ('/D /C ' + $command) $caseRoot
        } elseif ($case -eq 'cmd-utf8-batch') {
            $file = Join-Path $caseRoot 'probe.cmd'
            [IO.File]::WriteAllText($file, ('@echo off' + "`r`n" + $command + "`r`n"), $utf8)
            $processResult = Invoke-ProbeProcess $env:ComSpec '/D /C probe.cmd' $caseRoot
        } elseif ($case -eq 'ps51-encoded') {
            $base64 = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($script))
            $processResult = Invoke-ProbeProcess $systemPowerShell ('-NoProfile -NonInteractive -EncodedCommand ' + $base64) $caseRoot
        } else {
            $file = Join-Path $caseRoot 'probe.ps1'
            $encoding = if ($case -eq 'ps51-file-bom') { $utf8Bom } else { $utf8 }
            [IO.File]::WriteAllText($file, $script, $encoding)
            # Process-only policy, so probe scripts can run without changing the
            # machine policy. The script body is generated entirely above.
            $processResult = Invoke-ProbeProcess $systemPowerShell '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File probe.ps1' $caseRoot
        }
        $results += @{
            case = $case
            expectedPath = $expected
            correctPathExists = [IO.File]::Exists((Join-Path $expected 'probe.txt'))
            topDirectoryNames = @(Get-ChildItem -LiteralPath $caseRoot -Directory | ForEach-Object { $_.Name })
            process = $processResult
        }
    } catch {
        $results += @{ case = $case; error = $_.Exception.Message }
    }
}
$report = @{
    note = 'Environment probe only. Negative script cases do not prove how an earlier AtomCode incident happened.'
    root = $probeRoot
    powershellVersion = $PSVersionTable.PSVersion.ToString()
    hostCodePages = @{
        ansi = [AtomCodeEncodingCodePages]::GetACP()
        oem = [AtomCodeEncodingCodePages]::GetOEMCP()
        consoleInput = [AtomCodeEncodingCodePages]::GetConsoleCP()
        consoleOutput = [AtomCodeEncodingCodePages]::GetConsoleOutputCP()
    }
    results = $results
}
$reportPath = Join-Path $probeRoot 'report.json'
[IO.File]::WriteAllText($reportPath, ($report | ConvertTo-Json -Depth 10), $utf8Bom)
Write-Output ('Report: ' + $reportPath)
$results | ForEach-Object { Write-Output ($_.case + ': correctPathExists=' + $_.correctPathExists) }
