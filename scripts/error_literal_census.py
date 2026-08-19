#!/usr/bin/env python3
"""error_literal_census.py — every error string the incumbent can send, checked against fr.

WHY THIS IS A FILE AND NOT A GREP. This census has been run by hand four times across this
codebase and has produced real fixes each time (frankenredis-keymiss-oqhbi, the `new` keyspace
event, the LCS length guard, the mustObeyClient family). It has also UNDER-REPORTED twice, both
times the same way, and that is what makes it worth committing rather than retyping:

  * `"new"` hid from a 44-event hand census because the bare name matches unrelated literals
    anywhere in the tree -- a bench command list and a test expectation both contain it.
  * `"SUBSCRIBE isn't allowed for a DENY BLOCKING client"` hid because it is a SUBSTRING of
    `"SSUBSCRIBE isn't allowed for a DENY BLOCKING client"`, which fr does have. A plain
    `grep -F` reports the shorter one present while it is absent.

Both are the same bug: a substring match cannot distinguish "this literal is in fr" from "some
LONGER literal containing it is in fr". This file matches with the surrounding quote, which makes
the comparison exact -- a Rust string literal `"X"` contains `"SUBSCRIBE isn't..."` only when X
IS that string.

RUNS UNDER A BUILD FREEZE: no server, no cargo, no disk writes.

THE HONEST LIMIT. Finding a literal in fr proves the STRING exists, not that it is emitted on the
same condition -- the SUBSCRIBE family is three guards with three different conditions and
identical-looking messages. A hit here narrows the search; the upstream site and fr's call path
decide it. Every ACCEPTED_ABSENT entry below carries the reasoning that decision was made on.
"""
import glob
import io
import os
import re
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
VENDORED = os.path.join(REPO, "legacy_redis_code", "redis", "src", "*.c")
FR_SOURCES = os.path.join(REPO, "crates", "*", "src", "*.rs")

MIN_LEN = 14  # shorter literals are too generic to attribute; they produce noise, not findings

# Whole FILES whose errors fr cannot send because it does not implement the subsystem. This is a
# scope statement, not an exemption: every literal in them is unreachable by construction, and
# listing them individually in ACCEPTED_ABSENT would bury the reachable ones under dozens of
# entries that all say the same thing.
OUT_OF_SCOPE_FILES = {
    "module.c": "fr has no module system",
    "cluster.c": "fr has no cluster mode",
    "cluster_legacy.c": "fr has no cluster mode",
}

# Literals the incumbent can send that fr deliberately does not, each with the reason it was
# decided. An entry that fr LATER gains fails the gate: a stale exemption is how a list like this
# stops meaning anything.
ACCEPTED_ABSENT = {
    "BY option of SORT denied in Cluster mode.": "cluster-only; fr has no cluster mode",
    "GET option of SORT denied in Cluster mode.": "cluster-only; fr has no cluster mode",
    "SUBSCRIBE isn't allowed for a DENY BLOCKING client":
        "CORRECTLY absent. Upstream guards this with (DENY_BLOCKING && !CLIENT_MULTI) "
        "(pubsub.c:536) -- MULTI is exempted for backward compatibility -- and fr's only "
        "deny-blocking context IS EXEC. From a script the command is CMD_NOSCRIPT in both "
        "tables so the noscript refusal fires first, and fr has no modules. Porting it "
        "without the !CLIENT_MULTI half would REGRESS SUBSCRIBE inside MULTI.",
    "PSUBSCRIBE isn't allowed for a DENY BLOCKING client":
        "same as SUBSCRIBE above; pubsub.c:568 carries the identical !CLIENT_MULTI exemption",
    "There was an error trying to save the ACLs.": "fr does not implement ACL SAVE to file",
    "Error purging dirty pages": "jemalloc-specific MEMORY PURGE path; fr uses mimalloc",
    "TESTFAILED dense/sparse disagree": "DEBUG-only PFSELFTEST path",
    "TESTFAILED sparse encoding not used": "DEBUG-only PFSELFTEST path",
}

# Literals whose absence says nothing because they describe a C allocation failure: Rust aborts
# on allocation failure rather than replying, so there is no site to port them to.
ALLOC_FAILURE = re.compile(r"failed allocating transient memory")


