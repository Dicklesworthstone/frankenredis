#!/usr/bin/env python3
"""Mechanically enforce ledger integrity, so it cannot decay again.

Modelled on frankensqlite's `sql_pipeline_candidate_preflight` (exit 2 = BLOCKED),
which is why that repo sits at a 1.7% void rate while repos that audited once and
moved on sit at 25-91%. The 2026-07-26 fleet broadcast's conclusion was that
ledger integrity DECAYS, so the audit has to become a gate rather than an event.

Two modes:

  check-candidate  <target symbol or phrase>...
      Refuse a lever whose ground has already been covered. Greps both ledgers
      for REJECT-class rows naming the target and prints them.
      exit 0 = clear · exit 2 = BLOCKED by a prior row

  check-entry <file>
      Refuse a NEW REJECT-class ledger entry that is unfalsifiable, i.e. that
      records neither an A/A null control nor a counted mechanism. This is the
      VOID-NONULL class that is 124 of this repo's 125 void rows, and the whole
      point is to make writing another one impossible.
      exit 0 = admissible · exit 3 = REJECTED, the row cannot be adjudicated

Wire `check-entry` into a pre-commit hook to close the loop:

    git diff --cached -U0 -- docs/NEGATIVE_EVIDENCE.md \\
      | scripts/perf_candidate_preflight.py check-entry -
"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LEDGERS = [ROOT / "docs" / "NEGATIVE_EVIDENCE.md",
           ROOT / "docs" / "perf_negative_evidence_ledger.md"]

HDR = re.compile(r"^##+ (.+)$", re.M)
REJECT_RE = re.compile(
    r"\b(REJECT|REJECTED|NEGATIVE|NO[- ]SHIP|UNDECIDABLE|DECLINED|INVALID"
    r"|REVERT|REVERTED|NOT WORTH|ABANDON)\b", re.I)
NULL_RE = re.compile(r"A/A|null median|null control|null floor|null CV|\(null |null=", re.I)
COUNTED_RE = re.compile(
    r"instructions?(:u|:k)?\b|instr/op|instruction count|fewer instructions"
    r"|\bcycles\b|syscalls?\b|allocations?\b|alloc count|page faults?"
    r"|call count|calls/op|probe count", re.I)


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


def check_candidate(terms):
    if not terms:
        print("usage: check-candidate <symbol or phrase>...", file=sys.stderr)
        return 64
    hits = []
    for path in LEDGERS:
        for title, body, line in normalised_entries(path):
            if not REJECT_RE.search(title):
                continue
            for term in terms:
                if term.lower() in body.lower():
                    hits.append((path.name, line, title.strip(), term))
                    break
    if not hits:
        print(f"CLEAR: no REJECT-class ledger row names {terms}")
        return 0
    print(f"BLOCKED: {len(hits)} prior REJECT-class row(s) already cover this ground.\n")
    for name, line, title, term in hits[:12]:
        print(f"  {name}:{line}  (matched {term!r})")
        print(f"    {title[:150]}")
    print("\nRead those rows before proceeding. If one is VOID (no A/A null and no")
    print("counted mechanism), say so explicitly in your new entry and cite it —")
    print("re-running a void row is legitimate; silently re-deriving it is not.")
    return 2


def check_entry(source):
    text = sys.stdin.read() if source == "-" else Path(source).read_text(errors="replace")
    # Accept a raw diff: consider only added lines.
    if text.lstrip().startswith(("diff --git", "@@", "+++", "---")):
        text = "\n".join(l[1:] for l in text.splitlines()
                         if l.startswith("+") and not l.startswith("+++"))
    body = " ".join(text.split())
    titles = [m.group(1) for m in HDR.finditer(text)]
    rejects = [t for t in titles if REJECT_RE.search(t)]
    if not rejects:
        print("OK: no new REJECT-class entry in this change")
        return 0
    has_null = bool(NULL_RE.search(body))
    has_counted = bool(COUNTED_RE.search(body))
    if has_null or has_counted:
        which = "A/A null" if has_null else "counted mechanism"
        print(f"OK: REJECT entry is adjudicable ({which} recorded)")
        for t in rejects:
            print(f"  - {t[:120]}")
        return 0
    print("REJECTED: this REJECT-class entry records NEITHER an A/A null control")
    print("NOR a counted mechanism, so nobody can tell the lever from the harness.")
    print("That is the VOID-NONULL class — 124 of this repo's 125 void rows.\n")
    for t in rejects:
        print(f"  offending heading: {t[:150]}")
    print("\nAdd ONE of:")
    print("  * an A/A null control measured in the same invocation, with its band; or")
    print("  * a COUNTED mechanism showing no work was removed — instructions,")
    print("    cycles, syscalls, allocations, faults, or an exact call count.")
    print("A near-1.0 wall-clock ratio on its own is not evidence of anything.")
    return 3


def main(argv):
    if len(argv) < 2:
        print(__doc__)
        return 64
    mode = argv[1]
    if mode == "check-candidate":
        return check_candidate(argv[2:])
    if mode == "check-entry":
        return check_entry(argv[2] if len(argv) > 2 else "-")
    print(f"unknown mode {mode!r}", file=sys.stderr)
    return 64


if __name__ == "__main__":
    sys.exit(main(sys.argv))
