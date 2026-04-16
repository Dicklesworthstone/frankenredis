use fr_protocol::RespFrame;
use fr_runtime::Runtime;

#[test]
fn stale_replica_blocks_reads_when_configured() {
    let mut rt = Runtime::default_strict();
    
    // Configure as a replica
    rt.execute_frame(RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(b"REPLICAOF".to_vec())),
        RespFrame::BulkString(Some(b"127.0.0.1".to_vec())),
        RespFrame::BulkString(Some(b"6379".to_vec())),
    ])), 0);
    
    // Set replica-serve-stale-data to no
    rt.execute_frame(RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(b"CONFIG".to_vec())),
        RespFrame::BulkString(Some(b"SET".to_vec())),
        RespFrame::BulkString(Some(b"replica-serve-stale-data".to_vec())),
        RespFrame::BulkString(Some(b"no".to_vec())),
    ])), 100);
    
    // GET should be blocked (state = connect)
    let res = rt.execute_frame(RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(b"GET".to_vec())),
        RespFrame::BulkString(Some(b"foo".to_vec())),
    ])), 200);
    assert_eq!(res, RespFrame::Error("MASTERDOWN Link with MASTER is down and replica-serve-stale-data is set to 'no'.".to_string()));

    // INFO should be allowed
    let res = rt.execute_frame(RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(b"INFO".to_vec())),
    ])), 300);
    assert!(matches!(res, RespFrame::BulkString(Some(_))));
}
