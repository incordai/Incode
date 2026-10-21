<#
.SYNOPSIS
  Check that incode's sandbox (sbx) really confines commands on this
  Windows machine, and write a report to send back.

.DESCRIPTION
  Builds sbx and runs its Windows tests: writes outside the workspace,
  child processes, the time, CPU and memory limits, killing sbx, the
  environment, exit codes and the agent server (sbx mcp). These need real
  Windows: Wine and Linux can't check Windows' own security.
  Run from the incode folder; needs Rust (https://rustup.rs). Nothing is
  installed and nothing outside a temp folder is changed. The report is
  sbx-windows-report.txt in the incode folder.
  The network-deny tests run only if `sbx setup-windows` was run first (from
  an elevated prompt; `sbx setup-windows --remove` undoes it); otherwise they
  say SKIPPED.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\test-windows.ps1
#>
$ErrorActionPreference = "Continue"
$repo = (Get-Location).Path
if (-not (Test-Path (Join-Path $repo "crates\sandbox\Cargo.toml"))) {
    Write-Error "Run this from the incode folder."; exit 1
}
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "Rust is needed: install it from https://rustup.rs, then run this again."; exit 1
}
$report = Join-Path $repo "sbx-windows-report.txt"
$lines = New-Object System.Collections.Generic.List[string]
function Log($text) { Write-Host $text; $lines.Add($text) }

Log "sbx Windows check, $(Get-Date -Format s)"
Log "Windows: $([Environment]::OSVersion.VersionString); PowerShell $($PSVersionTable.PSVersion)"
Log "Admin: $(([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator))"
Log ""

Log "== build"
$build = & cargo build -p incord-sandbox 2>&1 | Out-String
Log ($build.Trim() -split "`n" | Select-Object -Last 3 | Out-String).Trim()

Log ""; Log "== sbx doctor"
$sbx = Join-Path $repo "target\debug\sbx.exe"
$env:SBX_BACKEND = "native"
Log ((& $sbx doctor 2>&1 | Out-String).Trim())

foreach ($suite in @("--lib", "--test escape_windows", "--test mcp")) {
    Log ""; Log "== cargo test -p incord-sandbox $suite"
    $args = @("test", "-p", "incord-sandbox") + ($suite -split " ") + @("--", "--test-threads=1")
    $out = & cargo @args 2>&1 | Out-String
    $keep = $out -split "`r?`n" | Where-Object {
        $_ -match "^test |test result|panicked|left:|right:|FAILED|error"
    }
    Log (($keep | Select-Object -First 120) -join "`n")
}

$lines | Set-Content -Encoding UTF8 $report
Write-Host ""
Write-Host "Report written to $report. Send it back (paste it, or attach the file)."
