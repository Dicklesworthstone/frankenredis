#!/usr/bin/env python3
"""Generate docs/LEDGER_RESURRECTION.md — the Meta-Lever 1 audit deliverable.

Method (PERF_CAMPAIGN_2026-07-25 §1), applied to frankenredis's two ledgers:
  docs/NEGATIVE_EVIDENCE.md             (422 `##` + 451 `###` entries)
  docs/perf_negative_evidence_ledger.md (339 `##` entries)

Void predicates, in the order they are tested:

  V1  the claimed ratio lies INSIDE the entry's own null floor.  The floor is
      taken from the entry when it records one (null median / null CV / effect
      CV); only when the row records NO null control at all do we fall back to
      the era-default floor for the harness shape the row names.
  V2  no A/A null control recorded at all.
  V3  target-frame self-time ~0%, or no self-time recorded (V3n).
  V4  the decision was gated on `cv < X%` rather than on a null floor.
  V5  no binary sha256 recorded.
  V6  the row concedes a REAL, tight, non-noise effect and rejects it anyway on
      an arbitrary magnitude threshold ("below the 1% keep gate", "too small").
      §2.3 of the campaign says this is a harness rejection, not a lever
      rejection: decidability is set by the null CI, not by a round number.

Verdicts:
  VOID        V1 or V4 fired — the measurement could not have decided the lever.
  GATE-VOID   V6 fired — the measurement DID decide it, and a threshold vetoed it.
  PROVENANCE  effect far outside any plausible floor, but the row is missing
              null control / sha / self-time. Decision probably sound; evidence
              is not reproducible to the current contract.
  SOUND       null control + sha + self-time all present.

The 'self-time of target frame' column the campaign asks us to RANK on does not
exist in the pre-hardening rows, so it is supplied by joining each entry's named
target symbols against a FRESH symbolized live profile of the running server.
"""
import json
import re
import sys
from collections import Counter
from pathlib import Path

ROOT = Path("/data/projects/frankenredis")
LEDGERS = ["NEGATIVE_EVIDENCE.md", "perf_negative_evidence_ledger.md"]

HDR = re.compile(r"^##+ (.+)$", re.M)
REJECT_RE = re.compile(
    r"\b(REJECT|REJECTED|BLOCKER|NEGATIVE|UNDECIDABLE|NO[- ]LEVER|NO[- ]SHIP|NULL RESULT"
    r"|DECLINED|INVALID|REVERT|REVERTED|NOT WORTH|ABANDON)\b", re.I)
SHA_RE = re.compile(r"\b[0-9a-f]{64}\b")
NULL_RE = re.compile(r"A/A|null median|null control|null floor|null CV|null 1\.|null=|\(null ", re.I)
NULL_MED_RE = re.compile(r"null(?:\s+median)?[^0-9]{0,12}([01]\.\d+)", re.I)
NULL_CV_RE = re.compile(r"null\s*CV[^0-9]{0,6}([0-9.]+)\s*%", re.I)
SELFTIME_RE = re.compile(r"([0-9]+(?:\.[0-9]+)?)\s*%\s*(?:exact\s+)?self[- ]time", re.I)
CVGATE_RE = re.compile(r"cv\s*<\s*[0-9]|CV gate|cv-gate|coefficient of variation gate", re.I)
MAGGATE_RE = re.compile(
    r"keep gate|below the [0-9.]+\s*%|below the gate|too small|sub-gate|SUB-GATE"
    r"|fails? the .{0,20}gate|below (?:the )?(?:1|one)\s*%", re.I)
REALEFFECT_RE = re.compile(
    r"real,? tight,? non-noise|a real .{0,30}improvement|genuine .{0,20}(win|gain)"
    r"|non-noise improvement", re.I)
DATE_RE = re.compile(r"(20\d\d-\d\d-\d\d)")
BEAD_RE = re.compile(r"`(frankenredis-[0-9a-z]+)`")
RATIO_RE = re.compile(r"\b([01]\.\d{2,})\s*x\b", re.I)
IDENT = re.compile(r"`([A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*)`")

