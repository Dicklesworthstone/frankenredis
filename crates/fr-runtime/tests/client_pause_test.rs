//! Integration tests for CLIENT PAUSE / CLIENT UNPAUSE behavior.

use fr_protocol::RespFrame;
use fr_runtime::Runtime;

fn command(parts: &[&[u8]]) -> RespFrame {
    RespFrame::Array(Some(
        parts
            .iter()
            .map(|part| RespFrame::BulkString(Some((*part).to_vec())))
            .collect(),
    ))
}

#[test]
fn client_pause_default_blocks_all_commands() {
    let mut rt = Runtime::default_strict();

    // Default CLIENT PAUSE mode is ALL (not WRITE)
    let pause = rt.execute_frame(command(&[b"CLIENT", b"PAUSE", b"5000"]), 1000);
    assert_eq!(pause, RespFrame::SimpleString("OK".to_string()));

    // Both write and read commands should be paused in ALL mode
    assert!(rt.is_command_paused(&[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()], 2000));
    assert!(rt.is_command_paused(&[b"GET".to_vec(), b"k".to_vec()], 2000));
}

#[test]
fn client_pause_write_mode_only_blocks_writes() {
    let mut rt = Runtime::default_strict();

    // WRITE mode only blocks write commands
    let pause = rt.execute_frame(command(&[b"CLIENT", b"PAUSE", b"5000", b"WRITE"]), 1000);
    assert_eq!(pause, RespFrame::SimpleString("OK".to_string()));

    // Write commands should be paused
    assert!(rt.is_command_paused(&[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()], 2000));

    // Read commands should NOT be paused in WRITE mode
    assert!(!rt.is_command_paused(&[b"GET".to_vec(), b"k".to_vec()], 2000));
}

#[test]
fn client_pause_all_blocks_all_commands() {
    let mut rt = Runtime::default_strict();

    // Pause for 5000ms in ALL mode
    let pause = rt.execute_frame(command(&[b"CLIENT", b"PAUSE", b"5000", b"ALL"]), 1000);
    assert_eq!(pause, RespFrame::SimpleString("OK".to_string()));

    // Both write and read commands should be paused
    assert!(rt.is_command_paused(&[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()], 2000));
    assert!(rt.is_command_paused(&[b"GET".to_vec(), b"k".to_vec()], 2000));
}

#[test]
fn client_pause_expires_after_timeout() {
    let mut rt = Runtime::default_strict();

    // Pause for 3000ms
    let pause = rt.execute_frame(command(&[b"CLIENT", b"PAUSE", b"3000"]), 1000);
    assert_eq!(pause, RespFrame::SimpleString("OK".to_string()));

    // Should be paused at 2000ms
    assert!(rt.is_client_paused(2000));

    // Should NOT be paused at 4001ms (deadline = 1000 + 3000 = 4000)
    assert!(!rt.is_client_paused(4001));
}

#[test]
fn client_unpause_clears_pause() {
    let mut rt = Runtime::default_strict();

    // Pause for 10000ms
    rt.execute_frame(command(&[b"CLIENT", b"PAUSE", b"10000"]), 1000);
    assert!(rt.is_client_paused(2000));

    // Unpause
    let unpause = rt.execute_frame(command(&[b"CLIENT", b"UNPAUSE"]), 3000);
    assert_eq!(unpause, RespFrame::SimpleString("OK".to_string()));

    // Should no longer be paused
    assert!(!rt.is_client_paused(3001));
}

#[test]
fn client_pause_zero_is_noop() {
    let mut rt = Runtime::default_strict();

    // Pause with 0ms should effectively not pause (or pause for 0ms)
    let pause = rt.execute_frame(command(&[b"CLIENT", b"PAUSE", b"0"]), 1000);
    assert_eq!(pause, RespFrame::SimpleString("OK".to_string()));

    // Should not be paused after the pause call
    assert!(!rt.is_client_paused(1001));
}

#[test]
fn client_pause_wrong_arity() {
    let mut rt = Runtime::default_strict();

    let no_args = rt.execute_frame(command(&[b"CLIENT", b"PAUSE"]), 0);
    assert!(matches!(no_args, RespFrame::Error(_)));
}

#[test]
fn client_pause_invalid_timeout() {
    let mut rt = Runtime::default_strict();

    // -1 is clamped to 0 (unpause), so it's actually OK — not an error
    let neg = rt.execute_frame(command(&[b"CLIENT", b"PAUSE", b"-1"]), 0);
    assert_eq!(neg, RespFrame::SimpleString("OK".to_string()));

    let notnum = rt.execute_frame(command(&[b"CLIENT", b"PAUSE", b"abc"]), 0);
    assert!(matches!(notnum, RespFrame::Error(_)));
}

#[test]
fn client_pause_commands_execute_after_expiry() {
    let mut rt = Runtime::default_strict();

    // Set a key first
    rt.execute_frame(command(&[b"SET", b"pause_key", b"before"]), 0);

    // Pause for 2000ms
    rt.execute_frame(command(&[b"CLIENT", b"PAUSE", b"2000"]), 1000);

    // At 2000ms, pause is active — command should be paused
    assert!(rt.is_command_paused(
        &[b"SET".to_vec(), b"pause_key".to_vec(), b"during".to_vec()],
        2000
    ));

    // At 3001ms, pause has expired — command can proceed
    assert!(!rt.is_command_paused(
        &[b"SET".to_vec(), b"pause_key".to_vec(), b"after".to_vec()],
        3001
    ));

    // Execute SET after pause expires
    rt.execute_frame(command(&[b"SET", b"pause_key", b"after"]), 3001);
    let val = rt.execute_frame(command(&[b"GET", b"pause_key"]), 3002);
    assert_eq!(val, RespFrame::BulkString(Some(b"after".to_vec())));
}
