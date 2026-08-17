#!/usr/bin/env python3
"""Mechanically enforce ledger integrity, so it cannot decay again.

Modelled on frankensqlite's `sql_pipeline_candidate_preflight` (exit 2 = BLOCKED),
which is why that repo sits at a 1.7% void rate while repos that audited once and
moved on sit at 25-91%. The 2026-07-26 fleet broadcast's conclusion was that
ledger integrity DECAYS, so the audit has to become a gate rather than an event.

Modes:

  check-candidate  [--competitive] <target symbol or phrase>...
      Refuse a lever whose ground has already been covered. Greps both ledgers
      for rows naming the target and prints each concrete retry predicate it can
      find.
      exit 0 = clear · exit 2 = BLOCKED by a prior row

  check-entry <file>
      Refuse a NEW REJECT-class ledger entry that is unfalsifiable, i.e. that
      records neither a same-invocation A/A bootstrap median CI nor a counted
      mechanism. This is the VOID-NONULL class that is 100 of this repo's 107
      void rows, and the whole point is to make writing another one impossible.
      Also refuse a NEW KEEP-class entry without both the executing binary's
      self-reported 64-hex SHA-256 and that same null-control contract. Any
      timing verdict using CV as a gate is refused, as is any verdict without a
      concrete retry predicate. Every KEEP must declare exactly one claim class:
      COMPETITIVE requires a numeric FrankenRedis/Redis ratio from a live Redis
      arm in the same invocation; SELF-SPEEDUP is maintenance and must explicitly
      say it is not campaign output.
      exit 0 = admissible · exit 3 = bad REJECT · exit 4 = bad KEEP
      exit 8 = missing, contradictory, or unsupported KEEP claim class
      exit 5 = incomplete timing contract · exit 6 = CV-gated verdict
      exit 7 = missing retry predicate
      exit 9 = verdict heading outside a configured ledger schema
      exit 10 = entry contradicts a standing law it does not engage

  check-staged
      Inspect every added OR modified verdict entry in every repository ledger
      path and supported heading schema. This is the pre-commit mode.

  install-hook
      Install or refresh this repository's owned ledger-gate plugin in the
      chain-runner pre-commit directory. Refuses unrelated existing plugins.

  self-test
      Exercise the null-CI, counted-mechanism, binary-self-report, never-CV,
      claim-class, and per-ledger-path predicates without writing repository
      state.

The pre-commit hook delegates to:

    scripts/perf_candidate_preflight.py check-staged
"""
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LEDGER_SCHEMAS = {
    "docs/NEGATIVE_EVIDENCE.md": ("level-2", "level-3-verdict"),
    "docs/perf_negative_evidence_ledger.md": ("level-2",),
}
LEDGERS = [ROOT / relative for relative in LEDGER_SCHEMAS]

LEVEL_2_HDR = re.compile(r"^## (?!#)(.+)$", re.MULTILINE)
LEVEL_3_HDR = re.compile(r"^### (?!#)(.+)$", re.MULTILINE)
HUNK_RE = re.compile(
    r"^@@ -\d+(?:,\d+)? \+(?P<start>\d+)(?:,(?P<count>\d+))? @@",
    re.MULTILINE,
)
REJECT_RE = re.compile(
    r"\b(REJECT|REJECTED|NEGATIVE|NO[- ]SHIP|UNDECIDABLE|DECLINED|INVALID"
    r"|REVERT|REVERTED|NOT WORTH|ABANDON)\b", re.IGNORECASE)
