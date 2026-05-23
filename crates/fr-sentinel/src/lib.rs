#![forbid(unsafe_code)]

pub mod commands;
pub mod consensus;
pub mod discovery;
pub mod failover;
pub mod health;

use std::collections::HashMap;

pub const PING_PERIOD_MS: u64 = 1000;
pub const INFO_PERIOD_MS: u64 = 10000;
pub const ASK_PERIOD_MS: u64 = 1000;
pub const PUBLISH_PERIOD_MS: u64 = 2000;
pub const DEFAULT_DOWN_AFTER_MS: u64 = 30000;
pub const TILT_TRIGGER_MS: u64 = 2000;
pub const TILT_PERIOD_MS: u64 = 30000;
pub const SLAVE_RECONF_TIMEOUT_MS: u64 = 10000;
pub const MIN_LINK_RECONNECT_PERIOD_MS: u64 = 15000;
pub const ELECTION_TIMEOUT_MS: u64 = 10000;
pub const SCRIPT_MAX_RUNTIME_MS: u64 = 60000;
pub const SCRIPT_RETRY_DELAY_MS: u64 = 30000;
pub const DEFAULT_FAILOVER_TIMEOUT_MS: u64 = 180000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentinelDebugConfig {
    pub info_period: u64,
    pub ping_period: u64,
    pub ask_period: u64,
    pub publish_period: u64,
    pub default_down_after: u64,
    pub default_failover_timeout: u64,
    pub tilt_trigger: u64,
    pub tilt_period: u64,
    pub slave_reconf_timeout: u64,
    pub min_link_reconnect_period: u64,
    pub election_timeout: u64,
    pub script_max_runtime: u64,
    pub script_retry_delay: u64,
}

