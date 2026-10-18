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
pub const SENTINEL_SCRIPT_MAX_QUEUE: usize = 256;
pub const SENTINEL_SCRIPT_MAX_RUNNING: usize = 16;
pub const SENTINEL_SCRIPT_MAX_RETRY: u32 = 10;

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
    pub config_file_path: Option<std::path::PathBuf>,
    pub(crate) running_script_children: HashMap<i64, std::process::Child>,
}

impl Default for SentinelState {
    fn default() -> Self {
        Self::new()
    }
}

/// Generate a unique 40-char lowercase-hex run id for this sentinel instance,
/// the analogue of redis's `server.runid` (getRandomHexChars). It must differ
/// between instances: sentinels identify peers by run id, and a sentinel drops
/// its OWN hello messages by matching `hello.sentinel_runid == myid`
/// (discovery.rs) — a shared constant id would make every sentinel treat its
/// peers' hellos as its own and break multi-sentinel discovery/consensus.
/// (frankenredis-pkdgs)
///
/// Pure safe std: a SplitMix64 stream seeded from the wall-clock nanosecond
/// counter mixed with a process-global sequence counter, so two instances
/// constructed in the same process (or the same nanosecond) still diverge.
#[must_use]
pub fn generate_run_id() -> [u8; 40] {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0u64, |d| d.as_nanos() as u64);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut state = nanos ^ seq.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03;
    let mut out = [0u8; 40];
    // 40 hex nibbles = 3 SplitMix64 words (16 + 16 + 8 nibbles).
    for chunk in out.chunks_mut(16) {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        for (i, b) in chunk.iter_mut().enumerate() {
            let nibble = ((z >> (i * 4)) & 0xf) as u8;
            *b = if nibble < 10 {
                b'0' + nibble
            } else {
                b'a' + (nibble - 10)
            };
        }
    }
    out
}

impl SentinelState {
    #[must_use]
    pub fn new() -> Self {
        let myid = generate_run_id();
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
            config_file_path: None,
            running_script_children: HashMap::new(),
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

    pub fn flush_config_if_configured(&self) {
        if let Some(ref path) = self.config_file_path {
            let content = format_sentinel_config(self);
            let _ = std::fs::write(path, content);
        }
    }

    pub fn queue_script_job(&mut self, path: String, args: Vec<String>) {
        if self.scripts_queue.len() >= SENTINEL_SCRIPT_MAX_QUEUE {
            return;
        }
        self.scripts_queue.push(ScriptJob {
            path,
            args,
            retry_count: 0,
            pid: None,
            start_time_ms: 0,
        });
    }

    pub fn queue_notification_script(
        &mut self,
        master_name: &str,
        event_type: &str,
        event_description: &str,
    ) {
        let Some(path) = self
            .masters
            .get(master_name)
            .and_then(|m| m.notification_script.clone())
        else {
            return;
        };
        self.queue_script_job(
            path,
            vec![event_type.to_string(), event_description.to_string()],
        );
    }

    pub fn queue_client_reconfig_script(
        &mut self,
        master_name: &str,
        role: &str,
        state: &str,
        from_ip: &str,
        from_port: u16,
        to_ip: &str,
        to_port: u16,
    ) {
        let Some(path) = self
            .masters
            .get(master_name)
            .and_then(|m| m.client_reconfig_script.clone())
        else {
            return;
        };
        // Upstream sentinelRemoveMatchingScripts: cancel any older unstarted
        // client-reconfig jobs for the same master before queueing the newer state.
        self.scripts_queue.retain(|job| {
            !(job.pid.is_none()
                && job.path == path
                && job.args.first().is_some_and(|m| m == master_name))
        });
        self.queue_script_job(
            path,
            vec![
                master_name.to_string(),
                role.to_string(),
                state.to_string(),
                from_ip.to_string(),
                from_port.to_string(),
                to_ip.to_string(),
                to_port.to_string(),
            ],
        );
    }

    pub fn run_pending_scripts(&mut self, now_ms: u64) {
        let max_runtime = self.debug_config.script_max_runtime;
        let retry_delay = self.debug_config.script_retry_delay;

        // 1. Collect terminated scripts and kill timed-out scripts.
        let mut idx = 0;
        while idx < self.scripts_queue.len() {
            let Some(pid) = self.scripts_queue[idx].pid else {
                idx += 1;
                continue;
            };
            let Some(mut child) = self.running_script_children.remove(&pid) else {
                idx += 1;
                continue;
            };
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.running_scripts = self.running_scripts.saturating_sub(1);
                    let retryable = status.code().is_none_or(|c| c == 1);
                    if retryable
                        && self.scripts_queue[idx].retry_count + 1 < SENTINEL_SCRIPT_MAX_RETRY
                    {
                        self.scripts_queue[idx].retry_count += 1;
                        self.scripts_queue[idx].pid = None;
                        self.scripts_queue[idx].start_time_ms = now_ms.saturating_add(retry_delay);
                        idx += 1;
                    } else {
                        self.scripts_queue.remove(idx);
                    }
                }
                Ok(None) => {
                    if now_ms.saturating_sub(self.scripts_queue[idx].start_time_ms) > max_runtime {
                        let _ = child.kill();
                        let _ = child.wait();
                        self.running_scripts = self.running_scripts.saturating_sub(1);
                        if self.scripts_queue[idx].retry_count + 1 < SENTINEL_SCRIPT_MAX_RETRY {
                            self.scripts_queue[idx].retry_count += 1;
                            self.scripts_queue[idx].pid = None;
                            self.scripts_queue[idx].start_time_ms =
                                now_ms.saturating_add(retry_delay);
                            idx += 1;
                        } else {
                            self.scripts_queue.remove(idx);
                        }
                    } else {
                        self.running_script_children.insert(pid, child);
                        idx += 1;
                    }
                }
                Err(_) => {
                    self.running_scripts = self.running_scripts.saturating_sub(1);
                    self.scripts_queue.remove(idx);
                }
            }
        }