def upstream_literals():
    # C CONCATENATES ADJACENT STRING LITERALS, and upstream wraps long messages across lines:
    #
    #     addReplyError(c, "ACLs rules changed between the moment the transaction was "
    #                      "accumulated and the EXEC call...");
    #
    # Capturing only the first fragment reports the message ABSENT from fr, which spells it as
    # one string. So the whole run of adjacent literals is captured and joined.
    pat = re.compile(
        r'addReplyError(?:Format|Sds|Object)?\s*\(\s*[^,]+,\s*'
        r'((?:"(?:[^"\\]|\\.)*"\s*)+)'
    )
    found = {}
    for path in sorted(glob.glob(VENDORED)):
        if os.path.basename(path) in OUT_OF_SCOPE_FILES:
            continue
        text = io.open(path, encoding="utf-8", errors="replace").read()
        for m in pat.finditer(text):
            frags = re.findall(r'"((?:[^"\\]|\\.)*)"', m.group(1))
            lit = "".join(frags)
            if "%" in lit or len(lit) < MIN_LEN or ALLOC_FAILURE.search(lit):
                continue
            core = re.sub(r"^-[A-Z]+ ", "", lit)   # fr spells the code without the leading '-'
            found.setdefault(core, set()).add(os.path.basename(path))
    return found


def fr_code_text():
    """fr's sources with COMMENT LINES REMOVED, concatenated.

    Two things went wrong before this shape, and both are worth stating because each produced a
    confident wrong answer:

      * Extracting "every Rust string literal" by pairing quotes across a whole file does not
        work. An apostrophe in a comment, or a quote inside a char literal, shifts every
        subsequent pair -- the extractor then reports literals that do not exist and misses ones
        that do. It found 34862 "literals" and could not find one this file was written to check.
      * COMMENTS MUST GO. fr's sources quote upstream C extensively -- these very censuses put
        `addReplyError(caller, "...")` snippets into doc comments -- so searching raw text reports
        a message PRESENT when the only copy is a quotation of the incumbent in a comment
        explaining why fr does something else.

    So: drop comment lines, keep code, and let the caller anchor its search on the opening quote.
    """
    parts = []
    for path in sorted(glob.glob(FR_SOURCES)):
        for line in io.open(path, encoding="utf-8", errors="replace"):
            st = line.lstrip()
            if st.startswith("//"):
                continue
            parts.append(line)
    text = "".join(parts)
    # RUST LINE CONTINUATIONS. fr wraps long messages as
    #
    #     "OOM allow-oom flag is not set on the script, can not run it when used memory > \
    #      'maxmemory'"
    #
    # where `\` + newline + leading whitespace is elided by the compiler. The SOURCE text
    # therefore does not contain the message as a contiguous run, and a raw search reports it
    # absent -- which it did, for a guard this session implemented. Join them first, so the text
    # searched is the string the compiler builds.
    return re.sub(r"\\\n\s*", "", text)


def self_test():
    """Pin the matcher against the two ways this census has actually lied.

    Both predecessors of this file passed their author's eye and reported a confident wrong
    answer, so the claims in the docstring are asserted here rather than trusted.
    """
    # 1. SUBSTRING MASKING. fr contains the SSUBSCRIBE literal and NOT the SUBSCRIBE one; a
    #    plain `in` test reports the shorter present. This is the bug that hid a real absence.
    fr = '            "ERR SSUBSCRIBE isn\'t allowed for a DENY BLOCKING client".to_string(),\n'
    short = "SUBSCRIBE isn't allowed for a DENY BLOCKING client"
    assert short in fr, "precondition: the substring really is there"
    assert ('"' + short) not in fr, "anchored on the quote, the shorter literal must NOT match"
    assert re.search(r'"[A-Z]+ ' + re.escape(short), fr) is None, (
        "and the code-prefix form must not match it either -- 'ERR SSUBSCRIBE' is not "
        "'ERR ' + 'SUBSCRIBE...'"
    )

    # 2. CODE PREFIXES. fr writes the RESP code inside its literal; upstream leaves it to
    #    addReplyError. A message must not read as absent just because fr names its code.
    for code in ("ERR", "NOPERM", "WRONGTYPE"):
        text = '    "%s Some upstream phrase here".to_string()' % code
        assert re.search(r'"[A-Z]+ ' + re.escape("Some upstream phrase here"), text), code

    # 3. COMMENT STRIPPING. fr quotes upstream C in doc comments constantly -- including in the
    #    comments these censuses write -- so a quotation must not count as an implementation.
    lines = ['/// addReplyError(c, "Quoted from the incumbent");\n', 'let x = 1;\n']
    kept = "".join(l for l in lines if not l.lstrip().startswith("//"))
    assert '"Quoted from the incumbent"' not in kept, "comment lines must be dropped"
    assert "let x = 1;" in kept, "code lines must survive"

    # 4. C CONCATENATION. upstream wraps long messages across adjacent literals; capturing only
    #    the first fragment reports the whole message absent.
    frag = 'addReplyError(c, "first half " \n   "second half");'
    m = re.search(r'addReplyError(?:Format|Sds|Object)?\s*\(\s*[^,]+,\s*'
                  r'((?:"(?:[^"\\]|\\.)*"\s*)+)', frag)
    assert m, "the multi-fragment form must match at all"
    joined = "".join(re.findall(r'"((?:[^"\\]|\\.)*)"', m.group(1)))
    assert joined == "first half second half", joined

    # 5. RUST LINE CONTINUATIONS. fr wraps a long message with a trailing `\\` and re-indents
    #    the remainder; the compiler elides both, so the string the SERVER sends is contiguous
    #    while the string in the FILE is not. This one reported a guard absent that this census's
    #    own author had implemented -- the failure mode is a false ABSENCE, i.e. re-doing work.
    wrapped = ('        "OOM allow-oom flag is not set on the script, can not run it when used \\\n'
               '         memory > \'maxmemory\'"\n')
    phrase = "can not run it when used memory > 'maxmemory'"
    assert phrase not in wrapped, "precondition: raw text really does NOT contain the message"
    joined = re.sub(r"\\\n\s*", "", wrapped)
    assert phrase in joined, "after joining continuations it must be found"
    assert re.search(r'"[A-Z]+ ' + re.escape("allow-oom flag is not set"), joined), (
        "and the code-prefix form must still work on the joined text"
    )

    print("PASS error_literal_census self-test: substring masking, code prefixes, comment "
          "stripping, C concatenation, Rust line continuations")
    return 0


