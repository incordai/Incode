<#
.SYNOPSIS
  Set up incode's sandbox (sbx) on Windows: WSL2, the Linux sbx inside it,
  and sbx.exe on Windows, which runs a policy natively when it can and
  forwards it into WSL2 when it can't.

.DESCRIPTION
  sbx.exe confines writes, the environment and limits natively; network deny
  and proxy natively after `sbx setup-windows` (step 4); everything else
  (deny_read) through the Linux sandbox (Landlock + seccomp) inside WSL2.
  This script, run from the incode folder:
    1. checks for WSL2 and a Linux distribution, and installs them if missing
       (asks first; needs administrator rights once, then a restart);
    2. builds sbx inside WSL2 and installs it at ~/.local/bin/sbx there;
    3. builds sbx.exe on Windows and puts it on your PATH;
    4. when run as administrator, offers `sbx setup-windows`: an offline
       account the firewall cuts off, so network deny and proxy run natively
       too, without WSL2 (from a normal prompt it says how to do it later);
    5. runs `sbx doctor` to show what is enforced.
  Run it again after the restart if step 1 installed WSL2. It is safe to run
  more than once.

.PARAMETER Yes
  Do not ask before installing WSL2.

.PARAMETER Distro
  The WSL distribution to use (default: Ubuntu).

.PARAMETER NativeOnly
  Skip WSL2 (steps 1 and 2): only sbx.exe, and `sbx setup-windows`.

.PARAMETER SetupNative
  Run `sbx setup-windows` without asking (needs an elevated prompt).

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\install-windows.ps1

.EXAMPLE
  # From an elevated prompt: native only, network deny included.
  powershell -ExecutionPolicy Bypass -File scripts\install-windows.ps1 -NativeOnly -SetupNative
#>
param(
    [switch]$Yes,
    [string]$Distro = "Ubuntu",
    [switch]$NativeOnly,
    [switch]$SetupNative
)

$ErrorActionPreference = "Stop"

function Say($text) { Write-Host "sbx setup: $text" }

function Test-Admin {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    (New-Object Security.Principal.WindowsPrincipal $id).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator)
}

# wsl.exe prints UTF-16; read it as text and drop the NULs.
function Invoke-Wsl([string[]]$WslArgs) {
    $out = & wsl.exe @WslArgs 2>&1 | Out-String
    return @{ Code = $LASTEXITCODE; Text = ($out -replace "`0", "") }
}

$repo = (Get-Location).Path
if (-not (Test-Path (Join-Path $repo "crates\sandbox\Cargo.toml"))) {
    throw "Run this from the incode folder (crates\sandbox was not found in $repo)."
}

if (-not $NativeOnly) {
# --- 1. WSL2 and a distribution ------------------------------------------
$haveWsl = [bool](Get-Command wsl.exe -ErrorAction SilentlyContinue)
$distros = @()
if ($haveWsl) {
    $list = Invoke-Wsl @("--list", "--quiet")
    if ($list.Code -eq 0) {
        $distros = $list.Text -split "`r?`n" | ForEach-Object { $_.Trim() } | Where-Object { $_ }
    }
}

if (-not $haveWsl -or -not ($distros -contains $Distro)) {
    Say "WSL2 with '$Distro' is not set up yet. The sandbox needs it on Windows."
    Say "Installing it runs 'wsl --install -d $Distro', needs administrator rights,"
    Say "and usually a restart. Hardware virtualization must be on in the BIOS."
    if (-not $Yes) {
        $answer = Read-Host "Install WSL2 and $Distro now? [y/N]"
        if ($answer -notmatch '^(y|yes)$') { Say "Nothing installed."; exit 1 }
    }
    if (-not (Test-Admin)) {
        Say "Asking for administrator rights..."
        $argsList = "-NoProfile -ExecutionPolicy Bypass -Command `"wsl.exe --install -d $Distro --no-launch; exit `$LASTEXITCODE`""
        $p = Start-Process powershell -Verb RunAs -ArgumentList $argsList -Wait -PassThru
        if ($p.ExitCode -ne 0) { throw "wsl --install failed (exit $($p.ExitCode))." }
    } else {
        & wsl.exe --install -d $Distro --no-launch
        if ($LASTEXITCODE -ne 0) { throw "wsl --install failed (exit $LASTEXITCODE)." }
    }
    Say "WSL2 is installed. Next:"
    Say "  1. Restart Windows if it asks you to."
    Say "  2. Open '$Distro' once from the Start menu and create your Linux user."
    Say "  3. Run this script again to finish."
    exit 0
}

# WSL1 has no Linux kernel of its own, so no Landlock: insist on WSL2.
$verbose = Invoke-Wsl @("--list", "--verbose")
$line = ($verbose.Text -split "`r?`n") | Where-Object { $_ -match "\b$([regex]::Escape($Distro))\b" } | Select-Object -First 1
if ($line -and $line -match '\s1\s*$') {
    Say "'$Distro' runs under WSL1; converting it to WSL2..."
    & wsl.exe --set-version $Distro 2
    if ($LASTEXITCODE -ne 0) { throw "Could not convert $Distro to WSL2." }
}

# --- 2. sbx inside WSL2 -----------------------------------------------------
$repoInWsl = (& wsl.exe -d $Distro -e wslpath -a "$repo" | Out-String).Trim()
if (-not $repoInWsl) { throw "Could not find $repo from inside WSL." }
Say "Building sbx inside WSL2 ($Distro) from $repoInWsl ..."

$build = @'
set -eu
REPO="$1"
if ! command -v cc >/dev/null 2>&1; then
  echo "sbx setup: installing a C toolchain (sudo may ask for your Linux password)"
  sudo apt-get update -qq && sudo apt-get install -y -qq build-essential pkg-config
fi
if ! command -v cargo >/dev/null 2>&1 && [ ! -x "$HOME/.cargo/bin/cargo" ]; then
  echo "sbx setup: installing Rust (rustup, minimal profile)"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
# Build outside /mnt/c: the Windows drive is slow from WSL.
export CARGO_TARGET_DIR="$HOME/.cache/sbx-target"
cargo build --release --locked -p incord-sandbox --manifest-path "$REPO/Cargo.toml"
install -D -m 755 "$CARGO_TARGET_DIR/release/sbx" "$HOME/.local/bin/sbx"
echo "sbx setup: installed $HOME/.local/bin/sbx"
'@
& wsl.exe -d $Distro -e bash -c $build sbx-setup "$repoInWsl"
if ($LASTEXITCODE -ne 0) { throw "Building sbx inside WSL2 failed (exit $LASTEXITCODE)." }
} # -not $NativeOnly

