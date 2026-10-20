#!/bin/sh
# Run every check incode has, locally, the way CI does — and the parts CI
# cannot (the SDKs and the editor extension, when Node and Python are here).
#
#   scripts/test-all.sh            everything this machine can run
#   scripts/test-all.sh rust       only the named parts: rust sbx python ts vscode
#   QUICK=1 scripts/test-all.sh    skip clippy (the slowest step)
#
# A part whose toolchain is missing is skipped and said so; the exit code is
# non-zero when any part that ran failed. Run from anywhere inside the repo.
set -u

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root" || exit 1

parts=${*:-"rust sbx python ts vscode"}
failed=""
skipped=""

have() { command -v "$1" >/dev/null 2>&1; }

step() {
    name=$1
    shift
    printf '\n== %s: %s\n' "$name" "$*"
    if ! "$@"; then
        failed="$failed\n  $name: $*"
    fi
}

wants() {
    case " $parts " in
        *" $1 "*) return 0 ;;
        *) return 1 ;;
    esac
}

if wants rust; then
    if have cargo; then
        step rust cargo fmt --all -- --check
        [ -n "${QUICK:-}" ] || step rust cargo clippy --all-targets -- -D warnings
        step rust cargo test --lib
        step rust cargo test --bins
        # Runs real commands under the sbx engine (Linux; it says it skipped
        # elsewhere).
        step rust cargo test --test sbx_engine
    else
        skipped="$skipped rust(cargo)"
    fi
fi

if wants sbx; then
    if have cargo; then
        [ -n "${QUICK:-}" ] || step sbx cargo clippy -p incord-sandbox --all-targets -- -D warnings
        step sbx cargo test -p incord-sandbox
    else
        skipped="$skipped sbx(cargo)"
    fi
fi

if wants python; then
    if have python3 && [ -d sdk/python ]; then
        step python python3 -m unittest discover -s sdk/python/tests -t sdk/python
    else
        skipped="$skipped python"
    fi
fi

if wants ts; then
    if have npm && [ -d sdk/typescript ]; then
        step ts sh -c 'cd sdk/typescript && npm install --no-audit --no-fund && npm test'
    else
        skipped="$skipped ts"
    fi
fi

if wants vscode; then
    if have npm && [ -d editors/vscode ]; then
        step vscode sh -c 'cd editors/vscode && npm install --no-audit --no-fund && npm run compile && npm test'
    else
        skipped="$skipped vscode"
    fi
fi

printf '\n== summary\n'
[ -n "$skipped" ] && printf 'skipped (toolchain or folder missing):%s\n' "$skipped"
if [ -n "$failed" ]; then
    printf 'FAILED:%b\n' "$failed"
    exit 1
fi
echo "all parts that ran passed"
