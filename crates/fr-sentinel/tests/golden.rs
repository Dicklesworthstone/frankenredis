use fr_sentinel::discovery::{HelloMessage, parse_replica_info_from_master};
use fr_sentinel::health::parse_info_response;
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
