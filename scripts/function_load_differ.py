#!/usr/bin/env python3
"""frankenredis-o500d: differential FUNCTION LOAD against live Redis 7.2.4.

fr syntax-checks a library body and then TEXT-SCANS for register_function; it never
EXECUTES the body. Upstream functions.c compiles AND RUNS it at load time inside a
sandbox whose global table exposes only the declared set, so a load-time runtime
error — or a read of an undeclared global — fails the load before anything registers.

Both arms run in ONE invocation against two live servers. Usage:
    function_load_differ.py <redis_port> <fr_port>
Exit 0 = arms agree on every row, 1 = at least one divergence, 2 = a CONTROL row diverged,
which means the harness is not measuring what it claims and no row from it may be quoted.

(frankenredis-9hori) A second phase drives the whole LIFE CYCLE -- load, fcall, DEBUG RELOAD,
fcall, FUNCTION DUMP/RESTORE, fcall -- because loading is not using: a library whose function
name is computed at runtime can load perfectly and still be uncallable, lost by the next
restart, or unable to come back from its own dump. Those were seams 4, 2 and 3 of this bead.
"""
import socket
import sys

RS = int(sys.argv[1])
FR = int(sys.argv[2])

SHEBANG = "#!lua name=%s\n"


class Resp3OnResp2(RuntimeError):
    """A RESP3-only frame arrived on a connection that never negotiated RESP3.

    Carries the tag so the report names which shape leaked. The frame's PAYLOAD is left
    unread by design -- there is no safe way to skip a frame whose encoding the reader does
    not implement -- so any connection that raises this is desynced and must be replaced.
    """

    def __init__(self, tag, line):
        super().__init__(f"RESP3 frame {tag!r} on a RESP2 connection: {line!r}")
        self.tag = tag


class Conn:
    def __init__(self, port):
        self.port = port
        self.s = socket.create_connection(("127.0.0.1", port), 10)
        self.s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.buf = b""

    def _enc(self, args):
        out = [b"*%d\r\n" % len(args)]
        for a in args:
            if isinstance(a, str):
                a = a.encode()
            out.append(b"$%d\r\n%s\r\n" % (len(a), a))
        return b"".join(out)

    def _line(self):
        while b"\r\n" not in self.buf:
            chunk = self.s.recv(1 << 20)
            if not chunk:
                raise EOFError("server closed")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line

    def _read(self):
        line = self._line()
        tag, rest = line[:1], line[1:]
        if tag in (b"+", b":"):
            return rest
        if tag == b"-":
            return b"ERRREPLY:" + rest
        if tag == b"$":
            n = int(rest)
            if n == -1:
                return None
            while len(self.buf) < n + 2:
                self.buf += self.s.recv(1 << 20)
            val, self.buf = self.buf[:n], self.buf[n + 2:]
            return val
        if tag == b"*":
            n = int(rest)
            if n == -1:
                return None
            return [self._read() for _ in range(n)]
        if tag in (b"%", b"~", b",", b"#", b"(", b"="):
            # A RESP3-only frame on a connection that never sent HELLO 3. This is the shape
            # of frankenredis-luaresp2map: fr-protocol writes `%N\r\n` for a RespFrame::Map
            # whatever the client speaks, so a reply path that skips
            # downconvert_lua_reply_to_resp2 leaks one to a RESP2 caller. Raise a TYPED error
            # rather than the generic one below: the caller reconnects and records a
            # divergence, instead of the whole run dying on a "bad tag" traceback.
            raise Resp3OnResp2(tag.decode(), line.decode(errors="replace"))
        raise RuntimeError(f"bad tag {line!r}")

    def cmd(self, *args):
        self.s.sendall(self._enc(list(args)))
        return self._read()