SHAPES = [
    (re.compile(r"-c\s?1\b|single[- ]conn|one connection", re.I), "-c1 single-conn", 0.001),
    (re.compile(r"criterion|bench-reference|in-process|instructions", re.I), "in-process instr", 0.02),
    (re.compile(r"-c\s?50|50 clients|\bc50\b", re.I), "-c50 multi-conn", 0.105),
    (re.compile(r"-P\s?16|pipeline\s?=?\s?16|P16", re.I), "P16 multi-conn", 0.105),
]
DEFAULT_FLOOR = 0.105


def entries(name):
    text = (ROOT / "docs" / name).read_text(encoding="utf-8", errors="replace")
    pos = [(m.start(), m.group(1)) for m in HDR.finditer(text)]
    for i, (start, title) in enumerate(pos):
        end = pos[i + 1][0] if i + 1 < len(pos) else len(text)
        # Both ledgers hard-wrap prose at ~100 columns, so a multi-word phrase
        # ("null median", "keep gate", "self-time") is routinely split across a
        # newline. Matching the raw text silently scores those entries as though
        # they recorded no null control at all — the exact provenance error this
        # audit exists to catch. Collapse whitespace before any predicate runs.
        body = " ".join(text[start:end].split())
        yield title, body, text[:start].count("\n") + 1


def load_profile(path):
    prof = {}
    if not path or not Path(path).exists():
        return prof
    for line in Path(path).read_text(errors="replace").splitlines():
        m = re.match(r"\s*([0-9]+\.[0-9]+)%\s+\S+\s+\S+\s+\[[.k]\]\s+(.+?)\s*$", line)
        if not m:
            continue
        pct, sym = float(m.group(1)), m.group(2).strip()
        prof[sym] = max(prof.get(sym, 0.0), pct)
    return prof


def live_selftime(syms, prof):
    best, bestsym = 0.0, None
    for s in syms:
        tail = s.split("::")[-1]
        if len(tail) < 7:
            continue
        for psym, pct in prof.items():
            if tail in psym and pct > best:
                best, bestsym = pct, psym
    return best, bestsym


def audit(prof):
    rows = []
    for name in LEDGERS:
        for title, body, line in entries(name):
            if not REJECT_RE.search(title):
                continue
            sha = bool(SHA_RE.search(body))
            has_null = bool(NULL_RE.search(body))
            st = [float(x) for x in SELFTIME_RE.findall(body)]
            st_max = max(st) if st else None
            cvgate = bool(CVGATE_RE.search(body))
            maggate = bool(MAGGATE_RE.search(body))
            realeffect = bool(REALEFFECT_RE.search(body))
            d = DATE_RE.search(title)
            bead = BEAD_RE.search(title) or BEAD_RE.search(body)
            ratios = [float(r) for r in RATIO_RE.findall(body)]
            claimed = min(ratios, key=lambda r: abs(r - 1.0)) if ratios else None

            # ---- the floor: prefer what the row itself recorded --------------
            floor, floor_src = None, None
            mcv = NULL_CV_RE.search(body)
            if mcv:
                floor = max(3.0 * float(mcv.group(1)) / 100.0, 1e-5)
                floor_src = f"row: 3x null CV {mcv.group(1)}%"
            else:
                mmed = NULL_MED_RE.search(body)
                if mmed:
                    floor = max(2.0 * abs(float(mmed.group(1)) - 1.0), 1e-5)
                    floor_src = f"row: 2x |null median {mmed.group(1)} - 1|"
            if floor is None:
                floor, floor_src = DEFAULT_FLOOR, "assumed: era default (-c50/P16, +/-10.5%)"
                for rx, nm, f in SHAPES:
                    if rx.search(body):
                        floor, floor_src = f, f"assumed: {nm} shape"
                        break

            syms = set()
            for m in IDENT.finditer(body):
                s = m.group(1)
                if s.startswith("frankenredis-") or s.isupper() or len(s) < 6:
                    continue
                if "_" not in s and "::" not in s:
                    continue
                syms.add(s)
            live_pct, live_sym = live_selftime(syms, prof)

            reasons = []
            if claimed is not None and abs(claimed - 1.0) <= floor:
                reasons.append("V1")
            if not has_null:
                reasons.append("V2")
            if st_max is not None and st_max < 0.1:
                reasons.append("V3")
            elif st_max is None:
                reasons.append("V3n")
            if cvgate:
                reasons.append("V4")
            if not sha:
                reasons.append("V5")
            if maggate and realeffect and has_null:
                reasons.append("V6")

            if "V6" in reasons:
                verdict = "GATE-VOID"
            elif "V1" in reasons or "V4" in reasons:
                verdict = "VOID"
            elif not reasons:
                verdict = "SOUND"
            else:
                verdict = "PROVENANCE"

            rows.append(dict(file=name, line=line, title=title.strip(),
                             date=d.group(1) if d else None,
                             bead=bead.group(1) if bead else None,
                             claimed=claimed, floor=floor, floor_src=floor_src,
                             has_null=has_null, sha=sha, st_row=st_max,
                             st_live=live_pct, st_sym=live_sym,
                             reasons=reasons, verdict=verdict,
                             syms=sorted(syms)[:14]))
    return rows


