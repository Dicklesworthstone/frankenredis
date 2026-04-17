#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

use fr_store::glob_match;

#[derive(Debug, Arbitrary)]
enum FuzzInput {
    Raw { pattern: Vec<u8>, string: Vec<u8> },
    Structured { pattern: StructuredPattern, string: Vec<u8> },
}

#[derive(Debug, Arbitrary)]
struct StructuredPattern {
    segments: Vec<PatternSegment>,
}

#[derive(Debug, Arbitrary)]
enum PatternSegment {
    Literal(Vec<u8>),
    Star,
    Question,
    CharClass { negated: bool, chars: Vec<u8> },
    CharRange { negated: bool, start: u8, end: u8 },
    Escaped(u8),
}

impl StructuredPattern {
    fn to_bytes(&self) -> Vec<u8> {
        let mut result = Vec::new();
        for segment in &self.segments {
            match segment {
                PatternSegment::Literal(bytes) => {
                    for &b in bytes {
                        if b == b'*' || b == b'?' || b == b'[' || b == b'\\' {
                            result.push(b'\\');
                        }
                        result.push(b);
                    }
                }
                PatternSegment::Star => result.push(b'*'),
                PatternSegment::Question => result.push(b'?'),
                PatternSegment::CharClass { negated, chars } => {
                    result.push(b'[');
                    if *negated {
                        result.push(b'^');
                    }
                    for &c in chars {
                        if c == b']' || c == b'\\' {
                            result.push(b'\\');
                        }
                        result.push(c);
                    }
                    result.push(b']');
                }
                PatternSegment::CharRange { negated, start, end } => {
                    result.push(b'[');
                    if *negated {
                        result.push(b'^');
                    }
                    result.push(*start);
                    result.push(b'-');
                    result.push(*end);
                    result.push(b']');
                }
                PatternSegment::Escaped(b) => {
                    result.push(b'\\');
                    result.push(*b);
                }
            }
        }
        result
    }
}

fuzz_target!(|input: FuzzInput| {
    match input {
        FuzzInput::Raw { pattern, string } => {
            if pattern.len() > 1024 || string.len() > 4096 {
                return;
            }
            let _ = glob_match(&pattern, &string);
        }
        FuzzInput::Structured { pattern, string } => {
            if pattern.segments.len() > 32 || string.len() > 4096 {
                return;
            }

            let pattern_bytes = pattern.to_bytes();
            if pattern_bytes.len() > 1024 {
                return;
            }

            let result = glob_match(&pattern_bytes, &string);

            if pattern.segments.is_empty() {
                assert!(
                    !result || string.is_empty(),
                    "Empty pattern should only match empty string"
                );
            }

            if pattern.segments.len() == 1 {
                if let PatternSegment::Star = &pattern.segments[0] {
                    assert!(result, "Single * should match any string");
                }
            }

            if !pattern.segments.is_empty()
                && pattern.segments.iter().all(|s| matches!(s, PatternSegment::Star))
            {
                assert!(result, "All-star pattern should match any string");
            }
        }
    }
});
