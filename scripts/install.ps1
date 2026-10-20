# Install incode (and sbx, the standalone sandbox) on Windows from a GitHub
# release.
#
#   irm https://raw.githubusercontent.com/incordai/incode/main/scripts/install.ps1 | iex
#
# Environment:
#   INCODE_INSTALL_DIR  where the programs go (default: %LOCALAPPDATA%\incode\bin)
#   INCODE_VERSION      a release tag such as v0.2.0 (default: the latest)
#
# The archive is checked against the release's SHA256SUMS before anything is
# unpacked; a mismatch, or no checksum for it, stops the install. The folder is
# added to the user PATH (not the machine's), so no elevation is needed.

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
Say 'keep it current with: incode update'
