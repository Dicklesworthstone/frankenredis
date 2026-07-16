#![forbid(unsafe_code)]

use crate::{InstanceFlags, PUBLISH_PERIOD_MS, SentinelAddr, SentinelRedisInstance, SentinelState};

pub const HELLO_CHANNEL: &str = "__sentinel__:hello";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelloMessage {
    pub sentinel_ip: String,
    pub sentinel_port: u16,
    pub sentinel_runid: String,
    pub current_epoch: u64,
    pub master_name: String,
    pub master_ip: String,
    pub master_port: u16,
    pub master_config_epoch: u64,
}

impl HelloMessage {
    pub fn encode(&self) -> String {
        format!(
            "{},{},{},{},{},{},{},{}",
            self.sentinel_ip,
            self.sentinel_port,
            self.sentinel_runid,
            self.current_epoch,
            self.master_name,
            self.master_ip,
            self.master_port,
            self.master_config_epoch
        )
    }

    #[cfg_attr(feature = "bench-reference", inline(never))]
    pub fn parse(message: &str) -> Option<HelloMessage> {
        let mut parts = message.split(',');
        let sentinel_ip = parts.next()?;
        let sentinel_port = parts.next()?;
        let sentinel_runid = parts.next()?;
        let current_epoch = parts.next()?;
        let master_name = parts.next()?;
        let master_ip = parts.next()?;
        let master_port = parts.next()?;
        let master_config_epoch = parts.next()?;
        if parts.next().is_some() {
            return None;
        }

        Some(HelloMessage {
            sentinel_ip: sentinel_ip.to_string(),
            sentinel_port: parse_redis_hello_port(sentinel_port)?,
            sentinel_runid: sentinel_runid.to_string(),
            current_epoch: parse_redis_strtoull_prefix(current_epoch),
            master_name: master_name.to_string(),
            master_ip: master_ip.to_string(),
            master_port: parse_redis_hello_port(master_port)?,
            master_config_epoch: parse_redis_strtoull_prefix(master_config_epoch),
        })
    }
}

#[cfg(feature = "bench-reference")]
#[inline(never)]
pub fn bench_parse_hello_collect_reference(message: &str) -> Option<HelloMessage> {
    let parts: Vec<&str> = message.split(',').collect();
    if parts.len() != 8 {
        return None;
    }

    Some(HelloMessage {
        sentinel_ip: parts[0].to_string(),
        sentinel_port: parse_redis_hello_port(parts[1])?,
        sentinel_runid: parts[2].to_string(),
        current_epoch: parse_redis_strtoull_prefix(parts[3]),
        master_name: parts[4].to_string(),
        master_ip: parts[5].to_string(),
        master_port: parse_redis_hello_port(parts[6])?,
        master_config_epoch: parse_redis_strtoull_prefix(parts[7]),
    })
}

fn parse_redis_hello_port(value: &str) -> Option<u16> {
    let port = parse_redis_atoi_prefix(value);
    u16::try_from(port).ok()
}

fn parse_redis_atoi_prefix(value: &str) -> i64 {
    let bytes = value.as_bytes();
    let mut cursor = skip_ascii_space(bytes);
    let negative = match bytes.get(cursor) {
        Some(b'-') => {
            cursor += 1;
            true
        }
        Some(b'+') => {
            cursor += 1;
            false
        }
        _ => false,
    };

    let mut parsed = 0i64;
    let mut saw_digit = false;
    while let Some(byte) = bytes.get(cursor).filter(|byte| byte.is_ascii_digit()) {
        parsed = parsed
            .saturating_mul(10)
            .saturating_add(i64::from(*byte - b'0'));
        saw_digit = true;
        cursor += 1;
    }

    if !saw_digit {
        return 0;
    }
    if negative {
        parsed.saturating_neg()
    } else {
        parsed
    }
}

fn parse_redis_strtoull_prefix(value: &str) -> u64 {
    let bytes = value.as_bytes();
    let mut cursor = skip_ascii_space(bytes);
    let negative = match bytes.get(cursor) {
        Some(b'-') => {
            cursor += 1;
            true
        }
        Some(b'+') => {
            cursor += 1;
            false
        }
        _ => false,
    };

    let mut parsed = 0u64;
    let mut saw_digit = false;
    while let Some(byte) = bytes.get(cursor).filter(|byte| byte.is_ascii_digit()) {
        parsed = parsed
            .saturating_mul(10)
            .saturating_add(u64::from(*byte - b'0'));
        saw_digit = true;
        cursor += 1;
    }

    if !saw_digit {
        return 0;
    }
    if negative {
        0u64.wrapping_sub(parsed)
    } else {
        parsed
    }
}