KEEP_RE = re.compile(r"\b(KEEP|SHIPPED|LANDED|WIN|PROMOTED)\b", re.IGNORECASE)
NUMBER = r"[-+]?(?:\d+(?:\.\d+)?|\.\d+)"
NULL_MEDIAN_CI_RE = re.compile(
    rf"\bA/A(?:\s+null)?\b.{{0,500}}?\bmedian\b[^0-9+-]{{0,40}}"
    rf"(?P<median>{NUMBER}).{{0,500}}?\bbootstrap(?:ped)?\b.{{0,80}}?"
    rf"\b95%\s*(?:median\s*)?(?:CI|confidence interval)\b.{{0,80}}?"
    rf"[\[(]\s*(?P<lo>{NUMBER})\s*[,;]\s*(?P<hi>{NUMBER})\s*[\])]",
    re.IGNORECASE,
)
SAME_INVOCATION_RE = re.compile(
    r"\b(?:same[- ]invocation|same (?:top-level )?invocation|"
    r"single (?:top-level )?invocation|one (?:top-level )?invocation|"
    r"within (?:one|the same) invocation)\b",
    re.IGNORECASE,
)
MEDIAN_CI_GATE_RE = re.compile(
    r"(?:\bbootstrap(?:ped)?\b.{0,80}?\bmedian[- ]CI\b.{0,80}?\b"
    r"(?:gate|decision|verdict)\b|\b(?:gate|decision|verdict)\b.{0,80}?"
    r"\bbootstrap(?:ped)?\b.{0,80}?\bmedian[- ]CI\b)",
    re.IGNORECASE,
)
NEVER_CV_RE = re.compile(
    r"(?:\bnever\s+(?:on\s+)?CV\b|\bCV\b.{0,100}?\b"
    r"(?:provenance only|diagnostic only|did not influence|not used|"
    r"never influenced|not a gate)\b|\b(?:not|never)\b.{0,60}?\bCV\b"
    r".{0,60}?\b(?:gate|decision|verdict)\b)",
    re.IGNORECASE,
)
CV_TOKEN_RE = re.compile(r"\bCV\b|coefficient of variation", re.IGNORECASE)
CV_DECISION_RE = re.compile(
    r"\b(?:gate|gated|threshold|cutoff|decision|verdict|reject(?:ed|ion)?|"
    r"keep|decisive)\b",
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
NEGATED_NULL_RE = re.compile(
    r"(?:\b(?:no|without|missing|lacks?|lacking)\b.{0,35}?\bA/A\b|"
    r"\bA/A\b.{0,35}?\b(?:missing|not recorded|unrecorded|unavailable|"
    r"not measured)\b)",
    re.IGNORECASE,
)
NEGATED_COUNT_RE = re.compile(
    r"\b(?:not recorded|unrecorded|unavailable|not measured|measurement missing|"
    r"count missing|without (?:a )?(?:count|measurement))\b",
    re.IGNORECASE,
)
BINARY_SHA_RE = re.compile(
    r"\b(?:ELF|binary|executable|server(?:\s+(?:ELF|binary|executable))?)\b"
    r".{0,100}?\bsha(?:-?256)?\b[^0-9a-f]{0,20}"
    r"(?P<sha>[0-9a-f]{64})\b",
    re.IGNORECASE,
)
SELF_REPORT_RE = re.compile(
    r"\b(?:self[- ]report(?:ed|s|ing)?|benchmark(?:ed|ing)?\s+"
    r"(?:ELF|binary|executable)\s+(?:report(?:ed|s)|emit(?:ted|s))|"
    r"bench_elf_sha256)\b",
    re.IGNORECASE,
)
RETRY_MARK_RE = re.compile(
    r"\b(?:retry predicates?|retry condition|revisit only|do not retry unless)\b",
    re.IGNORECASE,
)
RETRY_CONDITION_RE = re.compile(
    r"(?:\b(?:if|only if|when|unless|until|after|changes?|lands?|exposes?|"
    r"shows?|exceeds?|falls?|clears?|reaches?)\b|[<>]=?|==)",
    re.IGNORECASE,
)
DATED_HEADING_RE = re.compile(r"^\d{4}-\d{2}-\d{2}\b")


def ledger_relative(path):
    """Return a repository-relative ledger path, or None for free-form input."""
    if path is None:
        return None
    candidate = Path(path)
    if candidate.is_absolute():
        try:
            candidate = candidate.resolve().relative_to(ROOT.resolve())
        except ValueError:
            return None
    return candidate.as_posix()


def entry_heading_matches(text, path=None):
    """Return supported verdict-entry headings in source order.

    The short ledger historically contains both level-2 entries and
    verdict-bearing level-3 entries. The canonical long ledger uses level 2;
    its level-3 headings are evidence subsections and must stay attached to
    their parent. Free-form `check-entry` input accepts both schemas.
    """
    relative = ledger_relative(path)
    matches = [
        (match.start(), match.end(), match.group(1))
        for match in LEVEL_2_HDR.finditer(text)
    ]
    allow_level_3 = relative in (None, "docs/NEGATIVE_EVIDENCE.md")
    if allow_level_3:
        for match in LEVEL_3_HDR.finditer(text):
            title = match.group(1)
            if (
                DATED_HEADING_RE.search(title)
                or REJECT_RE.search(title)
                or KEEP_RE.search(title)
            ):
                matches.append((match.start(), match.end(), title))
    return sorted(matches)


def normalised_entries(path):
    """Yield (title, whitespace-normalised body, line). Normalisation is not
    optional: both ledgers hard-wrap at ~100 columns, so a raw-text grep scores a
    wrapped `Null\\nmedian` as *no null recorded* — the exact error this gate
    exists to prevent."""
    if not path.exists():
        return
    text = path.read_text(encoding="utf-8", errors="replace")
    pos = entry_heading_matches(text, path)
    for i, (start, _heading_end, title) in enumerate(pos):
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


RATIO_RE = re.compile(r"\b\d+\.\d+\s*x\b", re.IGNORECASE)

# STANDING LAWS that live in docs/NEGATIVE_EVIDENCE.md while rows are appended to
# docs/perf_negative_evidence_ledger.md. Each is a measured REJECT whose conclusion
# generalises, and each has already been re-litigated by someone who could not see it
# from the file they were writing in.
#
# A law fires only when the entry TRIGGERS it and does not ENGAGE it. Engagement is
# deliberately generous -- these refuse ignorance of a result, not a phrasing, so a row
# arguing "this law does not apply because X" passes by naming it.
#
# DELETION CONDITION for the whole table: delete when the two ledgers are merged, or
# when docs/perf_negative_evidence_ledger.md carries the standing laws itself.
STANDING_LAWS = (
    (
        "RESTORE isolation",
        # OBSERVED: b1o02 closed 2026-08-08 (NEGATIVE_EVIDENCE.md); frankenredis-33832
        # filed EIGHT DAYS later on the isolation framing; three commits built on it.
        re.compile(r"\bRESTORE\b", re.IGNORECASE),
        re.compile(
            r"break-?even|reads?\s*/\s*RESTORE|RESTORE\s*\+\s*read|HGETALL|"
            r"b1o02|hash_restore_read_premise|isolation",
            re.IGNORECASE,
        ),
        "RESTORE-in-isolation flatters redis: fr decodes eagerly, redis attaches the\n"
        "listpack shallowly and walks it on EVERY read. The break-even is well under one\n"
        "read per restore, so an isolation ratio is not a deficit. Run\n"
        "scripts/hash_restore_read_premise_run.sh and quote the break-even.",
    ),
    (
        "medium-zset threshold",
        # OBSERVED: NEGATIVE_EVIDENCE.md:22581 -- the 2048 threshold is a genuine
        # optimization, and the "incremental ZADD O(n^2)" reading is not a lever.
        # `zsets?` because `\bzset\b` does not match the plural, which is how the
        # rows actually read ("medium zsets"). Caught by the unit cases below it.
        re.compile(r"\bzsets?\b.*\b(threshold|skiplist|btree|b-tree|tree)\b|"
                   r"\b(skiplist|btree|b-tree)\b.*\bzsets?\b",
                   re.IGNORECASE | re.DOTALL),
        re.compile(r"2048|Compact\(Vec\)|memmove|22581|NOT a lever|constant factor",
                   re.IGNORECASE),
        "Compact(Vec) beats BTreeMap for BOTH build and read below n=2048 -- the\n"
        "O(n^2)-looking Vec::insert is a hardware memmove that wins on constant factors.\n"
        "Lowering the threshold or moving medium zsets to a tree regresses both\n"
        "dimensions (NEGATIVE_EVIDENCE.md:22581).",
    ),
    (
        "per-element buffer pooling",
        # OBSERVED: NEGATIVE_EVIDENCE.md:26372 -- pool the container, not its elements.
        re.compile(r"pool(ing|ed)?\b.*\b(buffer|alloc|element)|"
                   r"\b(buffer|element)\b.*\bpool(ing|ed)?\b",
                   re.IGNORECASE | re.DOTALL),
        re.compile(r"allocator fast path|mimalloc|pool the container|26372",
                   re.IGNORECASE),
        "Pool the CONTAINER, not its elements. A recycling lever pays only when it removes\n"
        "an allocation without adding a per-element pass; once the bookkeeping is\n"
        "per-element, mimalloc's fast path beats it. Show the element allocation is NOT\n"
        "already on an allocator fast path (NEGATIVE_EVIDENCE.md:26372).",
    ),
)


def standing_laws_documented():
    """Every gated law must be listed in the ledger people actually append to.

    The gate alone is not enough: it fires at COMMIT time, after the row is written
    and the work is done. The point of the ledger section is that the law is visible
    BEFORE the lever is chosen. Checking them against each other means neither a law
    added to the gate nor a row removed from the doc can drift out of sync silently —
    which is exactly how the RESTORE law came to be invisible from this file for
    eight days.

    Returns the names that are gated but undocumented.
    """
    ledger = ROOT / "docs/perf_negative_evidence_ledger.md"
    try:
        text = ledger.read_text(encoding="utf-8")
    except OSError:
        return [name for name, _, _, _ in STANDING_LAWS]
    section = text.split("## Standing laws", 1)
    if len(section) == 1:
        return [name for name, _, _, _ in STANDING_LAWS]
    body = section[1].split("\n## ", 1)[0]
    return [name for name, _, _, _ in STANDING_LAWS if name not in body]


def violated_standing_laws(title, body):
    """Standing laws this entry triggers without engaging.

    Searches the HEADING as well as the body. Caught by testing rather than
    reasoning: a row headed "pooling the per-element binding buffers did not pay"
    whose body says only "recycling the buffer allocations" never triggered, because
    the word that names the lever lives in the heading. Ledger headings carry the
    claim at least as often as the prose does.

    A law only fires on an entry that also quotes a ratio, i.e. one making a
    performance claim about the surface, not merely mentioning it in passing.
    """
    text = f"{title}\n{body}"
    if not RATIO_RE.search(text):
        return []
    return [
        (name, message)
        for name, trigger, engagement, message in STANDING_LAWS
        if trigger.search(text) and not engagement.search(text)
    ]


def concrete_retry(body):
    """Require an actionable condition, not a bare `Retry predicate` label."""
    excerpt = retry_excerpt(body)
    if excerpt is None:
        return False
    marker = RETRY_MARK_RE.search(excerpt)
    if marker is None:
        return False
    condition = excerpt[marker.end():].strip(" :.—-*")
    return len(condition) >= 20 and RETRY_CONDITION_RE.search(condition) is not None


def check_candidate(terms, competitive=False):
    """Report prior ledger art for a proposed candidate.

    `competitive=True` means the proposal is a vs-incumbent AUTHENTICATION: a
    measurement of this surface against the live legacy incumbent. Only rows that
    themselves measured the incumbent can settle that question, so only those
    block.

    Why the distinction exists (2026-07-28). Plain substring prior art conflates
    two different questions. "I made SRANDMEMBER's dedup 2.1x faster" is a
    SELF-SPEEDUP row; it says nothing about whether our SRANDMEMBER beats Redis's.
    Before this split, ten candidate authentications were BLOCKED by 30-89 rows
    apiece, every one of them a self-speedup — the gate was refusing exactly the
    measurement that would have caught an frankensearch-class miss, where a 8.7x
    deficit hid behind ~90 commits of unmeasured gates. A self-speedup row is
    context for an authentication. It is not an answer to it.
    """
    if not terms:
        print("usage: check-candidate [--competitive] <symbol or phrase>...", file=sys.stderr)
        return 64
    blocking, context = [], []
    for path in LEDGERS:
        for title, body, line in normalised_entries(path):
            for term in terms:
                if term.lower() in body.lower():
                    hit = (path.name, line, title.strip(), term, retry_excerpt(body))
                    # In competitive mode a row settles the question only if it
                    # actually measured the incumbent side-by-side.
                    if competitive and not incumbent_measured(body):
                        context.append(hit)
                    else:
                        blocking.append(hit)
                    break

    def render(hits, limit=12):
        for name, line, title, term, retry in hits[:limit]:
            print(f"  {name}:{line}  (matched {term!r})")
            print(f"    {title[:150]}")
            print(f"    retry: {retry[:500] if retry else '(none recorded)'}")

    if not blocking:
        if context:
            print(f"CLEAR for a vs-incumbent authentication of {terms}.")
            print(f"{len(context)} prior row(s) name this surface, but NONE measured the")
            print("live incumbent — they are self-speedup or mechanism rows. That is")
            print("context for your run, not an answer to it. Cite the closest one.\n")
            render(context, limit=6)
        else:
            print(f"CLEAR: no ledger row names {terms}")
        return 0
    print(f"BLOCKED: {len(blocking)} prior ledger row(s) already cover this ground.\n")
    render(blocking)
    if context:
        print(f"\n  (plus {len(context)} non-incumbent row(s) naming the same surface)")
    print("\nRead those rows before proceeding. If one is VOID (no A/A null and no")
    print("counted mechanism), say so explicitly in your new entry and cite it —")
    print("re-running a void row is legitimate; silently re-deriving it is not.")
    print("")
    print("A BLOCK MEANS SOMEONE HAS BEEN HERE. IT DOES NOT MEAN THEY MEASURED YOUR")
    print("LEVER. These rows matched your target as a STRING; whether any of them")
    print("measured the same QUANTITY is a judgement only you can make, and getting")
    print("it wrong reads exactly like diligence. Before treating a row as an answer,")
    print("check it measured your lever and not a neighbouring one:")
    print("  * a cascade-BYPASS gap (candidate vs the generic path) is not a")
    print("    front-CLASSIFICATION prize (recognised before the chain is entered).")
    print("    frankenredis-nkvkp has four routes the gap rejected — PERSIST at")
    print("    -132/op, LSET at 1.08x — where front-classification then gave up")
    print("    3,326 and 4,177 instr/op. Both numbers are correct; they are")
    print("    answers to different questions.")
    print("  * an allocation or instruction count is not a throughput row, and a")
    print("    row for one shape is not a row for another (frankenredis-1t8c5:")
    print("    run-to-run spread is a property of the SHAPE, 0.09% on hget and")
    print("    1.01% on sinter_2, so a tolerance quoted from one does not carry).")
    return 2


def added_entry_blocks(text, path=None):
    """Yield each newly added heading with only its own added body."""
    matches = entry_heading_matches(text, path)
    for index, (start, heading_end, title) in enumerate(matches):
        end = matches[index + 1][0] if index + 1 < len(matches) else len(text)
        yield title, " ".join(text[heading_end:end].split())


def measured_null(body):
    """Require a same-invocation A/A median within 2% plus its bootstrap CI.

    Merely writing `no A/A null control` must not satisfy the gate. Evidence is
    bound to its A/A label. The median is the gross-bias guard; the confidence
    interval is reported telemetry and need not straddle 1.0 because greater
    precision can put a sound null wholly on one side of 1.0.
    """
    if not SAME_INVOCATION_RE.search(body):
        return False
    for match in NULL_MEDIAN_CI_RE.finditer(body):
        prefix = body[max(0, match.start() - 80):match.start()]
        if NEGATED_NULL_RE.search(prefix):
            continue
        null_ci_span = body[match.end("median"):match.start("lo")]
        if re.search(r"\b(?:A/B|candidate(?:/control)?|effect)\b", null_ci_span,
                     re.IGNORECASE):
            continue
        median = float(match.group("median"))
        lo = float(match.group("lo"))
        hi = float(match.group("hi"))
        if 0.98 <= median <= 1.02 and lo <= hi:
            return True
    return False


def counted_mechanism(body):
    """Require a mechanism name and a count (or an explicit unchanged result)
    in the same clause."""
    fragments = re.split(r"(?<=[.!?;])\s+|\s+\|\s+", body)
    for fragment in fragments:
        if NEGATED_COUNT_RE.search(fragment):
            continue
        if MECHANISM_RE.search(fragment) and COUNT_VALUE_RE.search(fragment):
            return True
    return False


REDIS_RE = re.compile(
    r"\b(?:(?:vendored|legacy)\s+)?redis(?:\s+7\.2\.4|-server|\s+server)?\b",
    re.IGNORECASE,
)
INCUMBENT_RATIO_RE = re.compile(
    rf"\b(?:FrankenRedis|candidate|fr)\s*(?:/|÷|vs\.?|against)\s*"
    rf"(?:(?:vendored|legacy)\s+)?Redis(?:\s+7\.2\.4)?\b"
    rf".{{0,160}}?(?P<ratio>{NUMBER})\s*[x×]",
    re.IGNORECASE,
)
CLAIM_CLASS_RE = re.compile(
    r"\bclaim\s+class\b\s*[*_`]*\s*[:=]\s*[*_`]*\s*"
    r"(?P<class>COMPETITIVE|SELF[- ]SPEEDUP)\b",
    re.IGNORECASE,
)
CAMPAIGN_OUTPUT_RE = re.compile(
    r"\bcampaign\s+output\b\s*[*_`]*\s*[:=]\s*[*_`]*\s*"
    r"(?P<answer>yes|no)\b",
    re.IGNORECASE,
)
LIVE_ARM_RE = re.compile(
    r"\b(?:arm|process|server|ran|runs|running|contained|included|"
    r"side[- ]by[- ]side)\b",
    re.IGNORECASE,
)
SELF_SPEEDUP_HEADING_RE = re.compile(r"\bSELF[- ]SPEEDUP\b", re.IGNORECASE)


def claim_class(body):
    """Return the one explicit KEEP claim class, or None if ambiguous/missing."""
    classes = {
        match.group("class").upper().replace(" ", "-")
        for match in CLAIM_CLASS_RE.finditer(body)
    }
    return classes.pop() if len(classes) == 1 else None


def campaign_output(body):
    """Return the one explicit campaign-output answer, or None if contradictory."""
    answers = {
        match.group("answer").lower()
        for match in CAMPAIGN_OUTPUT_RE.finditer(body)
    }
    return answers.pop() if len(answers) == 1 else None


def incumbent_same_invocation(body):
    """Bind a named Redis process/arm to the invocation, not merely to prose."""
    for match in REDIS_RE.finditer(body):
        context = body[max(0, match.start() - 260):match.end() + 260]
        if SAME_INVOCATION_RE.search(context) and LIVE_ARM_RE.search(context):
            return True
    return False


def incumbent_measured(body):
    """Require a numeric ratio and live Redis arm in the same invocation.

    Policy 2 (2026-07-27): a self-speedup — our own code before vs after — is
    maintenance. Only a ratio against the actual legacy incumbent, produced by a
    harness that runs the incumbent arm side-by-side in the same invocation,
    counts as campaign output.
    """
    return INCUMBENT_RATIO_RE.search(body) is not None and incumbent_same_invocation(body)


def self_reported_binary_sha(body):
    """Require the benchmarked executable to report its own full SHA-256.

    A source-tree hash, commit hash, or a hash calculated later by prose does not
    satisfy the harness contract.
    """
    for match in BINARY_SHA_RE.finditer(body):
        context = body[max(0, match.start() - 180):match.end()]
        if SELF_REPORT_RE.search(context):
            return True
    assignment = re.search(
        r"\bbench_elf_sha256\s*[=:]\s*[`*]*[0-9a-f]{64}\b",
        body,
        re.IGNORECASE,
    )
    return assignment is not None


def median_ci_gate(body):
    """Require an explicit statement that the verdict gate is median-CI."""
    return MEDIAN_CI_GATE_RE.search(body) is not None


def never_cv_asserted(body):
    """Require an explicit statement that CV is provenance, not a gate."""
    return NEVER_CV_RE.search(body) is not None


def cv_used_as_gate(body):
    """Detect a positive CV decision rule while allowing explicit negations."""
    fragments = re.split(r"(?<=[.!?;])\s+|\s+\|\s+", body)
    for fragment in fragments:
        if not CV_TOKEN_RE.search(fragment):
            continue
        if NEVER_CV_RE.search(fragment):
            continue
        if CV_DECISION_RE.search(fragment):
            return True
    return False


def check_entry_blocks(blocks):
    """Check already-parsed ledger blocks and return the documented exit code."""
    blocks = list(blocks)
    claim_errors = []
    rejects = []
    keeps_without_sha = []
    timing_contract = []
    cv_gated = []
    missing_retry = []
    restore_isolation = []
    for title, body in blocks:
        is_reject = REJECT_RE.search(title) is not None
        is_keep = KEEP_RE.search(title) is not None
        has_null = measured_null(body)
        has_mechanism = counted_mechanism(body)

        if is_reject and not (has_null or has_mechanism):
            rejects.append(title)
        if is_keep and not self_reported_binary_sha(body):
            keeps_without_sha.append(title)
        if is_keep and not has_null:
            timing_contract.append((title, "missing same-invocation A/A bootstrap median CI"))
        if (is_keep or (is_reject and has_null)) and not median_ci_gate(body):
            timing_contract.append((title, "missing explicit bootstrap median-CI gate"))
        if (is_keep or (is_reject and has_null)) and not never_cv_asserted(body):
            timing_contract.append((title, "missing explicit never-CV decision statement"))
        if (is_keep or is_reject) and cv_used_as_gate(body):
            cv_gated.append(title)
        if (is_keep or is_reject) and not concrete_retry(body):
            missing_retry.append(title)
        if is_keep or is_reject:
            for name, message in violated_standing_laws(title, body):
                restore_isolation.append((title, name, message))

        if is_keep:
            classification = claim_class(body)
            output = campaign_output(body)
            if classification is None:
                claim_errors.append((
                    title,
                    "declare exactly one `Claim class: COMPETITIVE|SELF-SPEEDUP`",
                ))
            elif classification == "COMPETITIVE":
                if output != "yes":
                    claim_errors.append((
                        title,
                        "COMPETITIVE requires `Campaign output: yes`",
                    ))
                if not incumbent_measured(body):
                    claim_errors.append((
                        title,
                        (
                            "COMPETITIVE requires a numeric FrankenRedis/Redis ratio "
                            "and a named live Redis arm in the same invocation"
                        ),
                    ))
                if SELF_SPEEDUP_HEADING_RE.search(title):
                    claim_errors.append((
                        title,
                        "COMPETITIVE contradicts a SELF-SPEEDUP heading label",
                    ))
            elif classification == "SELF-SPEEDUP":
                if output != "no":
                    claim_errors.append((
                        title,
                        "SELF-SPEEDUP requires `Campaign output: no`",
                    ))
                if not SELF_SPEEDUP_HEADING_RE.search(title):
                    claim_errors.append((
                        title,
                        "SELF-SPEEDUP must be visible in the entry heading",
                    ))

    if claim_errors:
        print("REJECTED (Policy 2): invalid KEEP claim classification.")
        print("A self-speedup is maintenance. Only a measured ratio against a live")
        print("legacy Redis arm in the same invocation is campaign output.\n")
        for title, reason in claim_errors:
            print(f"  {title[:130]}: {reason}")
        return 8

    if rejects:
        print("REJECTED: this REJECT-class entry records NEITHER an A/A null control")
        print("NOR a counted mechanism, so nobody can tell the lever from the harness.")
        print("That is the VOID-NONULL class — 100 of this repo's 107 void rows.\n")
        for title in rejects:
            print(f"  offending heading: {title[:150]}")
        print("\nAdd ONE of:")
        print("  * an A/A null measured in the same invocation, with a bootstrap")
        print("    median within 2% of 1.0 plus a reported bootstrap 95% median CI; or")
        print("  * a COUNTED mechanism showing no work was removed — instructions,")
        print("    cycles, syscalls, allocations, faults, or an exact call count.")
        print("A near-1.0 wall-clock ratio on its own is not evidence of anything.")
        return 3

    if keeps_without_sha:
        print("REJECTED: this KEEP-class entry has no self-reported executing-binary SHA-256.")
        for title in keeps_without_sha:
            print(f"  offending heading: {title[:150]}")
        print("\nRecord the full 64-hex SHA-256 emitted by the benchmarked ELF itself.")
        return 4

    if cv_gated:
        print("REJECTED: CV was used as a verdict gate; CV is provenance only.")
        for title in cv_gated:
            print(f"  offending heading: {title[:150]}")
        return 6

    if timing_contract:
        print("REJECTED: incomplete Meta-Lever 2 timing contract.")
        for title, reason in timing_contract:
            print(f"  {title[:130]}: {reason}")
        print("\nTiming verdicts require one invocation containing A/A and A/B,")
        print("a bootstrap 95% median-CI gate, and an explicit never-CV statement.")
        return 5

    if missing_retry:
        print("REJECTED: verdict entry has no concrete retry predicate.")
        for title in missing_retry:
            print(f"  offending heading: {title[:150]}")
        print("\nName the measurable condition that would justify reopening the surface.")
        return 7

    if restore_isolation:
        print("REJECTED: entry contradicts a STANDING LAW it does not engage.")
        for title, name, message in restore_isolation:
            print(f"  [{name}] {title[:120]}")
            for line in message.splitlines():
                print(f"      {line}")
        print("\nThese laws are measured REJECTs in docs/NEGATIVE_EVIDENCE.md, which is a")
        print("DIFFERENT file from the ledger you are appending to -- that split is why")
        print("each has already been re-litigated. Engage the law (naming it is enough) or")
        print("say why it does not apply to your row.")
        return 10

    reviewed = [
        title
        for title, _ in blocks
        if REJECT_RE.search(title) or KEEP_RE.search(title)
    ]
    if reviewed:
        print("OK: all changed verdict entries satisfy the ledger contract")
        for title in reviewed:
            print(f"  - {title[:120]}")
    else:
        print("OK: no changed REJECT- or KEEP-class entry in this change")
    return 0


def check_entry_text(text, path=None):
    """Check one proposed ledger addition and return the documented exit code."""
    # Accept a raw diff: consider only added lines.
    if text.lstrip().startswith(("diff --git", "@@", "+++", "---")):
        text = "\n".join(l[1:] for l in text.splitlines()
                         if l.startswith("+") and not l.startswith("+++"))
    return check_entry_blocks(added_entry_blocks(text, path))


def check_entry(source):
    text = sys.stdin.read() if source == "-" else Path(source).read_text(errors="replace")
    return check_entry_text(text, None if source == "-" else source)


def changed_line_ranges(diff_text):
    """Return inclusive staged-file line ranges touched by a zero-context diff."""
    ranges = []
    for match in HUNK_RE.finditer(diff_text):
        start = int(match.group("start"))
        count = int(match.group("count") or "1")
        if count == 0:
            ranges.append((max(1, start - 1), max(1, start)))
        else:
            ranges.append((start, start + count - 1))
    return ranges


def changed_entry_blocks(path, staged_text, diff_text):
    """Recover complete staged entries for every added or modified hunk."""
    ranges = changed_line_ranges(diff_text)
    matches = entry_heading_matches(staged_text, path)
    blocks = []
    seen = set()
    for index, (start, heading_end, title) in enumerate(matches):
        end = matches[index + 1][0] if index + 1 < len(matches) else len(staged_text)
        start_line = staged_text.count("\n", 0, start) + 1
        end_line = staged_text.count("\n", 0, end)
        if end == len(staged_text) and staged_text and not staged_text.endswith("\n"):
            end_line += 1
        end_line = max(start_line, end_line)
        if not any(lo <= end_line and hi >= start_line for lo, hi in ranges):
            continue
        key = (start_line, title)
        if key in seen:
            continue
        seen.add(key)
        body = " ".join(staged_text[heading_end:end].split())
        blocks.append((f"{ledger_relative(path)}:{start_line}: {title}", body))
    return blocks


def unsupported_verdict_headings(relative, diff_text):
    """Refuse new verdict headings that no configured ledger schema can parse."""
    bad = []
    for raw in diff_text.splitlines():
        if not raw.startswith("+") or raw.startswith("+++"):
            continue
        line = raw[1:]
        match = re.match(r"^(?P<marks>#{2,6})\s+(?P<title>.+)$", line)
        if match is None:
            continue
        title = match.group("title")
        if not (REJECT_RE.search(title) or KEEP_RE.search(title)):
            continue
        level = len(match.group("marks"))
        allowed = level == 2 or (
            relative == "docs/NEGATIVE_EVIDENCE.md" and level == 3
        )
        if not allowed:
            bad.append(line)
    return bad


def git_capture(argv, *, text=True):
    return subprocess.run(
        argv,
        cwd=ROOT,
        capture_output=True,
        text=text,
        check=False,
        timeout=10,
    )


def check_staged():
    """Check complete staged entries across every configured verdict path."""
    blocks = []
    unsupported = []
    for relative in LEDGER_SCHEMAS:
        diff = git_capture(["git", "diff", "--cached", "-U0", "--", relative])
        if diff.returncode != 0:
            print(
                f"perf-ledger preflight: failed to inspect {relative}: "
                f"{diff.stderr.strip()}",
                file=sys.stderr,
            )
            return 2
        if not diff.stdout:
            continue
        unsupported.extend(
            (relative, heading)
            for heading in unsupported_verdict_headings(relative, diff.stdout)
        )
        staged = git_capture(["git", "show", f":{relative}"])
        if staged.returncode != 0:
            print(
                f"perf-ledger preflight: cannot read staged {relative}: "
                f"{staged.stderr.strip()}",
                file=sys.stderr,
            )
            return 2
        blocks.extend(changed_entry_blocks(relative, staged.stdout, diff.stdout))

    if unsupported:
        print("REJECTED: verdict heading is outside the configured ledger schema.")
        for relative, heading in unsupported:
            print(f"  {relative}: {heading[:150]}")
        print("\nUse `##` in either ledger, or the short ledger's historical `###` schema.")
        return 9
    return check_entry_blocks(blocks)


def self_test():
    from contextlib import redirect_stdout
    from io import StringIO

    sha = "a" * 64
    valid_timing = (
        "One top-level invocation interleaved A/A and A/B. "
        "A/A null median 1.000001; bootstrap 95% median CI [0.9998, 1.0002]. "
        "The bootstrap median-CI gate determined the verdict, never CV. "
    )
    valid = (
        valid_timing
        + f"The harness self-reported ELF SHA-256 {sha}. "
        "Retry predicate: reopen only if a fresh profile exposes >=5% self-time."
    )
    competitive = (
        valid
        + " Claim class: COMPETITIVE. Campaign output: yes. "
        "One top-level same invocation ran FrankenRedis and vendored Redis "
        "7.2.4 as live server arms. Candidate/Redis median 1.250x."
    )
    self_speedup = (
        valid
        + " Claim class: SELF-SPEEDUP. Campaign output: no. "
        "This maintenance result is not a competitive claim."
    )
    checks = [
        (measured_null(valid), "valid null contract"),
        (median_ci_gate(valid), "median-CI gate"),
        (never_cv_asserted(valid), "never-CV assertion"),
        (not cv_used_as_gate(valid), "never-CV is not a positive CV gate"),
        (self_reported_binary_sha(valid), "self-reported ELF SHA"),
        (concrete_retry(valid), "concrete retry predicate"),
        (claim_class(competitive) == "COMPETITIVE", "competitive claim class"),
        (campaign_output(competitive) == "yes", "campaign-output yes"),
        (incumbent_measured(competitive), "same-invocation incumbent ratio"),
        (claim_class(self_speedup) == "SELF-SPEEDUP", "self-speedup claim class"),
        (campaign_output(self_speedup) == "no", "campaign-output no"),
        (
            not concrete_retry("Retry predicate: TBD."),
            "bare retry label is not concrete",
        ),
        (
            not measured_null(
                "A/A null median 1.0; bootstrap 95% median CI [0.99, 1.01]."
            ),
            "null without same invocation",
        ),
        (
            measured_null(
                "One invocation. A/A null median 0.997706060; "
                "bootstrap 95% median CI [0.995417921, 0.999456789]."
            ),
            "precise non-straddling null CI",
        ),
        (
            not measured_null(
                "One invocation. A/A null median 1.03; "
                "bootstrap 95% median CI [0.99, 1.05]."
            ),
            "gross-biased null median",
        ),
        (
            not measured_null(
                "One invocation. A/A null median 1.0. Candidate effect used a "
                "bootstrap 95% median CI [0.99, 1.01]."
            ),
            "candidate CI cannot authenticate the null",
        ),
        (
            counted_mechanism("instructions:u 1001 -> 1001, unchanged"),
            "counted mechanism",
        ),
        (
            not self_reported_binary_sha(
                f"Source SHA-256 {sha}; benchmark provenance unavailable."
            ),
            "source hash is not binary self-report",
        ),
        (
            cv_used_as_gate("CV < 5% was the rejection threshold and verdict gate."),
            "positive CV gate",
        ),
    ]

    def entry_status(title, body, marks="##"):
        with redirect_stdout(StringIO()):
            return check_entry_text(f"{marks} {title}\n{body}\n")

    def staged_status(relative, heading, body):
        staged = f"# Ledger\n\n{heading}\n{body}\n"
        line_count = len(staged.splitlines())
        diff = f"@@ -0,0 +1,{line_count} @@\n" + "\n".join(
            f"+{line}" for line in staged.splitlines()
        )
        with redirect_stdout(StringIO()):
            return check_entry_blocks(
                changed_entry_blocks(relative, staged, diff)
            )

    def modified_status(relative, heading, body):
        staged = f"# Ledger\n\n{heading}\n{body}\n"
        diff = "@@ -4 +4 @@\n-old verdict body\n+changed verdict body"
        with redirect_stdout(StringIO()):
            return check_entry_blocks(
                changed_entry_blocks(relative, staged, diff)
            )

    checks.extend([
        (
            entry_status(
                "2099-01-01 cod: REJECT — weak near-one row",
                "A/B ratio 1.001. A/A null present. "
                "Retry predicate: reopen only if a fresh profile exposes "
                ">=5% self-time.",
            ) == 3,
            "end-to-end VOID-NONULL refusal",
        ),
        (
            entry_status(
                "2099-01-01 cod: REJECT — biased null",
                "One top-level invocation interleaved A/A and A/B. "
                "A/A null median 1.03; bootstrap 95% median CI [0.99, 1.05]. "
                "The bootstrap median-CI gate determined the verdict, never CV. "
                "Retry predicate: reopen only if a fresh profile exposes "
                ">=5% self-time.",
            ) == 3,
            "end-to-end gross-biased-null refusal",
        ),
        (
            entry_status(
                "2099-01-01 cod: KEEP — labelled hash only",
                f"{valid_timing} Binary SHA-256 {sha}. "
                "Retry predicate: reopen only if a fresh profile exposes "
                ">=5% self-time. Claim class: COMPETITIVE. "
                "Campaign output: yes. One top-level same invocation ran "
                "FrankenRedis and vendored Redis 7.2.4 as live server arms. "
                "Candidate/Redis median 1.250x.",
            ) == 4,
            "end-to-end non-self-reported SHA refusal",
        ),
        (
            entry_status(
                "2099-01-01 cod: REJECT — CV gate",
                f"{valid} CV < 5% was the verdict threshold.",
            ) == 6,
            "end-to-end CV-gate refusal",
        ),
        (
            entry_status(
                "2099-01-01 cod: REJECT — counted closure",
                "instructions:u 1001 -> 1001, unchanged. "
                "Retry predicate: reopen only if a fresh profile shows "
                "instructions fall by >=2%.",
            ) == 0,
            "end-to-end counted-mechanism acceptance",
        ),
        (
            entry_status(
                "2099-01-01 cod: COMPETITIVE KEEP — full contract",
                competitive,
            ) == 0,
            "end-to-end competitive KEEP acceptance",
        ),
        (
            entry_status(
                "2099-01-01 cod: KEEP — missing claim class",
                valid,
            ) == 8,
            "KEEP without claim class refusal",
        ),
        (
            entry_status(
                "2099-01-01 cod: COMPETITIVE KEEP — no incumbent ratio",
                valid
                + " Claim class: COMPETITIVE. Campaign output: yes. "
                "One top-level same invocation included a vendored Redis "
                "7.2.4 server arm.",
            ) == 8,
            "competitive KEEP without numeric incumbent ratio refusal",
        ),
        (
            entry_status(
                "2099-01-01 cod: KEEP — hidden maintenance label",
                self_speedup,
            ) == 8,
            "self-speedup missing heading label refusal",
        ),
        (
            entry_status(
                "2099-01-01 cod: SELF-SPEEDUP KEEP — contradictory output",
                self_speedup.replace("Campaign output: no", "Campaign output: yes"),
            ) == 8,
            "self-speedup campaign-output contradiction refusal",
        ),
        (
            entry_status(
                "2099-01-01 cod: SELF-SPEEDUP KEEP — maintenance",
                self_speedup,
            ) == 0,
            "end-to-end self-speedup KEEP acceptance",
        ),
    ])

    invalid_provenance = competitive.replace(
        f"The harness self-reported ELF SHA-256 {sha}.",
        f"Binary SHA-256 {sha}.",
    )
    path_boundaries = [
        (
            "docs/NEGATIVE_EVIDENCE.md",
            "## 2099-01-01 cod: COMPETITIVE KEEP — short level 2",
            "short-ledger level-2",
        ),
        (
            "docs/NEGATIVE_EVIDENCE.md",
            "### 2099-01-01 cod: COMPETITIVE KEEP — short level 3",
            "short-ledger level-3",
        ),
        (
            "docs/perf_negative_evidence_ledger.md",
            "## 2099-01-01 cod: COMPETITIVE KEEP — long level 2",
            "long-ledger level-2",
        ),
    ]
    for relative, heading, label in path_boundaries:
        checks.extend([
            (
                staged_status(relative, heading, invalid_provenance) == 4,
                f"{label} fail boundary",
            ),
            (
                staged_status(relative, heading, competitive) == 0,
                f"{label} pass boundary",
            ),
        ])
    for relative, heading, label in (
        path_boundaries[0],
        path_boundaries[2],
    ):
        checks.extend([
            (
                modified_status(relative, heading, invalid_provenance) == 4,
                f"{label} modified-entry fail boundary",
            ),
            (
                modified_status(relative, heading, competitive) == 0,
                f"{label} modified-entry pass boundary",
            ),
        ])
    checks.append((
        bool(unsupported_verdict_headings(
            "docs/perf_negative_evidence_ledger.md",
            "+### 2099-01-01 cod: KEEP — unsupported nested verdict",
        )),
        "long-ledger unsupported level-3 verdict refusal",
    ))
    # (standing laws) Each case is one the implementation FAILED before it was
    # written: `\bzset\b` missed the plural "zsets"; the pooling trigger missed a
    # lever named only in the heading; and the engagement list for pooling once
    # accepted "per-element", the very phrase the law is about. Regexes over prose
    # are exactly the thing that looks right and is not.
    for label, title, body, want in (
        ("zset plural triggers", "REJECTED: a skiplist for medium zsets did not pay",
         "Lowered the threshold and moved medium zsets to a skiplist. Measured 1.02x.", 1),
        ("zset singular triggers", "REJECTED: skiplist for a medium zset",
         "Measured 1.02x.", 1),
        ("zset engaged passes", "REJECTED: a skiplist for medium zsets did not pay",
         "The 2048 threshold is a genuine optimization; re-tested anyway at 1.02x.", 0),
        ("pooling named only in the heading triggers",
         "REJECTED: pooling the per-element binding buffers did not pay",
         "Recycling the buffer allocations measured 1.01x.", 1),
        ("pooling engaged passes",
         "REJECTED: pooling the per-element binding buffers did not pay",
         "mimalloc's fast path already covers it; measured 1.01x.", 0),
        ("RESTORE isolation triggers", "REJECTED: RESTORE decode is 2.81x redis",
         "The lever did not move it.", 1),
        ("RESTORE engaged passes", "REJECTED: RESTORE decode is 2.81x redis in isolation",
         "Break-even 0.373 reads/RESTORE.", 0),
        ("no ratio never fires", "correctness: RESTORE rejects a duplicate field",
         "A skiplist zset pooling buffers, all named, but no ratio quoted.", 0),
        ("unrelated row never fires", "KEEP: front-classify GEOADD",
         "Dispatch share fell to 12.0 pct, 1.43x.", 0),
    ):
        checks.append((
            len(violated_standing_laws(title, body)) == want,
            f"standing law: {label}",
        ))

    checks.append((
        standing_laws_documented() == [],
        "every gated standing law is listed in the ledger's Standing laws section",
    ))

    failed = [name for passed, name in checks if not passed]
    if failed:
        for name in failed:
            print(f"FAIL: {name}", file=sys.stderr)
        return 1
    print(f"OK: {len(checks)} preflight contract checks passed")
    return 0


HOOK_MARKER = "# frankenredis perf-ledger gate; owned by perf_candidate_preflight.py"
HOOK = f"""#!/usr/bin/env python3
{HOOK_MARKER}
import subprocess
import sys
from pathlib import Path

root = Path(
    subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        capture_output=True,
        text=True,
        check=True,
        timeout=10,
    ).stdout.strip()
)
guard = root / "scripts" / "perf_candidate_preflight.py"
result = subprocess.run(
    [sys.executable, str(guard), "check-staged"],
    cwd=root,
    check=False,
    timeout=10,
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
        current = hook.read_text(errors="replace")
        owned = (
            HOOK_MARKER in current
            or (
                "perf_candidate_preflight.py" in current
                and "perf-ledger pre-commit" in current
            )
        )
        if not owned:
            print(f"refusing to overwrite unrelated hook plugin: {hook}", file=sys.stderr)
            return 1
        if current == HOOK:
            print(f"already installed {hook}")
            return 0
        hook.write_text(HOOK)
        hook.chmod(0o755)
        print(f"refreshed {hook}")
        return 0
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
        terms = argv[2:]
        competitive = "--competitive" in terms
        return check_candidate([t for t in terms if t != "--competitive"], competitive)
    if mode == "check-entry":
        return check_entry(argv[2] if len(argv) > 2 else "-")
    if mode == "check-staged":
        return check_staged()
    if mode == "install-hook":
        return install_hook()
    if mode == "self-test":
        return self_test()
    print(f"unknown mode {mode!r}", file=sys.stderr)
    return 64


if __name__ == "__main__":
    sys.exit(main(sys.argv))