def esc(s):
    return s.replace("|", "\\|").replace("\n", " ").strip()


def emit_markdown(rows, prof, profile_note, out_path):
    c = Counter(r["verdict"] for r in rows)
    n = len(rows)
    q = sorted([r for r in rows if r["verdict"] in ("VOID", "GATE-VOID")],
               key=lambda r: (-(r["st_live"] or 0), r["file"], r["line"]))
    L = []
    A = L.append
    A("# Ledger Resurrection Audit — frankenredis\n\n")
    A("> Meta-Lever #1 of `PERF_CAMPAIGN_2026-07-25`. Owner: cc / STRUCTURAL lane.\n")
    A("> Regenerate with `scripts/ledger_resurrection_audit.py <profile.txt> "
      "docs/LEDGER_RESURRECTION.md \"<profile description>\"`.\n\n")
    A("Audited ledgers: `docs/NEGATIVE_EVIDENCE.md` (422 `##` + 451 `###` entries) and "
      "`docs/perf_negative_evidence_ledger.md` (339 `##` entries). Every heading whose verdict "
      "word is REJECT-class — REJECT/REJECTED/NEGATIVE/NO-SHIP/BLOCKER/REVERT/DECLINED/"
      "INVALID/UNDECIDABLE — is scored against the void predicates below.\n")
    A("\n## Method\n\n")
    A("Both ledgers hard-wrap prose at ~100 columns, so every predicate runs against a "
      "whitespace-normalised body. Matching the raw text scores a line-wrapped "
      "`Null\\nmedian 1.0000000` as *no null control recorded* — the exact provenance error "
      "this audit exists to catch. (The first run of this script made precisely that mistake "
      "and reported a 95% void rate; the corrected rate is below.)\n\n")
    A("**Void predicates**, in the order tested:\n\n")
    A("| Predicate | Test |\n|---|---|\n")
    A("| V1 | the claimed ratio lies INSIDE the entry's own null floor. The floor comes from "
      "the entry when it records one (null median / null CV); only when the row records no "
      "null control at all do we fall back to the era-default floor for the harness shape the "
      "row names (`-c1` ±0.1%, in-process instruction counts ±2%, `-c50`/P16 ±10.5%). |\n")
    A("| V2 | no A/A null control recorded at all. |\n")
    A("| V3 / V3n | target-frame self-time recorded as ~0% / not recorded at all. |\n")
    A("| V4 | the decision was gated on `cv < X%` rather than on a null floor. |\n")
    A("| V5 | no binary sha256 recorded. |\n")
    A("| V6 | the row concedes a real, tight, non-noise effect and rejects it anyway on an "
      "arbitrary magnitude threshold (\"below the 1% keep gate\"). §2.3 of the campaign says "
      "decidability is set by the null CI, not by a round number. |\n")
    A("\n**Verdicts.** The campaign's literal predicate list marks nearly every pre-hardening "
      "row void, which is true but not actionable, so the result is stratified:\n\n")
    A("| Verdict | Meaning | Actionable? |\n|---|---|---|\n")
    A("| VOID | V1 or V4 fired — the measurement could not have decided the lever. | "
      "**Yes — re-run.** |\n")
    A("| GATE-VOID | V6 fired — the measurement *did* decide it and a threshold vetoed it. | "
      "**Yes — re-adjudicate.** |\n")
    A("| PROVENANCE | the effect is far outside any plausible floor, but the row is missing "
      "null control / sha / self-time. The decision is probably sound; the evidence is not "
      "reproducible to the current contract. | No — record only. |\n")
    A("| SOUND | null control + binary sha + self-time all present. | No. |\n")
    A("\nThe **self-time of target frame** column the campaign asks us to rank on does not exist "
      "in the pre-hardening rows. Rather than leave it blank, every entry's backticked Rust "
      "identifiers are joined against a **fresh symbolized live profile** of the running server, "
      "so the column reports what the named frame costs *now* instead of what the row claimed "
      "then. A void entry whose target frame is invisible in the live profile is not worth "
      "re-running; a void entry sitting on a live frame is.\n")
    A(f"\nProfile used for the join: {profile_note} ({len(prof)} symbols).\n")
    A("\n## Yield\n")
    A(f"| Metric | Count |\n|---|---|\n")
    A(f"| REJECT-class entries audited | {n} |\n")
    for k in ("VOID", "GATE-VOID", "PROVENANCE", "SOUND"):
        A(f"| {k} | {c.get(k, 0)} |\n")
    A(f"| Re-run under the corrected harness | see *Re-run results* below |\n")
    A("\n### Predicate frequency across all audited entries\n\n")
    pc = Counter()
    for r in rows:
        for x in r["reasons"]:
            pc[x] += 1
    A("| Predicate | Meaning | Entries |\n|---|---|---|\n")
    meanings = {
        "V1": "claimed ratio inside the entry's own null floor",
        "V2": "no A/A null control recorded at all",
        "V3": "target-frame self-time recorded as ~0%",
        "V3n": "no target-frame self-time recorded",
        "V4": "decision gated on `cv < X%`",
        "V5": "no binary sha256 recorded",
        "V6": "real, tight effect vetoed by an arbitrary magnitude gate",
    }
    for k in ("V1", "V2", "V3", "V3n", "V4", "V5", "V6"):
        A(f"| {k} | {meanings[k]} | {pc.get(k, 0)} |\n")
    A("\n## Rehabilitation queue — VOID / GATE-VOID ranked by live self-time\n\n")
    A("| # | Entry | Ratio claimed | Null floor at the time | Self-time of target frame (live) | "
      "Binary sha? | Verdict |\n|---|---|---|---|---|---|---|\n")
    for i, r in enumerate(q[:60], 1):
        loc = f'`{r["file"]}:{r["line"]}`'
        st = (f'{r["st_live"]:.2f}% (`{r["st_sym"]}`)' if r["st_live"]
              else "not visible in the live profile")
        A(f'| {i} | {loc}<br>{esc(r["title"])[:150]} | '
          f'{r["claimed"] if r["claimed"] is not None else "not stated"} | '
          f'±{r["floor"]*100:.3g}% — {r["floor_src"]} | {st} | '
          f'{"yes" if r["sha"] else "no"} | {r["verdict"]} |\n')
    A("\n## Full audit\n\nMachine-readable: `artifacts/optimization/campaign-20260725-cc/"
      "ledger_resurrection.json` (one object per audited entry).\n")
    Path(out_path).write_text("".join(L))


def main():
    prof_path = sys.argv[1] if len(sys.argv) > 1 else None
    prof = load_profile(prof_path)
    print(f"profile symbols loaded: {len(prof)}")
    rows = audit(prof)
    Path("resurrection.json").write_text(json.dumps(rows, indent=1))
    print("audited", len(rows), dict(Counter(r["verdict"] for r in rows)))
    q = sorted([r for r in rows if r["verdict"] in ("VOID", "GATE-VOID")],
               key=lambda r: -(r["st_live"] or 0))
    print("\n-- rehabilitation queue (ranked by LIVE self-time of a named target frame) --")
    for r in q[:25]:
        print(f'{r["st_live"]:6.2f}%  {r["verdict"]:9s} claimed={r["claimed"]} '
              f'{r["file"][:14]}:{r["line"]:5d} {r["st_sym"] or "-"} | {r["title"][:70]}')
    if len(sys.argv) > 2:
        emit_markdown(rows, prof, sys.argv[3] if len(sys.argv) > 3 else str(prof_path),
                      sys.argv[2])
        print("\nwrote", sys.argv[2])


if __name__ == "__main__":
    main()
