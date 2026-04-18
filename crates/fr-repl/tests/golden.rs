use fr_repl::parse_psync_reply;
use std::fs;
use std::path::Path;

fn assert_golden(test_name: &str, actual: &str) {
    let golden_path = Path::new("tests/golden").join(format!("{}.golden", test_name));

    if std::env::var("UPDATE_GOLDENS").is_ok() {
        fs::create_dir_all(golden_path.parent().unwrap()).unwrap();
        fs::write(&golden_path, actual).unwrap();
        eprintln!("[GOLDEN] Updated: {}", golden_path.display());
        return;
    }

    let expected = fs::read_to_string(&golden_path).unwrap_or_else(|_| {
        panic!(
            "Golden file missing: {}\n\
             Run with UPDATE_GOLDENS=1 to create it",
            golden_path.display()
        )
    });

    if actual != expected {
        let actual_path = golden_path.with_extension("actual");
        fs::write(&actual_path, actual).unwrap();

        panic!(
            "GOLDEN MISMATCH: {}\n\
             To update: UPDATE_GOLDENS=1 cargo test --test golden\n\
             To review: diff {} {}",
            test_name,
            golden_path.display(),
            actual_path.display(),
        );
    }
}

fn parse_and_snapshot(test_name: &str, line: &str) {
    match parse_psync_reply(line) {
        Ok(result) => {
            let actual = format!("{:#?}", result);
            assert_golden(test_name, &actual);
        }
        Err(e) => {
            let actual = format!("Error: {:#?}", e);
            assert_golden(test_name, &actual);
        }
    }
}

#[test]
fn golden_psync_continue() {
    parse_and_snapshot("psync_continue", "CONTINUE");
}

#[test]
fn golden_psync_continue_with_trailing() {
    parse_and_snapshot("psync_continue_trailing", "CONTINUE xyz");
}

#[test]
fn golden_psync_continue_invalid() {
    parse_and_snapshot("psync_continue_invalid", "CONTINUE arg1 arg2");
}

#[test]
fn golden_psync_fullresync() {
    parse_and_snapshot("psync_fullresync", "FULLRESYNC 1234567890abcdef1234567890abcdef12345678 100500");
}

#[test]
fn golden_psync_fullresync_missing_args() {
    parse_and_snapshot("psync_fullresync_missing_args", "FULLRESYNC 1234567890abcdef");
}

#[test]
fn golden_psync_fullresync_invalid_offset() {
    parse_and_snapshot("psync_fullresync_invalid_offset", "FULLRESYNC 1234567890abcdef1234567890abcdef12345678 bad_offset");
}

#[test]
fn golden_psync_invalid_reply() {
    parse_and_snapshot("psync_invalid_reply", "NOMATCH");
}

#[test]
fn golden_psync_empty() {
    parse_and_snapshot("psync_empty", "");
}
