#!/bin/sh
# Build a CAND and an ORIG frankenredis in ONE rch invocation, on ONE worker.
#
# (frankenredis-gvm6z) docs/BENCH_METHODOLOGY section 3: an A/B split across two
# `rch exec` invocations is INVALID -- rch has no --worker flag and the ORIG/CAND
# ratio is not worker-invariant. This builds both arms in the same remote shell,
# from the same tree, with the same toolchain, by swapping ONE file between them.
#
#   $1  path (repo-relative) of the file that differs between the arms
#   $2  path (repo-relative) of the ORIG content for that file
#   $3  repo-relative output dir for the two ELFs (pass the SAME path to
#       `rch exec --job --result-dir <dir>` and both arms come back on their own;
#       `rch exec` does not otherwise copy a linked binary back, see
#       docs/BENCH_METHODOLOGY section 2)
#
# The executable path comes from cargo's own --message-format=json `executable`
# field, NOT from `find`: this repo's working tree carries frbuild/ directories
# holding OLD binaries named `frankenredis` under a `release-perf` path, and a
# find-based lookup silently copies one of those twice -- producing two arms with
# an IDENTICAL sha256 and a perfectly null "measurement".
set -e
SWAP="$1"
ORIG_SRC="$2"
OUT="${3:-frbuild/paired-arms}"
[ -n "$SWAP" ] && [ -n "$ORIG_SRC" ] || { echo "usage: $0 <swap-path> <orig-content-path> [outdir]"; exit 2; }
mkdir -p "$OUT"

build_arm() {
    cargo build -j 2 --profile release-perf -p fr-server --bin frankenredis \
        --message-format=json 2>/dev/null \
      | tr ',' '\n' | grep '"executable":"' | grep -v '"executable":null' \
      | tail -1 | sed 's/.*"executable":"//; s/"//'
}

echo "WORKERHOST $(hostname)"
echo "RUSTC $(cargo --version)"

CANDPATH=$(build_arm)
[ -n "$CANDPATH" ] && [ -f "$CANDPATH" ] || { echo "CAND build produced no executable"; exit 1; }
cp "$CANDPATH" "$OUT/fr_cand"
echo "CANDSHA $(sha256sum "$OUT/fr_cand" | cut -d' ' -f1)"

cp "$ORIG_SRC" "$SWAP"
ORIGPATH=$(build_arm)
[ -n "$ORIGPATH" ] && [ -f "$ORIGPATH" ] || { echo "ORIG build produced no executable"; exit 1; }
cp "$ORIGPATH" "$OUT/fr_orig"
echo "ORIGSHA $(sha256sum "$OUT/fr_orig" | cut -d' ' -f1)"

if [ "$(sha256sum "$OUT/fr_cand" | cut -d' ' -f1)" = "$(sha256sum "$OUT/fr_orig" | cut -d' ' -f1)" ]; then
    echo "ARMS ARE THE SAME ELF -- the swap did not take; refusing to report a null"
    exit 1
fi
echo PAIRED_BUILD_OK
