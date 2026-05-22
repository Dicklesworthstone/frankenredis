#![forbid(unsafe_code)]

use crate::{INFO_PERIOD_MS, InstanceLink, PING_PERIOD_MS, Role, SentinelRedisInstance};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HealthCheckResult {
    pub should_mark_s_down: bool,
    pub should_clear_s_down: bool,
    pub should_send_ping: bool,
    pub reason: Option<&'static str>,
}

pub fn evaluate_instance_health(instance: &SentinelRedisInstance, now: u64) -> HealthCheckResult {
    let mut result = HealthCheckResult::default();

    let elapsed_since_pong = now.saturating_sub(instance.link.last_pong_time);
    let elapsed_since_ping = now.saturating_sub(instance.link.last_ping_time);

    if elapsed_since_ping >= PING_PERIOD_MS {
        result.should_send_ping = true;
    }

    if instance.link.disconnected {
        let disconnected_elapsed = if instance.link.act_ping_time > 0 {
            now.saturating_sub(instance.link.act_ping_time)
        } else {
            now.saturating_sub(instance.link.last_avail_time)
        };
        if !instance.is_s_down() && disconnected_elapsed > instance.down_after_period {
            result.should_mark_s_down = true;
            result.reason = Some("disconnected longer than down-after-period");
        }
        return result;
    }

    if instance.link.act_ping_time > 0 {
        let ping_in_flight_duration = now.saturating_sub(instance.link.act_ping_time);
        if ping_in_flight_duration > instance.down_after_period {
            if !instance.is_s_down() {
                result.should_mark_s_down = true;
                result.reason = Some("no PONG received within down-after-period");
            }
            return result;
        }
    }

    if instance.is_master()
        && instance.role_reported == Role::Slave
        && now.saturating_sub(instance.role_reported_time)
            > instance
                .down_after_period
                .saturating_add(INFO_PERIOD_MS.saturating_mul(2))
    {
        if !instance.is_s_down() {
            result.should_mark_s_down = true;
            result.reason = Some("master reports role=slave past grace period");
        }
        return result;
    }

    if elapsed_since_pong > instance.down_after_period {
        if !instance.is_s_down() {
            result.should_mark_s_down = true;
            result.reason = Some("last PONG too old");
        }
        return result;
    }

    if instance.is_s_down() && elapsed_since_pong < instance.down_after_period / 2 {
        result.should_clear_s_down = true;
        result.reason = Some("recent valid PONG received");
    }

    result
}

pub fn apply_health_result(
    instance: &mut SentinelRedisInstance,
    result: &HealthCheckResult,
    now: u64,
) {
    if result.should_mark_s_down {
        instance.set_s_down(true, now);
    } else if result.should_clear_s_down {
        instance.set_s_down(false, now);
    }
}

pub fn record_pong(link: &mut InstanceLink, now: u64) {
    link.last_pong_time = now;
    link.act_ping_time = 0;
    link.last_avail_time = now;
}

pub fn record_ping_sent(link: &mut InstanceLink, now: u64) {
    link.last_ping_time = now;
    if link.act_ping_time == 0 {
        link.act_ping_time = now;
    }
}

pub fn record_disconnect(link: &mut InstanceLink) {
    link.disconnected = true;
    link.pending_commands = 0;
}

