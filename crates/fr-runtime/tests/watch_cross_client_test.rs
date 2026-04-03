use fr_protocol::RespFrame;
use fr_runtime::Runtime;

fn command(args: &[&[u8]]) -> RespFrame {
    RespFrame::Array(Some(
        args.iter()
            .map(|a| RespFrame::BulkString(Some(a.to_vec())))
            .collect(),
    ))
}

#[test]
fn test_watch_ignores_unrelated_key_modifications() {
    let mut rt = Runtime::default_strict();

    // Client A (session 1) WATCHes a key
    rt.execute_frame(command(&[b"SET", b"mykey", b"1"]), 0);
    rt.execute_frame(command(&[b"WATCH", b"mykey"]), 1);

    // Swap to Client B (session 2)
    let mut session_b = fr_runtime::ClientSession::new_for_server(&rt.server);
    session_b.client_id = 2;
    let session_a = rt.swap_session(session_b);

    // Client B modifies an UNRELATED key, bumping the global dirty counter
    rt.execute_frame(command(&[b"SET", b"otherkey", b"2"]), 2);

    // Swap back to Client A
    rt.swap_session(session_a);

    // Client A executes transaction
    rt.execute_frame(command(&[b"MULTI"]), 3);
    rt.execute_frame(command(&[b"GET", b"mykey"]), 4);

    let exec = rt.execute_frame(command(&[b"EXEC"]), 5);

    // Exec should succeed because `mykey` wasn't modified!
    assert_eq!(
        exec,
        RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"1".to_vec()))]))
    );
}