# (body_suffix, description, expectation_class)
# (frankenredis-o500d) KNOWN, TRACKED divergences. Everything NOT listed here must agree,
# so a regression on an already-fixed row fails this gate instead of blending into a count.
#
# Before this table the script returned 1 whenever ANY row diverged, and one row has been
# permanently red since the bead was filed. A permanently-red gate cannot detect the SECOND
# divergence -- it just reads "2" instead of "1" and nobody looks. Three of the bead's four
# rows are now fixed (top_level_error, undeclared_global, tonumber_call all agree), and this
# is what stops them regressing silently.
#
# An entry here is a debt, not a permission: if a listed row starts AGREEING the gate fails
# too, demanding the entry be removed, because an allowance that outlives its bug is how a
# gate rots into permanent green.
# (frankenredis-o500d) EMPTY, and it should stay that way. `nil_index` lived here until fr
# began EXECUTING the library body at load time (lua_eval::function_load_execute); the differ
# itself flagged the entry as stale the moment the row went green, which is the behaviour that
# keeps an allowance from outliving its bug. All four behavioural rows now agree with 7.2.4.
#
# Add an entry ONLY with a bug id and a reason, never to make a red run green.
EXPECTED_DIVERGENCES: dict[str, str] = {
    # (frankenredis-9hori) dyn_name_local, dyn_name_concat and dyn_callback_local were removed
    # from this table when the text scan stopped being the source of truth for registrations.
    # FUNCTION LOAD, the five reload paths and FUNCTION RESTORE now execute the library body and
    # register whatever `redis.register_function` was actually called with, and FCALL falls back
    # to executing when the scan cannot express the function.
    #
    # THEY ARE REMOVED WHILE THE FIX IS STILL UNCOMPILED, deliberately. This table's own rule is
    # that an expectation left in place after a bug is fixed is how a gate rots into permanent
    # green, and the code that made these three diverge is gone. If the fix is wrong, these cases
    # fail here as UNEXPECTED divergences, which is the signal that is wanted -- an allowance
    # would have hidden it behind a stale excuse instead.
}

CASES = [
    ("top_level_error",
     "error('boom')\nredis.register_function('f', function(k,a) return 1 end)",
     "load-time runtime error"),
    ("undeclared_global",
     "local x = nosuchglobal_zz\nredis.register_function('f', function(k,a) return 1 end)",
     "read of undeclared global"),
    ("tonumber_call",
     "local x = tonumber('1')\nredis.register_function('f', function(k,a) return 1 end)",
     "call of undeclared global tonumber"),
    ("nil_index",
     "local t = nil\nlocal y = t.field\nredis.register_function('f', function(k,a) return 1 end)",
     "nil index at load time"),
    # (frankenredis-p98mw) DYNAMIC REGISTRATION ARGUMENTS. fr derives the registered names
    # by TEXT-SCANNING the source for `register_function(...)`; a name or callback held in a
    # LOCAL is invisible to that scan, so fr refuses libraries 7.2.4 loads. This is a FALSE
    # REJECTION -- the opposite failure direction from o500d rows 1-4, and the reason these
    # rows exist rather than being folded into the executed-body work.
    ("dyn_name_local",
     "local n = 'dyn'\nredis.register_function(n, function(k,a) return 1 end)",
     "registration name supplied by a local"),
    ("dyn_name_concat",
     "local n = 'a' .. 'b'\nredis.register_function(n, function(k,a) return 1 end)",
     "registration name computed at load time"),
    ("dyn_callback_local",
     "local cb = function(k,a) return 1 end\nredis.register_function('f', cb)",
     "registration callback supplied by a local"),
    # (frankenredis-o500d) THE OTHER DIRECTION. Every row above is a false REJECTION -- fr
    # refusing a library 7.2.4 loads. These six are false ACCEPTANCES: until 4390bc9c5 fr
    # LOADED all of them and 7.2.4 refuses every one, so a library authored against fr would
    # have failed on real redis. Upstream's refusals are in
    # function_lua.c::luaRegisterFunctionReadNamedArgs / ReadFlags, with the flag set in
    # script.c:34 (no-writes, allow-oom, allow-stale, no-cluster, allow-cross-slot-keys).
    ("unknown_flag",
     "redis.register_function{function_name='f', callback=function(k,a) return 1 end,"
     " flags={'no-write'}}",
     "flag one character off a real one"),
    ("flag_not_string",
     "redis.register_function{function_name='f', callback=function(k,a) return 1 end,"
     " flags={1}}",
     "non-string entry inside the flags table"),
    ("flags_not_table",
     "redis.register_function{function_name='f', callback=function(k,a) return 1 end,"
     " flags='no-writes'}",
     "flags present but not a table"),
    ("unknown_named_arg",
     "redis.register_function{function_name='f', callback=function(k,a) return 1 end,"
     " nosuch='x'}",
     "named argument outside the four upstream knows"),
    ("description_not_string",
     "redis.register_function{function_name='f', callback=function(k,a) return 1 end,"
     " description=1}",
     "description present but not a string"),
    ("single_non_table_arg",
     "redis.register_function('f')",
     "lone argument that is not a table"),
    # (frankenredis-fnfdup, regression pin for 2ec539a02) The same name registered TWICE in one
    # library. fr's SCANNING path always refused this and its unit test still passes, because that
    # test calls store.function_load directly -- but 9hori moved FUNCTION LOAD onto the executed
    # path, which had no per-library duplicate check, so this LOADED. Upstream refuses it in
    # functions.c::functionLibCreateFunction. This row goes through FUNCTION LOAD, which is the
    # whole point: it can see a change of which store API the command calls, and the unit test
    # cannot.
    ("duplicate_fn_name",
     "redis.register_function('dupe', function(k,a) return 1 end)\n"
     "redis.register_function('dupe', function(k,a) return 2 end)",
     "one name registered twice in a library"),
    # ---- controls: both arms MUST already agree on these ----
    ("CONTROL_no_register",
     "local x = 1",
     "CONTROL: body registers nothing"),
    ("CONTROL_valid",
     "redis.register_function('f', function(k,a) return 1 end)",
     "CONTROL: valid library"),
    # (frankenredis-o500d) The guard against over-correcting: a LEGAL flag, in the case
    # upstream's strcasecmp accepts and a naive exact-match would refuse. If the validation
    # added in 4390bc9c5 is too strict, this row goes red rather than the six above going green.
    ("CONTROL_legal_flags",
     "redis.register_function{function_name='f', callback=function(k,a) return 1 end,"
     " flags={'no-writes', 'ALLOW-OOM'}}",
     "CONTROL: real flags, mixed case"),
]


