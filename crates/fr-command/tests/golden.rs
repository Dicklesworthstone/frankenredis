use fr_command::{parse_client_tracking_state, parse_migrate_request};
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

fn to_argv(args: &[&str]) -> Vec<Vec<u8>> {
    args.iter().map(|s| s.as_bytes().to_vec()).collect()
}

fn parse_migrate_and_snapshot(test_name: &str, args: &[&str]) {
    let argv = to_argv(args);
    match parse_migrate_request(&argv) {
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

fn parse_tracking_and_snapshot(test_name: &str, args: &[&str]) {
    let argv = to_argv(args);
    match parse_client_tracking_state(&argv) {
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
fn golden_migrate_basic() {
    parse_migrate_and_snapshot("migrate_basic", &["MIGRATE", "192.168.1.34", "6379", "mykey", "0", "5000"]);
}

#[test]
fn golden_migrate_copy_replace() {
    parse_migrate_and_snapshot("migrate_copy_replace", &["MIGRATE", "127.0.0.1", "7777", "", "1", "2000", "COPY", "REPLACE", "KEYS", "k1", "k2"]);
}

#[test]
fn golden_migrate_auth() {
    parse_migrate_and_snapshot("migrate_auth", &["MIGRATE", "127.0.0.1", "6379", "key1", "0", "1000", "AUTH", "secret"]);
}

#[test]
fn golden_migrate_auth2() {
    parse_migrate_and_snapshot("migrate_auth2", &["MIGRATE", "127.0.0.1", "6379", "key1", "0", "1000", "AUTH2", "user", "secret"]);
}

#[test]
fn golden_migrate_invalid_port() {
    parse_migrate_and_snapshot("migrate_invalid_port", &["MIGRATE", "127.0.0.1", "65536", "key1", "0", "1000"]);
}

#[test]
fn golden_tracking_on() {
    parse_tracking_and_snapshot("tracking_on", &["CLIENT", "TRACKING", "ON"]);
}

#[test]
fn golden_tracking_off() {
    parse_tracking_and_snapshot("tracking_off", &["CLIENT", "TRACKING", "OFF"]);
}

#[test]
fn golden_tracking_bcast() {
    parse_tracking_and_snapshot("tracking_bcast", &["CLIENT", "TRACKING", "ON", "BCAST"]);
}

#[test]
fn golden_tracking_optin() {
    parse_tracking_and_snapshot("tracking_optin", &["CLIENT", "TRACKING", "ON", "OPTIN"]);
}

#[test]
fn golden_tracking_optout() {
    parse_tracking_and_snapshot("tracking_optout", &["CLIENT", "TRACKING", "ON", "OPTOUT"]);
}

#[test]
fn golden_tracking_redirect() {
    parse_tracking_and_snapshot("tracking_redirect", &["CLIENT", "TRACKING", "ON", "REDIRECT", "12345"]);
}

#[test]
fn golden_tracking_prefixes() {
    parse_tracking_and_snapshot("tracking_prefixes", &["CLIENT", "TRACKING", "ON", "BCAST", "PREFIX", "foo:", "PREFIX", "bar:"]);
}

#[test]
fn golden_tracking_noloop() {
    parse_tracking_and_snapshot("tracking_noloop", &["CLIENT", "TRACKING", "ON", "NOLOOP"]);
}

#[test]
fn golden_tracking_invalid() {
    parse_tracking_and_snapshot("tracking_invalid", &["CLIENT", "TRACKING", "YES"]);
}
