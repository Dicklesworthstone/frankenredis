#!/usr/bin/env python3
"""Repo guard: no UNTRACKED .rs in a directory cargo compiles.

Ported from franken_networkx (br-r37-c1-aqwmm), which took it from frankenlibc --
where 273 gitignored throwaway .rs files had accumulated under tests/, one of them
broken since June.

WHY IT MATTERS. cargo compiles every .rs under a crate's tests/ as its own test
target (no crate here sets autotests = false), so a throwaway probe left there is
built on every `cargo test` for that crate. If it stops compiling it aborts the
ENTIRE run for that crate -- while being invisible to `git status`, `git log`,
`git blame` and code review, so the usual "what changed" reflexes find nothing.
The inverse is worse and just as quiet: an ignored file under src/ that someone
later references with a `mod` declaration builds on that machine and fails in
every other checkout, because the file was never committed.

WHY THIS IS A PRE-COMMIT GATE AND NOT A CARGO TEST, which is where it differs
from the franken_networkx original. networkx runs its guard as a local pytest.
frankenredis builds and tests through `rch` on a remote worker, and the tree
transferred to that worker is NOT a usable git checkout -- I wrote the cargo-test
version first and it failed there with "git ls-files failed" on vmi1227854. A
cargo test would therefore have had to skip on every remote run, which is every
run, and would have rotted into a permanent pass. A pre-commit hook runs in the
real checkout, which is the only place the question can be answered.

The guard keys on UNTRACKED, not on gitignored. Ignoring is what hides a file,
but tracking is what makes it real; an untracked .rs in a compiled directory is a
hazard whether or not some pattern happens to cover it. The report marks which
offenders are ignored, because that is the part a human cannot see any other way.

Usage:
    untracked_rust_guard.py            scan; exit 1 and name offenders
    untracked_rust_guard.py check-staged   same (pre-commit hook entry point)
    untracked_rust_guard.py self-check     prove the guard is not vacuous
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

# Untracked .rs files that already existed when this guard was written.
#
# Both are 1-byte placeholders that .gitignore itself labels "Empty test stubs
# (agent-generated, never completed)". They compile to zero tests, so they cannot
# abort a run today. They are allowlisted rather than deleted because deleting
# another agent's files is not this guard's call. The list must only ever shrink;
# _check_allowlist_is_not_stale enforces that, so it cannot become the new hiding
# place -- which is how frankenlibc reached 273.
KNOWN_UNTRACKED = {
    "crates/fr-server/tests/replica_of_replica.rs",
    "crates/fr-server/tests/test_replica_of_replica.rs",
}

# Directories cargo turns into build targets. The workspace root manifest is
# virtual ([workspace] with no [package]), so a root tests/ is NOT compiled and is
# deliberately not scanned -- reporting files cargo never builds would be noise.
COMPILED_SUBDIRS = ("src", "tests", "benches", "examples")


def repo_root() -> Path:
    out = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        capture_output=True,
        text=True,
        check=True,
        timeout=10,
    )
    return Path(out.stdout.strip())


def cargo_source_dirs(root: Path) -> list[Path]:
    dirs: list[Path] = []
    for manifest in sorted(root.glob("crates/*/Cargo.toml")):
        crate = manifest.parent
        for sub in COMPILED_SUBDIRS:
            candidate = crate / sub
            if candidate.is_dir():
                dirs.append(candidate)
    return dirs


def _rs_files(directory: Path):
    for path in directory.rglob("*.rs"):
        parts = set(path.parts)
        if "target" in parts or any(p.startswith(".rch-target-") for p in path.parts):
            continue
        yield path


def tracked_paths(root: Path) -> set[str]:
    """Every path git tracks, read ONCE.

    The networkx original shells out `git ls-files --error-unmatch` per file. This
    repo has enough .rs files that per-file process spawns would dominate a
    pre-commit hook's runtime, and a slow hook is a hook someone disables.
    """
    out = subprocess.run(
        ["git", "ls-files", "-z"],
        cwd=root,
        capture_output=True,
        text=True,
        check=True,
        timeout=60,
    )
    return {p for p in out.stdout.split("\0") if p}


def is_ignored(root: Path, relative: str) -> bool:
    return (
        subprocess.run(
            ["git", "check-ignore", "-q", relative],
            cwd=root,
            check=False,
            timeout=10,
        ).returncode
        == 0
    )


def untracked_rust_files(root: Path) -> list[tuple[str, bool]]:
    """(repo-relative path, is_gitignored) for every untracked .rs cargo compiles."""
    tracked = tracked_paths(root)
    offenders: list[tuple[str, bool]] = []
    for directory in cargo_source_dirs(root):
        for path in _rs_files(directory):
            relative = path.relative_to(root).as_posix()
            if relative in tracked:
                continue
            offenders.append((relative, is_ignored(root, relative)))
    return sorted(offenders)


def _check_not_vacuous(root: Path) -> list[str]:
    """A guard that silently scanned nothing would pass forever."""
    problems = []
    dirs = cargo_source_dirs(root)
    if not dirs:
        problems.append("no cargo source directories found")
    if not [d for d in dirs if d.name == "tests"]:
        problems.append("no crate tests/ directory found - test targets unchecked")
    seen = sum(1 for d in dirs if d.name == "tests" for _ in _rs_files(d))
    if seen == 0:
        problems.append("no .rs files seen under any crate tests/ directory")
    return problems


def _check_allowlist_is_not_stale(root: Path) -> list[str]:
    current = {path for path, _ in untracked_rust_files(root)}
    return sorted(KNOWN_UNTRACKED - current)


def scan(root: Path) -> int:
    vacuity = _check_not_vacuous(root)
    if vacuity:
        print("untracked-rust guard is not scanning what it claims to scan:")
        for problem in vacuity:
            print(f"  {problem}")
        return 1

    stale = _check_allowlist_is_not_stale(root)
    if stale:
        print(
            "these allowlisted paths are no longer untracked (committed or removed).\n"
            "Drop them from KNOWN_UNTRACKED so the allowlist cannot hide new files:"
        )
        for path in stale:
            print(f"  {path}")
        return 1

    offenders = [
        (path, ignored)
        for path, ignored in untracked_rust_files(root)
        if path not in KNOWN_UNTRACKED
    ]
    if offenders:
        print(
            "untracked .rs files live in directories cargo compiles.\n"
            "cargo builds every .rs under a crate's tests/ as its own test target, so one\n"
            "that stops compiling aborts that crate's entire test run; an ignored file under\n"
            "src/ that later gains a `mod` declaration breaks every checkout but yours.\n"
            "Commit them, move them outside the crate, or delete them:"
        )
        for path, ignored in offenders:
            suffix = "   [GITIGNORED - invisible to review]" if ignored else ""
            print(f"  {path}{suffix}")
        return 1
    return 0


def self_check(root: Path) -> int:
    """Prove the guard actually fires, by planting the exact hazard.

    For a guard, "does it detect anything" is the whole question -- a scan that
    quietly covered nothing would pass forever and look identical to a clean repo.
    """
    test_dirs = [d for d in cargo_source_dirs(root) if d.name == "tests"]
    if not test_dirs:
        print("self-check: no crate tests/ directory to plant in")
        return 1
    planted = test_dirs[0] / "untracked_rust_guard_selfcheck.rs"
    if planted.exists():
        print(f"self-check: {planted} already exists; refusing to overwrite")
        return 1

    baseline = scan(root)
    if baseline != 0:
        print("self-check: repo is already failing the guard; fix that first")
        return 1

    planted.write_text("// planted by untracked_rust_guard.py self-check\n")
    try:
        relative = planted.relative_to(root).as_posix()
        found = {path for path, _ in untracked_rust_files(root)}
        if relative not in found:
            print(f"self-check FAILED: planted {relative} was NOT detected")
            return 1
        if scan(root) == 0:
            print(f"self-check FAILED: guard passed despite {relative} being present")
            return 1
    finally:
        planted.unlink(missing_ok=True)

    if scan(root) != 0:
        print("self-check FAILED: guard still fails after removing the planted file")
        return 1
    print(
        "self-check OK: guard is clean, fires on a planted .rs in a compiled "
        "directory, and returns to clean when it is removed"
    )
    return 0


def main(argv: list[str]) -> int:
    root = repo_root()
    mode = argv[1] if len(argv) > 1 else "check"
    if mode == "self-check":
        return self_check(root)
    return scan(root)


if __name__ == "__main__":
    sys.exit(main(sys.argv))
