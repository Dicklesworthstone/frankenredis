#!/bin/bash
# paired_ab_build.sh — build the BEFORE and AFTER arms of a reverse-patch A/B and
# PROVE the tree did not move between them.
#
# WHY THIS EXISTS. A dozen agents commit into this one checkout. The usual A/B is
# `git apply -R patch; build; git apply patch; build`, and it is silently wrong
# whenever a peer edits any crate in the dependency closure between the two
# builds: the arms then differ by that edit as well as by your patch, and nothing
# in the build output says so. Building the arms close together is not a defence.
# This caught it TWICE in one session (2026-08-16) — once on fr-store/packed_set.rs
# mid-pair, once on a later pair — and both times the only symptom was a third SHA.
#
# THE PROOF. This release build is deterministic: identical sources reproduce the
# ELF SHA exactly. So build AFTER, then BEFORE, then AFTER AGAIN and require the
# two AFTER SHAs to match. If they do, no source in the closure moved across the
# pair and the arms differ by the patch alone. Costs one extra build.
#
# Step 0 VERIFIES that determinism rather than assuming it, with a `touch` to force
# a real relink first — without the touch cargo no-ops and the check proves nothing.
#
# It also requires BEFORE != AFTER. Without that, a `git apply` that silently
# failed hands you two identical binaries and a beautiful 0.00 pct null.
#
# Usage:  paired_ab_build.sh <patch-file> [out-dir]
#   patch-file : the diff whose effect you are measuring, e.g. `git show <sha> -- crates/`
#   out-dir    : where the two ELFs are copied (default: a mktemp -d under /data/tmp)
#
# Leaves the working tree exactly as it found it. Prints the two ELF paths and SHAs
# for the provenance block of your ledger row.
set -eu

PATCH=${1:?usage: paired_ab_build.sh <patch-file> [out-dir]}
OUT=${2:-$(mktemp -d /data/tmp/paired_ab.XXXXXX)}
REPO=/data/projects/frankenredis
BIN=target/release/frankenredis

cd "$REPO"
mkdir -p "$OUT"

# Local build, per the campaign rules: rch compiles remotely and does not return a
# linked binary, so a measurable ELF must come from here. Never point
# CARGO_TARGET_DIR at the shared cargo-target.
export RCH_CARGO_WRAPPER_BYPASS=1
build() { env -u CARGO_TARGET_DIR cargo build --release -p fr-server --bin frankenredis 2>&1 | tail -1; }
sha() { sha256sum "$1" | cut -d' ' -f1; }

# (frankenredis-getexgate) REFUSE WHILE ANOTHER BUILD IS RUNNING. Two cargo processes
# writing this one ./target interleave artifacts, and the arms come out with different
# SHAs even though HEAD held and no peer file changed. MEASURED: a pair produced
# 844d8cc11371a711 then 0072d4db319d664e with git reporting a clean tree throughout, and
# a later clean run reproduced 0072d4db... exactly -- so that mismatch was the race, not
# a code difference. This is what "ONE build per project at a time" protects, and the
# cost of breaking it is silent.
#
# Match the rustup TOOLCHAIN BINARY, not any command line containing "rustc": a peer's
# shell wrapper mentions both "rustc" and "frankenredis" and produced a false positive
# that would have blocked a clean window.
builders=$(ps -eo args --no-headers | grep -c '^/home/ubuntu/\.rustup/[^ ]*/bin/rustc .*frankenredis' || true)
if [ "$builders" -gt 0 ]; then
  echo "REFUSING: $builders frankenredis rustc process(es) already building." >&2
  echo "A shared target dir makes a paired build non-deterministic. Wait for the slot." >&2
  exit 2
fi

free_g=$(df -BG --output=avail /data | tail -1 | tr -dc '0-9')
if [ "$free_g" -lt 42 ]; then
  echo "REFUSING: /data has ${free_g}G free, below the 42G hard stop" >&2
  exit 2
fi
echo "== /data ${free_g}G free, $(uptime | sed 's/.*load average: //') loadavg =="

echo "== step 0: is this build deterministic at all? =="
build > /dev/null
D1=$(sha "$BIN")
touch crates/fr-server/src/main.rs          # force a real relink, or cargo no-ops
build > /dev/null
D2=$(sha "$BIN")
if [ "$D1" != "$D2" ]; then
  echo "REFUSING: identical sources produced $D1 then $D2." >&2
  echo "The build is not reproducible, so the stability proof below cannot work." >&2
  exit 3
fi
echo "   deterministic: $D1"

echo "== AFTER arm (tree as-is) =="
build
cp "$BIN" "$OUT/fr_after"
A1=$(sha "$OUT/fr_after")
echo "   after  = $A1"

echo "== BEFORE arm (patch reversed) =="
git apply -R "$PATCH"
# Restore the tree even if the build dies, so a failure never leaves a peer's
# checkout holding a reversed patch.
trap 'git apply "$PATCH" 2>/dev/null || true' EXIT
build
cp "$BIN" "$OUT/fr_before"
B=$(sha "$OUT/fr_before")
git apply "$PATCH"
trap - EXIT
echo "   before = $B"

echo "== stability proof: rebuild the AFTER arm =="
build
A2=$(sha "$BIN")
echo "   after' = $A2"

if [ "$A1" != "$A2" ]; then
  echo
  echo "CONTAMINATED: the arms differ ($A1 then $A2)." >&2
  echo "TWO causes, and the second is the one that fooled me: (a) a peer edit landed" >&2
  echo "between the arms, or (b) another cargo process was writing this same ./target," >&2
  echo "which produces this symptom with HEAD unchanged and git reporting a clean tree." >&2
  echo "The guard at the top of this script now refuses (b) up front." >&2
  echo "DISCARD this pair and re-run; the arms differ by more than your patch." >&2
  exit 1
fi
if [ "$A1" = "$B" ]; then
  echo
  echo "REFUSING: both arms are byte-identical, so the patch did not apply." >&2
  echo "A pair like this measures a perfect 0.00 pct null and means nothing." >&2
  exit 4
fi

echo
echo "STABLE — the arms differ by the patch alone."
echo "  BEFORE $B  $OUT/fr_before"
echo "  AFTER  $A1  $OUT/fr_after"
echo
echo "Both SHAs are COMPUTED here, not self-reported by the process; a ledger row"
echo "using them is not KEEP-class on this repo's contract."