        if self.tilt {
            return;
        }

        // 2. Spawn pending scripts up to SENTINEL_SCRIPT_MAX_RUNNING.
        let mut spawn_idx = 0;
        while spawn_idx < self.scripts_queue.len()
            && self.running_scripts < SENTINEL_SCRIPT_MAX_RUNNING
        {
            if self.scripts_queue[spawn_idx].pid.is_some()
                || self.scripts_queue[spawn_idx].start_time_ms > now_ms
            {
                spawn_idx += 1;
                continue;
            }
            let spawned = std::process::Command::new(&self.scripts_queue[spawn_idx].path)
                .args(&self.scripts_queue[spawn_idx].args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            match spawned {
                Ok(child) => {
                    let pid = i64::from(child.id());
                    self.scripts_queue[spawn_idx].pid = Some(pid);
                    self.scripts_queue[spawn_idx].start_time_ms = now_ms;
                    self.running_scripts += 1;
                    self.running_script_children.insert(pid, child);
                    spawn_idx += 1;
                }
                Err(_) => {
                    // Upstream child exits with _exit(2) on execve failure, which
                    // sentinelCollectTerminatedScripts removes without retrying.
                    self.scripts_queue.remove(spawn_idx);
                }
            }
        }
    }
}

fn parse_yes_no_token(token: &str) -> Result<bool, String> {
    if token.eq_ignore_ascii_case("yes") {
        Ok(true)
    } else if token.eq_ignore_ascii_case("no") {
        Ok(false)
    } else {
        Err(format!("Invalid yes/no value '{token}'"))
    }
}

struct ParsedSentinelDirective {
    line_number: usize,
    name: String,
    args: Vec<String>,
}

fn tokenize_config_line(line: &str, line_number: usize) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(&ch) = chars.peek() {
        if ch.is_ascii_whitespace() {
            chars.next();
            continue;
        }
        if ch == '#' {
            break;
        }
        if ch == '"' || ch == '\'' {
            let quote = ch;
            chars.next();
            let mut token = String::new();
            let mut closed = false;
            while let Some(c) = chars.next() {
                if c == '\\' {
                    if let Some(esc) = chars.next() {
                        token.push(esc);
                    }
                } else if c == quote {
                    closed = true;
                    break;
                } else {
                    token.push(c);
                }
            }
            if !closed {
                return Err(format!("line {line_number}: unbalanced quotes in config"));
            }
            tokens.push(token);
        } else {
            let mut token = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_ascii_whitespace() || c == '#' {
                    break;
                }
                token.push(c);
                chars.next();
            }
            tokens.push(token);
        }
    }
    Ok(tokens)
}