# --- 3. sbx.exe on Windows ----------------------------------------------------
$bin = Join-Path $env:USERPROFILE ".local\bin"
New-Item -ItemType Directory -Force -Path $bin | Out-Null
if (Get-Command cargo -ErrorAction SilentlyContinue) {
    Say "Building sbx.exe ..."
    & cargo build --release --locked -p incord-sandbox
    if ($LASTEXITCODE -ne 0) { throw "Building sbx.exe failed." }
    Copy-Item -Force (Join-Path $repo "target\release\sbx.exe") $bin
} else {
    Say "Rust is not installed on Windows, so sbx.exe was not built."
    Say "Install it from https://rustup.rs and run this script again."
    exit 1
}
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (-not ($userPath -split ';' | Where-Object { $_ -eq $bin })) {
    [Environment]::SetEnvironmentVariable("Path", "$userPath;$bin", "User")
    $env:Path = "$env:Path;$bin"
    Say "Added $bin to your PATH (new terminals pick it up)."
}

# --- 4. Network deny natively (sbx setup-windows) ---------------------------------
# An offline local account that Windows Filtering Platform filters cut off
# from the network; `sbx run --net deny` then runs as that account natively
# instead of in WSL2. Needs administrator rights once. See crates\sandbox\README.md.
$sbxExe = Join-Path $bin "sbx.exe"
if (Test-Admin) {
    $answer = "y"
    if (-not $SetupNative) {
        Say "sbx setup-windows creates a local account, sbx-offline, that firewall filters"
        Say "cut off from the network, so network deny runs natively (without WSL2)."
        $answer = Read-Host "Run sbx setup-windows now? [y/N]"
    }
    if ($answer -match '^(y|yes)$') {
        & $sbxExe setup-windows
        if ($LASTEXITCODE -ne 0) {
            Say "sbx setup-windows failed (exit $LASTEXITCODE); network deny still needs WSL2."
        }
    }
} else {
    if ($SetupNative) {
        Say "-SetupNative needs an elevated prompt (Run as administrator); skipped."
    }
    Say "For network deny natively (without WSL2), run 'sbx setup-windows' once from an"
    Say "elevated prompt (Run as administrator)."
}

# --- 5. What is enforced --------------------------------------------------------
if ($Distro -ne "Ubuntu") { $env:SBX_WSL_DISTRO = $Distro }
Say "Checking the sandbox:"
& $sbxExe doctor
if ($NativeOnly) {
    Say "Done. 'sbx run -w . -- <command>' runs <command> sandboxed natively."
} else {
    Say "Done. 'sbx run -w . -- <command>' runs <command> sandboxed, natively or inside WSL2."
}
if ($Distro -ne "Ubuntu" -and -not $NativeOnly) {
    Say "Set SBX_WSL_DISTRO=$Distro in your environment so sbx.exe uses it."
}
