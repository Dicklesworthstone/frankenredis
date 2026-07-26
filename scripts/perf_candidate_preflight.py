#!/usr/bin/env python3
"""Mechanically enforce ledger integrity, so it cannot decay again.

Modelled on frankensqlite's `sql_pipeline_candidate_preflight` (exit 2 = BLOCKED),
which is why that repo sits at a 1.7% void rate while repos that audited once and
moved on sit at 25-91%. The 2026-07-26 fleet broadcast's conclusion was that
ledger integrity DECAYS, so the audit has to become a gate rather than an event.

Two modes:

  check-candidate  <target symbol or phrase>...
      Refuse a lever whose ground has already been covered. Greps both ledgers
      for rows naming the target and prints each concrete retry predicate it can
      find.
      exit 0 = clear · exit 2 = BLOCKED by a prior row

  check-entry <file>
      Refuse a NEW REJECT-class ledger entry that is unfalsifiable, i.e. that
      records neither an A/A null control nor a counted mechanism. This is the
      VOID-NONULL class that is 124 of this repo's 125 void rows, and the whole
      point is to make writing another one impossible.
      Also refuse a NEW KEEP-class entry without the executing binary's full
      64-hex SHA-256.
      exit 0 = admissible · exit 3 = bad REJECT · exit 4 = bad KEEP

  install-hook
      Install the check-entry gate into the repository's chain-runner pre-commit
      directory. Refuses to overwrite an existing plugin.

Wire `check-entry` into a pre-commit hook to close the loop:

    git diff --cached -U0 -- docs/NEGATIVE_EVIDENCE.md \\
      | scripts/perf_candidate_preflight.py check-entry -
"""
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LEDGERS = [ROOT / "docs" / "NEGATIVE_EVIDENCE.md",
           ROOT / "docs" / "perf_negative_evidence_ledger.md"]

HDR = re.compile(r"^## (?!#)(.+)$", re.MULTILINE)
REJECT_RE = re.compile(
    r"\b(REJECT|REJECTED|NEGATIVE|NO[- ]SHIP|UNDECIDABLE|DECLINED|INVALID"
    r"|REVERT|REVERTED|NOT WORTH|ABANDON)\b", re.IGNORECASE)
KEEP_RE = re.compile(r"\b(KEEP|SHIPPED|LANDED|WIN|PROMOTED)\b", re.IGNORECASE)
NULL_MEASUREMENT_RE = re.compile(
    r"\b(?:A/A(?:\s+null)?|null(?:\s+control)?)\b"
    r".{0,120}?\b(?:median|CI|confidence interval|band|floor|ratio|p0?5|p95)\b"
    r".{0,80}?[-+]?\d+(?:\.\d+)?",
    re.IGNORECASE,
)
MECHANISM_RE = re.compile(
    r"\b(?:instructions?(?::[uk])?|instr/op|instruction count|cycles?|"
    r"syscalls?|allocations?|alloc count|page faults?|faults?|call count|"
    r"calls/op|probe count)\b",
    re.IGNORECASE,
)
COUNT_VALUE_RE = re.compile(
    r"(?:[-+]?\d+(?:[.,]\d+)*(?:\.\d+)?(?:\s*[%x×])?|"
    r"\b(?:zero|one|two|three|unchanged|identical|same)\b)",
    re.IGNORECASE,
)
NEGATED_EVIDENCE_RE = re.compile(
    r"\b(?:no|without|lacks?|lacking|missing|neither|not recorded|unrecorded|"
    r"unavailable|not measured)\b",
    re.IGNORECASE,
)
BINARY_SHA_RE = re.compile(
    r"\b(?:ELF|binary|executable|server(?:\s+(?:ELF|binary|executable))?)\b"
    r".{0,100}?\bsha(?:-?256)?\b[^0-9a-f]{0,20}"
    r"(?P<sha>[0-9a-f]{64})\b",
    re.IGNORECASE,
)
RETRY_MARK_RE = re.compile(
    r"\b(?:retry predicates?|retry condition|revisit only|do not retry unless)\b",
    re.IGNORECASE,
)


