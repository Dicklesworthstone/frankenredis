#!/usr/bin/env python3
"""Audit the KEEP-class claim base for vs-incumbent coverage.

THE QUESTION (fleet audit, 2026-07-31)
--------------------------------------
Policy 2 says a perf KEEP is campaign output only when it carries a numeric
FrankenRedis/Redis ratio produced by a harness that ran the LIVE incumbent arm in
the SAME invocation. A self-speedup -- our own code before versus after -- is
maintenance. The policy has been enforced on NEW entries since 2026-07-27 by
`perf_candidate_preflight.py check-staged`. It has never been run BACKWARDS over
the claims that predate it. This does that.

WHY IT REUSES THE GATE'S OWN PREDICATE
--------------------------------------
`incumbent_measured()` is imported from `perf_candidate_preflight`, not
reimplemented. An audit that invents its own looser definition of "supported"
would produce a flattering number and prove nothing. If the gate would refuse the
entry today, this audit counts it as unsupported today.

RANKING: LOAD-BEARING FIRST
---------------------------
An unsupported claim a user might act on is worse than an unsupported claim
buried in a ledger. Each unsupported entry is scored by whether its subject
surfaces in reader-facing documentation (README, scorecards, parity/readiness
docs) rather than only in the lab notebook, and the queue is ordered by that.

CONVERTIBLE vs STRUCTURALLY UNMEASURABLE
----------------------------------------
These are different problems and the report separates them. A claim nobody has
gotten around to measuring against the incumbent can be converted by running the
harness. A claim on a surface where NO incumbent arm can exist -- a command or
mode vendored Redis does not implement, or an internal-only metric like bytes
allocated -- cannot be converted at all, and saying so is the honest answer
rather than leaving it in a conversion queue forever.
"""

import re
import sys
from collections import Counter
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from perf_candidate_preflight import (  # noqa: E402
    INCUMBENT_RATIO_RE,
    incumbent_measured,
    incumbent_same_invocation,
)

LEDGERS = [
    Path("docs/NEGATIVE_EVIDENCE.md"),
    Path("docs/perf_negative_evidence_ledger.md"),
]

# Reader-facing surfaces, most load-bearing first. A claim echoed in README is
# something a user may act on; a claim echoed only in a ledger is not.
PUBLIC_DOCS = [
    (Path("README.md"), 100),
    (Path("docs/perf_domination_scorecard.md"), 60),
    (Path("docs/RELEASE_READINESS_SCORECARD.md"), 60),
    (Path("docs/planning/FEATURE_PARITY.md"), 30),
    (Path("CHANGELOG.md"), 20),
]

KEEP_HEADING_RE = re.compile(r"^#{2,3}\s+(?P<title>.*\b(?:KEEP|SHIPPED)\b.*)$")
BEAD_RE = re.compile(r"frankenredis-[a-z0-9][a-z0-9-]{3,}")
RATIO_TOKEN_RE = re.compile(r"\b\d+\.\d{2,4}\s*[x×]")

# A DELIBERATELY LOOSER second detector, because the strict gate predicate
# matches a declared FORM ("FrankenRedis/Redis ... N.NNx") and a lot of older
# entries carry the same evidence written the other way round -- "0.49x -> 1.16x
# vs redis". Counting those as unsupported would produce a number as dishonest
# as a flattering one, just in the opposite direction. The gap between the two
# counts is the reformat-vs-remeasure split, and it is the single most useful
# number in this report: reformatting is minutes, re-measuring is hours.
RATIO_NEAR_REDIS_RE = re.compile(
    rf"(?:{RATIO_TOKEN_RE.pattern}.{{0,120}}?\bredis\b"
    rf"|\bredis\b.{{0,120}}?{RATIO_TOKEN_RE.pattern})",
    re.IGNORECASE | re.DOTALL,
)
# Self-referential counters with no incumbent analogue. Redis does not publish
# "instructions per operation" or "eventfd writes per operation", so an entry
# whose ONLY quantity is one of these cannot be turned into a ratio against it
# by any amount of harness work -- it is a different problem from unmeasured.
INTERNAL_ONLY_METRIC_RE = re.compile(
    r"\b(?:instructions?/op|instr/op|allocations?/op|alloc/op|"
    r"syscalls?/op|futex/op|write/op|probe count|calls/op|"
    r"bytes/op|cycles/op)\b",
    re.IGNORECASE,
)

# Surfaces where an incumbent arm cannot exist by construction.
NO_INCUMBENT_MARKERS = (
    "no incumbent",
    "not implemented by redis",
    "redis has no",
    "no redis equivalent",
    "frankenredis-only",
    "internal-only",
    "no vendored equivalent",
)


