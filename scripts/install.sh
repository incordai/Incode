#!/bin/sh
# Install incode (and sbx, the standalone sandbox) from a GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/incordai/incode/main/scripts/install.sh | sh
#
# Environment:
#   INCODE_INSTALL_DIR  where the programs go (default: ~/.local/bin)
#   INCODE_VERSION      a release tag such as v0.2.0 (default: the latest)
#
# The archive is checked against the release's SHA256SUMS before anything is
# unpacked; a mismatch, or no checksum for it, stops the install.
#
# POSIX sh on purpose: it runs from a pipe on systems where bash is not there.

set -eu

REPO="incordai/incode"
INSTALL_DIR="${INCODE_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${INCODE_VERSION:-latest}"

say() { printf 'incode-install: %s\n' "$*" >&2; }
die() {
    say "error: $*"
    exit 1
}

# The Rust target the release names its archive after.
detect_target() {
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$arch" in
        x86_64 | amd64) arch="x86_64" ;;
        aarch64 | arm64) arch="aarch64" ;;
        *) die "no release is built for the $arch architecture" ;;
    esac
    case "$os" in
        Linux) echo "${arch}-unknown-linux-gnu" ;;
        Darwin)
            # An x86_64 shell under Rosetta on Apple silicon still gets the
            # native build.
            if [ "$arch" = "x86_64" ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = "1" ]; then
                arch="aarch64"
            fi
            echo "${arch}-apple-darwin"
            ;;
        MINGW* | MSYS* | CYGWIN*) die "on Windows, use scripts/install.ps1 from PowerShell" ;;
        *) die "no release is built for $os" ;;
    esac
}

# fetch <url> <file>
fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl --proto '=https' --tlsv1.2 -fsSL "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
        wget -q --https-only -O "$2" "$1"
    else
        die "neither curl nor wget is installed"
    fi
}

# sha256 <file> — lowercase hex only.
sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    else
        die "neither sha256sum nor shasum is installed, so the download cannot be checked"
    fi
}

main() {
    target="$(detect_target)"
    asset="incode-${target}.tar.gz"
    if [ "$VERSION" = "latest" ]; then
        base="https://github.com/${REPO}/releases/latest/download"
    else
        base="https://github.com/${REPO}/releases/download/${VERSION}"
    fi

    tmp="$(mktemp -d 2>/dev/null || mktemp -d -t incode)"
    trap 'rm -rf "$tmp"' EXIT INT TERM

    say "downloading $asset ($VERSION)"
    fetch "$base/$asset" "$tmp/$asset" || die "could not download $base/$asset"
    fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "could not download $base/SHA256SUMS"

    # `<hex>  <name>`, or `<hex> *<name>` from a binary-mode sha256sum.
    expected="$(awk -v name="$asset" '$2 == name || $2 == "*" name { print tolower($1); exit }' "$tmp/SHA256SUMS")"
    [ -n "$expected" ] || die "SHA256SUMS has no line for $asset"
    actual="$(sha256 "$tmp/$asset")"
    [ "$expected" = "$actual" ] || die "checksum mismatch for $asset: expected $expected, got $actual"
    say "sha256 checked"

    mkdir -p "$tmp/unpacked"
    tar -xzf "$tmp/$asset" -C "$tmp/unpacked"
    [ -f "$tmp/unpacked/incode" ] || die "$asset holds no incode program"

    mkdir -p "$INSTALL_DIR"
    for program in incode sbx; do
        [ -f "$tmp/unpacked/$program" ] || continue
        # Copy then rename, so a running copy is replaced whole, never
        # half-written.
        cp "$tmp/unpacked/$program" "$INSTALL_DIR/.$program.new"
        chmod 755 "$INSTALL_DIR/.$program.new"
        mv -f "$INSTALL_DIR/.$program.new" "$INSTALL_DIR/$program"
    done

    say "installed $("$INSTALL_DIR/incode" --version 2>/dev/null || echo incode) to $INSTALL_DIR"
    case ":${PATH}:" in
        *":${INSTALL_DIR}:"*) ;;
        *)
            say "$INSTALL_DIR is not on your PATH. Add it, e.g.:"
            say "    echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.profile"
            ;;
    esac
    say "keep it current with: incode update"
}

main "$@"
