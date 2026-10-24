#!/usr/bin/env python3
"""Package-manager manifests for one incode release: winget, Scoop and Homebrew.

Each is rendered from the release's own SHA256SUMS (the `sha256sum` lines the
release workflow publishes beside the archives), so a manifest can only name
an archive and hash that release actually has.

    python3 scripts/package_manifests.py --tag v0.1.0 --sums SHA256SUMS --out DIR

writes
    DIR/winget/IncordAI.Incode.yaml                 (version)
    DIR/winget/IncordAI.Incode.installer.yaml
    DIR/winget/IncordAI.Incode.locale.en-US.yaml
    DIR/scoop/incode.json
    DIR/homebrew/incode.rb

Submitting them is the owner's: winget-pkgs takes a pull request under
manifests/i/IncordAI/Incode/<version>/, Scoop a bucket, Homebrew a tap.

Wired to: .github/workflows/package-check.yml, which runs this for a tag and
installs with what it wrote (Scoop on Windows, Homebrew on macOS and Linux,
winget on Windows); release.yml calls that after publishing. Tested by
scripts/tests/test_package_manifests.py. The archive names are the ones
release.yml builds (its `build` matrix) and scripts/install.sh and
scripts/install.ps1 download.

Standard library only, so it runs wherever Python 3 does.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

REPO = "incordai/Incode"
HOMEPAGE = f"https://github.com/{REPO}"
WINGET_ID = "IncordAI.Incode"
PUBLISHER = "Incord AI"
LICENSE = "Apache-2.0"
# Under 80 characters and not starting with an article or the name: what
# Homebrew's audit asks of `desc`. Used by all three.
SUMMARY = "Coding harness that runs your agent CLI under its own tools, checks and sandbox"
WINGET_SCHEMA = "1.10.0"

# The archive each platform gets, as release.yml names them.
WINDOWS_X64 = "incode-x86_64-pc-windows-msvc.zip"
MACOS_ARM = "incode-aarch64-apple-darwin.tar.gz"
MACOS_INTEL = "incode-x86_64-apple-darwin.tar.gz"
LINUX_ARM = "incode-aarch64-unknown-linux-gnu.tar.gz"
LINUX_INTEL = "incode-x86_64-unknown-linux-gnu.tar.gz"
ARCHIVES = (WINDOWS_X64, MACOS_ARM, MACOS_INTEL, LINUX_ARM, LINUX_INTEL)

TAG = re.compile(r"^v(\d+\.\d+\.\d+)$")
SUM_LINE = re.compile(r"^([0-9a-f]{64}) [ *]?(\S+)$")


class ManifestError(Exception):
    """A tag or SHA256SUMS these manifests cannot be made from."""


def version_of(tag: str) -> str:
    """`v1.2.3` -> `1.2.3`. A pre-release tag (`v1.2.3-rc.1`) is refused:
    package managers offer releases, and GitHub's `releases/latest` skips
    pre-releases for the same reason."""
    match = TAG.match(tag)
    if not match:
        raise ManifestError(f"'{tag}' is not a release tag like v1.2.3")
    return match.group(1)


def parse_sums(text: str) -> dict[str, str]:
    """Archive name -> lower-case sha256, from `sha256sum` output. Every
    archive the manifests name must be there, and a line that is not a sum
    is an error rather than skipped."""
    sums: dict[str, str] = {}
    for number, raw in enumerate(text.splitlines(), start=1):
        line = raw.strip()
        if not line:
            continue
        match = SUM_LINE.match(line)
        if not match:
            raise ManifestError(f"SHA256SUMS line {number} is not `<sha256>  <name>`: {raw!r}")
        sums[match.group(2)] = match.group(1)
    missing = [name for name in ARCHIVES if name not in sums]
    if missing:
        raise ManifestError(f"SHA256SUMS has no line for {', '.join(missing)}")
    return sums


def url(tag: str, archive: str) -> str:
    return f"{HOMEPAGE}/releases/download/{tag}/{archive}"


def winget(tag: str, version: str, sums: dict[str, str]) -> dict[str, str]:
    """The three winget files: a zip holding two portable commands."""
    head = f"PackageIdentifier: {WINGET_ID}\nPackageVersion: {version}\n"

    def tail(kind: str) -> str:
        return f"ManifestType: {kind}\nManifestVersion: {WINGET_SCHEMA}\n"

    def schema(kind: str) -> str:
        return (
            "# yaml-language-server: $schema=https://aka.ms/winget-manifest."
            f"{kind}.{WINGET_SCHEMA}.schema.json\n"
        )

    return {
        f"{WINGET_ID}.yaml": schema("version")
        + head
        + "DefaultLocale: en-US\n"
        + tail("version"),
        f"{WINGET_ID}.installer.yaml": schema("installer")
        + head
        + "InstallerType: zip\n"
        + "NestedInstallerType: portable\n"
        + "NestedInstallerFiles:\n"
        + "- RelativeFilePath: incode.exe\n  PortableCommandAlias: incode\n"
        + "- RelativeFilePath: sbx.exe\n  PortableCommandAlias: sbx\n"
        + "Installers:\n"
        + "- Architecture: x64\n"
        + f"  InstallerUrl: {url(tag, WINDOWS_X64)}\n"
        + f"  InstallerSha256: {sums[WINDOWS_X64].upper()}\n"
        + tail("installer"),
        f"{WINGET_ID}.locale.en-US.yaml": schema("defaultLocale")
        + head
        + "PackageLocale: en-US\n"
        + f"Publisher: {PUBLISHER}\n"
        + "PublisherUrl: https://github.com/incordai\n"
        + "PackageName: incode\n"
        + f"PackageUrl: {HOMEPAGE}\n"
        + f"License: {LICENSE}\n"
        + f"LicenseUrl: {HOMEPAGE}/blob/{tag}/LICENSE\n"
        + f"ShortDescription: {SUMMARY}\n"
        + f"ReleaseNotesUrl: {HOMEPAGE}/releases/tag/{tag}\n"
        + tail("defaultLocale"),
    }


def scoop(tag: str, version: str, sums: dict[str, str]) -> str:
    """A Scoop manifest, with `checkver`/`autoupdate` so a bucket can follow
    new releases from the same SHA256SUMS."""
    manifest = {
        "version": version,
        "description": SUMMARY,
        "homepage": HOMEPAGE,
        "license": LICENSE,
        "architecture": {
            "64bit": {"url": url(tag, WINDOWS_X64), "hash": sums[WINDOWS_X64]},
        },
        "bin": ["incode.exe", "sbx.exe"],
        "checkver": {"github": HOMEPAGE},
        "autoupdate": {
            "architecture": {
                "64bit": {"url": f"{HOMEPAGE}/releases/download/v$version/{WINDOWS_X64}"},
            },
            "hash": {"url": "$baseurl/SHA256SUMS"},
        },
    }
    return json.dumps(manifest, indent=4) + "\n"


def homebrew(tag: str, version: str, sums: dict[str, str]) -> str:
    """A Homebrew formula for macOS and Linux, Intel and ARM: the prebuilt
    archives, installed as they are, and a test that each program reports
    the formula's version."""

    def block(archive: str) -> str:
        return f'      url "{url(tag, archive)}"\n      sha256 "{sums[archive]}"\n'

    return (
        "class Incode < Formula\n"
        f'  desc "{SUMMARY}"\n'
        f'  homepage "{HOMEPAGE}"\n'
        f'  version "{version}"\n'
        f'  license "{LICENSE}"\n'
        "\n"
        "  on_macos do\n"
        "    on_arm do\n" + block(MACOS_ARM) + "    end\n"
        "    on_intel do\n" + block(MACOS_INTEL) + "    end\n"
        "  end\n"
        "\n"
        "  on_linux do\n"
        "    on_arm do\n" + block(LINUX_ARM) + "    end\n"
        "    on_intel do\n" + block(LINUX_INTEL) + "    end\n"
        "  end\n"
        "\n"
        "  def install\n"
        '    bin.install "incode", "sbx"\n'
        "  end\n"
        "\n"
        "  test do\n"
        '    assert_equal "incode #{version}", shell_output("#{bin}/incode --version").strip\n'
        '    assert_equal "sbx #{version}", shell_output("#{bin}/sbx --version").strip\n'
        "  end\n"
        "end\n"
    )


def render(tag: str, sums_text: str) -> dict[str, str]:
    """Every file, by its path under the output folder."""
    version = version_of(tag)
    sums = parse_sums(sums_text)
    files = {f"winget/{name}": text for name, text in winget(tag, version, sums).items()}
    files["scoop/incode.json"] = scoop(tag, version, sums)
    files["homebrew/incode.rb"] = homebrew(tag, version, sums)
    return files


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--tag", required=True, help="the release tag, such as v0.1.0")
    parser.add_argument("--sums", required=True, type=Path, help="that release's SHA256SUMS")
    parser.add_argument("--out", required=True, type=Path, help="folder to write into")
    args = parser.parse_args(argv)
    try:
        files = render(args.tag, args.sums.read_text(encoding="utf-8"))
    except (ManifestError, OSError) as why:
        print(f"package_manifests: {why}", file=sys.stderr)
        return 1
    for relative, text in files.items():
        path = args.out / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        # LF on every platform: these files are submitted as they are.
        path.write_bytes(text.encode("utf-8"))
        print(path)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