def split_entries(path):
    """Yield (title, body) for every KEEP/SHIPPED-class heading."""
    if not path.exists():
        return
    lines = path.read_text(errors="replace").splitlines()
    starts = []
    for i, line in enumerate(lines):
        if line.startswith("#") and re.match(r"^#{2,3}\s", line):
            starts.append(i)
    starts.append(len(lines))
    for idx in range(len(starts) - 1):
        head = lines[starts[idx]]
        m = KEEP_HEADING_RE.match(head)
        if not m:
            continue
        body = "\n".join(lines[starts[idx] : starts[idx + 1]])
        yield m.group("title").strip(), body


def load_public_text():
    out = []
    for path, weight in PUBLIC_DOCS:
        if path.exists():
            out.append((path, weight, path.read_text(errors="replace")))
    return out


# A claim's SUBJECT: the Redis command or subsystem it is about. Uppercase
# runs of >=3 chars in a title are overwhelmingly command names (ZRANGEBYLEX,
# HGETALL, BITOP). Verdict/process words are not subjects and are excluded, or
# every KEEP would "surface" on every page that says KEEP.
SUBJECT_TOKEN_RE = re.compile(r"\b[A-Z][A-Z0-9]{2,}(?:\.[A-Z]+)?\b")
SUBJECT_STOPWORDS = frozenset(
    {
        "KEEP", "SHIPPED", "LANDED", "WIN", "PROMOTED", "REJECT", "REJECTED",
        "NEGATIVE", "INVALID", "REVERT", "REVERTED", "HOLD", "MEASURE",
        "MEASURED", "STRUCTURAL", "AUDIT", "SURFACE", "DIAGNOSTIC", "COMPETITIVE",
        "SELF", "SPEEDUP", "CAVEAT", "FIX", "BUG", "TODO", "WIP", "NOTE",
        "ORIG", "CAND", "CTL", "AND", "NOT", "THE", "FOR", "WITH", "VS",
        "CPU", "RAM", "RSS", "CI", "CV", "ELF", "SHA", "ISA", "P16", "P1",
        "RDB", "AOF", "RESP", "LTO", "SIMD", "AVX2", "MIN", "MAX",
    }
)


def subject_tokens(title):
    """Command/subsystem names a public page would have to mention by name."""
    return {
        token
        for token in SUBJECT_TOKEN_RE.findall(title)
        if token not in SUBJECT_STOPWORDS
    }


def load_bearing_score(title, body, public):
    """How reachable is this claim from something a reader would act on?

    CORRECTED 2026-07-31 (BlackThrush), twice, and both corrections are recorded
    because the first one was wrong in the opposite direction.

    v1 accepted a bare ratio token ("1.18x") found anywhere in a public doc as
    proof the claim surfaced there. That is a collision, not a citation: ratios
    are three or four characters from a tiny alphabet and scorecards are full of
    them. It scored the ZRANGEBYLEX entry 120 ("in BOTH scorecards") when
    ZRANGEBYLEX appears in NEITHER -- the `1.18x` it matched belonged to an
    unrelated zset/hash RDB-decode row and an unrelated set/get/incr table.

    v2 replaced that with a subject-name match, which over-corrected: a claim
    about `SET key value EX` matched every page, because every page says SET.
    That inflated 79 entries to score 270 (README + both scorecards + parity +
    changelog).

    v3, used here: a bead id in the page is sufficient on its own. Otherwise the
    page must BOTH name the subject AND quote one of the entry's own ratio
    tokens -- naming the thing and citing its number. Neither half establishes
    reachability alone.
    """
    score = 0
    hits = []
    beads = set(BEAD_RE.findall(title)) | set(BEAD_RE.findall(body))
    subjects = subject_tokens(title)
    ratios = {r.replace(" ", "") for r in RATIO_TOKEN_RE.findall(title)}
    for path, weight, text in public:
        matched = any(bead in text for bead in beads)
        if not matched:
            squashed = text.replace(" ", "")
            matched = any(subject in text for subject in subjects) and any(
                ratio in squashed for ratio in ratios
            )
        if matched:
            score += weight
            hits.append(path.name)
    return score, hits


def unconvertible_reason(body):
    low = body.lower()
    for marker in NO_INCUMBENT_MARKERS:
        if marker in low:
            return marker
    # No incumbent-comparable quantity anywhere: the entry's only numbers are
    # internal counters Redis does not expose an analogue of.
    if INTERNAL_ONLY_METRIC_RE.search(body) and not RATIO_NEAR_REDIS_RE.search(body):
        return "quantifies only internal counters (instr/op, syscalls/op) with no Redis analogue"
    return None


