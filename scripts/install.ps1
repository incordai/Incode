# Install incode (and sbx, the standalone sandbox) on Windows from a GitHub
# release.
#
#   irm https://raw.githubusercontent.com/incordai/incode/main/scripts/install.ps1 | iex
#
# Environment:
#   INCODE_INSTALL_DIR  where the programs go (default: %LOCALAPPDATA%\incode\bin)
#   INCODE_VERSION      a release tag such as v0.2.0 (default: the latest)
#   INCODE_NO_SANDBOX_SETUP=1  skip the sandbox's one-time machine setup
#   INCODE_NO_TOOLS_INSTALL=1  skip installing incode's tools at the end
#
# The archive is checked against the release's SHA256SUMS before anything is
# unpacked; a mismatch, or no checksum for it, stops the install. The folder is
# added to the user PATH (not the machine's), so installing needs no
# elevation. The sandbox's network layer does: its one-time setup (`sbx
# setup-windows`: an offline account and firewall filters) is run at the end
# through the Windows elevation prompt, so nobody has to run it by hand. It is
# skipped when already done; declining the prompt leaves incode installed and
# says what was not set up. Last, `incode --install-tools` installs the tools
# incode runs (the headless browser among them), each checked against its
# release's sha256.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$Repo = 'incordai/incode'
$InstallDir = if ($env:INCODE_INSTALL_DIR) { $env:INCODE_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'incode\bin' }
$Version = if ($env:INCODE_VERSION) { $env:INCODE_VERSION } else { 'latest' }

function Say([string]$Message) { Write-Host "incode-install: $Message" }

# Only x86_64 Windows is built. An ARM64 machine runs it under emulation.
$arch = $env:PROCESSOR_ARCHITECTURE
if ($arch -ne 'AMD64' -and $arch -ne 'ARM64') {
    throw "incode-install: no release is built for the $arch architecture"
}
$target = 'x86_64-pc-windows-msvc'
$asset = "incode-$target.zip"
$base = if ($Version -eq 'latest') {
    "https://github.com/$Repo/releases/latest/download"
} else {
    "https://github.com/$Repo/releases/download/$Version"
}

# Windows PowerShell 5.1 does not offer TLS 1.2 by default.
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("incode-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Say "downloading $asset ($Version)"
    $archive = Join-Path $tmp $asset
    $sums = Join-Path $tmp 'SHA256SUMS'
    Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile $archive
    Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS" -OutFile $sums

    # `<hex>  <name>`, or `<hex> *<name>` from a binary-mode sha256sum.
    $expected = $null
    foreach ($line in Get-Content $sums) {
        $parts = $line.Trim() -split '\s+', 2
        if ($parts.Count -eq 2 -and $parts[1].TrimStart('*') -eq $asset) {
            $expected = $parts[0].ToLowerInvariant()
            break
        }
    }
    if (-not $expected) { throw "incode-install: SHA256SUMS has no line for $asset" }
    $actual = (Get-FileHash -Algorithm SHA256 -Path $archive).Hash.ToLowerInvariant()
    if ($expected -ne $actual) {
        throw "incode-install: checksum mismatch for ${asset}: expected $expected, got $actual"
    }
    Say 'sha256 checked'

    $unpacked = Join-Path $tmp 'unpacked'
    Expand-Archive -Path $archive -DestinationPath $unpacked
    if (-not (Test-Path (Join-Path $unpacked 'incode.exe'))) {
        throw "incode-install: $asset holds no incode.exe"
    }

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    foreach ($program in 'incode.exe', 'sbx.exe') {
        $from = Join-Path $unpacked $program
        if (-not (Test-Path $from)) { continue }
        $to = Join-Path $InstallDir $program
        # A running .exe cannot be overwritten but can be renamed: move it
        # aside, then put the new one in its place.
        if (Test-Path $to) {
            $old = "$to.old"
            Remove-Item -Force -ErrorAction SilentlyContinue $old
            Move-Item -Force $to $old
        }
        Copy-Item $from $to
    }
} finally {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $tmp
}

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$entries = if ($userPath) { $userPath -split ';' } else { @() }
if ($entries -notcontains $InstallDir) {
    $newPath = (@($entries | Where-Object { $_ }) + $InstallDir) -join ';'
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    $env:Path = "$env:Path;$InstallDir"
    Say "added $InstallDir to your user PATH; open a new terminal to pick it up"
}

$installed = & (Join-Path $InstallDir 'incode.exe') --version
Say "installed $installed to $InstallDir"

# The sandbox's one-time machine setup, asked for here rather than left to
# the user. `sbx check --net deny` exits 0 once network deny is enforced
# natively, that is, once the setup is in place.
$sbx = Join-Path $InstallDir 'sbx.exe'
if ($env:INCODE_NO_SANDBOX_SETUP -eq '1') {
    Say 'sandbox setup skipped (INCODE_NO_SANDBOX_SETUP=1)'
} elseif (Test-Path $sbx) {
    & $sbx check --net deny --quiet 2>$null | Out-Null
    if ($LASTEXITCODE -eq 0) {
        Say 'sandbox already set up'
    } else {
        Say 'setting up the sandbox; Windows asks once for administrator rights'
        try {
            $setup = Start-Process -FilePath $sbx -ArgumentList 'setup-windows' -Verb RunAs -Wait -PassThru
            & $sbx check --net deny --quiet 2>$null | Out-Null
            if ($LASTEXITCODE -eq 0) {
                Say 'sandbox set up: commands can now be cut off the network natively'
            } else {
                Say "sandbox setup did not complete (exit $($setup.ExitCode)); incode works, network deny under sbx is not available yet"
            }
        } catch {
            Say "sandbox setup was not allowed ($($_.Exception.Message)); incode works, network deny under sbx is not available yet"
        }
    }
} else {
    Say 'sandbox: this release has no sbx.exe, so commands run unconfined'
}

# `incode --install-tools` prints a line per tool on stdout and exits non-zero
# when one was not installed; incode itself is installed either way. Only
# stdout is captured: with ErrorActionPreference Stop, redirecting a native
# program's stderr would make its first line a terminating error.
if ($env:INCODE_NO_TOOLS_INSTALL -eq '1') {
    Say 'tools install skipped (INCODE_NO_TOOLS_INSTALL=1)'
} else {
    $said = & (Join-Path $InstallDir 'incode.exe') --install-tools
    if ($LASTEXITCODE -eq 0) {
        Say 'tools installed:'
    } else {
        Say 'tools: not every one was installed; incode works without them, and `incode --install-tools` retries:'
    }
    foreach ($line in @($said)) { Say "    $line" }
}
Say 'keep it current with: incode update'