def normalised_entries(path):
    """Yield (title, whitespace-normalised body, line). Normalisation is not
    optional: both ledgers hard-wrap at ~100 columns, so a raw-text grep scores a
    wrapped `Null\\nmedian` as *no null recorded* — the exact error this gate
    exists to prevent. Auditing raw text once reported a 95% void rate here
    against a true 69.4%."""
    if not path.exists():
        return
    text = path.read_text(encoding="utf-8", errors="replace")
    pos = [(m.start(), m.group(1)) for m in HDR.finditer(text)]
    for i, (start, title) in enumerate(pos):
        end = pos[i + 1][0] if i + 1 < len(pos) else len(text)
        yield title, " ".join(text[start:end].split()), text[:start].count("\n") + 1


def retry_excerpt(body):
    """Return the retry clause, not merely a heading such as
    `Retry predicate (concrete).`."""
    marker = RETRY_MARK_RE.search(body)
    if marker is None:
        return None
    excerpt = body[marker.start():marker.start() + 900]
    next_bullet = excerpt.find(" - **", 40)
    if next_bullet != -1:
        excerpt = excerpt[:next_bullet]
    return excerpt.strip()


def check_candidate(terms):
    if not terms:
        print("usage: check-candidate <symbol or phrase>...", file=sys.stderr)
        return 64
    hits = []
    for path in LEDGERS:
        for title, body, line in normalised_entries(path):
            for term in terms:
                if term.lower() in body.lower():
                    hits.append((
                        path.name,
                        line,
                        title.strip(),
                        term,
                        retry_excerpt(body),
                    ))
                    break
    if not hits:
        print(f"CLEAR: no ledger row names {terms}")
        return 0
    print(f"BLOCKED: {len(hits)} prior ledger row(s) already cover this ground.\n")
    for name, line, title, term, retry in hits[:12]:
        print(f"  {name}:{line}  (matched {term!r})")
        print(f"    {title[:150]}")
        print(f"    retry: {retry[:500] if retry else '(none recorded)'}")
    print("\nRead those rows before proceeding. If one is VOID (no A/A null and no")
    print("counted mechanism), say so explicitly in your new entry and cite it —")
    print("re-running a void row is legitimate; silently re-deriving it is not.")
    return 2


def added_entry_blocks(text):
    """Yield each newly added heading with only its own added body."""
    matches = list(HDR.finditer(text))
    for index, match in enumerate(matches):
        end = matches[index + 1].start() if index + 1 < len(matches) else len(text)
        yield match.group(1), " ".join(text[match.end():end].split())


def measured_null(body):
    """Require an actual A/A statistic.

    Merely writing `no A/A null control` must not satisfy the gate. Evidence is
    considered in sentence-sized fragments so a later candidate ratio cannot
    accidentally authenticate that negated statement.
    """
    fragments = re.split(r"(?<=[.!?;])\s+|\s+\|\s+", body)
    for fragment in fragments:
        if NEGATED_EVIDENCE_RE.search(fragment):
            continue
        if NULL_MEASUREMENT_RE.search(fragment):
            return True
    return False


def counted_mechanism(body):
    """Require a mechanism name and a count (or an explicit unchanged result)
    in the same clause."""
    fragments = re.split(r"(?<=[.!?;])\s+|\s+\|\s+", body)
    for fragment in fragments:
        if NEGATED_EVIDENCE_RE.search(fragment):
            continue
        if MECHANISM_RE.search(fragment) and COUNT_VALUE_RE.search(fragment):
            return True
    return False