def main():
    public = load_public_text()
    supported, substantive, unsupported = [], [], []
    partial = Counter()

    for ledger in LEDGERS:
        for title, body in split_entries(ledger):
            if incumbent_measured(body):
                supported.append((ledger.name, title))
                continue
            has_ratio = INCUMBENT_RATIO_RE.search(body) is not None
            has_live = incumbent_same_invocation(body)
            loose = RATIO_NEAR_REDIS_RE.search(body) is not None
            if loose and has_live:
                # Evidence looks present but is not in the gate's declared form.
                substantive.append(
                    {"ledger": ledger.name, "title": title, "body": body}
                )
                continue
            if has_ratio and not has_live:
                partial["ratio quoted, no live same-invocation arm"] += 1
            elif has_live and not has_ratio:
                partial["live arm described, no numeric ratio"] += 1
            elif loose:
                partial["vs-redis ratio present, no live-arm evidence at all"] += 1
            else:
                partial["no vs-redis ratio anywhere in the entry"] += 1
            score, hits = load_bearing_score(title, body, public)
            if loose:
                # Quotes a number against Redis with nothing establishing that
                # the incumbent ran side by side. This repo has three recorded
                # false positives of exactly this shape (io_uring 1.43->0.92,
                # HGETALL 1.45->0.98, P16 1.49->1.33), so a load-bearing claim
                # in this bucket is the most dangerous kind here.
                bucket = "QUOTES-RATIO-NO-LIVE-ARM"
            else:
                bucket = "NO-VS-REDIS-NUMBER"
            unsupported.append(
                {
                    "ledger": ledger.name,
                    "title": title,
                    "score": score,
                    "hits": hits,
                    "bucket": bucket,
                    "has_ratio": has_ratio,
                    "has_live": has_live,
                    "unconvertible": unconvertible_reason(body),
                }
            )

    total = len(supported) + len(substantive) + len(unsupported)
    print("=" * 78)
    print("KEEP-CLASS CLAIM COVERAGE AUDIT")
    print("=" * 78)
    print(
        f"HEADLINE: {total} KEEP claims total; {len(supported)} carry a vs-incumbent "
        f"ratio in the gate's declared form (numeric FrankenRedis/Redis ratio AND a "
        f"live incumbent arm bound to the same invocation); "
        f"{len(substantive) + len(unsupported)} do not."
    )
    strict_pct = ((len(substantive) + len(unsupported)) / total * 100) if total else 0.0
    hard_pct = (len(unsupported) / total * 100) if total else 0.0
    print(f"          Unsupported share, strict gate form: {strict_pct:.1f}%")
    print()
    print(
        f"          Of those, {len(substantive)} DO quote a vs-Redis ratio alongside "
        f"live-arm evidence,\n          but write it in a form the gate does not "
        f"recognise (\"0.49x -> 1.16x vs redis\"\n          rather than \"FrankenRedis/"
        f"Redis 1.16x\"). Those need REFORMATTING, not\n          re-measurement."
    )
    print(
        f"          Claims with no vs-Redis evidence at all: {len(unsupported)} "
        f"({hard_pct:.1f}%). This is the\n          number that actually needs "
        f"measurement work."
    )
    print()
    print("The strict predicate is the repo's OWN gate, imported not reimplemented:")
    print("  perf_candidate_preflight.incumbent_measured(). An audit that invented a")
    print("  looser definition would produce a flattering number; one that ignored")
    print("  the reformat/re-measure split would produce a misleading harsh one.")
    print("  Both are reported.")
    print()
    print("-- why the unsupported ones fall short --")
    for reason, count in partial.most_common():
        print(f"   {count:>4}  {reason}")

    unconvertible = [u for u in unsupported if u["unconvertible"]]
    convertible = [u for u in unsupported if not u["unconvertible"]]
    print()
    print(
        f"-- of the {len(unsupported)} unsupported: {len(convertible)} appear convertible, "
        f"{len(unconvertible)} declare no incumbent exists --"
    )

    print()
    print("=" * 78)
    print("CONVERSION QUEUE, ranked by how load-bearing the claim is")
    print("=" * 78)
    print("Score: README=100, scorecards=60, parity=30, changelog=20.")
    print("A score of 0 means the claim lives only in a ledger a user never reads.")
    print()
    ranked = sorted(convertible, key=lambda u: (-u["score"], u["title"]))
    shown = [u for u in ranked if u["score"] > 0]
    for i, u in enumerate(shown, 1):
        print(f"{i:>3}. [{u['score']:>3}] {u['bucket']:<24} {u['title'][:96]}")
        print(f"      surfaces in: {', '.join(u['hits'])}")
    buried = len(ranked) - len(shown)
    print()
    print(
        f"   ... plus {buried} convertible claims with score 0 (ledger-only, "
        f"no reader-facing surface)."
    )

    if unconvertible:
        print()
        print("=" * 78)
        print("NOT CONVERTIBLE -- no incumbent arm exists for this surface")
        print("=" * 78)
        print("These are a DIFFERENT problem from unmeasured claims: no amount of")
        print("harness work produces a vs-incumbent ratio for them.")
        for u in sorted(unconvertible, key=lambda u: -u["score"]):
            print(f"  [{u['score']:>3}] {u['title'][:100]}")
            print(f"        declared: {u['unconvertible']}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