# (frankenredis-9hori) ROUND-TRIP CASES for seams 2, 3 and 4.
#
# Loading is not using. A library whose function name is computed at runtime can LOAD and still
# be uncallable (seam 4), lost by the next restart (seam 2), or refuse to come back from its own
# DUMP (seam 3). Each case below registers a function under a name this harness KNOWS, then
# drives the whole life cycle and compares the sequence against the incumbent.
#
# `%s` in the body is the function name, so a dynamic case computes at runtime exactly the name
# the harness will later call.
ROUND_TRIP_CASES = [
    ("rt_name_local",
     "local n = '%s'\nredis.register_function(n, function(k,a) return 41 end)",
     "name from a local"),
    ("rt_name_concat",
     "local n = '%s' .. ''\nredis.register_function(n, function(k,a) return 41 end)",
     "name computed at load time"),
    ("rt_callback_local",
     "local cb = function(k,a) return 41 end\nredis.register_function('%s', cb)",
     "callback from a local"),
    # (frankenredis-luaresp2map, regression case for d15b2e455) The ONLY case here that
    # returns something other than an integer, and the only one that exercises the reply
    # CONVERSION rather than just the registration. `{map=...}` becomes RespFrame::Map, which
    # fr-protocol writes as `%N` regardless of the caller's protocol, so a path missing
    # downconvert_lua_reply_to_resp2 leaks a RESP3 frame to this RESP2 connection. The callback
    # is an IDENTIFIER on purpose: transform_register_function requires a literal `function`
    # keyword in that position, so the scan cannot express it and fcall_cmd takes the EXECUTING
    # path -- which is exactly where the conversion was missing.
    ("rt_map_reply",
     "local cb = function(k,a) return { map = { f = 41 } } end\nredis.register_function('%s', cb)",
     "map reply through the executing path"),
    ("rt_CONTROL_literal",
     "redis.register_function('%s', function(k,a) return 41 end)",
     "CONTROL: everything literal"),
]


def step(conn, *args):
    """Run one command, tolerating a RESP3 frame leaked onto this RESP2 connection.

    Returns (classified_reply, replacement_conn_or_None). The connection is REPLACED rather
    than reused: the offending frame's payload was never consumed, so everything after it on
    that socket is misaligned and would produce invented divergences for the rest of the run.
    """
    try:
        return classify(conn.cmd(*args)), None
    except Resp3OnResp2 as exc:
        return f"RESP3-FRAME {exc.tag}", Conn(conn.port)


