use fr_config::{parse_redis_config_bytes, parse_tls_protocols, split_config_line_args_bytes};
use std::fs;
use std::path::Path;

fn assert_golden(test_name: &str, actual: &str) -> Result<(), String> {
    let golden_path = Path::new("tests/golden").join(format!("{}.golden", test_name));

    if std::env::var("UPDATE_GOLDENS").is_ok() {
        if let Some(parent) = golden_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("create golden directory {}: {err}", parent.display()))?;
        }
        fs::write(&golden_path, actual)
            .map_err(|err| format!("write golden {}: {err}", golden_path.display()))?;
        eprintln!("[GOLDEN] Updated: {}", golden_path.display());
        return Ok(());
    }

    let expected = fs::read_to_string(&golden_path).map_err(|_| {
        format!(
            "Golden file missing: {}\n\
             Run with UPDATE_GOLDENS=1 to create it",
            golden_path.display()
        )
    });
    let expected = expected?;

    if actual != expected {
        let actual_path = golden_path.with_extension("actual");
        fs::write(&actual_path, actual)
            .map_err(|err| format!("write actual {}: {err}", actual_path.display()))?;

        return Err(format!(
            "GOLDEN MISMATCH: {}\n\
             To update: UPDATE_GOLDENS=1 cargo test --test golden\n\
             To review: diff {} {}",
            test_name,
            golden_path.display(),
            actual_path.display(),
        ));
    }

    Ok(())
}

fn parse_and_snapshot(test_name: &str, input: &[u8]) -> Result<(), String> {
    match parse_redis_config_bytes(input) {
        Ok(result) => {
            let actual = format!("{:#?}", result);
            assert_golden(test_name, &actual)
        }
        Err(e) => {
            let actual = format!("Error: {:#?}", e);
            assert_golden(test_name, &actual)
        }
    }
}

fn split_and_snapshot(test_name: &str, input: &[u8]) -> Result<(), String> {
    match split_config_line_args_bytes(input) {
        Ok(result) => {
            let actual = format!(
                "{:#?}",
                result
                    .iter()
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .collect::<Vec<_>>()
            );
            assert_golden(test_name, &actual)
        }
        Err(e) => {
            let actual = format!("Error: {:#?}", e);
            assert_golden(test_name, &actual)
        }
    }
}

#[test]
fn golden_parse_basic_config() -> Result<(), String> {
    let input = b"port 6379\nbind 127.0.0.1\n# This is a comment\ntimeout 0\n";
    parse_and_snapshot("basic_config", input)
}

#[test]
fn golden_parse_quoted_strings() -> Result<(), String> {
    let input = b"requirepass \"my secret password\"\nmasterauth 'another_password'\n";
    parse_and_snapshot("quoted_strings", input)
}

#[test]
fn golden_parse_multiline() -> Result<(), String> {
    let input = b"rename-command CONFIG \"\"\nrename-command FLUSHDB \"\"\n";
    parse_and_snapshot("multiline_commands", input)
}

#[test]
fn golden_parse_invalid_quotes() -> Result<(), String> {
    let input = b"requirepass \"unclosed quote\n";
    parse_and_snapshot("invalid_quotes", input)
}

#[test]
fn golden_split_basic() -> Result<(), String> {
    split_and_snapshot("split_basic", b"port 6379")
}

#[test]
fn golden_split_quotes() -> Result<(), String> {
    split_and_snapshot("split_quotes", b"requirepass \"hello world\"")
}

#[test]
fn golden_split_escapes() -> Result<(), String> {
    split_and_snapshot("split_escapes", b"masterauth \"foo\\\"bar\\\\baz\"")
}

#[test]
fn golden_split_invalid() -> Result<(), String> {
    split_and_snapshot("split_invalid", b"requirepass \"unclosed")
}

fn parse_tls_and_snapshot(test_name: &str, input: &str) -> Result<(), String> {
    match parse_tls_protocols(input) {
        Ok(result) => {
            let actual = format!("{:#?}", result);
            assert_golden(test_name, &actual)
        }
        Err(e) => {
            let actual = format!("Error: {:#?}", e);
            assert_golden(test_name, &actual)
        }
    }
}

#[test]
fn golden_tls_protocols_valid() -> Result<(), String> {
    parse_tls_and_snapshot("tls_protocols_valid", "TLSv1.2 TLSv1.3")
}

#[test]
fn golden_tls_protocols_legacy() -> Result<(), String> {
    parse_tls_and_snapshot("tls_protocols_legacy", "TLSv1 TLSv1.1")
}

#[test]
fn golden_tls_protocols_comma_separated() -> Result<(), String> {
    parse_tls_and_snapshot("tls_protocols_comma", "TLSv1.2,TLSv1.3")
}

#[test]
fn golden_tls_protocols_invalid() -> Result<(), String> {
    parse_tls_and_snapshot("tls_protocols_invalid", "TLSv1.2 SSLv3")
}

#[test]
fn golden_tls_protocols_empty() -> Result<(), String> {
    parse_tls_and_snapshot("tls_protocols_empty", "")
}