pub fn record_reconnect(link: &mut InstanceLink, now: u64) {
    link.disconnected = false;
    link.last_reconn_time = now;
    link.cc_conn_time = now;
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedInfo {
    pub role: Option<Role>,
    pub master_host: Option<String>,
    pub master_port: Option<u16>,
    pub master_link_status: Option<bool>,
    pub master_link_down_since: Option<u64>,
    pub slave_repl_offset: Option<u64>,
    pub slave_priority: Option<u32>,
    pub run_id: Option<String>,
    pub connected_slaves: Option<u32>,
}

pub fn parse_info_response(info: &str) -> ParsedInfo {
    let mut result = ParsedInfo::default();

    for line in info.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some((key, value)) = line.split_once(':') {
            match key {
                "role" => {
                    if let Some(role) = match value {
                        "master" => Some(Role::Master),
                        "slave" => Some(Role::Slave),
                        _ => None,
                    } {
                        result.role = Some(role);
                    }
                }
                "master_host" => {
                    result.master_host = Some(value.to_string());
                }
                "master_port" => {
                    result.master_port = value.parse().ok();
                }
                "master_link_status" => {
                    result.master_link_status = Some(value.eq_ignore_ascii_case("up"));
                }
                "master_link_down_since_seconds" => {
                    result.master_link_down_since = value
                        .parse::<u64>()
                        .ok()
                        .map(|seconds| seconds.saturating_mul(1000));
                }
                "slave_repl_offset" | "master_repl_offset"
                    if result.slave_repl_offset.is_none() =>
                {
                    result.slave_repl_offset = value.parse().ok();
                }
                "slave_priority" | "replica_priority" => {
                    result.slave_priority = value.parse().ok();
                }
                "run_id" => {
                    if let Some(run_id) = parse_redis_run_id(value) {
                        result.run_id = Some(run_id);
                    }
                }
                "connected_slaves" => {
                    result.connected_slaves = value.parse().ok();
                }
                _ => {}
            }
        }
    }

    result
}

fn parse_redis_run_id(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if bytes.len() < 40 {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes[..40]).into_owned())
}

pub fn apply_info_to_instance(instance: &mut SentinelRedisInstance, info: &ParsedInfo, now: u64) {
    instance.info_refresh = now;

    if let Some(role) = info.role {
        let old_role = instance.role_reported;
        instance.role_reported = role;
        if old_role != role {
            instance.role_reported_time = now;
        }
    }

    if let Some(ref host) = info.master_host {
        instance.slave_master_host = Some(host.clone());
    }
    if let Some(port) = info.master_port {
        instance.slave_master_port = Some(port);
    }
    if let Some(up) = info.master_link_status {
        instance.slave_master_link_status = if up {
            crate::LinkStatus::Up
        } else {
            crate::LinkStatus::Down
        };
    }
    instance.master_link_down_time = info.master_link_down_since.unwrap_or(0);
    if let Some(offset) = info.slave_repl_offset {
        instance.slave_repl_offset = offset;
    }
    if let Some(priority) = info.slave_priority {
        instance.slave_priority = priority;
    }
    if let Some(ref runid) = info.run_id {
        instance.runid = Some(runid.clone());
    }
}

pub fn record_info_response(
    instance: &mut SentinelRedisInstance,
    info: impl Into<String>,
    now: u64,
) {
    let info = info.into();
    let parsed = parse_info_response(&info);
    apply_info_to_instance(instance, &parsed, now);
    instance.info = Some(info);
}

