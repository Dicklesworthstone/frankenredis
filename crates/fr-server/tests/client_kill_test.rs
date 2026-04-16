use fr_protocol::RespFrame;
use std::io::{Read, Write};
use std::thread;
use std::time::Duration;

#[test]
fn client_kill_by_id_disconnects_target() {
    let port = crate::tests::reserve_port();
    let _server = crate::tests::spawn_frankenredis(port, None);

    let mut target = crate::tests::connect_client(port);
    let mut killer = crate::tests::connect_client(port);

    // Get target's ID
    let res = crate::tests::send_command(&mut target, &[b"CLIENT", b"ID"]);
    let target_id = match res {
        RespFrame::Integer(id) => id,
        _ => panic!("Expected integer ID, got {:?}", res),
    };

    // Killer kills target
    let res = crate::tests::send_command(&mut killer, &[b"CLIENT", b"KILL", b"ID", target_id.to_string().as_bytes()]);
    assert_eq!(res, RespFrame::Integer(1));

    // Wait for event loop to process kill
    thread::sleep(Duration::from_millis(200));

    // Target should be disconnected
    target.write_all(b"*1\r\n$4\r\nPING\r\n").unwrap_or(());
    let mut buf = [0u8; 1024];
    let read_res = target.read(&mut buf);
    assert!(read_res.unwrap_or(0) == 0, "Target should have been disconnected");

    crate::tests::send_shutdown_nosave(port);
}
