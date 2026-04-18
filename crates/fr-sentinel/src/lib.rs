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
    pub port: u16,
}

impl SentinelAddr {
    #[must_use]
    pub fn new(hostname: impl Into<String>, port: u16) -> Self {
        Self {
            hostname: hostname.into(),
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

    pub role_reported: Role,
    pub role_reported_time: u64,

    pub sentinels: HashMap<String, SentinelRedisInstance>,
    pub slaves: HashMap<String, SentinelRedisInstance>,
    pub quorum: u32,
    pub parallel_syncs: u32,
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
            notification_script: None,
            client_reconfig_script: None,
        }
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
            deny_scripts_reconfig: false,
        }
    }

    pub fn monitor(
        &mut self,
        name: impl Into<String>,
        hostname: impl Into<String>,
        port: u16,
        quorum: u32,
    ) -> Result<(), &'static str> {
        let name = name.into();
        if self.masters.contains_key(&name) {
            return Err("ERR Duplicated master name");
        }
        let addr = SentinelAddr::new(hostname, port);
        let instance = SentinelRedisInstance::new_master(&name, addr, quorum);
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
        let delta = if now > self.previous_time {
            now - self.previous_time
        } else {
            self.previous_time - now
        };
        if delta > TILT_TRIGGER_MS {
            if !self.tilt {
                self.tilt = true;
                self.tilt_start_time = now;
            }
        }
        if self.tilt && now.saturating_sub(self.tilt_start_time) > TILT_PERIOD_MS {
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

        state.check_tilt(36000);
        assert!(!state.tilt);
    }

    #[test]
    fn failover_state_default() {
        assert_eq!(FailoverState::default(), FailoverState::None);
    }
}
