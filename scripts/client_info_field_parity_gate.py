#!/usr/bin/env python3
"""Static parity gate: the CLIENT INFO / CLIENT LIST field line (frankenredis-cinfo-fields).

WHY STATIC, AND WHY THAT IS THE POINT. Every field fr hardcodes in this line is
hardcoded to upstream's DEFAULT value -- `rbs=16384`, `rbp=16384`, `oll=0`, `omem=0`,
`events=r` -- so a fresh connection issuing a small command AGREES on all of them. The
divergence only appears once the field becomes interesting: a reply large enough to queue
an output list, a reply buffer the resize cron has moved, a socket with a write handler
installed. That is why a live differential on the obvious case passes and has always
passed. A gate that only ever tests the trivial case reads less than it thinks.

So this gate does not query a server at all. It reads the two FORMAT STRINGS out of the
two sources and compares them structurally:

  * upstream  legacy_redis_code/redis/src/networking.c :: catClientInfoString
  * fr        crates/fr-runtime/src/lib.rs             :: the CLIENT INFO format literal

It checks three things, in order of how badly they break a client:

  1. FIELD NAMES AND ORDER must match exactly. Raw-byte parsers and every differ in this
     repo compare this line positionally, so an added, removed or reordered field is a
     wire break. This is the check that would have caught the CRLF-instead-of-LF defect
     recorded at the fr call site (frankenredis-cudmd) had it existed then.
  2. Fields fr renders as a LITERAL where upstream renders a COMPUTED value are reported
     as LATENT DIVERGENCES, with the condition each one needs to become observable. These
     are not failures by default -- some are deliberate, documented approximations -- but
     an UNDECLARED one is a finding, so the gate carries the known set and fails when the
     set CHANGES in either direction.
  3. Fields upstream computes that fr does not emit at all.

Runs with no server, no build, no network and no disk writes, which is why it is usable
during a build freeze.

Usage: client_info_field_parity_gate.py [--verbose]
       Exit 0 = field names/order match and the literal set is as declared.
       Exit 1 = a wire-shape break, or the literal set drifted.
"""
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
UPSTREAM = os.path.join(ROOT, "legacy_redis_code", "redis", "src", "networking.c")
FR = os.path.join(ROOT, "crates", "fr-runtime", "src", "lib.rs")

# A field is `name=` followed by either a printf/format placeholder (computed) or a
# literal run of non-space characters (hardcoded). Upstream also emits ONE bare `%s`
# between laddr and name -- that is connGetInfo(), which expands to `fd=<n>` -- so the
# gate substitutes it rather than treating it as an unnamed field.
FIELD_RE = re.compile(r"([a-z][a-z0-9-]*)=(\S*)")

# Declared literal-valued fields in fr, with what upstream computes and the condition
# that makes the difference observable. Keyed by field name.
#
# `tot-mem` is deliberately NOT listed: fr substitutes a computed value there and the
# call site documents it as a lower-bound estimate (frankenredis-tepuj). A declared
# approximation of a computed field is a different thing from a hardcoded constant.
DECLARED_LITERALS = {
    # (frankenredis-edwnn) `oll` and `omem` were hardcoded to 0 and are now COMPUTED from
    # session.output_buffer_bytes as the spill past the 16384-byte static buffer, so they are
    # deliberately NOT declared here any more. Re-adding either would re-allow the constant
    # this gate exists to catch -- and 98e67ec2f MEASURED that constant against a client with
    # 1.4 MB of undrained output.
    # (frankenredis-edwnn) `events` was hardcoded to "r" and is now COMPUTED from
    # session.output_buffer_bytes, so it is deliberately NOT declared here any more.
    # Re-adding it would re-allow the constant this gate exists to catch.
}

# `laddr` is a partial literal: fr substitutes the real PORT but hardcodes the host to
# 127.0.0.1, where upstream reports getClientSockname(). Tracked separately because the
# field is neither fully computed nor fully literal.
# (frankenredis-edwnn) `laddr` was a partial literal -- real port, hardcoded 127.0.0.1 host --
# and is now rendered from the accepted socket's own local address, so it is deliberately NOT
# declared here any more. Re-adding it would re-allow the hardcoded host this gate exists to catch.
PARTIAL_LITERALS: dict[str, tuple[str, str]] = {}

STRUCTURAL_LITERALS = frozenset()


def upstream_fields():
    src = open(UPSTREAM, encoding="utf-8", errors="replace").read()
    i = src.index("sds catClientInfoString")
    # The single long format literal begins at the sdscatfmt call after that.
    j = src.index('"id=%U addr=', i)
    end = src.index('"', j + 1)
    fmt = src[j + 1:end]
    # connGetInfo() expands to `fd=<n>`; name it so the positional comparison lines up.
    fmt = fmt.replace("laddr=%s %s name=", "laddr=%s fd=%s name=")
    return fmt, [m.group(1) for m in FIELD_RE.finditer(fmt)]


