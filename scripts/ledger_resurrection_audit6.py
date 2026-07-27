#!/usr/bin/env python3
"""Re-audit both frankenredis ledgers under the frankenfs six-class taxonomy.

Adopted verbatim per the 2026-07-26 fleet broadcast, replacing my own V1-V6
scheme. The screen below is TRIAGE, not a verdict. Every flagged row must be
read and adjudicated by hand; the durable adjudication for snapshot
`112b133f80e81ff00ad7874641236cc66c136d1f` is in
`docs/LEDGER_RESURRECTION.md`.

  VALID-PROFILE    rejected before any source edit, on a named frame with
                   non-zero self-time plus a computed Amdahl ceiling
  VALID-MECHANISM  no A/A null, but refuted on a COUNTED mechanism
                   (instructions / cycles / syscalls / allocations / faults
                   unchanged) — a null cannot change "no work was removed"
  VALID-AB         A/B with a recorded A/A null, effect inside it
  VOID-CV          killed ONLY by a cv<5 gate
  VOID-ZEROSELF    target frame ~0% self-time in the profile actually run
  VOID-NONULL      near-1.0 ratio, no null, no counted mechanism

This repo's convention is instructions:u A/Bs, so VALID-MECHANISM is expected to
carry real weight here. The broadcast is explicit that it cuts both ways, so it
is applied only when the row names a counted quantity AND reports it unchanged.
"""
import re
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LEDGERS = ["NEGATIVE_EVIDENCE.md", "perf_negative_evidence_ledger.md"]

# A ledger entry starts at exactly `## `. Nested `###` headings are part of the
# parent entry. Treating both as entries split evidence away from its verdict and
# was the root cause of the superseded 180-row regex "audit".
HDR = re.compile(r"^## (?!#)(.+)$", re.M)
REJECT_RE = re.compile(
    r"\b(REJECT|REJECTED|NEGATIVE|NO[- ]SHIP|UNDECIDABLE|DECLINED|INVALID"
    r"|REVERT|REVERTED|NOT WORTH|ABANDON)\b", re.I)
KEEP_RE = re.compile(r"\b(KEEP|SHIPPED|WIN|LANDED|FIXED|PROMOTED)\b", re.I)
SURVEY_RE = re.compile(r"\b(SURFACE|SURVEY|BLOCKER|TRIAGE|VERIFY|CORRECTION|LESSON)\b", re.I)

NULL_RE = re.compile(r"A/A|null median|null control|null floor|null CV|\(null |null=", re.I)
CVGATE_RE = re.compile(r"cv\s*<\s*[0-9]|CV gate|cv-gate", re.I)
SELFTIME_RE = re.compile(r"([0-9]+(?:\.[0-9]+)?)\s*%\s*(?:exact\s+)?self[- ]time", re.I)
AMDAHL_RE = re.compile(r"amdahl|ceiling|upper bound|at most [0-9.]+%", re.I)
SHA_RE = re.compile(r"\b[0-9a-f]{64}\b")
RATIO_RE = re.compile(r"\b([01]\.\d{2,})\s*x\b", re.I)
# A COUNTED mechanism: the row names a counted quantity as the thing measured.
COUNTED_RE = re.compile(
    r"instructions?(:u|:k)?\b|instr/op|instruction count|fewer instructions"
    r"|\bcycles\b|syscalls?\b|allocations?\b|alloc count|page faults?"
    r"|mod_count|probe count|lookups?\b", re.I)
UNCHANGED_RE = re.compile(
    r"~?0[- ]gain|no measurable|no stable gain|unchanged|neutral|no work"
    r"|below the .{0,20}gate|identical instruction|same instruction", re.I)


def entries(name):
    text = (ROOT / "docs" / name).read_text(encoding="utf-8", errors="replace")
    pos = [(m.start(), m.group(1)) for m in HDR.finditer(text)]
    for i, (start, title) in enumerate(pos):
        end = pos[i + 1][0] if i + 1 < len(pos) else len(text)
        # Ledgers hard-wrap at ~100 cols; normalise before any predicate runs.
        yield title, " ".join(text[start:end].split()), text[:start].count("\n") + 1