def round_trip(conn, lib, fname, body):
    """Load, call, survive a reload, and survive its own DUMP/RESTORE.

    Returns a list of (step, classified reply). Each engine round-trips ITS OWN dump: the payload
    bytes are an implementation detail and may legitimately differ between engines, while the
    OUTCOME -- that the function still answers 41 afterwards -- is the parity claim.
    """
    # Own connection per case: a leaked RESP3 frame desyncs the socket it arrived on (its
    # payload is never consumed), and reusing it would turn one real bug into a screen of
    # invented divergences in every later case.
    c = Conn(conn.port)
    out = []
    c.cmd("FUNCTION", "FLUSH")
    src = (SHEBANG % lib) + (body % fname)

    def record(label, *args):
        nonlocal c
        reply, replacement = step(c, *args)
        if replacement is not None:
            c = replacement
        out.append((label, reply))

    record("load", "FUNCTION", "LOAD", "REPLACE", src)
    record("fcall", "FCALL", fname, "0")

    # seam 2: the registration must survive a reload. DEBUG RELOAD round-trips the dataset
    # through RDB in-process, which is the same path a restart takes.
    record("reload", "DEBUG", "RELOAD")
    record("fcall_after_reload", "FCALL", fname, "0")

    # seam 3: the library must come back from its own FUNCTION DUMP.
    try:
        dumped = c.cmd("FUNCTION", "DUMP")
    except Resp3OnResp2 as exc:
        out.append(("dump", f"RESP3-FRAME {exc.tag}"))
        out.append(("restore", "SKIPPED: dump failed"))
        out.append(("fcall_after_restore", "SKIPPED: dump failed"))
        return out
    if not isinstance(dumped, bytes) or dumped.startswith(b"ERRREPLY:"):
        out.append(("dump", classify(dumped)))
        out.append(("restore", "SKIPPED: dump failed"))
        out.append(("fcall_after_restore", "SKIPPED: dump failed"))
        return out
    out.append(("dump", "OK %d bytes" % len(dumped)))
    c.cmd("FUNCTION", "FLUSH")
    record("restore", "FUNCTION", "RESTORE", dumped)
    record("fcall_after_restore", "FCALL", fname, "0")
    return out


def run_round_trips(redis, fr):
    """Returns (divergences, control_failures). Any divergence here is a FAILURE.

    There are deliberately no EXPECTED_DIVERGENCES for these rows. The three seams they cover
    were all fixed in the same session as this harness, so an allowance would be an excuse
    written before the first run rather than a tracked gap.
    """
    print()
    print("ROUND TRIP: load -> fcall -> reload -> fcall -> dump/restore -> fcall")
    print(f"{'case':<20} {'step':<20} {'fr':<30} {'redis 7.2.4'}")
    print("-" * 100)
    divergences = 0
    control_failures = 0
    for i, (name, body, _desc) in enumerate(ROUND_TRIP_CASES):
        lib, fname = f"rtlib{i}", f"rtfn{i}"
        r_steps = round_trip(redis, lib, fname, body)
        f_steps = round_trip(fr, lib, fname, body)
        for (step, r_reply), (_, f_reply) in zip(r_steps, f_steps):
            agree = (r_reply.split(":")[0] == f_reply.split(":")[0]) and (
                r_reply.startswith("ERR") == f_reply.startswith("ERR"))
            mark = "" if agree else "   <-- DIVERGES"
            if not agree:
                divergences += 1
                if name.startswith("rt_CONTROL"):
                    control_failures += 1
            print(f"{name:<20} {step:<20} {f_reply[:29]:<30} {r_reply[:29]}{mark}")
    return divergences, control_failures


def classify(reply):
    if reply is None:
        return "nil"
    if isinstance(reply, bytes) and reply.startswith(b"ERRREPLY:"):
        return "ERR " + reply[len(b"ERRREPLY:"):].decode(errors="replace")
    if isinstance(reply, bytes):
        return "OK " + reply.decode(errors="replace")
    return repr(reply)