def main():
    if "--self-test" in sys.argv:
        return self_test()
    upstream = upstream_literals()
    fr = fr_code_text()
    if len(upstream) < 100:
        print("FAIL — read only %d upstream literals; the vendored source at\n  %s\nwas not "
              "read. Wiring failure, not a parity result." % (len(upstream), VENDORED))
        return 1
    if len(fr) < 100000:
        print("FAIL — read only %d chars of fr code. Same wiring failure, other side." % len(fr))
        return 1

    def present(lit):
        """Anchored on the OPENING QUOTE, which is what makes this exact.

        `"SUBSCRIBE isn't allowed..."` must not be reported present because fr contains
        `"ERR SSUBSCRIBE isn't allowed..."`. Requiring the quote (optionally followed by fr's
        `ERR ` code prefix) means the literal has to START where fr's literal starts.
        """
        if ('"' + lit) in fr:
            return True
        # fr spells the RESP error CODE inside its literal where upstream leaves it to
        # addReplyError -- "ERR ", but also "NOPERM ", "WRONGTYPE ", "NOSCRIPT " and friends.
        # Allowing any leading uppercase code keeps this from reporting a message absent purely
        # because fr names its code explicitly.
        return re.search(r'"[A-Z]+ ' + re.escape(lit), fr) is not None

    missing = sorted(l for l in upstream if not present(l) and l not in ACCEPTED_ABSENT)
    stale = sorted(l for l in ACCEPTED_ABSENT if present(l))

    print("upstream: %d distinct error literals (>= %d chars, no format specifiers)"
          % (len(upstream), MIN_LEN))
    print("fr:       %d chars of code scanned (comments stripped)" % len(fr))

    rc = 0
    if missing:
        print("\nFAIL — %d literal(s) the incumbent can send and fr never does:" % len(missing))
        for l in missing:
            print("    %-64s %s" % (l[:64], ", ".join(sorted(upstream[l]))))
        print()
        print("  THIS IS A BACKLOG, NOT A VERDICT. Each line means only that the exact literal")
        print("  is not in fr's code -- it does NOT mean fr answers nothing, or answers wrongly.")
        print("  Several will be like the SUBSCRIBE pair already in ACCEPTED_ABSENT: correctly")
        print("  absent, where porting them would introduce a regression. Triage one at a time")
        print("  against its upstream site and fr's call path, then either port it or record")
        print("  the reason here. A count going DOWN is progress; the count itself is not a score.")
        rc = 1
    if stale:
        print("\nFAIL — ACCEPTED_ABSENT entries fr now emits (implemented; remove them):")
        for l in stale:
            print("    %s" % l[:70])
        rc = 1
    if rc == 0:
        print("\nPASS — every upstream error literal is present in fr or accounted for "
              "(%d accounted)." % len(ACCEPTED_ABSENT))
        print("A PASS does not prove the CONDITION matches: identical messages can sit behind")
        print("different guards. See the module docstring.")
    return rc


if __name__ == "__main__":
    sys.exit(main())
