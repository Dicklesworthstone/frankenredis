use fr_expire::evaluate_expiry;
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

fn eval_and_snapshot(test_name: &str, now: u64, expiry: Option<u64>) {
    let result = evaluate_expiry(now, expiry);
    let actual = format!("{:#?}", result);
    assert_golden(test_name, &actual);
}

#[test]
fn golden_no_expiry() {
    eval_and_snapshot("no_expiry", 1000, None);
}

#[test]
fn golden_expired_past() {
    eval_and_snapshot("expired_past", 1000, Some(500));
}

#[test]
fn golden_expired_exact() {
    eval_and_snapshot("expired_exact", 1000, Some(1000));
}

#[test]
fn golden_future() {
    eval_and_snapshot("future", 1000, Some(2000));
}

#[test]
fn golden_far_future() {
    eval_and_snapshot("far_future", 0, Some(u64::MAX));
}

#[test]
fn golden_near_far_future_boundary() {
    let now_ms = 5_u64;
    let deadline = (i64::MAX as u64) + now_ms;
    eval_and_snapshot("near_far_future_boundary", now_ms, Some(deadline));
}

#[test]
fn golden_past_far_future_boundary() {
    let now_ms = 5_u64;
    let deadline = (i64::MAX as u64) + now_ms + 1;
    eval_and_snapshot("past_far_future_boundary", now_ms, Some(deadline));
}
