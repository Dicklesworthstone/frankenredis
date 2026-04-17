use fr_protocol::RespFrame;
use fr_runtime::Runtime;

#[test]
fn noreplicas_blocks_writes_when_not_enough_replicas() {
    let mut rt = Runtime::default_strict();

    // Set min-replicas-to-write to 1
    rt.execute_frame(
        RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"CONFIG".to_vec())),
            RespFrame::BulkString(Some(b"SET".to_vec())),
            RespFrame::BulkString(Some(b"min-replicas-to-write".to_vec())),
            RespFrame::BulkString(Some(b"1".to_vec())),
        ])),
        0,
    );

    // Write should be blocked
    let res = rt.execute_frame(
        RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"SET".to_vec())),
            RespFrame::BulkString(Some(b"foo".to_vec())),
            RespFrame::BulkString(Some(b"bar".to_vec())),
        ])),
        1000,
    );
    assert_eq!(
        res,
        RespFrame::Error("NOREPLICAS Not enough good replicas to write.".to_string())
    );

    // Reads should still be allowed
    let res = rt.execute_frame(
        RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"GET".to_vec())),
            RespFrame::BulkString(Some(b"foo".to_vec())),
        ])),
        2000,
    );
    assert_eq!(res, RespFrame::BulkString(None));
}