def classify_verdict(title):
    """KEEP / REJECT / SURVEY / UNKNOWN from the heading."""
    if KEEP_RE.search(title) and not REJECT_RE.search(title):
        return "KEEP"
    if REJECT_RE.search(title):
        return "REJECT"
    if SURVEY_RE.search(title):
        return "SURVEY"
    return "UNKNOWN"


def audit():
    rows, tally = [], Counter()
    for name in LEDGERS:
        for title, body, line in entries(name):
            tally["parsed"] += 1
            verdict = classify_verdict(title)
            tally[verdict] += 1
            if verdict != "REJECT":
                continue

            has_null = bool(NULL_RE.search(body))
            cv_only = bool(CVGATE_RE.search(body)) and not has_null
            st = [float(x) for x in SELFTIME_RE.findall(body)]
            st_max = max(st) if st else None
            counted = bool(COUNTED_RE.search(body))
            unchanged = bool(UNCHANGED_RE.search(body))
            ratios = [float(r) for r in RATIO_RE.findall(body)]
            claimed = min(ratios, key=lambda r: abs(r - 1.0)) if ratios else None
            near_one = claimed is not None and abs(claimed - 1.0) <= 0.10

            # These are triage hints only. Regex cannot decide whether the named
            # mechanism was actually counted, whether a heading is merely a
            # survey/correction, or whether the workload routed through the target.
            # Order matters only for producing a useful hand-review queue.
            if st_max is not None and st_max < 0.1:
                cls = "VOID-ZEROSELF"
            elif cv_only:
                cls = "VOID-CV"
            elif has_null:
                cls = "VALID-AB"
            elif counted and unchanged:
                cls = "VALID-MECHANISM"
            elif st_max is not None and st_max > 0 and AMDAHL_RE.search(body):
                cls = "VALID-PROFILE"
            elif near_one or claimed is None:
                cls = "VOID-NONULL"
            else:
                # A large claimed effect, no null, no counted mechanism. Not
                # near-1.0, so VOID-NONULL does not fit; flag for hand review.
                cls = "VOID-NONULL"
            tally[cls] += 1
            rows.append(dict(file=name, line=line, title=title.strip(), cls=cls,
                             claimed=claimed, self_time=st_max, has_null=has_null,
                             counted=counted, sha=bool(SHA_RE.search(body))))
    return rows, tally


def main():
    rows, t = audit()
    audited = t["REJECT"]
    if audited == 0:
        print("no REJECT-like headings found; refusing to report percentages")
        return 1
    void = sum(t[k] for k in ("VOID-NONULL", "VOID-CV", "VOID-ZEROSELF"))
    valid = sum(t[k] for k in ("VALID-AB", "VALID-MECHANISM", "VALID-PROFILE"))
    print(f"entries parsed         {t['parsed']}")
    for k in ("KEEP", "SURVEY", "UNKNOWN", "REJECT"):
        print(f"  {k:<20} {t[k]}")
    print(f"\nREJECT audited         {audited}")
    for k in ("VALID-AB", "VALID-MECHANISM", "VALID-PROFILE",
              "VOID-NONULL", "VOID-CV", "VOID-ZEROSELF"):
        print(f"  {k:<20} {t[k]}")
    print(f"\nVOID total             {void} / {audited} = {100*void/audited:.1f}%")
    print(f"VALID total            {valid}")
    sha = sum(1 for r in rows if r["sha"])
    print(f"rows with any 64-hex hash (triage only) {sha} / {audited} = {100*sha/audited:.1f}%")
    q = sorted((r for r in rows if r["cls"].startswith("VOID") and r["self_time"]),
               key=lambda r: -r["self_time"])
    print("\n-- VOID rows carrying a recorded target self-time (rank by it) --")
    for r in q[:10]:
        print(f'  {r["self_time"]:6.2f}%  {r["cls"]:<14} {r["file"][:14]}:{r["line"]:<6} {r["title"][:66]}')
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