impl Default for SentinelDebugConfig {
    fn default() -> Self {
        Self {
            info_period: INFO_PERIOD_MS,
            ping_period: PING_PERIOD_MS,
            ask_period: ASK_PERIOD_MS,
            publish_period: PUBLISH_PERIOD_MS,
            default_down_after: DEFAULT_DOWN_AFTER_MS,
            default_failover_timeout: DEFAULT_FAILOVER_TIMEOUT_MS,
            tilt_trigger: TILT_TRIGGER_MS,
            tilt_period: TILT_PERIOD_MS,
            slave_reconf_timeout: SLAVE_RECONF_TIMEOUT_MS,
            min_link_reconnect_period: MIN_LINK_RECONNECT_PERIOD_MS,
            election_timeout: ELECTION_TIMEOUT_MS,
            script_max_runtime: SCRIPT_MAX_RUNTIME_MS,
            script_retry_delay: SCRIPT_RETRY_DELAY_MS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SimFailureFlags(u8);

impl SimFailureFlags {
    pub const CRASH_AFTER_ELECTION: Self = Self(1 << 0);
    pub const CRASH_AFTER_PROMOTION: Self = Self(1 << 1);

    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InstanceFlags(u16);

impl InstanceFlags {
    pub const MASTER: Self = Self(1 << 0);
    pub const SLAVE: Self = Self(1 << 1);
    pub const SENTINEL: Self = Self(1 << 2);
    pub const S_DOWN: Self = Self(1 << 3);
    pub const O_DOWN: Self = Self(1 << 4);
    pub const MASTER_DOWN: Self = Self(1 << 5);
    pub const FAILOVER_IN_PROGRESS: Self = Self(1 << 6);
    pub const PROMOTED: Self = Self(1 << 7);
    pub const RECONF_SENT: Self = Self(1 << 8);
    pub const RECONF_INPROG: Self = Self(1 << 9);
    pub const RECONF_DONE: Self = Self(1 << 10);
    pub const FORCE_FAILOVER: Self = Self(1 << 11);
    pub const SCRIPT_KILL_SENT: Self = Self(1 << 12);
    pub const MASTER_REBOOT: Self = Self(1 << 13);

    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FailoverState {
    #[default]
    None,
    WaitStart,
    SelectSlave,
    SendSlaveofNoone,
    WaitPromotion,
    ReconfSlaves,
    UpdateConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Role {
    #[default]
    Unknown,
    Master,
    Slave,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinkStatus {
    #[default]
    Down,
    Up,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentinelAddr {
    pub hostname: String,
    pub ip: String,
    pub port: u16,
}

impl SentinelAddr {
    #[must_use]
    pub fn new(hostname: impl Into<String>, port: u16) -> Self {
        let hostname = hostname.into();
        Self {
            ip: hostname.clone(),
            hostname,
            port,
        }
    }

    #[must_use]
    pub fn new_with_ip(hostname: impl Into<String>, ip: impl Into<String>, port: u16) -> Self {
        Self {
            hostname: hostname.into(),
            ip: ip.into(),
            port,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct InstanceLink {
    pub refcount: u32,
    pub disconnected: bool,
    pub pending_commands: u32,
    pub cc_conn_time: u64,
    pub pc_conn_time: u64,
    pub pc_last_activity: u64,
    pub last_avail_time: u64,
    pub act_ping_time: u64,
    pub last_ping_time: u64,
    pub last_pong_time: u64,
    pub last_reconn_time: u64,
}

#[derive(Debug, Clone)]
pub struct SentinelRedisInstance {
    pub flags: InstanceFlags,
    pub name: String,
    pub runid: Option<String>,
    pub config_epoch: u64,

    pub addr: SentinelAddr,
    pub link: InstanceLink,

    pub last_pub_time: u64,
    pub last_hello_time: u64,
    pub last_master_down_reply_time: u64,
    pub s_down_since_time: u64,
    pub o_down_since_time: u64,
    pub down_after_period: u64,
    pub info_refresh: u64,
    pub info: Option<String>,

    pub role_reported: Role,
    pub role_reported_time: u64,

    pub sentinels: HashMap<String, SentinelRedisInstance>,
    pub slaves: HashMap<String, SentinelRedisInstance>,
    pub quorum: u32,
    pub parallel_syncs: i32,
    pub auth_pass: Option<String>,
    pub auth_user: Option<String>,

    pub master_link_down_time: u64,
    pub slave_priority: u32,
    pub replica_announced: bool,
    pub slave_repl_offset: u64,
    pub slave_master_host: Option<String>,
    pub slave_master_port: Option<u16>,
    pub slave_master_link_status: LinkStatus,

    pub leader: Option<String>,
    pub leader_epoch: u64,
    pub failover_epoch: u64,
    pub failover_state: FailoverState,
    pub failover_state_change_time: u64,
    pub failover_start_time: u64,
    pub failover_timeout: u64,
    pub promoted_slave: Option<Box<SentinelRedisInstance>>,
    pub master_reboot_down_after_period: u64,

    pub renamed_commands: HashMap<String, String>,
    pub notification_script: Option<String>,
    pub client_reconfig_script: Option<String>,
}

impl SentinelRedisInstance {
    #[must_use]
    pub fn new_master(name: impl Into<String>, addr: SentinelAddr, quorum: u32) -> Self {
        Self {
            flags: InstanceFlags::MASTER,
            name: name.into(),
            runid: None,
            config_epoch: 0,
            addr,
            link: InstanceLink::default(),
            last_pub_time: 0,
            last_hello_time: 0,
            last_master_down_reply_time: 0,
            s_down_since_time: 0,
            o_down_since_time: 0,
            down_after_period: DEFAULT_DOWN_AFTER_MS,
            info_refresh: 0,
            info: None,
            role_reported: Role::Unknown,
            role_reported_time: 0,
            sentinels: HashMap::new(),
            slaves: HashMap::new(),
            quorum,
            parallel_syncs: 1,
            auth_pass: None,
            auth_user: None,
            master_link_down_time: 0,
            slave_priority: 100,
            replica_announced: true,
            slave_repl_offset: 0,
            slave_master_host: None,
            slave_master_port: None,
            slave_master_link_status: LinkStatus::Down,
            leader: None,
            leader_epoch: 0,
            failover_epoch: 0,
            failover_state: FailoverState::None,
            failover_state_change_time: 0,
            failover_start_time: 0,
            failover_timeout: DEFAULT_FAILOVER_TIMEOUT_MS,
            promoted_slave: None,
            master_reboot_down_after_period: 0,
            renamed_commands: HashMap::new(),
            notification_script: None,
            client_reconfig_script: None,
        }
    }

    pub fn initialize_created_link_state(&mut self, now: u64) {
        self.link.refcount = 1;
        self.link.disconnected = true;
        self.link.act_ping_time = now;
        self.link.last_ping_time = 0;
        self.link.last_avail_time = now;
        self.link.last_pong_time = now;
        self.last_pub_time = now;
        self.last_hello_time = now;
        self.last_master_down_reply_time = now;
        self.role_reported_time = now;
        self.role_reported = if self.is_master() {
            Role::Master
        } else if self.is_slave() {
            Role::Slave
        } else {
            Role::Unknown
        };
    }

    #[must_use]
    pub fn is_master(&self) -> bool {
        self.flags.contains(InstanceFlags::MASTER)
    }

    #[must_use]
    pub fn is_slave(&self) -> bool {
        self.flags.contains(InstanceFlags::SLAVE)
    }

    #[must_use]
    pub fn is_sentinel(&self) -> bool {
        self.flags.contains(InstanceFlags::SENTINEL)
    }

    #[must_use]
    pub fn is_s_down(&self) -> bool {
        self.flags.contains(InstanceFlags::S_DOWN)
    }

    #[must_use]
    pub fn is_o_down(&self) -> bool {
        self.flags.contains(InstanceFlags::O_DOWN)
    }

    pub fn set_s_down(&mut self, down: bool, now: u64) {
        if down {
            if !self.is_s_down() {
                self.flags.insert(InstanceFlags::S_DOWN);
                self.s_down_since_time = now;
            }
        } else {
            self.flags.remove(InstanceFlags::S_DOWN);
            self.s_down_since_time = 0;
        }
    }

    pub fn set_o_down(&mut self, down: bool, now: u64) {
        if down {
            if !self.is_o_down() {
                self.flags.insert(InstanceFlags::O_DOWN);
                self.o_down_since_time = now;
            }
        } else {
            self.flags.remove(InstanceFlags::O_DOWN);
            self.o_down_since_time = 0;
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScriptJob {
    pub path: String,
    pub args: Vec<String>,
    pub retry_count: u32,
    pub pid: Option<i64>,
    pub start_time_ms: u64,
}

#[derive(Debug)]
pub struct SentinelState {
    pub myid: [u8; 40],
    pub current_epoch: u64,
    pub masters: HashMap<String, SentinelRedisInstance>,
    pub tilt: bool,
    pub tilt_start_time: u64,
    pub previous_time: u64,
    pub running_scripts: usize,
    pub scripts_queue: Vec<ScriptJob>,
    pub announce_ip: Option<String>,
    pub announce_port: Option<u16>,
    pub sentinel_auth_pass: Option<String>,
    pub sentinel_auth_user: Option<String>,
    pub resolve_hostnames: bool,
    pub announce_hostnames: bool,
    pub deny_scripts_reconfig: bool,
    pub simfailure_flags: SimFailureFlags,
    pub loglevel: String,
    pub debug_config: SentinelDebugConfig,
}

impl Default for SentinelState {
    fn default() -> Self {
        Self::new()
    }
}

impl SentinelState {
    #[must_use]
    pub fn new() -> Self {
        let mut myid = [0u8; 40];
        for (i, b) in myid.iter_mut().enumerate() {
            *b = b"0123456789abcdef"[i % 16];
        }
        Self {
            myid,
            current_epoch: 0,
            masters: HashMap::new(),
            tilt: false,
            tilt_start_time: 0,
            previous_time: 0,
            running_scripts: 0,
            scripts_queue: Vec::new(),
            announce_ip: None,
            announce_port: None,
            sentinel_auth_pass: None,
            sentinel_auth_user: None,
            resolve_hostnames: false,
            announce_hostnames: false,
            deny_scripts_reconfig: true,
            simfailure_flags: SimFailureFlags::empty(),
            loglevel: "notice".to_string(),
            debug_config: SentinelDebugConfig::default(),
        }
    }

    pub fn monitor(
        &mut self,
        name: impl Into<String>,
        hostname: impl Into<String>,
        port: u16,
        quorum: u32,
    ) -> Result<(), &'static str> {
        self.monitor_addr(name, SentinelAddr::new(hostname, port), quorum)
    }

    pub fn monitor_addr(
        &mut self,
        name: impl Into<String>,
        addr: SentinelAddr,
        quorum: u32,
    ) -> Result<(), &'static str> {
        if quorum == 0 {
            return Err("ERR Quorum must be 1 or greater.");
        }
        self.monitor_addr_after_quorum_validation(name, addr, quorum)
    }

    pub(crate) fn monitor_addr_after_quorum_validation(
        &mut self,
        name: impl Into<String>,
        addr: SentinelAddr,
        quorum: u32,
    ) -> Result<(), &'static str> {
        let name = name.into();
        if self.masters.contains_key(&name) {
            return Err("ERR Duplicate master name.");
        }
        let mut instance = SentinelRedisInstance::new_master(&name, addr, quorum);
        instance.initialize_created_link_state(self.previous_time);
        self.masters.insert(name, instance);
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> Result<(), &'static str> {
        if self.masters.remove(name).is_some() {
            Ok(())
        } else {
            Err("ERR No such master with that name")
        }
    }

    #[must_use]
    pub fn get_master(&self, name: &str) -> Option<&SentinelRedisInstance> {
        self.masters.get(name)
    }

    #[must_use]
    pub fn get_master_mut(&mut self, name: &str) -> Option<&mut SentinelRedisInstance> {
        self.masters.get_mut(name)
    }

    pub fn check_tilt(&mut self, now: u64) {
        if self.previous_time == 0 {
            self.previous_time = now;
            return;
        }
        let moved_backward = now < self.previous_time;
        let moved_too_far_forward = now
            .checked_sub(self.previous_time)
            .is_some_and(|delta| delta > self.debug_config.tilt_trigger);
        if moved_backward || moved_too_far_forward {
            self.tilt = true;
            self.tilt_start_time = now;
        } else if self.tilt
            && now.saturating_sub(self.tilt_start_time) > self.debug_config.tilt_period
        {
            self.tilt = false;
        }
        self.previous_time = now;
    }

    #[must_use]
    pub fn myid_hex(&self) -> String {
        String::from_utf8_lossy(&self.myid).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_flags_operations() {
        let mut flags = InstanceFlags::MASTER;
        assert!(flags.contains(InstanceFlags::MASTER));
        assert!(!flags.contains(InstanceFlags::SLAVE));

        flags.insert(InstanceFlags::S_DOWN);
        assert!(flags.contains(InstanceFlags::S_DOWN));

        flags.remove(InstanceFlags::S_DOWN);
        assert!(!flags.contains(InstanceFlags::S_DOWN));

        let combined = InstanceFlags::MASTER.union(InstanceFlags::S_DOWN);
        assert!(combined.contains(InstanceFlags::MASTER));
        assert!(combined.contains(InstanceFlags::S_DOWN));
    }

    #[test]
    fn sentinel_state_monitor_remove() {
        let mut state = SentinelState::new();
        assert!(state.monitor("mymaster", "127.0.0.1", 6379, 2).is_ok());
        assert!(state.monitor("mymaster", "127.0.0.1", 6380, 2).is_err());
        assert!(state.get_master("mymaster").is_some());
        assert!(state.remove("mymaster").is_ok());
        assert!(state.remove("mymaster").is_err());
    }

    #[test]
    fn sentinel_state_monitor_rejects_zero_quorum() {
        let mut state = SentinelState::new();

        assert_eq!(
            state.monitor("zero", "127.0.0.1", 6379, 0),
            Err("ERR Quorum must be 1 or greater.")
        );
        assert!(state.get_master("zero").is_none());
    }

    #[test]
    fn instance_s_down_transitions() {
        let addr = SentinelAddr::new("127.0.0.1", 6379);
        let mut instance = SentinelRedisInstance::new_master("test", addr, 2);

        assert!(!instance.is_s_down());
        instance.set_s_down(true, 1000);
        assert!(instance.is_s_down());
        assert_eq!(instance.s_down_since_time, 1000);

        instance.set_s_down(false, 2000);
        assert!(!instance.is_s_down());
        assert_eq!(instance.s_down_since_time, 0);
    }

    #[test]
    fn tilt_mode_detection() {
        let mut state = SentinelState::new();

        state.check_tilt(1000);
        assert!(!state.tilt);

        state.check_tilt(1100);
        assert!(!state.tilt);

        state.check_tilt(5000);
        assert!(state.tilt);

        state.previous_time = 35_999;
        state.check_tilt(36000);
        assert!(!state.tilt);
    }

    #[test]
    fn tilt_triggers_on_backward_clock_step_and_refreshes_start() {
        let mut state = SentinelState::new();
        state.check_tilt(1000);

        state.check_tilt(999);
        assert!(state.tilt);
        assert_eq!(state.tilt_start_time, 999);

        state.check_tilt(4000);
        assert!(state.tilt);
        assert_eq!(state.tilt_start_time, 4000);

        state.previous_time = 34_000;
        state.check_tilt(34_001);
        assert!(!state.tilt);
    }

    #[test]
    fn failover_state_default() {
        assert_eq!(FailoverState::default(), FailoverState::None);
    }

    #[test]
    fn fuzz_sentinel_parsers_corpus_matches_documented_contract() -> Result<(), String> {
        use crate::discovery::{HelloMessage, parse_replica_info_from_master};
        use crate::{Role, health::parse_info_response};
        use std::{fs, path::Path};

        let corpus_root =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/fuzz_sentinel_parsers");
        if !corpus_root.exists() {
            return Ok(());
        }

        for entry in fs::read_dir(&corpus_root)
            .map_err(|err| format!("read fuzz_sentinel_parsers corpus: {err}"))?
        {
            let path = entry
                .map_err(|err| format!("read fuzz_sentinel_parsers entry: {err}"))?
                .path();
            if !path.is_file() {
                continue;
            }
            // Skip fuzz-generated binary seeds (40 hex char SHA-1 filenames).
            // Only human-readable text seeds are validated against the contract.
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.len() == 40 && name.chars().all(|c| c.is_ascii_hexdigit()) {
                    continue;
                }
            }
            let body = fs::read_to_string(&path).map_err(|err| {
                format!(
                    "sentinel parser seed {} must be UTF-8: {err}",
                    path.display()
                )
            })?;
            let _ = HelloMessage::parse(&body);
            let _ = parse_replica_info_from_master(&body);
            let _ = parse_info_response(&body);
        }

        let hello = fs::read_to_string(corpus_root.join("hello_valid_ipv4.txt"))
            .map_err(|err| format!("read valid hello seed: {err}"))?;
        assert_eq!(
            HelloMessage::parse(hello.trim_end()).map(|msg| (msg.sentinel_port, msg.master_port)),
            Some((26379, 6379))
        );
        assert_eq!(
            HelloMessage::parse(&read_seed(&corpus_root, "hello_invalid_port.txt")?)
                .map(|msg| (msg.sentinel_port, msg.master_port)),
            Some((0, 6379))
        );
        assert!(HelloMessage::parse(&read_seed(&corpus_root, "hello_extra_field.txt")?).is_none());

        let replicas =
            parse_replica_info_from_master(&read_seed(&corpus_root, "info_master_replicas.txt")?);
        assert_eq!(
            replicas
                .iter()
                .map(|replica| (replica.ip.as_str(), replica.port, replica.slave_repl_offset))
                .collect::<Vec<_>>(),
            vec![("10.0.0.10", 6379, 12345), ("10.0.0.11", 6380, 12340)]
        );

        let partial_replicas = parse_replica_info_from_master(&read_seed(
            &corpus_root,
            "info_replica_missing_ip_or_port.txt",
        )?);
        assert_eq!(partial_replicas.len(), 1);
        assert_eq!(partial_replicas[0].ip, "10.0.0.12");
        assert_eq!(partial_replicas[0].port, 6382);

        let slave = parse_info_response(&read_seed(&corpus_root, "info_slave_down.txt")?);
        assert_eq!(slave.role, Some(Role::Slave));
        assert_eq!(slave.master_link_status, Some(false));
        assert_eq!(slave.master_link_down_since, Some(42_000));

        let malformed_link = parse_info_response(&read_seed(
            &corpus_root,
            "info_slave_malformed_link_status.txt",
        )?);
        assert_eq!(malformed_link.master_link_status, Some(false));
        assert_eq!(malformed_link.master_link_down_since, Some(7_000));

        let noisy = parse_info_response(&read_seed(&corpus_root, "info_noise_colons.txt")?);
        assert_eq!(noisy.role, Some(Role::Master));
        assert_eq!(noisy.connected_slaves, Some(0));
        Ok(())
    }

    fn read_seed(root: &std::path::Path, name: &str) -> Result<String, String> {
        std::fs::read_to_string(root.join(name))
            .map_err(|err| format!("read sentinel fuzz seed {name}: {err}"))
    }
}
