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
      records neither a same-invocation A/A bootstrap median CI nor a counted
      mechanism. This is the VOID-NONULL class that is 100 of this repo's 107
      void rows, and the whole point is to make writing another one impossible.
      Also refuse a NEW KEEP-class entry without both the executing binary's
      self-reported 64-hex SHA-256 and that same null-control contract. Any
      timing verdict using CV as a gate is refused, as is any verdict without a
      concrete retry predicate.
      exit 0 = admissible · exit 3 = bad REJECT · exit 4 = bad KEEP provenance
      exit 5 = incomplete timing contract · exit 6 = CV-gated verdict
      exit 7 = missing retry predicate

  install-hook
      Install the check-entry gate into the repository's chain-runner pre-commit
      directory. Refuses to overwrite an existing plugin.

  self-test
      Exercise the null-CI, counted-mechanism, binary-self-report, and never-CV
      predicates without reading or writing repository state.

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


def normalised_entries(path):
    """Yield (title, whitespace-normalised body, line). Normalisation is not
    optional: both ledgers hard-wrap at ~100 columns, so a raw-text grep scores a
    wrapped `Null\\nmedian` as *no null recorded* — the exact error this gate
    exists to prevent."""
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


def concrete_retry(body):
    """Require an actionable condition, not a bare `Retry predicate` label."""
    excerpt = retry_excerpt(body)
    if excerpt is None:
        return False
    marker = RETRY_MARK_RE.search(excerpt)
    assert marker is not None
    condition = excerpt[marker.end():].strip(" :.—-*")
    return len(condition) >= 20 and RETRY_CONDITION_RE.search(condition) is not None


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
    """Require a same-invocation A/A bootstrap median CI that brackets 1.0.

    Merely writing `no A/A null control` must not satisfy the gate. Evidence is
    bound to its A/A label, and a contaminated null whose CI excludes 1.0 is not
    admissible evidence.
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
        lo = float(match.group("lo"))
        hi = float(match.group("hi"))
        if lo <= 1.0 <= hi:
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


def check_entry(source):
    text = sys.stdin.read() if source == "-" else Path(source).read_text(errors="replace")
    # Accept a raw diff: consider only added lines.
    if text.lstrip().startswith(("diff --git", "@@", "+++", "---")):
        text = "\n".join(l[1:] for l in text.splitlines()
                         if l.startswith("+") and not l.startswith("+++"))
    blocks = list(added_entry_blocks(text))
    rejects = []
    keeps_without_sha = []
    timing_contract = []
    cv_gated = []
    missing_retry = []
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

    if rejects:
        print("REJECTED: this REJECT-class entry records NEITHER an A/A null control")
        print("NOR a counted mechanism, so nobody can tell the lever from the harness.")
        print("That is the VOID-NONULL class — 100 of this repo's 107 void rows.\n")
        for title in rejects:
            print(f"  offending heading: {title[:150]}")
        print("\nAdd ONE of:")
        print("  * an A/A null measured in the same invocation, with a bootstrap")
        print("    95% median CI that brackets 1.0; or")
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


def self_test():
    sha = "a" * 64
    valid = (
        "One top-level invocation interleaved A/A and A/B. "
        "A/A null median 1.000001; bootstrap 95% median CI [0.9998, 1.0002]. "
        "The bootstrap median-CI gate determined the verdict, never CV. "
        f"The harness self-reported ELF SHA-256 {sha}. "
        "Retry predicate: reopen only if a fresh profile exposes >=5% self-time."
    )
    checks = [
        (measured_null(valid), "valid null contract"),
        (median_ci_gate(valid), "median-CI gate"),
        (never_cv_asserted(valid), "never-CV assertion"),
        (not cv_used_as_gate(valid), "never-CV is not a positive CV gate"),
        (self_reported_binary_sha(valid), "self-reported ELF SHA"),
        (concrete_retry(valid), "concrete retry predicate"),
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
            not measured_null(
                "One invocation. A/A null median 1.02; "
                "bootstrap 95% median CI [1.01, 1.03]."
            ),
            "contaminated null CI",
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
    failed = [name for passed, name in checks if not passed]
    if failed:
        for name in failed:
            print(f"FAIL: {name}", file=sys.stderr)
        return 1
    print(f"OK: {len(checks)} preflight contract checks passed")
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
    if mode == "self-test":
        return self_test()
    print(f"unknown mode {mode!r}", file=sys.stderr)
    return 64


if __name__ == "__main__":
    sys.exit(main(sys.argv))
