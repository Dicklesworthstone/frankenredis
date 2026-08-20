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
    "There was an error trying to save the ACLs. Please check the server logs for more "
    "information": "fr does not implement ACL SAVE to file",
    "Error purging dirty pages": "jemalloc-specific MEMORY PURGE path; fr uses mimalloc",
    "TESTFAILED dense/sparse disagree": "DEBUG-only PFSELFTEST path",
    "TESTFAILED sparse encoding not used": "DEBUG-only PFSELFTEST path",
    # -- LUA DEBUGGER (LDB). fr accepts SCRIPT DEBUG YES/SYNC/NO and DISCARDS the mode
    #    (fr-command lib.rs, the arm ending "debugging mode won't actually trigger"): there is no
    #    ldb session, no forked debug client, and no redis-cli --ldb protocol. Both literals gate
    #    a session fr never enters. Porting the strings alone would refuse commands on behalf of
    #    a mode that does nothing -- the reply would be faithful and the behaviour behind it still
    #    absent, which is worse than answering nothing, because it advertises a feature.
    "Please use EVAL instead of EVALSHA for debugging":
        "LDB-only; fr implements no Lua debugger session (SCRIPT DEBUG is accepted and dropped)",
    "SCRIPT DEBUG must be called outside a pipeline":
        "LDB-only; same reason. Also needs a notion of pending pipelined input fr does not carry "
        "into the script layer.",
    # -- SCRIPT KILL of a BUSY script. fr never has one: scripts run to completion synchronously,
    #    fr sends no BUSY reply anywhere in the tree, and SCRIPT KILL is unconditionally
    #    "NOTBUSY No scripts in execution right now." Both of these are arms of the UNKILLABLE
    #    branch, reachable only once a script is already busy AND has either written or arrived
    #    from a master. The capability gap is the busy-script timeout, not the literal; when
    #    lua-time-limit is made to bite, this gate fails and both come back.
    "Sorry the script already executed write commands against the dataset. You can either wait "
    "the script termination or kill the server in a hard way using the SHUTDOWN NOSAVE command.":
        "reachable only for a BUSY script; fr runs scripts to completion and never replies BUSY",
    "The busy script was sent by a master instance in the context of replication and cannot be "
    "killed.":
        "same UNKILLABLE branch; fr has no busy-script state to kill",
    # -- NO REPLICA STATE MACHINE. Upstream gives every attached replica a `replstate`
    #    (WAIT_BGSAVE_START -> WAIT_BGSAVE_END -> SEND_BULK -> ONLINE) and the server a
    #    `failover_state`. fr has neither: `ReplicaState` carries offsets, port, ip and ack time
    #    and NO state field, INFO renders `state=online` as a literal with the comment "We only
    #    track registered replicas", and FAILOVER completes synchronously inside the command --
    #    which is why FAILOVER ABORT answers "No failover in progress." unconditionally. A replica
    #    fr knows about is online by construction, and a failover is never in flight to collide
    #    with, so none of these five conditions can arise.
    #
    #    The gap is the state machine, not the strings. Build it and this gate fails on all five
    #    at once, which is the point of naming them individually here.
    "BGSAVE failed, replication can't continue":
        "needs SLAVE_STATE_WAIT_BGSAVE_START; fr tracks no replica sync state",
    "FAILOVER target replica is not online.":
        "fr's replica list holds only registered replicas -- INFO hardcodes state=online -- so a "
        "known replica is online by construction. fr DOES implement the sibling check, "
        "'FAILOVER target HOST and PORT is not a replica.'",
    "FAILOVER already in progress.":
        "fr's FAILOVER is synchronous; there is no pending failover_state to collide with",
    "Can't SYNC while failing over":
        "same: no failover_state, so a SYNC can never arrive during one",
    "REPLICAOF not allowed while failing over.":
        "same: no failover_state",
    # -- NO `REPLCONF rdb-filter-only`. fr does not implement the filtered-RDB replica request at
    #    all (zero occurrences in the tree), so neither its argument parse nor the EOF-capability
    #    precondition it guards has a site to live at. Both return together with that feature.
    "Filtered replica requires EOF capability":
        "fr implements no REPLCONF rdb-filter-only, so SLAVE_REQ_RDB_MASK is never set",
    "Missing rdb-filter-only values":
        "same: the argument this parses is never accepted",
    # -- C NEEDS A `default:`; RUST DOES NOT. Both of these are the unreachable arm of a switch
    #    over a closed set, kept by upstream against a future caller that forgets a case. Rust's
    #    match is exhaustive over the same set, so there is no arm for them to live in -- the
    #    condition is a compile error here rather than a runtime reply. Verified reachable by
    #    NOBODY rather than assumed: all six `georadiusGeneric` callers pass exactly one of
    #    RADIUS_COORDS / RADIUS_MEMBER / GEOSEARCH (geo.c:848-871), and db.c's arm is the
    #    `default:` of a switch over OBJ_* in COPY's object duplication.
    "Unknown georadius search type":
        "upstream's own defensive else; every georadiusGeneric caller sets one of the three "
        "search-type flags, so the branch is unreachable in the incumbent too",
    "unknown type object":
        "the `default:` of COPY's switch over OBJ_*; fr matches its type enum exhaustively",
    # -- DEBUG subcommands whose FAILURE path fr has no way to reach. Each is the error arm of a
    #    subcommand fr answers unconditionally: DEBUG RELOAD calls `request_debug_reload()` and
    #    returns OK without inspecting a result, DEBUG LOADAOF is an explicit no-op returning OK,
    #    DEBUG RESTART is not implemented at all. The gap is that fr's DEBUG persistence surface
    #    cannot REPORT failure, which is worth more than the strings: when reload becomes an
    #    inline fallible call, all three come back at once.
    "Error trying to load the RDB dump, check server logs.":
        "fr's DEBUG RELOAD requests a reload and returns OK; there is no result to test",
    "Error trying to load the AOF files, check server logs.":
        "fr's DEBUG LOADAOF is a documented no-op returning OK",
    "failed to restart the server. Check server logs.":
        "fr implements neither DEBUG RESTART nor DEBUG CRASH-AND-RECOVER",
    "OOM in dictTryExpand":
        "DEBUG POPULATE's pre-expand. Same class as ALLOC_FAILURE below: Rust aborts on "
        "allocation failure rather than replying, so there is no site to answer from",
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