fn skip_ascii_space(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len())
}

pub fn create_hello_message(state: &SentinelState, master: &SentinelRedisInstance) -> HelloMessage {
    let sentinel_ip = state
        .announce_ip
        .clone()
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let sentinel_port = state
        .announce_port
        .filter(|port| *port != 0)
        .unwrap_or(26379);

    HelloMessage {
        sentinel_ip,
        sentinel_port,
        sentinel_runid: state.myid_hex(),
        current_epoch: state.current_epoch,
        master_name: master.name.clone(),
        master_ip: if state.announce_hostnames {
            master.addr.hostname.clone()
        } else {
            master.addr.ip.clone()
        },
        master_port: master.addr.port,
        master_config_epoch: master.config_epoch,
    }
}

pub fn should_publish_hello(master: &SentinelRedisInstance, now: u64) -> bool {
    now.saturating_sub(master.last_pub_time) > PUBLISH_PERIOD_MS
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryAction {
    AddSentinel {
        master_name: String,
        sentinel_key: String,
        addr: SentinelAddr,
        runid: String,
        current_epoch: u64,
        master_addr: SentinelAddr,
        master_config_epoch: u64,
    },
    UpdateSentinel {
        master_name: String,
        sentinel_key: String,
        addr: SentinelAddr,
        current_epoch: u64,
        master_addr: SentinelAddr,
        master_config_epoch: u64,
    },
    None,
}

#[cfg_attr(feature = "bench-reference", inline(never))]
pub fn process_hello_message(
    state: &SentinelState,
    hello: &HelloMessage,
    now: u64,
) -> DiscoveryAction {
    if hello.sentinel_runid.as_str() == String::from_utf8_lossy(&state.myid).as_ref() {
        return DiscoveryAction::None;
    }

    process_hello_message_non_self(state, hello, now)
}

#[cfg(feature = "bench-reference")]
#[inline(never)]
pub fn bench_process_hello_owned_self_id_reference(
    state: &SentinelState,
    hello: &HelloMessage,
    now: u64,
) -> DiscoveryAction {
    if hello.sentinel_runid == state.myid_hex() {
        return DiscoveryAction::None;
    }

    process_hello_message_non_self(state, hello, now)
}

#[inline]
fn process_hello_message_non_self(
    state: &SentinelState,
    hello: &HelloMessage,
    _now: u64,
) -> DiscoveryAction {
    let master = match state.get_master(&hello.master_name) {
        Some(m) => m,
        None => return DiscoveryAction::None,
    };

    let sentinel_key = format!("{}:{}", hello.sentinel_ip, hello.sentinel_port);
    let master_addr = SentinelAddr::new(&hello.master_ip, hello.master_port);
    let same_addr_same_runid = master
        .sentinels
        .get(&sentinel_key)
        .is_some_and(|sentinel| sentinel.runid.as_deref() == Some(hello.sentinel_runid.as_str()));

    if !same_addr_same_runid {
        return DiscoveryAction::AddSentinel {
            master_name: hello.master_name.clone(),
            sentinel_key,
            addr: SentinelAddr::new(&hello.sentinel_ip, hello.sentinel_port),
            runid: hello.sentinel_runid.clone(),
            current_epoch: hello.current_epoch,
            master_addr,
            master_config_epoch: hello.master_config_epoch,
        };
    }

    DiscoveryAction::UpdateSentinel {
        master_name: hello.master_name.clone(),
        sentinel_key,
        addr: SentinelAddr::new(&hello.sentinel_ip, hello.sentinel_port),
        current_epoch: hello.current_epoch,
        master_addr,
        master_config_epoch: hello.master_config_epoch,
    }
}

pub fn apply_discovery_action(state: &mut SentinelState, action: DiscoveryAction, now: u64) {
    match action {
        DiscoveryAction::AddSentinel {
            master_name,
            sentinel_key,
            addr,
            runid,
            current_epoch,
            master_addr,
            master_config_epoch,
        } => {
            let should_update_runid_address =
                state.get_master(&master_name).is_some_and(|master| {
                    master
                        .sentinels
                        .values()
                        .any(|sentinel| sentinel.runid.as_deref() == Some(runid.as_str()))
                });
            let obsolete_runid = state
                .get_master(&master_name)
                .and_then(|master| master.sentinels.get(&sentinel_key))
                .and_then(|sentinel| sentinel.runid.clone())
                .filter(|existing_runid| existing_runid != &runid);
            if let Some(obsolete_runid) = obsolete_runid {
                remove_sentinel_runid_from_all_masters(state, &obsolete_runid);
            }
            advance_current_epoch(state, current_epoch);
            if let Some(master) = state.get_master_mut(&master_name) {
                remove_matching_sentinel_from_master(master, &runid);
                let mut sentinel =
                    SentinelRedisInstance::new_master(&sentinel_key, addr.clone(), 0);
                sentinel.flags = InstanceFlags::SENTINEL;
                sentinel.down_after_period = master.down_after_period;
                sentinel.initialize_created_link_state(now);
                sentinel.runid = Some(runid.clone());
                sentinel.last_hello_time = now;
                master.sentinels.insert(sentinel_key.clone(), sentinel);
                apply_master_config_from_hello(master, master_addr, master_config_epoch);
            }
            if should_update_runid_address {
                update_sentinel_address_in_all_masters(state, &runid, &sentinel_key, &addr);
            }
        }
        DiscoveryAction::UpdateSentinel {
            master_name,
            sentinel_key,
            addr,
            current_epoch,
            master_addr,
            master_config_epoch,
        } => {
            advance_current_epoch(state, current_epoch);
            if let Some(master) = state.get_master_mut(&master_name) {
                if let Some(sentinel) = master.sentinels.get_mut(&sentinel_key) {
                    sentinel.addr = addr;
                    sentinel.last_hello_time = now;
                }
                apply_master_config_from_hello(master, master_addr, master_config_epoch);
            }
        }
        DiscoveryAction::None => {}
    }
}

fn remove_sentinel_runid_from_all_masters(state: &mut SentinelState, runid: &str) {
    for master in state.masters.values_mut() {
        remove_matching_sentinel_from_master(master, runid);
    }
}

fn remove_matching_sentinel_from_master(master: &mut SentinelRedisInstance, runid: &str) {
    master
        .sentinels
        .retain(|_, sentinel| sentinel.runid.as_deref() != Some(runid));
}

fn update_sentinel_address_in_all_masters(
    state: &mut SentinelState,
    runid: &str,
    sentinel_key: &str,
    addr: &SentinelAddr,
) {
    for master in state.masters.values_mut() {
        let stale_keys: Vec<String> = master
            .sentinels
            .iter()
            .filter(|(key, sentinel)| {
                key.as_str() != sentinel_key && sentinel.runid.as_deref() == Some(runid)
            })
            .map(|(key, _)| key.clone())
            .collect();

        for stale_key in stale_keys {
            let Some(mut sentinel) = master.sentinels.remove(&stale_key) else {
                continue;
            };
            sentinel.name = sentinel_key.to_string();
            sentinel.addr = addr.clone();
            master.sentinels.insert(sentinel_key.to_string(), sentinel);
        }
    }
}

fn advance_current_epoch(state: &mut SentinelState, current_epoch: u64) {
    if current_epoch > state.current_epoch {
        state.current_epoch = current_epoch;
    }
}

fn apply_master_config_from_hello(
    master: &mut SentinelRedisInstance,
    master_addr: SentinelAddr,
    master_config_epoch: u64,
) {
    if master_config_epoch > master.config_epoch {
        master.addr = master_addr;
        master.config_epoch = master_config_epoch;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaInfo {
    pub ip: String,
    pub port: u16,
    pub runid: Option<String>,
    pub flags: String,
    pub master_link_status: String,
    pub master_link_down_time: u64,
    pub slave_priority: u32,
    pub slave_repl_offset: u64,
}

pub fn parse_replica_info_from_master(info_output: &str) -> Vec<ReplicaInfo> {
    let mut replicas = Vec::new();

    for line in info_output.lines() {
        let line = line.trim();
        if !line.starts_with("slave") || !line.contains(':') {
            continue;
        }

        if let Some(value) = line.split_once(':').map(|(_, v)| v) {
            let mut replica = ReplicaInfo {
                ip: String::new(),
                port: 0,
                runid: None,
                flags: String::new(),
                master_link_status: String::new(),
                master_link_down_time: 0,
                slave_priority: 100,
                slave_repl_offset: 0,
            };

            if value.contains("ip=") {
                for part in value.split(',') {
                    if let Some((k, v)) = part.split_once('=') {
                        match k {
                            "ip" => replica.ip = v.to_string(),
                            "port" => replica.port = v.parse().unwrap_or(0),
                            "state" => replica.flags = v.to_string(),
                            "offset" => replica.slave_repl_offset = v.parse().unwrap_or(0),
                            "lag" => {}
                            _ => {}
                        }
                    }
                }
            } else {
                let mut parts = value.split(',');
                replica.ip = parts.next().unwrap_or_default().to_string();
                replica.port = parts.next().and_then(|port| port.parse().ok()).unwrap_or(0);
                replica.flags = parts.next().unwrap_or_default().to_string();
            }

            if replica.port > 0 && !replica.ip.is_empty() {
                replicas.push(replica);
            }
        }
    }

    replicas
}

pub fn discover_replicas_from_info(
    master: &mut SentinelRedisInstance,
    replicas: &[ReplicaInfo],
    now: u64,
) {
    for replica in replicas {
        let key = format!("{}:{}", replica.ip, replica.port);

        match master.slaves.entry(key) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                let key = entry.key().clone();
                let addr = SentinelAddr::new(&replica.ip, replica.port);
                let mut slave = SentinelRedisInstance::new_master(&key, addr, 0);
                slave.flags = InstanceFlags::SLAVE;
                slave.down_after_period = master.down_after_period;
                slave.initialize_created_link_state(now);
                slave.slave_repl_offset = replica.slave_repl_offset;
                slave.info_refresh = now;
                entry.insert(slave);
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let slave = entry.get_mut();
                slave.slave_repl_offset = replica.slave_repl_offset;
                slave.info_refresh = now;
            }
        }
    }
}

pub fn prune_stale_sentinels(master: &mut SentinelRedisInstance, now: u64, max_age_ms: u64) {
    let stale_keys: Vec<String> = master
        .sentinels
        .iter()
        .filter(|(_, s)| now.saturating_sub(s.last_hello_time) > max_age_ms)
        .map(|(k, _)| k.clone())
        .collect();

    for key in stale_keys {
        master.sentinels.remove(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_message_roundtrip() {
        let msg = HelloMessage {
            sentinel_ip: "192.168.1.1".to_string(),
            sentinel_port: 26379,
            sentinel_runid: "abc123".to_string(),
            current_epoch: 5,
            master_name: "mymaster".to_string(),
            master_ip: "10.0.0.1".to_string(),
            master_port: 6379,
            master_config_epoch: 3,
        };

        let encoded = msg.encode();
        let decoded = HelloMessage::parse(&encoded).unwrap();

        assert_eq!(decoded.sentinel_ip, "192.168.1.1");
        assert_eq!(decoded.sentinel_port, 26379);
        assert_eq!(decoded.sentinel_runid, "abc123");
        assert_eq!(decoded.current_epoch, 5);
        assert_eq!(decoded.master_name, "mymaster");
        assert_eq!(decoded.master_ip, "10.0.0.1");
        assert_eq!(decoded.master_port, 6379);
        assert_eq!(decoded.master_config_epoch, 3);
    }

    #[test]
    fn hello_message_parse_invalid() {
        assert!(HelloMessage::parse("not,enough,parts").is_none());
        assert!(HelloMessage::parse("").is_none());
        assert!(HelloMessage::parse("ip,-1,runid,1,mymaster,host,6379,1").is_none());
        assert!(HelloMessage::parse("ip,26379,runid,1,mymaster,host,6379,1,extra").is_none());
    }

    #[test]
    fn hello_message_parse_uses_redis_numeric_prefixes() {
        let decoded =
            HelloMessage::parse("192.0.2.1,abc,runid,12epoch,mymaster,10.0.0.1,6379tail,7cfg")
                .unwrap();

        assert_eq!(decoded.sentinel_port, 0);
        assert_eq!(decoded.current_epoch, 12);
        assert_eq!(decoded.master_port, 6379);
        assert_eq!(decoded.master_config_epoch, 7);
    }

    #[test]
    fn create_hello_message_uses_defaults() {
        let mut state = SentinelState::new();
        state.monitor("mymaster", "10.0.0.1", 6379, 2).unwrap();
        let master = state.get_master("mymaster").unwrap();

        let msg = create_hello_message(&state, master);
        assert_eq!(msg.sentinel_ip, "127.0.0.1");
        assert_eq!(msg.sentinel_port, 26379);
        assert_eq!(msg.master_name, "mymaster");
        assert_eq!(msg.master_ip, "10.0.0.1");
        assert_eq!(msg.master_port, 6379);
    }

    #[test]
    fn create_hello_message_treats_zero_announce_port_as_default() {
        let mut state = SentinelState::new();
        state.announce_port = Some(0);
        state.monitor("mymaster", "10.0.0.1", 6379, 2).unwrap();
        let master = state.get_master("mymaster").unwrap();

        let msg = create_hello_message(&state, master);
        assert_eq!(msg.sentinel_port, 26379);

        let result = crate::commands::dispatch_sentinel_command(
            &mut state,
            &[b"CONFIG", b"GET", b"announce-port"],
        );
        assert_eq!(
            result,
            fr_protocol::RespFrame::Map(Some(vec![(
                fr_protocol::RespFrame::BulkString(Some(b"announce-port".to_vec())),
                fr_protocol::RespFrame::BulkString(Some(b"0".to_vec())),
            )]))
        );
    }

    #[test]
    fn process_hello_discovers_new_sentinel() {
        let mut state = SentinelState::new();
        state.monitor("mymaster", "10.0.0.1", 6379, 2).unwrap();
        let master = state.get_master_mut("mymaster").unwrap();
        master.down_after_period = 7_000;

        let hello = HelloMessage {
            sentinel_ip: "192.168.1.2".to_string(),
            sentinel_port: 26379,
            sentinel_runid: "other123".to_string(),
            current_epoch: 1,
            master_name: "mymaster".to_string(),
            master_ip: "10.0.0.1".to_string(),
            master_port: 6379,
            master_config_epoch: 0,
        };

        let action = process_hello_message(&state, &hello, 1000);
        assert!(matches!(action, DiscoveryAction::AddSentinel { .. }));

        apply_discovery_action(&mut state, action, 1000);
        let master = state.get_master("mymaster").unwrap();
        assert_eq!(master.sentinels.len(), 1);
        let Some(sentinel) = master.sentinels.get("192.168.1.2:26379") else {
            assert!(
                master.sentinels.contains_key("192.168.1.2:26379"),
                "sentinel was inserted"
            );
            return;
        };
        assert_eq!(sentinel.link.refcount, 1);
        assert!(sentinel.link.disconnected);
        assert_eq!(sentinel.link.act_ping_time, 1000);
        assert_eq!(sentinel.link.last_avail_time, 1000);
        assert_eq!(sentinel.link.last_pong_time, 1000);
        assert_eq!(sentinel.role_reported_time, 1000);
        assert_eq!(sentinel.down_after_period, 7_000);
    }

    #[test]
    fn process_hello_ignores_self() {
        let state = SentinelState::new();

        let hello = HelloMessage {
            sentinel_ip: "127.0.0.1".to_string(),
            sentinel_port: 26379,
            sentinel_runid: state.myid_hex(),
            current_epoch: 1,
            master_name: "mymaster".to_string(),
            master_ip: "10.0.0.1".to_string(),
            master_port: 6379,
            master_config_epoch: 0,
        };

        let action = process_hello_message(&state, &hello, 1000);
        assert_eq!(action, DiscoveryAction::None);
    }

    #[test]
    fn process_hello_self_id_comparison_preserves_lossy_invalid_utf8() {
        let mut state = SentinelState::new();
        state.myid = [0xff; 40];
        let hello = HelloMessage {
            sentinel_ip: "127.0.0.1".to_string(),
            sentinel_port: 26379,
            sentinel_runid: String::from_utf8_lossy(&state.myid).into_owned(),
            current_epoch: 1,
            master_name: "mymaster".to_string(),
            master_ip: "10.0.0.1".to_string(),
            master_port: 6379,
            master_config_epoch: 0,
        };

        let action = process_hello_message(&state, &hello, 1000);
        assert_eq!(action, DiscoveryAction::None);
        #[cfg(feature = "bench-reference")]
        assert_eq!(
            action,
            bench_process_hello_owned_self_id_reference(&state, &hello, 1000)
        );
    }

    #[test]
    fn process_hello_updates_master_addr() {
        let mut state = SentinelState::new();
        state.monitor("mymaster", "10.0.0.1", 6379, 2).unwrap();

        let hello = HelloMessage {
            sentinel_ip: "192.168.1.2".to_string(),
            sentinel_port: 26379,
            sentinel_runid: "other123".to_string(),
            current_epoch: 1,
            master_name: "mymaster".to_string(),
            master_ip: "10.0.0.2".to_string(),
            master_port: 6380,
            master_config_epoch: 5,
        };

        let action = process_hello_message(&state, &hello, 1000);
        assert!(matches!(action, DiscoveryAction::AddSentinel { .. }));

        apply_discovery_action(&mut state, action, 1000);
        let master = state.get_master("mymaster").unwrap();
        assert_eq!(master.addr.hostname, "10.0.0.2");
        assert_eq!(master.addr.port, 6380);
        assert_eq!(master.config_epoch, 5);
    }

    #[test]
    fn process_hello_advances_current_epoch() {
        let mut state = SentinelState::new();
        state.current_epoch = 2;
        state.monitor("mymaster", "10.0.0.1", 6379, 2).unwrap();

        let hello = HelloMessage {
            sentinel_ip: "192.168.1.2".to_string(),
            sentinel_port: 26379,
            sentinel_runid: "other123".to_string(),
            current_epoch: 7,
            master_name: "mymaster".to_string(),
            master_ip: "10.0.0.1".to_string(),
            master_port: 6379,
            master_config_epoch: 0,
        };

        let action = process_hello_message(&state, &hello, 1000);
        apply_discovery_action(&mut state, action, 1000);

        assert_eq!(state.current_epoch, 7);
    }

    #[test]
    fn process_hello_updates_master_config_epoch_without_address_change() {
        let mut state = SentinelState::new();
        state.monitor("mymaster", "10.0.0.1", 6379, 2).unwrap();

        let hello = HelloMessage {
            sentinel_ip: "192.168.1.2".to_string(),
            sentinel_port: 26379,
            sentinel_runid: "other123".to_string(),
            current_epoch: 1,
            master_name: "mymaster".to_string(),
            master_ip: "10.0.0.1".to_string(),
            master_port: 6379,
            master_config_epoch: 5,
        };

        let action = process_hello_message(&state, &hello, 1000);
        apply_discovery_action(&mut state, action, 1000);

        let master = state.get_master("mymaster").unwrap();
        assert_eq!(master.addr.hostname, "10.0.0.1");
        assert_eq!(master.addr.port, 6379);
        assert_eq!(master.config_epoch, 5);
    }

    #[test]
    fn process_hello_replaces_same_runid_at_new_address() {
        let mut state = SentinelState::new();
        state.monitor("mymaster", "10.0.0.1", 6379, 2).unwrap();
        let old_key = "192.168.1.2:26379".to_string();
        let old_addr = SentinelAddr::new("192.168.1.2", 26379);
        let mut old_sentinel = SentinelRedisInstance::new_master(&old_key, old_addr, 0);
        old_sentinel.flags = InstanceFlags::SENTINEL;
        old_sentinel.runid = Some("same-runid".to_string());
        state
            .get_master_mut("mymaster")
            .unwrap()
            .sentinels
            .insert(old_key.clone(), old_sentinel);

        let hello = HelloMessage {
            sentinel_ip: "192.168.1.3".to_string(),
            sentinel_port: 26379,
            sentinel_runid: "same-runid".to_string(),
            current_epoch: 1,
            master_name: "mymaster".to_string(),
            master_ip: "10.0.0.1".to_string(),
            master_port: 6379,
            master_config_epoch: 0,
        };

        let action = process_hello_message(&state, &hello, 1000);
        assert!(matches!(action, DiscoveryAction::AddSentinel { .. }));
        apply_discovery_action(&mut state, action, 1000);

        let master = state.get_master("mymaster").unwrap();
        assert!(!master.sentinels.contains_key(&old_key));
        let moved = master.sentinels.get("192.168.1.3:26379").unwrap();
        assert_eq!(moved.runid.as_deref(), Some("same-runid"));
        assert_eq!(master.sentinels.len(), 1);
    }

    #[test]
    fn process_hello_updates_same_runid_address_across_masters() {
        let mut state = SentinelState::new();
        state.monitor("mymaster", "10.0.0.1", 6379, 2).unwrap();
        state.monitor("othermaster", "10.0.0.2", 6379, 2).unwrap();

        let old_key = "192.168.1.2:26379".to_string();
        for master_name in ["mymaster", "othermaster"] {
            let mut old_sentinel = SentinelRedisInstance::new_master(
                &old_key,
                SentinelAddr::new("192.168.1.2", 26379),
                0,
            );
            old_sentinel.flags = InstanceFlags::SENTINEL;
            old_sentinel.runid = Some("same-runid".to_string());
            old_sentinel.leader = Some(format!("{master_name}-leader"));
            state
                .get_master_mut(master_name)
                .unwrap()
                .sentinels
                .insert(old_key.clone(), old_sentinel);
        }

        let hello = HelloMessage {
            sentinel_ip: "192.168.1.3".to_string(),
            sentinel_port: 26379,
            sentinel_runid: "same-runid".to_string(),
            current_epoch: 1,
            master_name: "mymaster".to_string(),
            master_ip: "10.0.0.1".to_string(),
            master_port: 6379,
            master_config_epoch: 0,
        };

        let action = process_hello_message(&state, &hello, 1000);
        assert!(matches!(action, DiscoveryAction::AddSentinel { .. }));
        apply_discovery_action(&mut state, action, 1000);

        let new_key = "192.168.1.3:26379";
        let master = state.get_master("mymaster").unwrap();
        assert!(!master.sentinels.contains_key(&old_key));
        assert_eq!(
            master
                .sentinels
                .get(new_key)
                .and_then(|sentinel| sentinel.runid.as_deref()),
            Some("same-runid")
        );

        let other_master = state.get_master("othermaster").unwrap();
        assert!(!other_master.sentinels.contains_key(&old_key));
        let updated = other_master.sentinels.get(new_key).unwrap();
        assert_eq!(updated.addr.hostname, "192.168.1.3");
        assert_eq!(updated.runid.as_deref(), Some("same-runid"));
        assert_eq!(updated.leader.as_deref(), Some("othermaster-leader"));
    }

    #[test]
    fn process_hello_replaces_same_address_new_runid_across_masters() {
        let mut state = SentinelState::new();
        state.monitor("mymaster", "10.0.0.1", 6379, 2).unwrap();
        state.monitor("othermaster", "10.0.0.2", 6379, 2).unwrap();

        let stale_key = "192.168.1.2:26379".to_string();
        let mut stale_for_master = SentinelRedisInstance::new_master(
            &stale_key,
            SentinelAddr::new("192.168.1.2", 26379),
            0,
        );
        stale_for_master.flags = InstanceFlags::SENTINEL;
        stale_for_master.runid = Some("old-runid".to_string());
        state
            .get_master_mut("mymaster")
            .unwrap()
            .sentinels
            .insert(stale_key.clone(), stale_for_master);

        let other_key = "192.168.1.9:26379".to_string();
        let mut stale_for_other = SentinelRedisInstance::new_master(
            &other_key,
            SentinelAddr::new("192.168.1.9", 26379),
            0,
        );
        stale_for_other.flags = InstanceFlags::SENTINEL;
        stale_for_other.runid = Some("old-runid".to_string());
        state
            .get_master_mut("othermaster")
            .unwrap()
            .sentinels
            .insert(other_key, stale_for_other);

        let hello = HelloMessage {
            sentinel_ip: "192.168.1.2".to_string(),
            sentinel_port: 26379,
            sentinel_runid: "new-runid".to_string(),
            current_epoch: 1,
            master_name: "mymaster".to_string(),
            master_ip: "10.0.0.1".to_string(),
            master_port: 6379,
            master_config_epoch: 0,
        };

        let action = process_hello_message(&state, &hello, 1000);
        assert!(matches!(action, DiscoveryAction::AddSentinel { .. }));
        apply_discovery_action(&mut state, action, 1000);

        let master = state.get_master("mymaster").unwrap();
        let replacement = master.sentinels.get(&stale_key).unwrap();
        assert_eq!(replacement.runid.as_deref(), Some("new-runid"));
        let other_master = state.get_master("othermaster").unwrap();
        assert!(other_master.sentinels.is_empty());
    }

    #[test]
    fn parse_replica_info() {
        let info = r#"
# Replication
role:master
connected_slaves:2
slave0:ip=10.0.0.10,port=6379,state=online,offset=12345,lag=0
slave1:ip=10.0.0.11,port=6379,state=online,offset=12340,lag=1
"#;

        let replicas = parse_replica_info_from_master(info);
        assert_eq!(replicas.len(), 2);
        assert_eq!(replicas[0].ip, "10.0.0.10");
        assert_eq!(replicas[0].port, 6379);
        assert_eq!(replicas[0].slave_repl_offset, 12345);
        assert_eq!(replicas[1].ip, "10.0.0.11");
    }

    #[test]
    fn parse_replica_info_accepts_legacy_slave_rows() {
        let info = r#"
# Replication
role:master
connected_slaves:1
slave0:10.0.0.10,6379,online
"#;

        let replicas = parse_replica_info_from_master(info);
        assert_eq!(replicas.len(), 1);
        assert_eq!(replicas[0].ip, "10.0.0.10");
        assert_eq!(replicas[0].port, 6379);
        assert_eq!(replicas[0].flags, "online");
    }

    #[test]
    fn discover_replicas_adds_new() {
        let addr = SentinelAddr::new("10.0.0.1", 6379);
        let mut master = SentinelRedisInstance::new_master("mymaster", addr, 2);
        master.down_after_period = 7_000;

        let replicas = vec![ReplicaInfo {
            ip: "10.0.0.10".to_string(),
            port: 6379,
            runid: None,
            flags: "online".to_string(),
            master_link_status: "up".to_string(),
            master_link_down_time: 0,
            slave_priority: 100,
            slave_repl_offset: 12345,
        }];

        discover_replicas_from_info(&mut master, &replicas, 1000);
        assert_eq!(master.slaves.len(), 1);
        assert!(master.slaves.contains_key("10.0.0.10:6379"));
        let Some(replica) = master.slaves.get("10.0.0.10:6379") else {
            assert!(
                master.slaves.contains_key("10.0.0.10:6379"),
                "replica was inserted"
            );
            return;
        };
        assert_eq!(replica.link.refcount, 1);
        assert!(replica.link.disconnected);
        assert_eq!(replica.link.act_ping_time, 1000);
        assert_eq!(replica.link.last_avail_time, 1000);
        assert_eq!(replica.link.last_pong_time, 1000);
        assert_eq!(replica.role_reported_time, 1000);
        assert_eq!(replica.down_after_period, 7_000);
    }

    #[test]
    fn prune_stale_sentinels_removes_old() {
        let addr = SentinelAddr::new("10.0.0.1", 6379);
        let mut master = SentinelRedisInstance::new_master("mymaster", addr, 2);

        let sentinel_addr = SentinelAddr::new("192.168.1.2", 26379);
        let mut sentinel = SentinelRedisInstance::new_master("192.168.1.2:26379", sentinel_addr, 0);
        sentinel.flags = InstanceFlags::SENTINEL;
        sentinel.last_hello_time = 0;
        master
            .sentinels
            .insert("192.168.1.2:26379".to_string(), sentinel);

        assert_eq!(master.sentinels.len(), 1);
        prune_stale_sentinels(&mut master, 100000, 60000);
        assert_eq!(master.sentinels.len(), 0);
    }

    #[test]
    fn should_publish_hello_checks_interval() {
        let addr = SentinelAddr::new("10.0.0.1", 6379);
        let mut master = SentinelRedisInstance::new_master("mymaster", addr, 2);
        master.last_pub_time = 1000;

        assert!(!should_publish_hello(&master, 1500));
        assert!(!should_publish_hello(&master, 3000));
        assert!(should_publish_hello(&master, 3001));
    }
}
