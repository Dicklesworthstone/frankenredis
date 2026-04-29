#!/usr/bin/env python3
"""Generate structured corpus seeds for fuzz_acl_rules.

The fuzz target feeds the entire input to `arbitrary` to derive an
`AclFuzzInput` enum (Valid or Raw). For seeds, raw text content is
the most useful starting point: arbitrary will mutate from these
into both the structured-Valid path AND the raw-text path, and
libfuzzer's mutator fans them out into the canonicalize_acl_rules
parser.

The seed catalogue covers each kind of ACL rule the parser must
handle (per `AuthState::load_acl_rules`):

  Valid:
    - empty file
    - only comments / only blank lines
    - canonical minimal user (alice with on/nopass/+@all/~*&*)
    - reset; on; >password; -@all; +get; ~prefix:*
    - reset; off
    - resetpass + addpassword chain
    - allcommands / nocommands toggles
    - allkeys / allchannels
    - per-category allow + deny in same line
    - explicit command allow + deny in same line
    - key pattern with glob
    - channel pattern with glob
    - multiple users (chained config)
    - user with username containing _ - : (the legal charset)
    - very long rule chain (stress test the parser's loop)
    - whitespace tolerance (multiple spaces, tabs, trailing spaces)

  Reject (must produce Err) — exercises the parser's fail path:
    - line missing the `user` prefix
    - user line without a username
    - unknown rule prefix (e.g. `user x reset @bogus`)
    - garbage bytes
    - just a `>` with no password

Run:
    python3 fuzz/scripts/gen_acl_rules_seeds.py
"""
from __future__ import annotations

from pathlib import Path


def seed(label: str, body: bytes) -> tuple[str, bytes]:
    return (label, body)


def main() -> None:
    repo = Path(__file__).resolve().parent.parent.parent
    out_dir = repo / "fuzz" / "corpus" / "fuzz_acl_rules"
    out_dir.mkdir(parents=True, exist_ok=True)

    seeds: list[tuple[str, bytes]] = [
        # ── Valid ACL files ──────────────────────────────────────
        seed("empty.acl", b""),
        seed("only_comment.acl", b"# this is just a comment\n"),
        seed("only_blank_lines.acl", b"\n\n\n"),
        seed(
            "canonical_alice.acl",
            b"user alice reset on nopass +@all ~* &*\n",
        ),
        seed(
            "user_with_password.acl",
            b"user bob reset on >secretpass +@all ~*\n",
        ),
        seed(
            "user_with_two_passwords.acl",
            b"user cathy reset on >pw1 >pw2 +@all ~*\n",
        ),
        seed(
            "user_off.acl",
            b"user eve reset off nopass +@all ~*\n",
        ),
        seed(
            "user_resetpass.acl",
            b"user dan reset on >old resetpass >new +@all ~*\n",
        ),
        seed(
            "user_allcommands_toggle.acl",
            b"user fred reset on nopass +@all -del ~*\n",
        ),
        seed(
            "user_nocommands_toggle.acl",
            b"user gary reset on nopass -@all +get +set ~*\n",
        ),
        seed(
            "user_category_allow_deny.acl",
            b"user hugo reset on nopass +@read -@dangerous ~*\n",
        ),
        seed(
            "user_command_allow_deny.acl",
            b"user iris reset on nopass +@all -keys -flushall ~*\n",
        ),
        seed(
            "user_key_pattern.acl",
            b"user jay reset on nopass +@all ~user:*\n",
        ),
        seed(
            "user_channel_pattern.acl",
            b"user kim reset on nopass +@all ~* &events:*\n",
        ),
        seed(
            "two_users.acl",
            b"user alice reset on nopass +@all ~*\n"
            b"user bob reset off nopass +@read ~ro:*\n",
        ),
        seed(
            "three_users_with_categories.acl",
            b"user reader reset on nopass +@read -@admin ~*\n"
            b"user writer reset on >wp +@write -@dangerous ~*\n"
            b"user admin reset on >ap +@all ~*\n",
        ),
        seed(
            "username_with_underscore.acl",
            b"user app_user reset on nopass +@all ~*\n",
        ),
        seed(
            "username_with_hyphen.acl",
            b"user app-user reset on nopass +@all ~*\n",
        ),
        seed(
            "long_rule_chain.acl",
            b"user lola reset on nopass +@read +@write -@dangerous "
            b"-@admin -flushall -keys -shutdown +get +set +del +mget "
            b"+mset ~app:* &chan:*\n",
        ),
        seed(
            "comment_then_user.acl",
            b"# header\n\nuser mike reset on nopass +@all ~*\n",
        ),
        seed(
            "interleaved_comments.acl",
            b"# first user\n"
            b"user nora reset on nopass +@all ~*\n"
            b"# second user\n"
            b"user oscar reset off nopass +@read ~*\n",
        ),
        seed(
            "many_passwords.acl",
            b"user pat reset on >p1 >p2 >p3 >p4 >p5 +@all ~*\n",
        ),
        seed(
            "trailing_blank_line.acl",
            b"user q reset on nopass +@all ~*\n\n",
        ),
        seed(
            "tab_separated.acl",
            b"user\tracquel\treset\ton\tnopass\t+@all\t~*\n",
        ),
        # ── Likely-reject inputs (parser may or may not error;
        #    seeds drive both branches usefully) ──────────────────
        seed("garbage_one_line.acl", b"totally garbage line\n"),
        seed("missing_user_prefix.acl", b"alice reset on nopass +@all ~*\n"),
        seed("user_no_rules.acl", b"user solo\n"),
        seed("password_token_alone.acl", b">orphanpass\n"),
        seed(
            "unknown_rule.acl",
            b"user x reset on @notarule ~*\n",
        ),
        seed("invalid_utf8.acl", b"user x reset on +@all ~* \xff\n"),
        seed(
            "multiple_blank_lines_between_users.acl",
            b"user one reset on nopass +@all ~*\n"
            b"\n\n\n"
            b"user two reset on nopass +@all ~*\n",
        ),
    ]

    for label, payload in seeds:
        path = out_dir / label
        path.write_bytes(payload)
        print(f"wrote {len(payload):4d} bytes to {path.relative_to(repo)}")
    print(f"\ngenerated {len(seeds)} corpus seeds")


if __name__ == "__main__":
    main()