def fr_fields():
    src = open(FR, encoding="utf-8", errors="replace").read()
    j = src.index('"id={} addr=')
    end = src.index('"', j + 1)
    fmt = src[j + 1:end]
    pairs = [(m.group(1), m.group(2)) for m in FIELD_RE.finditer(fmt)]
    return fmt, pairs


def is_computed(value: str) -> bool:
    """Does this field's rendered value come from a substitution rather than a literal?"""
    return "{}" in value or "%" in value


def main():
    verbose = "--verbose" in sys.argv
    up_fmt, up_names = upstream_fields()
    fr_fmt, fr_pairs = fr_fields()
    fr_names = [n for n, _ in fr_pairs]

    failures, notes = [], []

    # (1) names and order
    if fr_names != up_names:
        only_up = [n for n in up_names if n not in fr_names]
        only_fr = [n for n in fr_names if n not in up_names]
        if only_up:
            failures.append(f"fields upstream emits that fr does not: {only_up}")
        if only_fr:
            failures.append(f"fields fr emits that upstream does not: {only_fr}")
        if not only_up and not only_fr:
            failures.append(
                f"same field SET but different ORDER:\n    upstream {up_names}\n    fr       {fr_names}")

    # (2) literal-valued fields in fr where upstream computes
    literal_now = {n for n, v in fr_pairs if not is_computed(v)}
    # laddr renders as `laddr=127.0.0.1:{}` -- computed port, literal host.
    partial_now = {n for n, v in fr_pairs
                   if is_computed(v) and re.search(r"=[^{%]+[{%]", n + "=" + v)}

    undeclared = sorted(literal_now - set(DECLARED_LITERALS))
    disappeared = sorted(set(DECLARED_LITERALS) - literal_now)
    if undeclared:
        failures.append(
            "NEW hardcoded field(s) not in the declared set — either compute them or "
            f"declare them with their observability condition: {undeclared}")
    if disappeared:
        failures.append(
            "declared-hardcoded field(s) are now computed — good, but update "
            f"DECLARED_LITERALS so this gate keeps meaning something: {disappeared}")
    # (frankenredis-edwnn) Same rule for PARTIAL_LITERALS. This check did not exist, so a partial
    # that became fully computed kept being REPORTED as a live divergence and the gate still
    # passed -- the gate reading less than it thinks, which is the defect this whole bead is about.
    partial_fixed = sorted(
        n for n in PARTIAL_LITERALS
        if n in dict(fr_pairs) and n not in literal_now and n not in partial_now
    )
    if partial_fixed:
        failures.append(
            "declared-PARTIAL field(s) are now fully computed — good, but update "
            f"PARTIAL_LITERALS so this gate keeps meaning something: {partial_fixed}")

    structural = []
    for name in sorted(literal_now & set(DECLARED_LITERALS)):
        upstream_src, condition = DECLARED_LITERALS[name]
        value = dict(fr_pairs)[name]
        entry = f"{name}={value:<10} upstream: {upstream_src}\n      would be computable only if {condition}"
        if name in STRUCTURAL_LITERALS:
            structural.append(entry)
        else:
            notes.append(f"{name}={value:<10} upstream: {upstream_src}\n      observable when {condition}")
    for name, (upstream_src, condition) in sorted(PARTIAL_LITERALS.items()):
        if name in fr_names:
            notes.append(f"{name}={dict(fr_pairs)[name]:<10} upstream: {upstream_src}\n"
                         f"      observable when {condition}")

    print("=" * 78)
    print(f"CLIENT INFO field parity — {len(up_names)} upstream fields, {len(fr_names)} in fr")
    if verbose:
        print(f"\nupstream: {up_fmt}\n\nfr:       {fr_fmt}\n")
    print("=" * 78)
    if notes:
        print(f"LATENT DIVERGENCES — {len(notes)} field(s) fr renders as a constant equal to\n"
              "upstream's DEFAULT, so a fresh connection running a small command agrees on\n"
              "every one of them. Each needs its own setup to observe:\n")
        for n in notes:
            print(f"  * {n}")
        print()
    if structural:
        print(f"STRUCTURAL CONSTANTS — {len(structural)} field(s) describing a subsystem fr does\n"
              "NOT model. These are not latent divergences waiting for the right workload: there\n"
              "is no workload. fr has no per-client static reply buffer, which is why DEBUG\n"
              "REPLYBUFFER is accept-and-OK (frankenredis-53n6u). 16384 is PROTO_REPLY_CHUNK_BYTES\n"
              "used as a MODEL constant -- the same one obl/oll/omem are defined against -- not\n"
              "upstream's default standing in for a value fr could compute.\n")
        for n in structural:
            print(f"  * {n}")
        print()
    if failures:
        print(f"FAIL — {len(failures)} finding(s):")
        for f in failures:
            print(f"  {f}")
        sys.exit(1)
    print(f"PASS — field names and order match upstream exactly; "
          f"{len(notes)} latent and {len(structural)} structural literal-valued field(s), "
          "all declared.")


if __name__ == "__main__":
    main()