pub fn parse_sentinel_config(state: &mut SentinelState, config_text: &str) -> Result<(), String> {
    let mut directives = Vec::new();
    for (idx, line) in config_text.lines().enumerate() {
        let line_number = idx + 1;
        let tokens = tokenize_config_line(line, line_number)?;
        if let Some((first, rest)) = tokens.split_first() {
            directives.push(ParsedSentinelDirective {
                line_number,
                name: first.clone(),
                args: rest.to_vec(),
            });
        }
    }
    let now = state.previous_time;
    for directive in directives {
        if !directive.name.eq_ignore_ascii_case("sentinel") {
            continue;
        }
        let args = directive.args;
        let Some(sub) = args.first().map(|s| s.to_ascii_lowercase()) else {
            return Err(format!(
                "line {}: sentinel directive requires a subcommand",
                directive.line_number
            ));
        };
        match sub.as_str() {
            "monitor" => {
                if args.len() != 5 {
                    return Err(format!(
                        "line {}: sentinel monitor requires <name> <host> <port> <quorum>",
                        directive.line_number
                    ));
                }
                let port: u16 = args[3].parse().map_err(|_| {
                    format!(
                        "line {}: invalid port '{}' in sentinel monitor",
                        directive.line_number, args[3]
                    )
                })?;
                let quorum: u32 = args[4].parse().map_err(|_| {
                    format!(
                        "line {}: invalid quorum '{}' in sentinel monitor",
                        directive.line_number, args[4]
                    )
                })?;
                state
                    .monitor(&args[1], &args[2], port, quorum)
                    .map_err(|e| format!("line {}: {e}", directive.line_number))?;
            }
            "down-after-milliseconds" => {
                if args.len() != 3 {
                    return Err(format!(
                        "line {}: sentinel down-after-milliseconds requires <name> <ms>",
                        directive.line_number
                    ));
                }
                let ms: u64 = args[2].parse().map_err(|_| {
                    format!(
                        "line {}: invalid milliseconds '{}'",
                        directive.line_number, args[2]
                    )
                })?;
                let master = state.get_master_mut(&args[1]).ok_or_else(|| {
                    format!(
                        "line {}: No such master '{}'",
                        directive.line_number, args[1]
                    )
                })?;
                master.down_after_period = ms;
            }
            "parallel-syncs" => {
                if args.len() != 3 {
                    return Err(format!(
                        "line {}: sentinel parallel-syncs requires <name> <count>",
                        directive.line_number
                    ));
                }
                let count: i32 = args[2].parse().map_err(|_| {
                    format!(
                        "line {}: invalid parallel-syncs '{}'",
                        directive.line_number, args[2]
                    )
                })?;
                let master = state.get_master_mut(&args[1]).ok_or_else(|| {
                    format!(
                        "line {}: No such master '{}'",
                        directive.line_number, args[1]
                    )
                })?;
                master.parallel_syncs = count;
            }
            "failover-timeout" => {
                if args.len() != 3 {
                    return Err(format!(
                        "line {}: sentinel failover-timeout requires <name> <ms>",
                        directive.line_number
                    ));
                }
                let ms: u64 = args[2].parse().map_err(|_| {
                    format!(
                        "line {}: invalid failover-timeout '{}'",
                        directive.line_number, args[2]
                    )
                })?;
                let master = state.get_master_mut(&args[1]).ok_or_else(|| {
                    format!(
                        "line {}: No such master '{}'",
                        directive.line_number, args[1]
                    )
                })?;
                master.failover_timeout = ms;
            }
            "auth-pass" => {
                if args.len() != 3 {
                    return Err(format!(
                        "line {}: sentinel auth-pass requires <name> <password>",
                        directive.line_number
                    ));
                }
                let master = state.get_master_mut(&args[1]).ok_or_else(|| {
                    format!(
                        "line {}: No such master '{}'",
                        directive.line_number, args[1]
                    )
                })?;
                master.auth_pass = Some(args[2].clone());
            }
            "auth-user" => {
                if args.len() != 3 {
                    return Err(format!(
                        "line {}: sentinel auth-user requires <name> <username>",
                        directive.line_number
                    ));
                }
                let master = state.get_master_mut(&args[1]).ok_or_else(|| {
                    format!(
                        "line {}: No such master '{}'",
                        directive.line_number, args[1]
                    )
                })?;
                master.auth_user = Some(args[2].clone());
            }
            "notification-script" => {
                if args.len() != 3 {
                    return Err(format!(
                        "line {}: sentinel notification-script requires <name> <path>",
                        directive.line_number
                    ));
                }
                let master = state.get_master_mut(&args[1]).ok_or_else(|| {
                    format!(
                        "line {}: No such master '{}'",
                        directive.line_number, args[1]
                    )
                })?;
                master.notification_script = Some(args[2].clone());
            }
            "client-reconfig-script" => {
                if args.len() != 3 {
                    return Err(format!(
                        "line {}: sentinel client-reconfig-script requires <name> <path>",
                        directive.line_number
                    ));
                }
                let master = state.get_master_mut(&args[1]).ok_or_else(|| {
                    format!(
                        "line {}: No such master '{}'",
                        directive.line_number, args[1]
                    )
                })?;
                master.client_reconfig_script = Some(args[2].clone());
            }
            "config-epoch" => {
                if args.len() != 3 {
                    return Err(format!(
                        "line {}: sentinel config-epoch requires <name> <epoch>",
                        directive.line_number
                    ));
                }
                let epoch: u64 = args[2].parse().map_err(|_| {
                    format!(
                        "line {}: invalid config-epoch '{}'",
                        directive.line_number, args[2]
                    )
                })?;
                let master = state.get_master_mut(&args[1]).ok_or_else(|| {
                    format!(
                        "line {}: No such master '{}'",
                        directive.line_number, args[1]
                    )
                })?;
                master.config_epoch = epoch;
            }
            "leader-epoch" => {
                if args.len() != 3 {
                    return Err(format!(
                        "line {}: sentinel leader-epoch requires <name> <epoch>",
                        directive.line_number
                    ));
                }
                let epoch: u64 = args[2].parse().map_err(|_| {
                    format!(
                        "line {}: invalid leader-epoch '{}'",
                        directive.line_number, args[2]
                    )
                })?;
                let master = state.get_master_mut(&args[1]).ok_or_else(|| {
                    format!(
                        "line {}: No such master '{}'",
                        directive.line_number, args[1]
                    )
                })?;
                master.leader_epoch = epoch;
            }
            "known-replica" | "known-slave" => {
                if args.len() != 4 {
                    return Err(format!(
                        "line {}: sentinel {} requires <name> <ip> <port>",
                        directive.line_number, sub
                    ));
                }
                let port: u16 = args[3].parse().map_err(|_| {
                    format!(
                        "line {}: invalid replica port '{}'",
                        directive.line_number, args[3]
                    )
                })?;
                let master = state.get_master_mut(&args[1]).ok_or_else(|| {
                    format!(
                        "line {}: No such master '{}'",
                        directive.line_number, args[1]
                    )
                })?;
                let addr = SentinelAddr::new(&args[2], port);
                let slave_key = crate::failover::sentinel_slave_key(&addr);
                let slave = crate::failover::reset_slave_instance(
                    &slave_key,
                    addr,
                    master.quorum,
                    master.down_after_period,
                    now,
                );
                master.slaves.insert(slave_key, slave);
            }
            "known-sentinel" => {
                if args.len() < 4 || args.len() > 5 {
                    return Err(format!(
                        "line {}: sentinel known-sentinel requires <name> <ip> <port> [runid]",
                        directive.line_number
                    ));
                }
                let port: u16 = args[3].parse().map_err(|_| {
                    format!(
                        "line {}: invalid sentinel port '{}'",
                        directive.line_number, args[3]
                    )
                })?;
                let master = state.get_master_mut(&args[1]).ok_or_else(|| {
                    format!(
                        "line {}: No such master '{}'",
                        directive.line_number, args[1]
                    )
                })?;
                let addr = SentinelAddr::new(&args[2], port);
                let sentinel_key = format!("{}:{}", addr.ip, addr.port);
                let mut sentinel = SentinelRedisInstance::new_master(&sentinel_key, addr, 0);
                sentinel.flags = InstanceFlags::SENTINEL;
                sentinel.down_after_period = master.down_after_period;
                sentinel.initialize_created_link_state(now);
                sentinel.runid = args.get(4).cloned();
                master.sentinels.insert(sentinel_key, sentinel);
            }
            "rename-command" => {
                if args.len() != 4 {
                    return Err(format!(
                        "line {}: sentinel rename-command requires <name> <cmd> <renamed>",
                        directive.line_number
                    ));
                }
                let master = state.get_master_mut(&args[1]).ok_or_else(|| {
                    format!(
                        "line {}: No such master '{}'",
                        directive.line_number, args[1]
                    )
                })?;
                master
                    .renamed_commands
                    .insert(args[2].to_ascii_lowercase(), args[3].clone());
            }
            "announce-ip" => {
                if args.len() != 2 {
                    return Err(format!(
                        "line {}: sentinel announce-ip requires <ip>",
                        directive.line_number
                    ));
                }
                state.announce_ip = Some(args[1].clone());
            }
            "announce-port" => {
                if args.len() != 2 {
                    return Err(format!(
                        "line {}: sentinel announce-port requires <port>",
                        directive.line_number
                    ));
                }
                let port: u16 = args[1].parse().map_err(|_| {
                    format!(
                        "line {}: invalid announce-port '{}'",
                        directive.line_number, args[1]
                    )
                })?;
                state.announce_port = Some(port);
            }
            "sentinel-user" => {
                if args.len() != 2 {
                    return Err(format!(
                        "line {}: sentinel sentinel-user requires <user>",
                        directive.line_number
                    ));
                }
                state.sentinel_auth_user = Some(args[1].clone());
            }
            "sentinel-pass" => {
                if args.len() != 2 {
                    return Err(format!(
                        "line {}: sentinel sentinel-pass requires <password>",
                        directive.line_number
                    ));
                }
                state.sentinel_auth_pass = Some(args[1].clone());
            }
            "myid" => {
                if args.len() != 2 || args[1].len() != 40 {
                    return Err(format!(
                        "line {}: sentinel myid requires a 40-char id",
                        directive.line_number
                    ));
                }
                state.myid.copy_from_slice(args[1].as_bytes());
            }
            "current-epoch" => {
                if args.len() != 2 {
                    return Err(format!(
                        "line {}: sentinel current-epoch requires <epoch>",
                        directive.line_number
                    ));
                }
                let epoch: u64 = args[1].parse().map_err(|_| {
                    format!(
                        "line {}: invalid current-epoch '{}'",
                        directive.line_number, args[1]
                    )
                })?;
                state.current_epoch = epoch;
            }
            "resolve-hostnames" => {
                if args.len() != 2 {
                    return Err(format!(
                        "line {}: sentinel resolve-hostnames requires <yes|no>",
                        directive.line_number
                    ));
                }
                state.resolve_hostnames = parse_yes_no_token(&args[1])
                    .map_err(|e| format!("line {}: {e}", directive.line_number))?;
            }
            "announce-hostnames" => {
                if args.len() != 2 {
                    return Err(format!(
                        "line {}: sentinel announce-hostnames requires <yes|no>",
                        directive.line_number
                    ));
                }
                state.announce_hostnames = parse_yes_no_token(&args[1])
                    .map_err(|e| format!("line {}: {e}", directive.line_number))?;
            }
            "deny-scripts-reconfig" => {
                if args.len() != 2 {
                    return Err(format!(
                        "line {}: sentinel deny-scripts-reconfig requires <yes|no>",
                        directive.line_number
                    ));
                }
                state.deny_scripts_reconfig = parse_yes_no_token(&args[1])
                    .map_err(|e| format!("line {}: {e}", directive.line_number))?;
            }
            other => {
                return Err(format!(
                    "line {}: Unrecognized sentinel configuration statement 'sentinel {other}'",
                    directive.line_number
                ));
            }
        }
    }
    Ok(())
}