def main():
    redis, fr = Conn(RS), Conn(FR)
    for c in (redis, fr):
        c.cmd("FUNCTION", "FLUSH")

    divergences = 0
    control_failures = 0
    envelope_mismatches = []
    unexpected = []
    fixed_but_still_expected = []
    print(f"{'case':<22} {'fr':<44} {'redis 7.2.4'}")
    print("-" * 118)
    for i, (name, body, _desc) in enumerate(CASES):
        lib = f"o500d{i}"
        # (frankenredis-niu8g) Each case needs its own FUNCTION name, not just
        # its own library name. A function name is GLOBAL across libraries — a
        # second library registering an existing name is refused with
        # "Function <name> already exists" — so a row whose library LOADS
        # reserves 'f' and poisons every later row ON THAT ENGINE ONLY. That
        # made the CONTROL_valid row diverge for a reason having nothing to do
        # with what it measures: on fr the nil_index library still loads (o500d
        # row 4, unfixed) and takes 'f', while on redis it is rejected and 'f'
        # stays free. The rows must be independent for any of them to mean
        # anything.
        src = (SHEBANG % lib) + body.replace("'f'", f"'f{i}'")
        r_reply = classify(redis.cmd("FUNCTION", "LOAD", "REPLACE", src))
        f_reply = classify(fr.cmd("FUNCTION", "LOAD", "REPLACE", src))
        agree = (r_reply.split(":")[0] == f_reply.split(":")[0]) and (
            r_reply.startswith("ERR") == f_reply.startswith("ERR"))
        # (frankenredis-fnukn) The rule above compares the FIRST COLON SEGMENT, so two errors
        # agreeing on `ERR Error registering functions` count as agreement however their tails
        # differ. That is exactly how fnukn's measured wording regressed unnoticed when 9hori
        # moved FUNCTION LOAD onto the executed path and the refusal gained a
        # `user_function:<line>:` segment upstream does not emit. Collected and printed, NOT
        # failed: promoting it needs a run against two live servers to separate real envelope
        # bugs from tails that legitimately differ, and the freeze forbids that.
        if agree and r_reply.startswith("ERR") and f_reply != r_reply:
            envelope_mismatches.append((name, f_reply, r_reply))
        mark = "" if agree else "   <-- DIVERGES"
        if not agree:
            divergences += 1
            if name.startswith("CONTROL"):
                control_failures += 1
            if name in EXPECTED_DIVERGENCES:
                mark = "   <-- diverges (KNOWN: %s)" % EXPECTED_DIVERGENCES[name]
            else:
                unexpected.append(name)
        elif name in EXPECTED_DIVERGENCES:
            # A tracked gap that now AGREES. Say so loudly: an expectation left in
            # place after the bug is fixed is how a gate rots into permanent green.
            fixed_but_still_expected.append(name)
            mark = "   <-- FIXED, remove it from EXPECTED_DIVERGENCES"
        print(f"{name:<22} {f_reply[:43]:<44} {r_reply[:43]}{mark}")

    print()
    if control_failures:
        print(f"HARNESS INVALID: {control_failures} CONTROL row(s) diverged — the probe "
              f"is not measuring what it claims. Fix the harness before trusting any row.")
        return 2
    if envelope_mismatches:
        print()
        print(f"ENVELOPE MISMATCH — {len(envelope_mismatches)} row(s) that the first-segment rule")
        print("counted as AGREEING, but whose full error text differs. Not a failure here; read")
        print("them before trusting the count above. (frankenredis-fnukn)")
        for name, f_reply, r_reply in envelope_mismatches:
            print(f"  {name}")
            print(f"    fr        {f_reply}")
            print(f"    redis     {r_reply}")
    print(f"{divergences} divergence(s) on the {len(CASES) - 2} behavioural rows; "
          f"both controls agree, so the probe discriminates.")
    if fixed_but_still_expected:
        print("STALE EXPECTATION: %s now agree(s) with 7.2.4. Delete the entry from "
              "EXPECTED_DIVERGENCES -- an allowance that outlives its bug hides the next "
              "regression on that row." % ", ".join(fixed_but_still_expected))
        return 1
    if unexpected:
        print("FAIL: %d UNEXPECTED divergence(s): %s" % (len(unexpected), ", ".join(unexpected)))
        return 1
    rt_div, rt_control = run_round_trips(redis, fr)
    print()
    if rt_control:
        print(f"HARNESS INVALID: {rt_control} round-trip CONTROL step(s) diverged — a library "
              f"with everything literal must survive load, reload and restore on both engines. "
              f"Fix the harness before trusting any round-trip row.")
        return 2
    if rt_div:
        print(f"FAIL: {rt_div} round-trip divergence(s). These cover 9hori seams 2 (reload), "
              f"3 (FUNCTION RESTORE) and 4 (FCALL) — a library that LOADS is not the same as one "
              f"that survives and can be called.")
        return 1
    print("PASS: round trips agree — dynamic registrations load, call, survive a reload, "
          "and come back from their own dump.")

    if divergences:
        print("PASS: every divergence is a KNOWN, tracked gap. A new one fails this gate.")
    else:
        print("PASS: no divergences at all.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
