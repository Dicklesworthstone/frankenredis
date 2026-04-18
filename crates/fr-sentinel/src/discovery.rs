#![forbid(unsafe_code)]

use crate::{
    InstanceFlags, SentinelAddr, SentinelRedisInstance, SentinelState,
    PUBLISH_PERIOD_MS,
};

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

    pub fn parse(message: &str) -> Option<HelloMessage> {
        let parts: Vec<&str> = message.split(',').collect();
        if parts.len() != 8 {
            return None;
        }

        Some(HelloMessage {
            sentinel_ip: parts[0].to_string(),
            sentinel_port: parts[1].parse().ok()?,
            sentinel_runid: parts[2].to_string(),
            current_epoch: parts[3].parse().ok()?,
            master_name: parts[4].to_string(),
            master_ip: parts[5].to_string(),
            master_port: parts[6].parse().ok()?,
            master_config_epoch: parts[7].parse().ok()?,
        })
    }
}

pub fn create_hello_message(
    state: &SentinelState,
    master: &SentinelRedisInstance,
) -> HelloMessage {
    let sentinel_ip = state
        .announce_ip
        .clone()
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let sentinel_port = state.announce_port.unwrap_or(26379);

    HelloMessage {
        sentinel_ip,
        sentinel_port,
        sentinel_runid: state.myid_hex(),
        current_epoch: state.current_epoch,
        master_name: master.name.clone(),
        master_ip: master.addr.hostname.clone(),
        master_port: master.addr.port,
        master_config_epoch: master.config_epoch,
    }
}

pub fn should_publish_hello(master: &SentinelRedisInstance, now: u64) -> bool {
    now.saturating_sub(master.last_pub_time) >= PUBLISH_PERIOD_MS
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryAction {
    AddSentinel {
        master_name: String,
        sentinel_key: String,
        addr: SentinelAddr,
        runid: String,
    },
    UpdateSentinel {
        master_name: String,
        sentinel_key: String,
        addr: SentinelAddr,
    },
    UpdateMasterAddr {
        master_name: String,
        new_addr: SentinelAddr,
        new_epoch: u64,
    },
    None,
}

pub fn process_hello_message(
    state: &SentinelState,
    hello: &HelloMessage,
    _now: u64,
) -> DiscoveryAction {
    if hello.sentinel_runid == state.myid_hex() {
        return DiscoveryAction::None;
    }

    let master = match state.get_master(&hello.master_name) {
        Some(m) => m,
        None => return DiscoveryAction::None,
    };

    if hello.master_config_epoch > master.config_epoch {
        if hello.master_ip != master.addr.hostname || hello.master_port != master.addr.port {
            return DiscoveryAction::UpdateMasterAddr {
                master_name: hello.master_name.clone(),
                new_addr: SentinelAddr::new(&hello.master_ip, hello.master_port),
                new_epoch: hello.master_config_epoch,
            };
        }
    }

    let sentinel_key = format!("{}:{}", hello.sentinel_ip, hello.sentinel_port);

    if !master.sentinels.contains_key(&sentinel_key) {
        return DiscoveryAction::AddSentinel {
            master_name: hello.master_name.clone(),
            sentinel_key,
            addr: SentinelAddr::new(&hello.sentinel_ip, hello.sentinel_port),
            runid: hello.sentinel_runid.clone(),
        };
    }

    DiscoveryAction::UpdateSentinel {
        master_name: hello.master_name.clone(),
        sentinel_key,
        addr: SentinelAddr::new(&hello.sentinel_ip, hello.sentinel_port),
    }
}

pub fn apply_discovery_action(
    state: &mut SentinelState,
    action: DiscoveryAction,
    now: u64,
) {
    match action {
        DiscoveryAction::AddSentinel {
            master_name,
            sentinel_key,
            addr,
            runid,
        } => {
            if let Some(master) = state.get_master_mut(&master_name) {
                let mut sentinel = SentinelRedisInstance::new_master(&sentinel_key, addr, 0);
                sentinel.flags = InstanceFlags::SENTINEL;
                sentinel.runid = Some(runid);
                sentinel.last_hello_time = now;
                master.sentinels.insert(sentinel_key, sentinel);
            }
        }
        DiscoveryAction::UpdateSentinel {
            master_name,
            sentinel_key,
            addr,
        } => {
            if let Some(master) = state.get_master_mut(&master_name) {
                if let Some(sentinel) = master.sentinels.get_mut(&sentinel_key) {
                    sentinel.addr = addr;
                    sentinel.last_hello_time = now;
                }
            }
        }
        DiscoveryAction::UpdateMasterAddr {
            master_name,
            new_addr,
            new_epoch,
        } => {
            if let Some(master) = state.get_master_mut(&master_name) {
                master.addr = new_addr;
                master.config_epoch = new_epoch;
            }
        }
        DiscoveryAction::None => {}
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

        if !master.slaves.contains_key(&key) {
            let addr = SentinelAddr::new(&replica.ip, replica.port);
            let mut slave = SentinelRedisInstance::new_master(&key, addr, 0);
            slave.flags = InstanceFlags::SLAVE;
            slave.slave_repl_offset = replica.slave_repl_offset;
            slave.info_refresh = now;
            master.slaves.insert(key, slave);
        } else if let Some(slave) = master.slaves.get_mut(&key) {
            slave.slave_repl_offset = replica.slave_repl_offset;
            slave.info_refresh = now;
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
    fn process_hello_discovers_new_sentinel() {
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
            master_config_epoch: 0,
        };

        let action = process_hello_message(&state, &hello, 1000);
        assert!(matches!(action, DiscoveryAction::AddSentinel { .. }));

        apply_discovery_action(&mut state, action, 1000);
        let master = state.get_master("mymaster").unwrap();
        assert_eq!(master.sentinels.len(), 1);
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
        assert!(matches!(action, DiscoveryAction::UpdateMasterAddr { .. }));

        apply_discovery_action(&mut state, action, 1000);
        let master = state.get_master("mymaster").unwrap();
        assert_eq!(master.addr.hostname, "10.0.0.2");
        assert_eq!(master.addr.port, 6380);
        assert_eq!(master.config_epoch, 5);
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
    fn discover_replicas_adds_new() {
        let addr = SentinelAddr::new("10.0.0.1", 6379);
        let mut master = SentinelRedisInstance::new_master("mymaster", addr, 2);

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
    }

    #[test]
    fn prune_stale_sentinels_removes_old() {
        let addr = SentinelAddr::new("10.0.0.1", 6379);
        let mut master = SentinelRedisInstance::new_master("mymaster", addr, 2);

        let sentinel_addr = SentinelAddr::new("192.168.1.2", 26379);
        let mut sentinel = SentinelRedisInstance::new_master("192.168.1.2:26379", sentinel_addr, 0);
        sentinel.flags = InstanceFlags::SENTINEL;
        sentinel.last_hello_time = 0;
        master.sentinels.insert("192.168.1.2:26379".to_string(), sentinel);

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
        assert!(should_publish_hello(&master, 4000));
    }
}