pub fn check_role_mismatch(instance: &SentinelRedisInstance) -> Option<&'static str> {
    if instance.is_master() && instance.role_reported == Role::Slave {
        return Some("instance reports role=slave but we expect master");
    }
    if instance.is_slave() && instance.role_reported == Role::Master {
        return Some("instance reports role=master but we expect slave");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InstanceFlags, SentinelAddr};

    fn make_instance() -> SentinelRedisInstance {
        let addr = SentinelAddr::new("127.0.0.1", 6379);
        SentinelRedisInstance::new_master("test", addr, 2)
    }

    #[test]
    fn health_check_healthy_instance() {
        let mut instance = make_instance();
        instance.link.last_pong_time = 1000;
        instance.link.last_ping_time = 500;

        let result = evaluate_instance_health(&instance, 1500);
        assert!(!result.should_mark_s_down);
        assert!(!result.should_clear_s_down);
        assert!(result.should_send_ping);
    }

    #[test]
    fn health_check_no_pong_marks_s_down() {
        let mut instance = make_instance();
        instance.link.last_pong_time = 0;
        instance.link.act_ping_time = 1000;

        let result = evaluate_instance_health(&instance, 32000);
        assert!(result.should_mark_s_down);
        assert_eq!(
            result.reason,
            Some("no PONG received within down-after-period")
        );
    }

    #[test]
    fn health_check_disconnected_instance() {
        let mut instance = make_instance();
        instance.link.disconnected = true;
        instance.link.last_pong_time = 0;

        let result = evaluate_instance_health(&instance, 35000);
        assert!(result.should_mark_s_down);
        assert_eq!(
            result.reason,
            Some("disconnected longer than down-after-period")
        );
    }

    #[test]
    fn health_check_disconnected_uses_last_available_reply() {
        let mut instance = make_instance();
        instance.link.disconnected = true;
        instance.link.last_pong_time = 0;
        instance.link.last_avail_time = 34_500;

        let result = evaluate_instance_health(&instance, 35_000);

        assert!(!result.should_mark_s_down);
    }

    #[test]
    fn health_check_master_role_mismatch_marks_s_down_after_grace() {
        let mut instance = make_instance();
        let grace = instance
            .down_after_period
            .saturating_add(INFO_PERIOD_MS.saturating_mul(2));
        instance.role_reported = Role::Slave;
        instance.role_reported_time = 1_000;
        instance.link.last_pong_time = 1_000 + grace + 1;

        let at_boundary = evaluate_instance_health(&instance, 1_000 + grace);
        assert!(!at_boundary.should_mark_s_down);

        let after_boundary = evaluate_instance_health(&instance, 1_000 + grace + 1);
        assert!(after_boundary.should_mark_s_down);
        assert_eq!(
            after_boundary.reason,
            Some("master reports role=slave past grace period")
        );
    }

    #[test]
    fn health_check_clears_s_down_on_pong() {
        let mut instance = make_instance();
        instance.flags.insert(InstanceFlags::S_DOWN);
        instance.link.last_pong_time = 9000;

        let result = evaluate_instance_health(&instance, 10000);
        assert!(result.should_clear_s_down);
        assert_eq!(result.reason, Some("recent valid PONG received"));
    }

    #[test]
    fn parse_info_master() {
        let info = r#"
# Replication
role:master
connected_slaves:2
run_id:0123456789abcdef0123456789abcdef01234567
master_repl_offset:12345
"#;
        let parsed = parse_info_response(info);
        assert_eq!(parsed.role, Some(Role::Master));
        assert_eq!(parsed.connected_slaves, Some(2));
        assert_eq!(
            parsed.run_id,
            Some("0123456789abcdef0123456789abcdef01234567".to_string())
        );
        assert_eq!(parsed.slave_repl_offset, Some(12345));
    }

    #[test]
    fn parse_info_run_id_matches_redis_fixed_width() {
        let short = parse_info_response("role:master\nrun_id:abc123\n");
        assert_eq!(short.run_id, None);

        let long = parse_info_response(
            "role:master\nrun_id:0123456789abcdef0123456789abcdef0123456789extra\n",
        );
        assert_eq!(
            long.run_id,
            Some("0123456789abcdef0123456789abcdef01234567".to_string())
        );
    }

    #[test]
    fn parse_info_ignores_malformed_duplicate_role_lines() {
        let info = r#"
# Replication
role:master
role:master:with-extra-colon
connected_slaves:0
"#;
        let parsed = parse_info_response(info);
        assert_eq!(parsed.role, Some(Role::Master));
        assert_eq!(parsed.connected_slaves, Some(0));
    }

    #[test]
    fn parse_info_marks_non_up_master_link_status_down() {
        let info = r#"
# Replication
role:slave
master_link_status:up
master_link_status:up:with-extra-colon
"#;
        let parsed = parse_info_response(info);
        assert_eq!(parsed.master_link_status, Some(false));

        let malformed_only = parse_info_response("role:slave\nmaster_link_status:maybe\n");
        assert_eq!(malformed_only.master_link_status, Some(false));

        let uppercase_up = parse_info_response("role:slave\nmaster_link_status:UP\n");
        assert_eq!(uppercase_up.master_link_status, Some(true));
    }

    #[test]
    fn parse_info_slave() {
        let info = r#"
# Replication
role:slave
master_host:192.168.1.1
master_port:6379
master_link_status:up
slave_repl_offset:54321
slave_priority:100
"#;
        let parsed = parse_info_response(info);
        assert_eq!(parsed.role, Some(Role::Slave));
        assert_eq!(parsed.master_host, Some("192.168.1.1".to_string()));
        assert_eq!(parsed.master_port, Some(6379));
        assert_eq!(parsed.master_link_status, Some(true));
        assert_eq!(parsed.slave_repl_offset, Some(54321));
        assert_eq!(parsed.slave_priority, Some(100));
    }

    #[test]
    fn parse_info_saturates_huge_link_down_duration() {
        let parsed = parse_info_response(
            "role:slave\nmaster_link_down_since_seconds:18446744073709551615\n",
        );

        assert_eq!(parsed.master_link_down_since, Some(u64::MAX));
    }

    #[test]
    fn apply_info_updates_instance() {
        let mut instance = make_instance();
        let info = ParsedInfo {
            role: Some(Role::Master),
            run_id: Some("test123".to_string()),
            master_link_down_since: Some(12_000),
            slave_repl_offset: Some(99999),
            ..Default::default()
        };

        apply_info_to_instance(&mut instance, &info, 5000);
        assert_eq!(instance.role_reported, Role::Master);
        assert_eq!(instance.runid, Some("test123".to_string()));
        assert_eq!(instance.master_link_down_time, 12_000);
        assert_eq!(instance.slave_repl_offset, 99999);
        assert_eq!(instance.info_refresh, 5000);
    }

    #[test]
    fn apply_info_clears_missing_master_link_down_time() {
        let mut instance = make_instance();
        instance.master_link_down_time = 42_000;
        let info = ParsedInfo {
            role: Some(Role::Slave),
            master_link_down_since: None,
            ..Default::default()
        };

        apply_info_to_instance(&mut instance, &info, 6000);

        assert_eq!(instance.master_link_down_time, 0);
    }

    #[test]
    fn record_info_response_preserves_raw_cache_payload() {
        let mut instance = make_instance();
        let raw =
            "role:master\nrun_id:abcdef0123456789abcdef0123456789abcdef01\nmaster_repl_offset:42\n";

        record_info_response(&mut instance, raw, 7000);

        assert_eq!(instance.info_refresh, 7000);
        assert_eq!(instance.info.as_deref(), Some(raw));
        assert_eq!(
            instance.runid.as_deref(),
            Some("abcdef0123456789abcdef0123456789abcdef01")
        );
        assert_eq!(instance.slave_repl_offset, 42);
    }

    #[test]
    fn role_mismatch_detection() {
        let mut instance = make_instance();
        instance.role_reported = Role::Slave;

        assert!(check_role_mismatch(&instance).is_some());

        instance.role_reported = Role::Master;
        assert!(check_role_mismatch(&instance).is_none());
    }

    #[test]
    fn record_pong_clears_act_ping() {
        let mut link = InstanceLink {
            act_ping_time: 1000,
            ..Default::default()
        };

        record_pong(&mut link, 2000);
        assert_eq!(link.last_pong_time, 2000);
        assert_eq!(link.act_ping_time, 0);
        assert_eq!(link.last_avail_time, 2000);
    }

    #[test]
    fn record_ping_sent_sets_act_ping_once() {
        let mut link = InstanceLink::default();

        record_ping_sent(&mut link, 1000);
        assert_eq!(link.last_ping_time, 1000);
        assert_eq!(link.act_ping_time, 1000);

        record_ping_sent(&mut link, 2000);
        assert_eq!(link.last_ping_time, 2000);
        assert_eq!(link.act_ping_time, 1000);
    }
}
