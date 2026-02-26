use std::path::Path;

use fr_conformance::{
    HarnessConfig, run_fixture, run_protocol_fixture, run_replay_fixture, run_smoke,
};

#[test]
fn smoke_report_is_stable() {
    let cfg = HarnessConfig::default_paths();
    let report = run_smoke(&cfg);
    assert_eq!(report.suite, "smoke");
    assert!(report.fixture_count >= 1);
    assert!(report.oracle_present);

    let fixture_path = cfg.fixture_root.join("core_strings.json");
    assert!(Path::new(&fixture_path).exists());

    let diff = run_fixture(&cfg, "core_strings.json").expect("fixture runs");
    assert_eq!(diff.total, diff.passed);
    assert!(diff.failed.is_empty());

    let errors = run_fixture(&cfg, "core_errors.json").expect("error fixture");
    assert_eq!(errors.total, errors.passed);
    assert!(errors.failed.is_empty());

    let dispatch =
        run_fixture(&cfg, "fr_p2c_003_dispatch_journey.json").expect("packet-003 dispatch fixture");
    assert_eq!(dispatch.total, dispatch.passed);
    assert!(dispatch.failed.is_empty());

    let auth_acl =
        run_fixture(&cfg, "fr_p2c_004_acl_journey.json").expect("packet-004 auth/acl fixture");
    assert_eq!(auth_acl.total, auth_acl.passed);
    assert!(auth_acl.failed.is_empty());

    let protocol = run_protocol_fixture(&cfg, "protocol_negative.json").expect("protocol fixture");
    assert_eq!(protocol.total, protocol.passed);
    assert!(protocol.failed.is_empty());

    let replay = run_replay_fixture(&cfg, "persist_replay.json").expect("replay fixture");
    assert_eq!(replay.total, replay.passed);
    assert!(replay.failed.is_empty());
}

#[test]
fn fr_p2c_001_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "fr_p2c_001_eventloop_journey.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed);
    assert!(diff.failed.is_empty());
}

#[test]
fn fr_p2c_002_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_protocol_fixture(&cfg, "protocol_negative.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed);
    assert!(diff.failed.is_empty());
}

#[test]
fn fr_p2c_003_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "fr_p2c_003_dispatch_journey.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed);
    assert!(diff.failed.is_empty());
}

#[test]
fn fr_p2c_004_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "fr_p2c_004_acl_journey.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed);
    assert!(diff.failed.is_empty());
}

#[test]
fn fr_p2c_005_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_replay_fixture(&cfg, "persist_replay.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed);
    assert!(diff.failed.is_empty());
}

#[test]
fn fr_p2c_006_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "fr_p2c_006_replication_journey.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed);
    assert!(diff.failed.is_empty());
}

#[test]
fn fr_p2c_007_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "fr_p2c_007_cluster_journey.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed);
    assert!(diff.failed.is_empty());
}

#[test]
fn fr_p2c_008_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "fr_p2c_008_expire_evict_journey.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed);
    assert!(diff.failed.is_empty());
}

#[test]
fn fr_p2c_009_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "fr_p2c_009_tls_config_journey.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_hash_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_hash.json").expect("hash fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_list_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_list.json").expect("list fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_set_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_set.json").expect("set fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_zset_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_zset.json").expect("zset fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_geo_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_geo.json").expect("geo fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_stream_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_stream.json").expect("stream fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_generic_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_generic.json").expect("generic fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_acl_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_acl.json").expect("acl fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_hyperloglog_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_hyperloglog.json").expect("hyperloglog fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_bitmap_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_bitmap.json").expect("bitmap fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_transaction_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_transaction.json").expect("transaction fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_connection_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_connection.json").expect("connection fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_expiry_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_expiry.json").expect("expiry fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_client_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_client.json").expect("client fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_server_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_server.json").expect("server fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_scripting_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_scripting.json").expect("scripting fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_pubsub_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_pubsub.json").expect("pubsub fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_replication_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_replication.json").expect("replication fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}
