//! CrimsonHawk regression gate: the production `glob_match` literal fast paths
//! (exact/prefix/suffix/contains, landed this session) must stay byte-exact with the
//! pure backtracking matcher for EVERY input. The fast paths route `<lit>` / `<lit>*`
//! / `*<lit>` / `*<lit>*` away from the backtracker, so this differential proptest is
//! the guard that any future edit to them (or to `literal_glob_shape`) can't silently
//! diverge. Alphabet is rich in `*`/`?`/literals to stress both fast-path firing and
//! fallthrough (a `?` in the body must defeat the metachar-free literal check).

use fr_store::glob_match;
use proptest::prelude::*;

// Pure backtracking reference — the algorithm `glob_match` uses for the fallthrough,
// WITHOUT the literal fast paths. (No `[`/`\` in the test alphabet, so the class /
// escape branches are unreachable and need no faithful copy here.)
fn glob_reference(pattern: &[u8], string: &[u8]) -> bool {
    if string.is_empty() {
        return pattern.is_empty();
    }
    let mut pi = 0usize;
    let mut si = 0usize;
    let mut star_pi = usize::MAX;
    let mut star_si = usize::MAX;
    while si < string.len() {
        if pi < pattern.len() && pattern[pi] == b'*' {
            star_pi = pi;
            star_si = si;
            pi += 1;
        } else if pi < pattern.len() && pattern[pi] == b'?' {
            pi += 1;
            si += 1;
        } else if pi < pattern.len() && pattern[pi] == string[si] {
            pi += 1;
            si += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_si += 1;
            si = star_si;
        } else {
            return false;
        }
    }
    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }
    pi == pattern.len()
}

// Bytes chosen so generated patterns frequently form literal shapes (`a:1`, `ab*`,
// `*c*`) AND non-literal ones (`a?b`, `a*b*c`), exercising fast-path + fallthrough.
const ALPHABET: &[u8] = b"abc:01*?";

fn glob_string() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(proptest::sample::select(ALPHABET), 0..14)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    #[test]
    fn glob_match_fastpaths_equal_backtracking_reference(
        pattern in glob_string(),
        string in glob_string(),
    ) {
        prop_assert_eq!(
            glob_match(&pattern, &string),
            glob_reference(&pattern, &string),
            "glob_match diverged from backtracking reference: pattern={:?} string={:?}",
            pattern, string
        );
    }
}

// A few explicit boundary cases the proptest distribution under-samples, pinned so a
// regression shows a named failure rather than a random counterexample.
#[test]
fn glob_match_literal_shape_boundaries() {
    let cases: &[(&[u8], &[u8])] = &[
        (b"", b""),
        (b"", b"a"),
        (b"*", b""),
        (b"*", b"a"),
        (b"**", b""),
        (b"**", b"a"),
        (b"a*", b"a"),
        (b"*a", b"a"),
        (b"*a*", b"a"),
        (b"abc", b"abc"),
        (b"abc", b"abcd"),
        (b"*abc*", b"xxabcxx"),
        (b"*abc*", b"ab"),
        (b"a?c*", b"abcdef"),
    ];
    for (p, s) in cases {
        assert_eq!(glob_match(p, s), glob_reference(p, s), "p={p:?} s={s:?}");
    }
}