#[must_use]
pub fn format_sentinel_config(state: &SentinelState) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "sentinel myid {}", state.myid_hex());
    if state.deny_scripts_reconfig {
        let _ = writeln!(out, "sentinel deny-scripts-reconfig yes");
    } else {
        let _ = writeln!(out, "sentinel deny-scripts-reconfig no");
    }
    if state.resolve_hostnames {
        let _ = writeln!(out, "sentinel resolve-hostnames yes");
    }
    if state.announce_hostnames {
        let _ = writeln!(out, "sentinel announce-hostnames yes");
    }
    if let Some(ip) = &state.announce_ip {
        let _ = writeln!(out, "sentinel announce-ip {ip}");
    }
    if let Some(port) = state.announce_port {
        let _ = writeln!(out, "sentinel announce-port {port}");
    }
    if let Some(user) = &state.sentinel_auth_user {
        let _ = writeln!(out, "sentinel sentinel-user {user}");
    }
    if let Some(pass) = &state.sentinel_auth_pass {
        let _ = writeln!(out, "sentinel sentinel-pass {pass}");
    }

    let mut master_names: Vec<&String> = state.masters.keys().collect();
    master_names.sort();
    for name in master_names {
        let master = &state.masters[name];
        let host = if state.announce_hostnames {
            &master.addr.hostname
        } else {
            &master.addr.ip
        };
        let _ = writeln!(
            out,
            "sentinel monitor {} {} {} {}",
            master.name, host, master.addr.port, master.quorum
        );
        if master.down_after_period != DEFAULT_DOWN_AFTER_MS {
            let _ = writeln!(
                out,
                "sentinel down-after-milliseconds {} {}",
                master.name, master.down_after_period
            );
        }
        if master.failover_timeout != DEFAULT_FAILOVER_TIMEOUT_MS {
            let _ = writeln!(
                out,
                "sentinel failover-timeout {} {}",
                master.name, master.failover_timeout
            );
        }
        if master.parallel_syncs != 1 {
            let _ = writeln!(
                out,
                "sentinel parallel-syncs {} {}",
                master.name, master.parallel_syncs
            );
        }
        if let Some(pass) = &master.auth_pass {
            let _ = writeln!(out, "sentinel auth-pass {} {pass}", master.name);
        }
        if let Some(user) = &master.auth_user {
            let _ = writeln!(out, "sentinel auth-user {} {user}", master.name);
        }
        if let Some(script) = &master.notification_script {
            let _ = writeln!(out, "sentinel notification-script {} {script}", master.name);
        }
        if let Some(script) = &master.client_reconfig_script {
            let _ = writeln!(
                out,
                "sentinel client-reconfig-script {} {script}",
                master.name
            );
        }
        let _ = writeln!(
            out,
            "sentinel config-epoch {} {}",
            master.name, master.config_epoch
        );
        let _ = writeln!(
            out,
            "sentinel leader-epoch {} {}",
            master.name, master.leader_epoch
        );

        let mut slave_keys: Vec<&String> = master.slaves.keys().collect();
        slave_keys.sort();
        for sk in slave_keys {
            let slave = &master.slaves[sk];
            let _ = writeln!(
                out,
                "sentinel known-replica {} {} {}",
                master.name, slave.addr.ip, slave.addr.port
            );
        }

        let mut sentinel_keys: Vec<&String> = master.sentinels.keys().collect();
        sentinel_keys.sort();
        for sen_key in sentinel_keys {
            let sen = &master.sentinels[sen_key];
            if let Some(runid) = &sen.runid {
                let _ = writeln!(
                    out,
                    "sentinel known-sentinel {} {} {} {}",
                    master.name, sen.addr.ip, sen.addr.port, runid
                );
            }
        }
    }
    let _ = writeln!(out, "sentinel current-epoch {}", state.current_epoch);
    out
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
    fn myid_is_unique_well_formed_run_id_pkdgs() {
        // (frankenredis-pkdgs) Every sentinel instance must get a DISTINCT
        // 40-char lowercase-hex run id (not the old shared constant), else
        // self-hello filtering in discovery.rs collapses all peers into "self".
        let a = SentinelState::new();
        let b = SentinelState::new();
        assert_eq!(a.myid.len(), 40);
        assert!(
            a.myid
                .iter()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "run id must be lowercase hex: {:?}",
            a.myid_hex()
        );
        assert_ne!(a.myid, b.myid, "two instances must not share a run id");
        // Stable within an instance.
        assert_eq!(a.myid_hex(), a.myid_hex());
        // Not the old deterministic placeholder.
        assert_ne!(a.myid_hex(), "0123456789abcdef0123456789abcdef01234567");
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
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && name.len() == 40
                && name.chars().all(|c| c.is_ascii_hexdigit())
            {
                continue;
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

    #[test]
    fn sentinel_config_parse_and_flushconfig_roundtrip() {
        let cfg = "\
port 26379
sentinel myid 0123456789abcdef0123456789abcdef01234567
sentinel announce-ip 10.0.0.5
sentinel announce-port 26380
sentinel resolve-hostnames yes
sentinel announce-hostnames yes
sentinel monitor mymaster 127.0.0.1 6379 2
sentinel down-after-milliseconds mymaster 5000
sentinel failover-timeout mymaster 15000
sentinel parallel-syncs mymaster 3
sentinel auth-pass mymaster secretpass
sentinel auth-user mymaster sentineluser
sentinel config-epoch mymaster 4
sentinel leader-epoch mymaster 5
sentinel known-replica mymaster 127.0.0.1 6380
sentinel known-sentinel mymaster 127.0.0.1 26381 fedcba9876543210fedcba9876543210fedcba98
sentinel current-epoch 7
";
        let mut state = SentinelState::new();
        parse_sentinel_config(&mut state, cfg).expect("parse sentinel.conf");
        assert_eq!(state.myid_hex(), "0123456789abcdef0123456789abcdef01234567");
        assert_eq!(state.announce_ip.as_deref(), Some("10.0.0.5"));
        assert_eq!(state.announce_port, Some(26380));
        assert!(state.resolve_hostnames);
        assert!(state.announce_hostnames);
        assert_eq!(state.current_epoch, 7);

        let master = state.get_master("mymaster").expect("mymaster registered");
        assert_eq!(master.addr.port, 6379);
        assert_eq!(master.quorum, 2);
        assert_eq!(master.down_after_period, 5000);
        assert_eq!(master.failover_timeout, 15000);
        assert_eq!(master.parallel_syncs, 3);
        assert_eq!(master.auth_pass.as_deref(), Some("secretpass"));
        assert_eq!(master.auth_user.as_deref(), Some("sentineluser"));
        assert_eq!(master.config_epoch, 4);
        assert_eq!(master.leader_epoch, 5);
        assert!(master.slaves.contains_key("127.0.0.1:6380"));
        assert!(master.sentinels.contains_key("127.0.0.1:26381"));

        // Verify SENTINEL FLUSHCONFIG writes out a re-parseable config to disk.
        let tmp_path =
            std::env::temp_dir().join(format!("fr_sentinel_flush_{}.conf", std::process::id()));
        state.config_file_path = Some(tmp_path.clone());
        let reply = crate::commands::dispatch_sentinel_command(&mut state, &[b"FLUSHCONFIG"]);
        assert_eq!(reply, fr_protocol::RespFrame::SimpleString("OK".into()));
        let flushed = std::fs::read_to_string(&tmp_path).expect("read flushed sentinel.conf");
        let _ = std::fs::remove_file(&tmp_path);

        let mut reloaded = SentinelState::new();
        parse_sentinel_config(&mut reloaded, &flushed).expect("re-parse flushed sentinel.conf");
        assert_eq!(reloaded.myid_hex(), state.myid_hex());
        assert_eq!(reloaded.current_epoch, 7);
        let reloaded_master = reloaded.get_master("mymaster").expect("reloaded mymaster");
        assert_eq!(reloaded_master.quorum, 2);
        assert_eq!(reloaded_master.down_after_period, 5000);
        assert_eq!(reloaded_master.failover_timeout, 15000);
        assert_eq!(reloaded_master.parallel_syncs, 3);
        assert!(reloaded_master.slaves.contains_key("127.0.0.1:6380"));
        assert!(reloaded_master.sentinels.contains_key("127.0.0.1:26381"));
    }
}