def check_entry(source):
    text = sys.stdin.read() if source == "-" else Path(source).read_text(errors="replace")
    # Accept a raw diff: consider only added lines.
    if text.lstrip().startswith(("diff --git", "@@", "+++", "---")):
        text = "\n".join(l[1:] for l in text.splitlines()
                         if l.startswith("+") and not l.startswith("+++"))
    blocks = list(added_entry_blocks(text))
    rejects = []
    keeps = []
    for title, body in blocks:
        if REJECT_RE.search(title) and not (
            measured_null(body) or counted_mechanism(body)
        ):
            rejects.append(title)
        if KEEP_RE.search(title) and not BINARY_SHA_RE.search(body):
            keeps.append(title)

    if rejects:
        print("REJECTED: this REJECT-class entry records NEITHER an A/A null control")
        print("NOR a counted mechanism, so nobody can tell the lever from the harness.")
        print("That is the VOID-NONULL class — 124 of this repo's 125 void rows.\n")
        for title in rejects:
            print(f"  offending heading: {title[:150]}")
        print("\nAdd ONE of:")
        print("  * an A/A null control measured in the same invocation, with its band; or")
        print("  * a COUNTED mechanism showing no work was removed — instructions,")
        print("    cycles, syscalls, allocations, faults, or an exact call count.")
        print("A near-1.0 wall-clock ratio on its own is not evidence of anything.")
        return 3

    if keeps:
        print("REJECTED: this KEEP-class entry has no full executing-binary SHA-256.")
        for title in keeps:
            print(f"  offending heading: {title[:150]}")
        print("\nRecord the 64-hex SHA-256 self-reported by the benchmarked ELF.")
        return 4

    reviewed = [
        title
        for title, _ in blocks
        if REJECT_RE.search(title) or KEEP_RE.search(title)
    ]
    if reviewed:
        print("OK: all new verdict entries satisfy the ledger contract")
        for title in reviewed:
            print(f"  - {title[:120]}")
    else:
        print("OK: no new REJECT- or KEEP-class entry in this change")
    return 0


HOOK = """#!/usr/bin/env python3
import subprocess
import sys
from pathlib import Path

root = Path(subprocess.run(
    ["git", "rev-parse", "--show-toplevel"],
    capture_output=True, text=True, check=True, timeout=10,
).stdout.strip())
diff = subprocess.run(
    ["git", "diff", "--cached", "-U0", "--",
     "docs/NEGATIVE_EVIDENCE.md", "docs/perf_negative_evidence_ledger.md"],
    cwd=root, capture_output=True, check=False, timeout=10,
)
if diff.returncode != 0:
    sys.stderr.write("perf-ledger pre-commit: failed to inspect staged ledgers\\n")
    sys.exit(2)
if not diff.stdout:
    sys.exit(0)
guard = root / "scripts" / "perf_candidate_preflight.py"
result = subprocess.run(
    [sys.executable, str(guard), "check-entry", "-"],
    cwd=root, input=diff.stdout, check=False, timeout=10,
)
sys.exit(result.returncode)
"""


def install_hook():
    git_dir = Path(subprocess_run(
        ["git", "rev-parse", "--git-dir"],
    ).strip())
    if not git_dir.is_absolute():
        git_dir = ROOT / git_dir
    runner = git_dir / "hooks" / "pre-commit"
    if not runner.is_file() or runner.stat().st_mode & 0o111 == 0:
        print(
            f"pre-commit chain runner is missing or not executable: {runner}",
            file=sys.stderr,
        )
        return 2
    hook = git_dir / "hooks" / "hooks.d" / "pre-commit" / "60-perf-ledger.py"
    if hook.exists():
        print(f"refusing to overwrite existing hook plugin: {hook}", file=sys.stderr)
        return 1
    hook.parent.mkdir(parents=True, exist_ok=True)
    hook.write_text(HOOK)
    hook.chmod(0o755)
    print(f"installed {hook}")
    return 0


def subprocess_run(argv):
    result = subprocess.run(
        argv,
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
        timeout=10,
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"{' '.join(argv)} failed ({result.returncode}): {result.stderr.strip()}"
        )
    return result.stdout


def main(argv):
    if len(argv) < 2:
        print(__doc__)
        return 64
    mode = argv[1]
    if mode == "check-candidate":
        return check_candidate(argv[2:])
    if mode == "check-entry":
        return check_entry(argv[2] if len(argv) > 2 else "-")
    if mode == "install-hook":
        return install_hook()
    print(f"unknown mode {mode!r}", file=sys.stderr)
    return 64


if __name__ == "__main__":
    sys.exit(main(sys.argv))
