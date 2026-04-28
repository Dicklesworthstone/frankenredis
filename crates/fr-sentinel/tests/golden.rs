use fr_protocol::RespFrame;
use fr_sentinel::discovery::{HelloMessage, parse_replica_info_from_master};
use fr_sentinel::health::parse_info_response;
use fr_sentinel::{SentinelState, commands::dispatch_sentinel_command};
use std::fs;
use std::path::Path;

fn assert_golden(test_name: &str, actual: &str) {
    let golden_path = Path::new("tests/golden").join(format!("{}.golden", test_name));

    if std::env::var("UPDATE_GOLDENS").is_ok() {
        if let Some(parent) = golden_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&golden_path, actual);
        eprintln!("[GOLDEN] Updated: {}", golden_path.display());
        return;
    }

    let expected = match fs::read_to_string(&golden_path) {
        Ok(expected) => expected,
        Err(_) => {
            assert!(
                golden_path.exists(),
                "Golden file missing: {}\n\
             Run with UPDATE_GOLDENS=1 to create it",
                golden_path.display()
            );
            return;
        }
    };

    if actual != expected {
        let actual_path = golden_path.with_extension("actual");
        let _ = fs::write(&actual_path, actual);

        assert_eq!(
            actual,
            expected,
            "GOLDEN MISMATCH: {}\n\
             To update: UPDATE_GOLDENS=1 cargo test --test golden\n\
             To review: diff {} {}",
            test_name,
            golden_path.display(),
            actual_path.display(),
        );
    }
}

fn bulk_field<'a>(fields: &'a [RespFrame], key: &[u8]) -> Option<&'a [u8]> {
    fields
        .chunks_exact(2)
        .find_map(|pair| match (&pair[0], &pair[1]) {
            (RespFrame::BulkString(Some(field)), RespFrame::BulkString(Some(value)))
                if field == key =>
            {
                Some(value.as_slice())
            }
            _ => None,
        })
}

fn render_sentinel_list(reply: RespFrame) -> String {
    let RespFrame::Array(Some(instances)) = reply else {
        return String::new();
    };
    let mut rendered = String::new();
    for instance in instances {
        let RespFrame::Array(Some(fields)) = instance else {
            continue;
        };
        let name = String::from_utf8_lossy(bulk_field(&fields, b"name").unwrap_or_default());
        let ip = String::from_utf8_lossy(bulk_field(&fields, b"ip").unwrap_or_default());
        let port = String::from_utf8_lossy(bulk_field(&fields, b"port").unwrap_or_default());
        let flags = String::from_utf8_lossy(bulk_field(&fields, b"flags").unwrap_or_default());
        let quorum = String::from_utf8_lossy(bulk_field(&fields, b"quorum").unwrap_or_default());
        rendered.push_str(&format!(
            "{name} {ip}:{port} flags={flags} quorum={quorum}\n"
        ));
    }
    rendered
}

#[test]
fn golden_parse_info_master() {
    let info = "\
# Server
redis_version:7.2.4

# Replication
role:master
connected_slaves:2
master_replid:90a3fc4298135b62b7dd1dd0f81d110fecdfc776
master_replid2:0000000000000000000000000000000000000000
master_repl_offset:1500
";
    let parsed = parse_info_response(info);
    assert_golden("info_master", &format!("{:#?}", parsed));
}

#[test]
fn golden_parse_info_slave() {
    let info = "\
# Replication
role:slave
master_host:127.0.0.1
master_port:6379
master_link_status:up
master_last_io_seconds_ago:1
master_sync_in_progress:0
slave_repl_offset:23400
slave_priority:100
";
    let parsed = parse_info_response(info);
    assert_golden("info_slave", &format!("{:#?}", parsed));
}

#[test]
fn golden_parse_info_slave_down() {
    let info = "\
# Replication
role:slave
master_host:192.168.1.100
master_port:6379
master_link_status:down
master_link_down_since_seconds:45
slave_repl_offset:5000
slave_priority:0
";
    let parsed = parse_info_response(info);
    assert_golden("info_slave_down", &format!("{:#?}", parsed));
}

#[test]
fn golden_hello_message_valid() {
    let msg = "127.0.0.1,26379,3b5a1c0d,10,mymaster,192.168.1.50,6379,15";
    let parsed = HelloMessage::parse(msg);
    assert_golden("hello_valid", &format!("{:#?}", parsed));
}

#[test]
fn golden_hello_message_invalid_parts() {
    let msg = "127.0.0.1,26379,3b5a1c0d,10,mymaster,192.168.1.50,6379";
    let parsed = HelloMessage::parse(msg);
    assert_golden("hello_invalid", &format!("{:#?}", parsed));
}

#[test]
fn golden_replica_info_from_master() {
    let info = "\
# Replication
role:master
connected_slaves:2
slave0:ip=127.0.0.1,port=6380,state=online,offset=1000,lag=1
slave1:ip=192.168.1.101,port=6381,state=online,offset=990,lag=2
master_replid:90a3fc4298135b62b7dd1dd0f81d110fecdfc776
master_repl_offset:1000
";
    let replicas = parse_replica_info_from_master(info);
    assert_golden("replica_info_valid", &format!("{:#?}", replicas));
}

#[test]
fn golden_replica_info_empty() {
    let info = "\
# Replication
role:master
connected_slaves:0
master_replid:90a3fc4298135b62b7dd1dd0f81d110fecdfc776
master_repl_offset:1000
";
    let replicas = parse_replica_info_from_master(info);
    assert_golden("replica_info_empty", &format!("{:#?}", replicas));
}

#[test]
fn golden_sentinel_masters_sorted() {
    let mut state = SentinelState::new();
    let zeta = dispatch_sentinel_command(
        &mut state,
        &[b"MONITOR", b"zeta", b"127.0.0.2", b"6379", b"2"],
    );
    assert!(matches!(zeta, RespFrame::SimpleString(_)));
    let alpha = dispatch_sentinel_command(
        &mut state,
        &[b"MONITOR", b"alpha", b"127.0.0.1", b"6380", b"1"],
    );
    assert!(matches!(alpha, RespFrame::SimpleString(_)));

    let reply = dispatch_sentinel_command(&mut state, &[b"MASTERS"]);
    assert_golden("sentinel_masters_sorted", &render_sentinel_list(reply));
}