def composed_error_prefixes(fr):
    """Prefixes fr PROVES it composes into an error reply, e.g. "Protocol error".

    fr does not always spell a message as one literal. Its RESP parse errors are an enum whose
    `Display` writes the TAIL -- `write!(f, "too big inline request")` -- and the caller builds the
    reply as `RespFrame::Error(format!("ERR Protocol error: {err}"))`. The string the client
    receives is upstream's exactly; the string in the source is two halves that never touch. A
    census searching for the whole literal reports all five of fr's protocol errors ABSENT, which
    is the expensive direction: it bills a turn to re-implement what already ships.

    DERIVED, NOT LISTED, and that is the point -- a hand-written prefix list is an exemption, and
    an exemption is what this census exists to avoid. A prefix counts only when fr contains a
    format string that composes it INTO AN ERROR REPLY. Matching every `format!` instead would
    admit ~250 candidates, nearly all of them test-assertion messages ("wrong wording", "got"), and
    a bad prefix produces a false PRESENCE -- strictly worse than the false absence it fixes,
    because nothing downstream ever re-checks a literal this file calls present.
    """
    pat = re.compile(
        r'(?:RespFrame::Error|CommandError::Custom)\s*\(\s*format!\(\s*'
        r'"([^"\\]{1,80}?): \{[a-z_][a-z0-9_]*\}"'
    )
    out = set()
    for m in pat.finditer(fr):
        prefix = re.sub(r"^[A-Z]+ ", "", m.group(1))  # fr names its RESP code; upstream does not
        if len(prefix) >= 5:
            out.add(prefix)
    return out


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

    # 6. COMPOSED MESSAGES. fr's RESP parse errors are `Display` tails assembled under an
    #    `ERR Protocol error: ` prefix, so the full upstream literal is in no single fr string.
    #    All five of fr's protocol errors read as ABSENT before this -- shipped code, reported
    #    missing.
    composed_fr = (
        '            RespFrame::Error(format!("ERR Protocol error: {err}"));\n'
        '            Self::InlineRequestTooBig => write!(f, "too big inline request"),\n'
    )
    prefixes = composed_error_prefixes(composed_fr)
    assert "Protocol error" in prefixes, prefixes
    assert "ERR Protocol error" not in prefixes, "fr's RESP code must be stripped from the prefix"
    full = "Protocol error: too big inline request"
    assert ('"' + full) not in composed_fr, "precondition: the whole literal is NOT in the source"
    tail = full[len("Protocol error: "):]
    assert ('"' + tail) in composed_fr, "but the tail is, and that is what makes it present"

    #    THE NEGATIVE HALF, which is the half that matters: a prefix fr composes must NOT make
    #    every message under it present. A tail fr does not contain stays absent.
    absent_tail = "some wording fr has never implemented"
    assert ('"' + absent_tail) not in composed_fr, "an unimplemented tail must not be found"

    #    And the prefix set must stay narrow: a bare `format!` -- as in a test assertion -- is not
    #    evidence that fr composes anything into a REPLY.
    assert composed_error_prefixes('panic!("wrong wording: {got}")') == set(), (
        "only error-reply constructions may contribute a prefix"
    )

    # 7. INERT EXEMPTIONS. An ACCEPTED_ABSENT key that matches no upstream literal suppresses
    #    nothing while reading like a decision. Detection is set membership, so what is asserted
    #    here is that the comparison is against the FULL literal and not a prefix of it -- which is
    #    exactly how the ACL entry went inert.
    upstream_keys = {"There was an error trying to save the ACLs. Please check the server logs "
                     "for more information"}
    truncated = "There was an error trying to save the ACLs."
    assert truncated not in upstream_keys, "a truncated key must NOT be treated as a match"
    assert any(k.startswith(truncated) for k in upstream_keys), (
        "precondition: it really is a prefix of the real literal, which is why it looked right"
    )

    print("PASS error_literal_census self-test: substring masking, code prefixes, comment "
          "stripping, C concatenation, Rust line continuations, composed prefixes, "
          "inert exemptions")
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

    composed = composed_error_prefixes(fr)

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
        if re.search(r'"[A-Z]+ ' + re.escape(lit), fr) is not None:
            return True
        # COMPOSED MESSAGES. fr may build the reply as a prefix plus a tail that lives in a
        # `Display` impl, so the whole literal appears nowhere in the source even though the
        # client receives it verbatim. Only prefixes fr demonstrably composes into an error reply
        # are considered -- see `composed_error_prefixes` -- and the tail must still be long
        # enough to attribute on its own, so this cannot rescue a short generic fragment.
        for prefix in composed:
            head = prefix + ": "
            if lit.startswith(head):
                tail = lit[len(head):]
                if len(tail) >= MIN_LEN and ('"' + tail) in fr:
                    return True
        return False

    missing = sorted(l for l in upstream if not present(l) and l not in ACCEPTED_ABSENT)
    stale = sorted(l for l in ACCEPTED_ABSENT if present(l))
    # AN EXEMPTION THAT MATCHES NO UPSTREAM LITERAL IS INERT, and inert is indistinguishable from
    # working: it suppresses nothing, the literal stays on the backlog, and the entry reads like a
    # decision that was made. One was -- "There was an error trying to save the ACLs." was written
    # without upstream's trailing " Please check the server logs for more information", so it
    # exempted nothing for as long as it sat here. `stale` below catches the opposite failure (an
    # entry fr LATER gained); this catches an entry that never applied to begin with.
    inert = sorted(l for l in ACCEPTED_ABSENT if l not in upstream)

    print("upstream: %d distinct error literals (>= %d chars, no format specifiers)"
          % (len(upstream), MIN_LEN))
    print("fr:       %d chars of code scanned (comments stripped)" % len(fr))

    rc = 0
    if inert:
        print("\nFAIL — %d ACCEPTED_ABSENT entr(ies) match no upstream literal, so they exempt\n"
              "nothing. Fix the key or delete the entry:" % len(inert))
        for l in inert:
            print("    %s" % l[:100])
        print()
        rc = 1
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
