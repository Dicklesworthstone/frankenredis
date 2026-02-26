#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
};

use fr_command::{CommandError, dispatch_argv, frame_to_argv};
use fr_config::{
    DecisionAction, DriftSeverity, HardenedDeviationCategory, Mode, RuntimePolicy, ThreatClass,
    TlsCfgError, TlsConfig, TlsListenerTransition, TlsRuntimeState,
    evaluate_tls_hardened_deviation, plan_tls_runtime_apply,
};
use fr_eventloop::{
    AcceptPathError, ActiveExpireCycleBudget, ActiveExpireCycleKind, ActiveExpireCyclePlan,
    BarrierOrderError, BootstrapError, CallbackDispatchOrder, EventLoopMode, EventLoopPhase,
    FdRegistrationError, LoopBootstrap, PendingWriteError, PhaseReplayError, ReadPathError,
    TickBudget, TickPlan, apply_tls_accept_rate_limit, plan_active_expire_cycle,
    plan_fd_setsize_growth, plan_readiness_callback_order, plan_tick, replay_phase_trace,
    validate_accept_path, validate_ae_barrier_order, validate_bootstrap,
    validate_fd_registration_bounds, validate_pending_write_delivery, validate_read_path,
};
use fr_persist::{AofRecord, PersistError, decode_aof_stream, encode_aof_stream};
use fr_protocol::{RespFrame, RespParseError, parse_frame};
use fr_repl::{ReplOffset, WaitAofThreshold, WaitThreshold, evaluate_wait, evaluate_waitaof};
use fr_store::{
    EvictionLoopFailure, EvictionLoopResult, EvictionLoopStatus, EvictionSafetyGateState, Store,
};

static PACKET_COUNTER: AtomicU64 = AtomicU64::new(1);
static CLIENT_ID_COUNTER: AtomicU64 = AtomicU64::new(1);
const DEFAULT_AUTH_USER: &[u8] = b"default";
const NOAUTH_ERROR: &str = "NOAUTH Authentication required.";
const WRONGPASS_ERROR: &str = "WRONGPASS invalid username-password pair or user is disabled.";
const AUTH_NOT_CONFIGURED_ERROR: &str = "ERR AUTH <password> called without any password configured for the default user. Are you sure your configuration is correct?";
const CLUSTER_UNKNOWN_SUBCOMMAND_ERROR: &str =
    "ERR Unknown subcommand or wrong number of arguments for 'CLUSTER'. Try CLUSTER HELP.";
const ACL_UNKNOWN_SUBCOMMAND_ERROR: &str =
    "ERR unknown subcommand or wrong number of arguments for 'ACL'. Try ACL HELP.";
const DEFAULT_ACLLOG_MAX_LEN: i64 = 128;

/// Static configuration parameters returned by CONFIG GET.
/// These represent sensible defaults for a standalone FrankenRedis instance.
const CONFIG_STATIC_PARAMS: &[(&str, &str)] = &[
    ("bind", "127.0.0.1"),
    ("port", "6379"),
    ("databases", "16"),
    ("maxmemory", "0"),
    ("maxmemory-policy", "noeviction"),
    ("hz", "10"),
    ("timeout", "0"),
    ("tcp-keepalive", "300"),
    ("tcp-backlog", "511"),
    ("loglevel", "notice"),
    ("logfile", ""),
    ("maxclients", "10000"),
    ("save", ""),
    ("appendonly", "no"),
    ("dir", "."),
    ("dbfilename", "dump.rdb"),
    ("appendfilename", "appendonly.aof"),
    ("protected-mode", "yes"),
    ("daemonize", "no"),
    ("pidfile", ""),
    ("lfu-log-factor", "10"),
    ("lfu-decay-time", "1"),
    ("activedefrag", "no"),
    ("lazyfree-lazy-eviction", "no"),
    ("lazyfree-lazy-expire", "no"),
    ("lazyfree-lazy-server-del", "no"),
    ("lazyfree-lazy-user-del", "no"),
    ("latency-tracking", "yes"),
    ("latency-tracking-info-percentiles", "50 99 99.9"),
    ("proto-max-bulk-len", "512000000"),
    ("lua-time-limit", "5000"),
    ("slowlog-log-slower-than", "10000"),
    ("slowlog-max-len", "128"),
    ("list-max-listpack-size", "-2"),
    ("set-max-listpack-entries", "128"),
    ("hash-max-listpack-entries", "128"),
    ("hash-max-listpack-value", "64"),
    ("zset-max-listpack-entries", "128"),
    ("zset-max-listpack-value", "64"),
    ("stream-node-max-bytes", "4096"),
    ("stream-node-max-entries", "100"),
    ("cluster-enabled", "no"),
    ("cluster-config-file", ""),
    ("cluster-node-timeout", "15000"),
    ("repl-backlog-size", "1048576"),
    ("repl-backlog-ttl", "3600"),
    ("min-replicas-to-write", "0"),
    ("min-replicas-max-lag", "10"),
    ("notify-keyspace-events", ""),
    ("always-show-logo", "yes"),
    ("rename-command", ""),
    ("io-threads", "1"),
    ("io-threads-do-reads", "no"),
    ("jemalloc-bg-thread", "yes"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct AclUser {
    passwords: Vec<Vec<u8>>,
    enabled: bool,
}

impl AclUser {
    fn new_default() -> Self {
        Self {
            passwords: Vec::new(),
            enabled: true,
        }
    }

    fn check_password(&self, password: &[u8]) -> bool {
        if self.passwords.is_empty() {
            return true;
        }
        self.passwords.iter().any(|p| p.as_slice() == password)
    }

    fn acl_list_line(&self, username: &[u8]) -> String {
        let username_str = String::from_utf8_lossy(username);
        let on_off = if self.enabled { "on" } else { "off" };
        let pass_part = if self.passwords.is_empty() {
            " nopass".to_string()
        } else {
            self.passwords
                .iter()
                .map(|_| " #<hidden>".to_string())
                .collect::<String>()
        };
        format!("user {username_str} {on_off}{pass_part} ~* &* +@all")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AuthState {
    requirepass: Option<Vec<u8>>,
    acl_users: BTreeMap<Vec<u8>, AclUser>,
    authenticated_user: Option<Vec<u8>>,
}

impl Default for AuthState {
    fn default() -> Self {
        let mut acl_users = BTreeMap::new();
        acl_users.insert(DEFAULT_AUTH_USER.to_vec(), AclUser::new_default());
        Self {
            requirepass: None,
            acl_users,
            authenticated_user: Some(DEFAULT_AUTH_USER.to_vec()),
        }
    }
}

impl AuthState {
    fn set_requirepass(&mut self, requirepass: Option<Vec<u8>>) {
        self.set_requirepass_with_session_policy(requirepass, false);
    }

    fn set_requirepass_with_session_policy(
        &mut self,
        requirepass: Option<Vec<u8>>,
        preserve_authenticated_user: bool,
    ) {
        let previous_authenticated_user = self.authenticated_user.clone();
        self.requirepass = requirepass.clone();
        let default_user = self
            .acl_users
            .entry(DEFAULT_AUTH_USER.to_vec())
            .or_insert_with(AclUser::new_default);
        if let Some(pass) = requirepass {
            default_user.passwords = vec![pass];
        } else {
            // Redis bridge behavior: empty requirepass maps back to default-user nopass.
            default_user.passwords.clear();
        }

        if !self.auth_required() {
            self.authenticated_user = Some(DEFAULT_AUTH_USER.to_vec());
        } else if preserve_authenticated_user {
            self.authenticated_user = previous_authenticated_user;
        } else {
            self.authenticated_user = None;
        }
    }

    fn add_user(&mut self, username: Vec<u8>, password: Vec<u8>) {
        let user = self
            .acl_users
            .entry(username)
            .or_insert_with(AclUser::new_default);
        user.passwords = vec![password];
        self.authenticated_user = None;
    }

    fn is_authenticated(&self) -> bool {
        self.authenticated_user.is_some()
    }

    fn auth_required(&self) -> bool {
        self.requirepass.is_some() || self.acl_users.values().any(|u| !u.passwords.is_empty())
    }

    fn requirepass(&self) -> Option<&[u8]> {
        self.requirepass.as_deref()
    }

    fn requires_auth(&self) -> bool {
        self.auth_required() && !self.is_authenticated()
    }

    fn current_user_name(&self) -> &[u8] {
        self.authenticated_user
            .as_deref()
            .unwrap_or(DEFAULT_AUTH_USER)
    }

    fn user_names(&self) -> Vec<&[u8]> {
        self.acl_users.keys().map(Vec::as_slice).collect()
    }

    fn acl_list_entries(&self) -> Vec<String> {
        self.acl_users
            .iter()
            .map(|(name, user)| user.acl_list_line(name))
            .collect()
    }

    fn get_user(&self, username: &[u8]) -> Option<&AclUser> {
        self.acl_users.get(username)
    }

    fn set_user(&mut self, username: Vec<u8>, rules: &[&[u8]]) -> Result<(), String> {
        let user = self
            .acl_users
            .entry(username)
            .or_insert_with(AclUser::new_default);
        for rule in rules {
            let rule_str = std::str::from_utf8(rule).unwrap_or("");
            if rule_str.eq_ignore_ascii_case("on") {
                user.enabled = true;
            } else if rule_str.eq_ignore_ascii_case("off") {
                user.enabled = false;
            } else if rule_str.eq_ignore_ascii_case("nopass") {
                user.passwords.clear();
            } else if rule_str.eq_ignore_ascii_case("resetpass") {
                user.passwords.clear();
                user.enabled = false;
            } else if rule_str.eq_ignore_ascii_case("allcommands")
                || rule_str == "+@all"
                || rule_str.eq_ignore_ascii_case("allkeys")
                || rule_str == "~*"
                || rule_str.eq_ignore_ascii_case("allchannels")
                || rule_str == "&*"
            {
                // Accepted but no-op for now (all users have full access)
            } else if let Some(pass) = rule_str.strip_prefix('>') {
                user.passwords.push(pass.as_bytes().to_vec());
            } else if let Some(pass) = rule_str.strip_prefix('<') {
                user.passwords.retain(|p| p.as_slice() != pass.as_bytes());
            } else {
                return Err(format!(
                    "ERR Error in ACL SETUSER modifier '{}': Syntax error",
                    rule_str
                ));
            }
        }
        Ok(())
    }

    fn del_user(&mut self, username: &[u8]) -> bool {
        if username == DEFAULT_AUTH_USER {
            return false;
        }
        self.acl_users.remove(username).is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AuthFailure {
    NotConfigured,
    WrongPass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClusterClientMode {
    ReadWrite,
    ReadOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RuntimeSpecialCommand {
    Acl,
    Config,
    Client,
    Auth,
    Hello,
    Asking,
    Readonly,
    Readwrite,
    Cluster,
    Wait,
    Waitaof,
    Multi,
    Exec,
    Discard,
    Watch,
    Unwatch,
    Quit,
    Reset,
    Slowlog,
    Save,
    Bgsave,
    Lastsave,
    Bgrewriteaof,
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClusterSubcommand {
    Help,
    Unknown,
}

#[inline]
fn classify_runtime_special_command(cmd: &[u8]) -> Option<RuntimeSpecialCommand> {
    match cmd.len() {
        3 => {
            if eq_ascii_token(cmd, b"ACL") {
                Some(RuntimeSpecialCommand::Acl)
            } else {
                None
            }
        }
        4 => {
            if eq_ascii_token(cmd, b"AUTH") {
                Some(RuntimeSpecialCommand::Auth)
            } else if eq_ascii_token(cmd, b"WAIT") {
                Some(RuntimeSpecialCommand::Wait)
            } else if eq_ascii_token(cmd, b"EXEC") {
                Some(RuntimeSpecialCommand::Exec)
            } else if eq_ascii_token(cmd, b"QUIT") {
                Some(RuntimeSpecialCommand::Quit)
            } else if eq_ascii_token(cmd, b"SAVE") {
                Some(RuntimeSpecialCommand::Save)
            } else {
                None
            }
        }
        5 => {
            if eq_ascii_token(cmd, b"HELLO") {
                Some(RuntimeSpecialCommand::Hello)
            } else if eq_ascii_token(cmd, b"MULTI") {
                Some(RuntimeSpecialCommand::Multi)
            } else if eq_ascii_token(cmd, b"WATCH") {
                Some(RuntimeSpecialCommand::Watch)
            } else if eq_ascii_token(cmd, b"RESET") {
                Some(RuntimeSpecialCommand::Reset)
            } else {
                None
            }
        }
        6 => {
            if eq_ascii_token(cmd, b"ASKING") {
                Some(RuntimeSpecialCommand::Asking)
            } else if eq_ascii_token(cmd, b"CONFIG") {
                Some(RuntimeSpecialCommand::Config)
            } else if eq_ascii_token(cmd, b"CLIENT") {
                Some(RuntimeSpecialCommand::Client)
            } else if eq_ascii_token(cmd, b"BGSAVE") {
                Some(RuntimeSpecialCommand::Bgsave)
            } else {
                None
            }
        }
        7 => {
            if eq_ascii_token(cmd, b"CLUSTER") {
                Some(RuntimeSpecialCommand::Cluster)
            } else if eq_ascii_token(cmd, b"WAITAOF") {
                Some(RuntimeSpecialCommand::Waitaof)
            } else if eq_ascii_token(cmd, b"DISCARD") {
                Some(RuntimeSpecialCommand::Discard)
            } else if eq_ascii_token(cmd, b"UNWATCH") {
                Some(RuntimeSpecialCommand::Unwatch)
            } else if eq_ascii_token(cmd, b"SLOWLOG") {
                Some(RuntimeSpecialCommand::Slowlog)
            } else {
                None
            }
        }
        8 => {
            if eq_ascii_token(cmd, b"READONLY") {
                Some(RuntimeSpecialCommand::Readonly)
            } else if eq_ascii_token(cmd, b"LASTSAVE") {
                Some(RuntimeSpecialCommand::Lastsave)
            } else if eq_ascii_token(cmd, b"SHUTDOWN") {
                Some(RuntimeSpecialCommand::Shutdown)
            } else {
                None
            }
        }
        9 => {
            if eq_ascii_token(cmd, b"READWRITE") {
                Some(RuntimeSpecialCommand::Readwrite)
            } else {
                None
            }
        }
        12 => {
            if eq_ascii_token(cmd, b"BGREWRITEAOF") {
                Some(RuntimeSpecialCommand::Bgrewriteaof)
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
fn classify_runtime_special_command_linear(cmd: &[u8]) -> Option<RuntimeSpecialCommand> {
    let command = std::str::from_utf8(cmd).ok()?;
    if command.eq_ignore_ascii_case("ACL") {
        Some(RuntimeSpecialCommand::Acl)
    } else if command.eq_ignore_ascii_case("CONFIG") {
        Some(RuntimeSpecialCommand::Config)
    } else if command.eq_ignore_ascii_case("AUTH") {
        Some(RuntimeSpecialCommand::Auth)
    } else if command.eq_ignore_ascii_case("HELLO") {
        Some(RuntimeSpecialCommand::Hello)
    } else if command.eq_ignore_ascii_case("ASKING") {
        Some(RuntimeSpecialCommand::Asking)
    } else if command.eq_ignore_ascii_case("READONLY") {
        Some(RuntimeSpecialCommand::Readonly)
    } else if command.eq_ignore_ascii_case("READWRITE") {
        Some(RuntimeSpecialCommand::Readwrite)
    } else if command.eq_ignore_ascii_case("CLUSTER") {
        Some(RuntimeSpecialCommand::Cluster)
    } else if command.eq_ignore_ascii_case("WAIT") {
        Some(RuntimeSpecialCommand::Wait)
    } else if command.eq_ignore_ascii_case("WAITAOF") {
        Some(RuntimeSpecialCommand::Waitaof)
    } else if command.eq_ignore_ascii_case("MULTI") {
        Some(RuntimeSpecialCommand::Multi)
    } else if command.eq_ignore_ascii_case("EXEC") {
        Some(RuntimeSpecialCommand::Exec)
    } else if command.eq_ignore_ascii_case("DISCARD") {
        Some(RuntimeSpecialCommand::Discard)
    } else if command.eq_ignore_ascii_case("WATCH") {
        Some(RuntimeSpecialCommand::Watch)
    } else if command.eq_ignore_ascii_case("UNWATCH") {
        Some(RuntimeSpecialCommand::Unwatch)
    } else if command.eq_ignore_ascii_case("QUIT") {
        Some(RuntimeSpecialCommand::Quit)
    } else if command.eq_ignore_ascii_case("CLIENT") {
        Some(RuntimeSpecialCommand::Client)
    } else if command.eq_ignore_ascii_case("RESET") {
        Some(RuntimeSpecialCommand::Reset)
    } else if command.eq_ignore_ascii_case("SLOWLOG") {
        Some(RuntimeSpecialCommand::Slowlog)
    } else if command.eq_ignore_ascii_case("SAVE") {
        Some(RuntimeSpecialCommand::Save)
    } else if command.eq_ignore_ascii_case("BGSAVE") {
        Some(RuntimeSpecialCommand::Bgsave)
    } else if command.eq_ignore_ascii_case("LASTSAVE") {
        Some(RuntimeSpecialCommand::Lastsave)
    } else if command.eq_ignore_ascii_case("BGREWRITEAOF") {
        Some(RuntimeSpecialCommand::Bgrewriteaof)
    } else if command.eq_ignore_ascii_case("SHUTDOWN") {
        Some(RuntimeSpecialCommand::Shutdown)
    } else {
        None
    }
}

#[inline]
fn classify_cluster_subcommand(cmd: &[u8]) -> Result<ClusterSubcommand, CommandError> {
    if cmd.len() == 4 && eq_ascii_token(cmd, b"HELP") {
        return Ok(ClusterSubcommand::Help);
    }
    if std::str::from_utf8(cmd).is_err() {
        return Err(CommandError::InvalidUtf8Argument);
    }
    Ok(ClusterSubcommand::Unknown)
}

#[cfg(test)]
fn classify_cluster_subcommand_linear(cmd: &[u8]) -> Result<ClusterSubcommand, CommandError> {
    let subcommand = std::str::from_utf8(cmd).map_err(|_| CommandError::InvalidUtf8Argument)?;
    if subcommand.eq_ignore_ascii_case("HELP") {
        Ok(ClusterSubcommand::Help)
    } else {
        Ok(ClusterSubcommand::Unknown)
    }
}

#[inline]
fn eq_ascii_token(lhs: &[u8], rhs: &[u8]) -> bool {
    lhs.eq_ignore_ascii_case(rhs)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClusterClientState {
    mode: ClusterClientMode,
    asking: bool,
}

impl Default for ClusterClientState {
    fn default() -> Self {
        Self {
            mode: ClusterClientMode::ReadWrite,
            asking: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ReplicationAckState {
    primary_offset: ReplOffset,
    local_fsync_offset: ReplOffset,
    replica_ack_offsets: Vec<ReplOffset>,
    replica_fsync_offsets: Vec<ReplOffset>,
}

#[derive(Debug, Clone, Default)]
struct TransactionState {
    in_transaction: bool,
    command_queue: Vec<Vec<Vec<u8>>>,
    watched_keys: Vec<(Vec<u8>, u64)>,
    watch_dirty: bool,
}

/// A single slow log entry recording a command that exceeded the threshold.
#[derive(Debug, Clone)]
struct SlowlogEntry {
    id: u64,
    timestamp_sec: u64,
    duration_us: u64,
    argv: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvidenceEvent {
    pub ts_utc: String,
    pub ts_ms: u64,
    pub packet_id: u64,
    pub mode: Mode,
    pub severity: DriftSeverity,
    pub threat_class: ThreatClass,
    pub decision_action: DecisionAction,
    pub subsystem: &'static str,
    pub action: &'static str,
    pub reason_code: &'static str,
    pub reason: String,
    pub input_digest: String,
    pub output_digest: String,
    pub state_digest_before: String,
    pub state_digest_after: String,
    pub replay_cmd: String,
    pub artifact_refs: Vec<String>,
    pub confidence: Option<f64>,
}

#[derive(Debug, Default)]
pub struct EvidenceLedger {
    events: Vec<EvidenceEvent>,
}

impl EvidenceLedger {
    pub fn record(&mut self, event: EvidenceEvent) {
        self.events.push(event);
    }

    #[must_use]
    pub fn events(&self) -> &[EvidenceEvent] {
        &self.events
    }
}

#[derive(Debug)]
pub struct Runtime {
    policy: RuntimePolicy,
    store: Store,
    aof_records: Vec<AofRecord>,
    evidence: EvidenceLedger,
    tls_state: TlsRuntimeState,
    auth_state: AuthState,
    acllog_max_len: i64,
    cluster_state: ClusterClientState,
    replication_ack_state: ReplicationAckState,
    transaction_state: TransactionState,
    maxmemory_bytes: usize,
    maxmemory_not_counted_bytes: usize,
    maxmemory_eviction_sample_limit: usize,
    maxmemory_eviction_max_cycles: usize,
    eviction_safety_gate: EvictionSafetyGateState,
    last_eviction_loop: Option<EvictionLoopResult>,
    active_expire_db_cursor: usize,
    active_expire_key_cursor: usize,
    active_expire_budget: ActiveExpireCycleBudget,
    last_active_expire_cycle: Option<ActiveExpireCycleStats>,
    /// Per-client connection ID (monotonically increasing).
    client_id: u64,
    /// Per-client name set via CLIENT SETNAME.
    client_name: Option<Vec<u8>>,
    /// Flags: client no-evict mode.
    client_no_evict: bool,
    /// Flags: client no-touch mode.
    client_no_touch: bool,
    /// Slow log: ring buffer of slow queries.
    slowlog: Vec<SlowlogEntry>,
    /// Slow log entry ID counter.
    slowlog_next_id: u64,
    /// Slow log threshold in microseconds (slowlog-log-slower-than config).
    slowlog_log_slower_than_us: i64,
    /// Slow log maximum length.
    slowlog_max_len: usize,
    /// Last successful save timestamp (seconds since epoch).
    last_save_time_sec: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveExpireCycleStats {
    pub plan: ActiveExpireCyclePlan,
    pub sampled_keys: usize,
    pub evicted_keys: usize,
}

struct ThreatEventInput<'a> {
    now_ms: u64,
    packet_id: u64,
    threat_class: ThreatClass,
    preferred_deviation: Option<HardenedDeviationCategory>,
    subsystem: &'static str,
    action: &'static str,
    reason_code: &'static str,
    reason: String,
    input_digest: String,
    state_before: &'a str,
    output: &'a RespFrame,
}

impl Runtime {
    #[must_use]
    pub fn new(policy: RuntimePolicy) -> Self {
        Self {
            policy,
            store: Store::new(),
            aof_records: Vec::new(),
            evidence: EvidenceLedger::default(),
            tls_state: TlsRuntimeState::default(),
            auth_state: AuthState::default(),
            acllog_max_len: DEFAULT_ACLLOG_MAX_LEN,
            cluster_state: ClusterClientState::default(),
            replication_ack_state: ReplicationAckState::default(),
            transaction_state: TransactionState::default(),
            maxmemory_bytes: 0,
            maxmemory_not_counted_bytes: 0,
            maxmemory_eviction_sample_limit: 16,
            maxmemory_eviction_max_cycles: 4,
            eviction_safety_gate: EvictionSafetyGateState::default(),
            last_eviction_loop: None,
            active_expire_db_cursor: 0,
            active_expire_key_cursor: 0,
            active_expire_budget: ActiveExpireCycleBudget::default(),
            last_active_expire_cycle: None,
            client_id: CLIENT_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
            client_name: None,
            client_no_evict: false,
            client_no_touch: false,
            slowlog: Vec::new(),
            slowlog_next_id: 0,
            slowlog_log_slower_than_us: 10_000, // default: 10ms
            slowlog_max_len: 128,
            last_save_time_sec: 0,
        }
    }

    #[must_use]
    pub fn default_strict() -> Self {
        Self::new(RuntimePolicy::default())
    }

    #[must_use]
    pub fn default_hardened() -> Self {
        Self::new(RuntimePolicy::hardened())
    }

    #[must_use]
    pub fn plan_event_loop_tick(
        pending_accepts: usize,
        pending_commands: usize,
        budget: TickBudget,
        mode: EventLoopMode,
    ) -> TickPlan {
        plan_tick(pending_accepts, pending_commands, budget, mode)
    }

    #[must_use]
    pub fn plan_event_loop_tick_with_tls_budget(
        pending_accepts: usize,
        pending_commands: usize,
        pending_tls_accepts: usize,
        max_new_tls_connections_per_cycle: usize,
        budget: TickBudget,
        mode: EventLoopMode,
    ) -> TickPlan {
        let mut plan = plan_tick(pending_accepts, pending_commands, budget, mode);
        let pending_tls_accepts = pending_tls_accepts.min(pending_accepts);
        let pending_non_tls_accepts = pending_accepts.saturating_sub(pending_tls_accepts);
        let tls_accept_plan = apply_tls_accept_rate_limit(
            plan.stats.accepted,
            pending_tls_accepts,
            pending_non_tls_accepts,
            max_new_tls_connections_per_cycle,
        );

        plan.stats.accepted = tls_accept_plan.total_accepted;
        plan.stats.accept_backlog_remaining = pending_accepts.saturating_sub(plan.stats.accepted);
        plan
    }

    #[must_use]
    pub fn run_active_expire_cycle(
        &mut self,
        now_ms: u64,
        cycle_kind: ActiveExpireCycleKind,
    ) -> ActiveExpireCycleStats {
        let plan = plan_active_expire_cycle(
            cycle_kind,
            self.store.count_expiring_keys(),
            self.active_expire_db_cursor,
            1,
            self.active_expire_budget,
        );
        let cycle_result = self.store.run_active_expire_cycle(
            now_ms,
            self.active_expire_key_cursor,
            plan.sample_limit,
        );
        self.active_expire_db_cursor = plan.next_db_index;
        self.active_expire_key_cursor = cycle_result.next_cursor;

        let stats = ActiveExpireCycleStats {
            plan,
            sampled_keys: cycle_result.sampled_keys,
            evicted_keys: cycle_result.evicted_keys,
        };
        self.last_active_expire_cycle = Some(stats);
        stats
    }

    #[must_use]
    pub fn run_server_cron_active_expire_cycle(&mut self, now_ms: u64) -> ActiveExpireCycleStats {
        self.run_active_expire_cycle(now_ms, ActiveExpireCycleKind::Slow)
    }

    #[must_use]
    pub fn last_active_expire_cycle_stats(&self) -> Option<ActiveExpireCycleStats> {
        self.last_active_expire_cycle
    }

    pub fn replay_event_loop_phase_trace(
        trace: &[EventLoopPhase],
    ) -> Result<usize, PhaseReplayError> {
        replay_phase_trace(trace)
    }

    pub fn validate_event_loop_bootstrap(bootstrap: LoopBootstrap) -> Result<(), BootstrapError> {
        validate_bootstrap(bootstrap)
    }

    #[must_use]
    pub fn plan_event_loop_readiness_order(
        readable_ready: bool,
        writable_ready: bool,
        ae_barrier: bool,
    ) -> CallbackDispatchOrder {
        plan_readiness_callback_order(readable_ready, writable_ready, ae_barrier)
    }

    pub fn validate_event_loop_barrier_order(
        readable_ready: bool,
        writable_ready: bool,
        ae_barrier: bool,
        observed: CallbackDispatchOrder,
    ) -> Result<(), BarrierOrderError> {
        validate_ae_barrier_order(readable_ready, writable_ready, ae_barrier, observed)
    }

    pub fn validate_event_loop_fd_registration(
        fd: usize,
        setsize: usize,
    ) -> Result<(), FdRegistrationError> {
        validate_fd_registration_bounds(fd, setsize)
    }

    pub fn plan_event_loop_fd_resize(
        current_setsize: usize,
        requested_fd: usize,
        max_setsize: usize,
    ) -> Result<usize, FdRegistrationError> {
        plan_fd_setsize_growth(current_setsize, requested_fd, max_setsize)
    }

    pub fn validate_event_loop_accept_path(
        current_clients: usize,
        max_clients: usize,
        read_handler_bound: bool,
    ) -> Result<(), AcceptPathError> {
        validate_accept_path(current_clients, max_clients, read_handler_bound)
    }

    pub fn validate_event_loop_read_path(
        current_query_buffer_len: usize,
        newly_read_bytes: usize,
        query_buffer_limit: usize,
        fatal_read_error: bool,
    ) -> Result<usize, ReadPathError> {
        validate_read_path(
            current_query_buffer_len,
            newly_read_bytes,
            query_buffer_limit,
            fatal_read_error,
        )
    }

    pub fn validate_event_loop_pending_write_delivery(
        queued_before_flush: &[u64],
        flushed_now: &[u64],
        pending_after_flush: &[u64],
    ) -> Result<(), PendingWriteError> {
        validate_pending_write_delivery(queued_before_flush, flushed_now, pending_after_flush)
    }

    #[must_use]
    pub fn evidence(&self) -> &EvidenceLedger {
        &self.evidence
    }

    #[must_use]
    pub fn aof_records(&self) -> &[AofRecord] {
        &self.aof_records
    }

    #[must_use]
    pub fn encoded_aof_stream(&self) -> Vec<u8> {
        encode_aof_stream(&self.aof_records)
    }

    pub fn replay_aof_stream(
        &mut self,
        input: &[u8],
        now_ms: u64,
    ) -> Result<Vec<RespFrame>, PersistError> {
        let records = decode_aof_stream(input)?;
        Ok(self.replay_aof_records(&records, now_ms))
    }

    #[must_use]
    pub fn replay_aof_records(&mut self, records: &[AofRecord], now_ms: u64) -> Vec<RespFrame> {
        let mut replies = Vec::with_capacity(records.len());
        for (index, record) in records.iter().enumerate() {
            let replay_now_ms = now_ms.saturating_add(index as u64);
            let reply = self.execute_frame(record.to_resp_frame(), replay_now_ms);
            replies.push(reply);
        }
        replies
    }

    #[must_use]
    pub fn tls_runtime_state(&self) -> &TlsRuntimeState {
        &self.tls_state
    }

    pub fn configure_maxmemory_enforcement(
        &mut self,
        maxmemory_bytes: usize,
        not_counted_bytes: usize,
        sample_limit: usize,
        max_cycles: usize,
    ) {
        self.maxmemory_bytes = maxmemory_bytes;
        self.maxmemory_not_counted_bytes = not_counted_bytes;
        self.maxmemory_eviction_sample_limit = sample_limit.max(1);
        self.maxmemory_eviction_max_cycles = max_cycles;
    }

    pub fn set_eviction_safety_gate(&mut self, safety_gate: EvictionSafetyGateState) {
        self.eviction_safety_gate = safety_gate;
    }

    #[must_use]
    pub fn maxmemory_pressure_state(&self) -> fr_store::MaxmemoryPressureState {
        self.store
            .classify_maxmemory_pressure(self.maxmemory_bytes, self.maxmemory_not_counted_bytes)
    }

    #[must_use]
    pub fn last_eviction_loop_result(&self) -> Option<EvictionLoopResult> {
        self.last_eviction_loop
    }

    pub fn set_requirepass(&mut self, requirepass: Option<Vec<u8>>) {
        self.auth_state.set_requirepass(requirepass);
    }

    pub fn add_user(&mut self, username: Vec<u8>, password: Vec<u8>) {
        self.auth_state.add_user(username, password);
    }

    #[must_use]
    pub fn is_authenticated(&self) -> bool {
        self.auth_state.is_authenticated()
    }

    #[must_use]
    pub fn is_cluster_read_only(&self) -> bool {
        self.cluster_state.mode == ClusterClientMode::ReadOnly
    }

    #[must_use]
    pub fn is_cluster_asking(&self) -> bool {
        self.cluster_state.asking
    }

    #[cfg(test)]
    fn set_replication_ack_state_for_tests(
        &mut self,
        primary_offset: u64,
        local_fsync_offset: u64,
        replica_ack_offsets: &[u64],
        replica_fsync_offsets: &[u64],
    ) {
        self.replication_ack_state.primary_offset = ReplOffset(primary_offset);
        self.replication_ack_state.local_fsync_offset = ReplOffset(local_fsync_offset);
        self.replication_ack_state.replica_ack_offsets = replica_ack_offsets
            .iter()
            .map(|offset| ReplOffset(*offset))
            .collect();
        self.replication_ack_state.replica_fsync_offsets = replica_fsync_offsets
            .iter()
            .map(|offset| ReplOffset(*offset))
            .collect();
    }

    pub fn apply_tls_config(
        &mut self,
        candidate: TlsConfig,
        now_ms: u64,
    ) -> Result<(), TlsCfgError> {
        let packet_id = next_packet_id();
        let input_digest = digest_bytes(format!("{candidate:?}").as_bytes());
        let state_before = self.store.state_digest();

        let plan = match plan_tls_runtime_apply(&self.tls_state, candidate) {
            Ok(plan) => plan,
            Err(error) => {
                let preferred_deviation = preferred_tls_deviation_for_error(&error);
                let gated_error = self.gate_tls_error_for_mode(error, preferred_deviation);
                self.record_tls_config_event(
                    now_ms,
                    packet_id,
                    &input_digest,
                    &state_before,
                    &gated_error,
                    preferred_deviation,
                );
                return Err(gated_error);
            }
        };

        let mut next_state = self.tls_state.clone();
        match plan.listener_transition {
            TlsListenerTransition::Enable => next_state.tls_listener_enabled = true,
            TlsListenerTransition::Disable => next_state.tls_listener_enabled = false,
            TlsListenerTransition::Keep => {}
        }
        if plan.requires_context_swap {
            next_state.active_config = if plan.candidate_config.tls_enabled() {
                Some(plan.candidate_config.clone())
            } else {
                None
            };
        }
        if plan.requires_connection_type_configure {
            next_state.connection_type_configured = true;
        }
        self.tls_state = next_state;
        Ok(())
    }

    pub fn execute_frame(&mut self, frame: RespFrame, now_ms: u64) -> RespFrame {
        let packet_id = next_packet_id();
        let input_digest = digest_bytes(&frame.to_bytes());
        let state_before = self.store.state_digest();

        if let Some(reply) =
            self.preflight_gate(&frame, now_ms, packet_id, &input_digest, &state_before)
        {
            return reply;
        }

        let argv = match frame_to_argv(&frame) {
            Ok(argv) => argv,
            Err(_) => {
                let reply =
                    RespFrame::Error("ERR Protocol error: invalid command frame".to_string());
                self.record_threat_event(ThreatEventInput {
                    now_ms,
                    packet_id,
                    threat_class: ThreatClass::ParserAbuse,
                    preferred_deviation: Some(HardenedDeviationCategory::BoundedParserDiagnostics),
                    subsystem: "router",
                    action: "reject_frame",
                    reason_code: "invalid_command_frame",
                    reason: "invalid command frame".to_string(),
                    input_digest,
                    state_before: &state_before,
                    output: &reply,
                });
                return reply;
            }
        };

        let command_name = match std::str::from_utf8(&argv[0]) {
            Ok(command_name) => command_name,
            Err(_) => return command_error_to_resp(CommandError::InvalidUtf8Argument),
        };
        let special_command = classify_runtime_special_command(command_name.as_bytes());

        match special_command {
            Some(RuntimeSpecialCommand::Auth) => return self.handle_auth_command(&argv),
            Some(RuntimeSpecialCommand::Hello) => return self.handle_hello_command(&argv),
            _ => {}
        }

        if self.auth_state.requires_auth() {
            let reply = RespFrame::Error(NOAUTH_ERROR.to_string());
            self.record_threat_event(ThreatEventInput {
                now_ms,
                packet_id,
                threat_class: ThreatClass::AuthPolicyConfusion,
                preferred_deviation: None,
                subsystem: "admission_gate",
                action: "reject_unauthenticated_command",
                reason_code: "auth.noauth_gate_violation",
                reason: format!(
                    "rejected '{}' prior to dispatch while unauthenticated",
                    command_name
                ),
                input_digest,
                state_before: &state_before,
                output: &reply,
            });
            return reply;
        }

        match special_command {
            Some(RuntimeSpecialCommand::Acl) => return self.handle_acl_command(&argv),
            Some(RuntimeSpecialCommand::Config) => return self.handle_config_command(&argv),
            Some(RuntimeSpecialCommand::Client) => return self.handle_client_command(&argv),
            Some(RuntimeSpecialCommand::Asking) => return self.handle_asking_command(&argv),
            Some(RuntimeSpecialCommand::Readonly) => return self.handle_readonly_command(&argv),
            Some(RuntimeSpecialCommand::Readwrite) => return self.handle_readwrite_command(&argv),
            Some(RuntimeSpecialCommand::Cluster) => return self.handle_cluster_command(&argv),
            Some(RuntimeSpecialCommand::Wait) => return self.handle_wait_command(&argv),
            Some(RuntimeSpecialCommand::Waitaof) => return self.handle_waitaof_command(&argv),
            Some(RuntimeSpecialCommand::Multi) => return self.handle_multi_command(),
            Some(RuntimeSpecialCommand::Exec) => {
                return self.handle_exec_command(now_ms, packet_id, &input_digest, &state_before);
            }
            Some(RuntimeSpecialCommand::Discard) => return self.handle_discard_command(),
            Some(RuntimeSpecialCommand::Watch) => return self.handle_watch_command(&argv, now_ms),
            Some(RuntimeSpecialCommand::Unwatch) => return self.handle_unwatch_command(&argv),
            Some(RuntimeSpecialCommand::Quit) => return RespFrame::SimpleString("OK".to_string()),
            Some(RuntimeSpecialCommand::Reset) => return self.handle_reset_command(&argv),
            Some(RuntimeSpecialCommand::Slowlog) => {
                return self.handle_slowlog_command(&argv);
            }
            Some(RuntimeSpecialCommand::Save) => {
                return self.handle_save_command(&argv, now_ms);
            }
            Some(RuntimeSpecialCommand::Bgsave) => {
                return self.handle_bgsave_command(&argv, now_ms);
            }
            Some(RuntimeSpecialCommand::Lastsave) => {
                return self.handle_lastsave_command(&argv);
            }
            Some(RuntimeSpecialCommand::Bgrewriteaof) => {
                return self.handle_bgrewriteaof_command(&argv);
            }
            Some(RuntimeSpecialCommand::Shutdown) => {
                return self.handle_shutdown_command(&argv);
            }
            _ => {}
        }

        // If inside a MULTI transaction, queue the command instead of executing it
        if self.transaction_state.in_transaction {
            self.transaction_state.command_queue.push(argv);
            return RespFrame::SimpleString("QUEUED".to_string());
        }

        if let Some(reply) = self.enforce_maxmemory_before_dispatch(
            &argv,
            now_ms,
            packet_id,
            &input_digest,
            &state_before,
        ) {
            return reply;
        }

        let _ = self.run_active_expire_cycle(now_ms, ActiveExpireCycleKind::Fast);
        match dispatch_argv(&argv, &mut self.store, now_ms) {
            Ok(reply) => {
                self.capture_aof_record(&argv);
                reply
            }
            Err(err) => command_error_to_resp(err),
        }
    }

    pub fn execute_bytes(&mut self, input: &[u8], now_ms: u64) -> Vec<u8> {
        let packet_id = next_packet_id();
        let input_digest = digest_bytes(input);
        let state_before = self.store.state_digest();
        match parse_frame(input) {
            Ok(parsed) => self.execute_frame(parsed.frame, now_ms).to_bytes(),
            Err(err) => {
                let reason = err.to_string();
                let reply = protocol_error_to_resp(err);
                self.record_threat_event(ThreatEventInput {
                    now_ms,
                    packet_id,
                    threat_class: ThreatClass::ParserAbuse,
                    preferred_deviation: Some(HardenedDeviationCategory::BoundedParserDiagnostics),
                    subsystem: "protocol",
                    action: "parse_failure",
                    reason_code: "protocol_parse_failure",
                    reason,
                    input_digest,
                    state_before: &state_before,
                    output: &reply,
                });
                reply.to_bytes()
            }
        }
    }

    fn enforce_maxmemory_before_dispatch(
        &mut self,
        argv: &[Vec<u8>],
        now_ms: u64,
        packet_id: u64,
        input_digest: &str,
        state_before: &str,
    ) -> Option<RespFrame> {
        if self.maxmemory_bytes == 0 {
            self.last_eviction_loop = None;
            return None;
        }

        let loop_result = self.store.run_bounded_eviction_loop(
            now_ms,
            self.maxmemory_bytes,
            self.maxmemory_not_counted_bytes,
            self.maxmemory_eviction_sample_limit,
            self.maxmemory_eviction_max_cycles,
            self.eviction_safety_gate,
        );
        self.last_eviction_loop = Some(loop_result);

        if loop_result.status == EvictionLoopStatus::Ok {
            return None;
        }

        if !Self::command_advances_replication_offset(argv) {
            return None;
        }

        let hardened_nonallowlisted = self.policy.mode == Mode::Hardened
            && !self
                .policy
                .is_deviation_allowed(HardenedDeviationCategory::ResourceClamp);

        let reason_code = if hardened_nonallowlisted {
            "expireevict.hardened_nonallowlisted_rejected"
        } else if loop_result.failure == Some(EvictionLoopFailure::SafetyGateSuppressed) {
            "evict.safety_gate_contract_violation"
        } else {
            "evict.eviction_loop_contract_violation"
        };

        let reason = if hardened_nonallowlisted {
            format!(
                "hardened maxmemory pressure handling rejected because resource clamp is not allowlisted (bytes_to_free_after={})",
                loop_result.bytes_to_free_after
            )
        } else if loop_result.failure == Some(EvictionLoopFailure::SafetyGateSuppressed) {
            "eviction suppressed by safety gate while over maxmemory".to_string()
        } else if loop_result.failure == Some(EvictionLoopFailure::NoCandidates) {
            "eviction loop found no candidates while over maxmemory".to_string()
        } else {
            format!(
                "bounded eviction loop exhausted with status {:?} (bytes_to_free_after={})",
                loop_result.status, loop_result.bytes_to_free_after
            )
        };

        let reply =
            RespFrame::Error("OOM command not allowed when used memory > 'maxmemory'.".to_string());
        self.record_threat_event(ThreatEventInput {
            now_ms,
            packet_id,
            threat_class: ThreatClass::ResourceExhaustion,
            preferred_deviation: Some(HardenedDeviationCategory::ResourceClamp),
            subsystem: "eviction",
            action: "maxmemory_enforcement",
            reason_code,
            reason,
            input_digest: input_digest.to_string(),
            state_before,
            output: &reply,
        });
        Some(reply)
    }

    fn capture_aof_record(&mut self, argv: &[Vec<u8>]) {
        if argv.is_empty() {
            return;
        }
        if !Self::command_advances_replication_offset(argv) {
            return;
        }
        self.aof_records.push(AofRecord {
            argv: argv.to_vec(),
        });
        self.replication_ack_state.primary_offset.0 = self
            .replication_ack_state
            .primary_offset
            .0
            .saturating_add(1);
        self.replication_ack_state.local_fsync_offset = self.replication_ack_state.primary_offset;
    }

    fn command_advances_replication_offset(argv: &[Vec<u8>]) -> bool {
        let Some(command) = argv.first() else {
            return false;
        };
        fr_command::is_write_command(command)
    }

    fn handle_auth_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 2 && argv.len() != 3 {
            return command_error_to_resp(CommandError::WrongArity("AUTH"));
        }

        let (username, password) = if argv.len() == 2 {
            (DEFAULT_AUTH_USER, argv[1].as_slice())
        } else {
            (argv[1].as_slice(), argv[2].as_slice())
        };

        match self.authenticate_user(username, password) {
            Ok(()) => RespFrame::SimpleString("OK".to_string()),
            Err(AuthFailure::NotConfigured) => {
                RespFrame::Error(AUTH_NOT_CONFIGURED_ERROR.to_string())
            }
            Err(AuthFailure::WrongPass) => RespFrame::Error(WRONGPASS_ERROR.to_string()),
        }
    }

    fn handle_hello_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() < 2 {
            return command_error_to_resp(CommandError::WrongArity("HELLO"));
        }

        let protocol_version = match parse_i64_arg(&argv[1]) {
            Ok(version) => version,
            Err(err) => return command_error_to_resp(err),
        };

        if protocol_version != 2 && protocol_version != 3 {
            return RespFrame::Error(format!(
                "NOPROTO unsupported protocol version '{}'",
                protocol_version
            ));
        }

        let mut auth_credentials: Option<(&[u8], &[u8])> = None;
        let mut options = argv[2..].iter();
        while let Some(option_arg) = options.next() {
            let option = match std::str::from_utf8(option_arg) {
                Ok(option) => option,
                Err(_) => return command_error_to_resp(CommandError::InvalidUtf8Argument),
            };
            if option.eq_ignore_ascii_case("AUTH") {
                let Some(username) = options.next() else {
                    return command_error_to_resp(CommandError::SyntaxError);
                };
                let Some(password) = options.next() else {
                    return command_error_to_resp(CommandError::SyntaxError);
                };
                auth_credentials = Some((username.as_slice(), password.as_slice()));
                continue;
            }
            return command_error_to_resp(CommandError::SyntaxError);
        }

        if let Some((username, password)) = auth_credentials {
            match self.authenticate_user(username, password) {
                Ok(()) => {}
                Err(AuthFailure::NotConfigured) => {
                    return RespFrame::Error(AUTH_NOT_CONFIGURED_ERROR.to_string());
                }
                Err(AuthFailure::WrongPass) => {
                    return RespFrame::Error(WRONGPASS_ERROR.to_string());
                }
            }
        } else if self.auth_state.requires_auth() {
            return RespFrame::Error(NOAUTH_ERROR.to_string());
        }

        build_hello_response(protocol_version)
    }

    fn authenticate_user(&mut self, username: &[u8], password: &[u8]) -> Result<(), AuthFailure> {
        if !self.auth_state.auth_required() {
            return Err(AuthFailure::NotConfigured);
        }

        let Some(acl_user) = self.auth_state.acl_users.get(username) else {
            return Err(AuthFailure::WrongPass);
        };

        if !acl_user.enabled {
            return Err(AuthFailure::WrongPass);
        }

        if !acl_user.check_password(password) {
            return Err(AuthFailure::WrongPass);
        }

        self.auth_state.authenticated_user = Some(username.to_vec());
        Ok(())
    }

    fn handle_acl_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() < 2 {
            return RespFrame::Error(ACL_UNKNOWN_SUBCOMMAND_ERROR.to_string());
        }
        let sub = match std::str::from_utf8(&argv[1]) {
            Ok(s) => s,
            Err(_) => return command_error_to_resp(CommandError::InvalidUtf8Argument),
        };

        if sub.eq_ignore_ascii_case("WHOAMI") {
            self.handle_acl_whoami(argv)
        } else if sub.eq_ignore_ascii_case("LIST") {
            self.handle_acl_list(argv)
        } else if sub.eq_ignore_ascii_case("USERS") {
            self.handle_acl_users(argv)
        } else if sub.eq_ignore_ascii_case("SETUSER") {
            self.handle_acl_setuser(argv)
        } else if sub.eq_ignore_ascii_case("DELUSER") {
            self.handle_acl_deluser(argv)
        } else if sub.eq_ignore_ascii_case("GETUSER") {
            self.handle_acl_getuser(argv)
        } else if sub.eq_ignore_ascii_case("CAT") {
            self.handle_acl_cat(argv)
        } else if sub.eq_ignore_ascii_case("GENPASS") {
            self.handle_acl_genpass(argv)
        } else if sub.eq_ignore_ascii_case("LOG") {
            self.handle_acl_log(argv)
        } else if sub.eq_ignore_ascii_case("SAVE") || sub.eq_ignore_ascii_case("LOAD") {
            if argv.len() != 2 {
                return RespFrame::Error(ACL_UNKNOWN_SUBCOMMAND_ERROR.to_string());
            }
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("HELP") {
            self.handle_acl_help()
        } else {
            RespFrame::Error(ACL_UNKNOWN_SUBCOMMAND_ERROR.to_string())
        }
    }

    fn handle_acl_whoami(&self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 2 {
            return RespFrame::Error(ACL_UNKNOWN_SUBCOMMAND_ERROR.to_string());
        }
        let username = self.auth_state.current_user_name();
        RespFrame::BulkString(Some(username.to_vec()))
    }

    fn handle_acl_list(&self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 2 {
            return RespFrame::Error(ACL_UNKNOWN_SUBCOMMAND_ERROR.to_string());
        }
        let entries = self.auth_state.acl_list_entries();
        RespFrame::Array(Some(
            entries
                .into_iter()
                .map(|e| RespFrame::BulkString(Some(e.into_bytes())))
                .collect(),
        ))
    }

    fn handle_acl_users(&self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 2 {
            return RespFrame::Error(ACL_UNKNOWN_SUBCOMMAND_ERROR.to_string());
        }
        let names = self.auth_state.user_names();
        RespFrame::Array(Some(
            names
                .into_iter()
                .map(|n| RespFrame::BulkString(Some(n.to_vec())))
                .collect(),
        ))
    }

    fn handle_acl_setuser(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() < 3 {
            return RespFrame::Error(ACL_UNKNOWN_SUBCOMMAND_ERROR.to_string());
        }
        let username = argv[2].clone();
        let rules: Vec<&[u8]> = argv[3..].iter().map(Vec::as_slice).collect();
        match self.auth_state.set_user(username, &rules) {
            Ok(()) => RespFrame::SimpleString("OK".to_string()),
            Err(msg) => RespFrame::Error(msg),
        }
    }

    fn handle_acl_deluser(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() < 3 {
            return RespFrame::Error(ACL_UNKNOWN_SUBCOMMAND_ERROR.to_string());
        }
        let mut deleted = 0i64;
        for username in &argv[2..] {
            if username.as_slice() == DEFAULT_AUTH_USER {
                return RespFrame::Error("ERR The 'default' user cannot be removed".to_string());
            }
            if self.auth_state.del_user(username) {
                deleted += 1;
            }
        }
        RespFrame::Integer(deleted)
    }

    fn handle_acl_getuser(&self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 3 {
            return RespFrame::Error(ACL_UNKNOWN_SUBCOMMAND_ERROR.to_string());
        }
        let Some(user) = self.auth_state.get_user(&argv[2]) else {
            return RespFrame::BulkString(None);
        };
        let flags_str = if user.enabled {
            if user.passwords.is_empty() {
                "on nopass"
            } else {
                "on"
            }
        } else {
            "off"
        };
        RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"flags".to_vec())),
            RespFrame::Array(Some(
                flags_str
                    .split_whitespace()
                    .map(|f| RespFrame::BulkString(Some(f.as_bytes().to_vec())))
                    .collect(),
            )),
            RespFrame::BulkString(Some(b"passwords".to_vec())),
            RespFrame::Array(Some(Vec::new())),
            RespFrame::BulkString(Some(b"commands".to_vec())),
            RespFrame::BulkString(Some(b"+@all".to_vec())),
            RespFrame::BulkString(Some(b"keys".to_vec())),
            RespFrame::BulkString(Some(b"~*".to_vec())),
            RespFrame::BulkString(Some(b"channels".to_vec())),
            RespFrame::BulkString(Some(b"&*".to_vec())),
        ]))
    }

    fn handle_acl_cat(&self, argv: &[Vec<u8>]) -> RespFrame {
        const CATEGORIES: &[&str] = &[
            "keyspace",
            "read",
            "write",
            "set",
            "sortedset",
            "list",
            "hash",
            "string",
            "bitmap",
            "hyperloglog",
            "geo",
            "stream",
            "pubsub",
            "admin",
            "fast",
            "slow",
            "blocking",
            "dangerous",
            "connection",
            "transaction",
            "scripting",
            "server",
            "generic",
        ];

        if argv.len() == 2 {
            RespFrame::Array(Some(
                CATEGORIES
                    .iter()
                    .map(|c| RespFrame::BulkString(Some(c.as_bytes().to_vec())))
                    .collect(),
            ))
        } else if argv.len() == 3 {
            let cat = match std::str::from_utf8(&argv[2]) {
                Ok(c) => c,
                Err(_) => return command_error_to_resp(CommandError::InvalidUtf8Argument),
            };
            if CATEGORIES.iter().any(|c| c.eq_ignore_ascii_case(cat)) {
                RespFrame::Array(Some(Vec::new()))
            } else {
                RespFrame::Error(format!("ERR Unknown ACL cat category '{cat}'"))
            }
        } else {
            RespFrame::Error(ACL_UNKNOWN_SUBCOMMAND_ERROR.to_string())
        }
    }

    fn handle_acl_genpass(&self, argv: &[Vec<u8>]) -> RespFrame {
        let bits = if argv.len() == 3 {
            match parse_i64_arg(&argv[2]) {
                Ok(b) if b > 0 && b <= 4096 => b as usize,
                _ => {
                    return RespFrame::Error(
                        "ERR ACL GENPASS argument must be the number of bits for the output password, a positive number up to 4096"
                            .to_string(),
                    );
                }
            }
        } else if argv.len() == 2 {
            256
        } else {
            return RespFrame::Error(ACL_UNKNOWN_SUBCOMMAND_ERROR.to_string());
        };

        let hex_chars = bits.div_ceil(4);
        let bytes_needed = hex_chars.div_ceil(2);
        let mut buf = vec![0u8; bytes_needed];
        // Simple deterministic pseudo-random for now (seeded from packet counter)
        let seed = PACKET_COUNTER.load(Ordering::Relaxed);
        let mut state = seed.wrapping_mul(0x5851_f42d_4c95_7f2d).wrapping_add(1);
        for byte in &mut buf {
            state = state.wrapping_mul(0x5851_f42d_4c95_7f2d).wrapping_add(1);
            *byte = (state >> 33) as u8;
        }
        let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
        let truncated = &hex[..hex_chars];
        RespFrame::BulkString(Some(truncated.as_bytes().to_vec()))
    }

    fn handle_acl_log(&self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() == 2 {
            RespFrame::Array(Some(Vec::new()))
        } else if argv.len() == 3 {
            let sub = match std::str::from_utf8(&argv[2]) {
                Ok(s) => s,
                Err(_) => return command_error_to_resp(CommandError::InvalidUtf8Argument),
            };
            if sub.eq_ignore_ascii_case("RESET") {
                RespFrame::SimpleString("OK".to_string())
            } else {
                match sub.parse::<i64>() {
                    Ok(_) => RespFrame::Array(Some(Vec::new())),
                    Err(_) => RespFrame::Error(ACL_UNKNOWN_SUBCOMMAND_ERROR.to_string()),
                }
            }
        } else {
            RespFrame::Error(ACL_UNKNOWN_SUBCOMMAND_ERROR.to_string())
        }
    }

    fn handle_acl_help(&self) -> RespFrame {
        RespFrame::Array(Some(vec![
            hello_bulk("ACL <subcommand> [<arg> [value] [opt] ...]. Subcommands are:"),
            hello_bulk("CAT [<category>]"),
            hello_bulk(
                "    List all commands that belong to <category>, or all command categories",
            ),
            hello_bulk("    when no category is specified."),
            hello_bulk("DELUSER <username> [<username> ...]"),
            hello_bulk("    Delete a list of users."),
            hello_bulk("GENPASS [<bits>]"),
            hello_bulk("    Generate a secure password."),
            hello_bulk("GETUSER <username>"),
            hello_bulk("    Get the user's details."),
            hello_bulk("LIST"),
            hello_bulk("    List users access rules in the ACL format."),
            hello_bulk("LOAD"),
            hello_bulk("    Reload users from the ACL file."),
            hello_bulk("LOG [<count> | RESET]"),
            hello_bulk("    List latest events denied because of ACLs."),
            hello_bulk("SAVE"),
            hello_bulk("    Save the current ACL rules to the ACL file."),
            hello_bulk("SETUSER <username> <property> [<property> ...]"),
            hello_bulk("    Create or modify a user with the specified properties."),
            hello_bulk("USERS"),
            hello_bulk("    List all usernames."),
            hello_bulk("WHOAMI"),
            hello_bulk("    Return the current connection username."),
            hello_bulk("HELP"),
            hello_bulk("    Print this help."),
        ]))
    }

    fn handle_config_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() < 2 {
            return command_error_to_resp(CommandError::WrongArity("CONFIG"));
        }
        let sub = match std::str::from_utf8(&argv[1]) {
            Ok(sub) => sub,
            Err(_) => return command_error_to_resp(CommandError::InvalidUtf8Argument),
        };
        if sub.eq_ignore_ascii_case("GET") {
            return self.handle_config_get(argv);
        }
        if sub.eq_ignore_ascii_case("SET") {
            return self.handle_config_set(argv);
        }
        if sub.eq_ignore_ascii_case("RESETSTAT") || sub.eq_ignore_ascii_case("REWRITE") {
            if argv.len() != 2 {
                return command_error_to_resp(CommandError::WrongArity("CONFIG"));
            }
            return RespFrame::SimpleString("OK".to_string());
        }
        RespFrame::Error(format!(
            "ERR Unknown subcommand or wrong number of arguments for CONFIG {sub}",
        ))
    }

    fn handle_config_get(&self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 3 {
            return command_error_to_resp(CommandError::WrongArity("CONFIG"));
        }
        let raw_pattern = match std::str::from_utf8(&argv[2]) {
            Ok(pattern) => pattern,
            Err(_) => return command_error_to_resp(CommandError::InvalidUtf8Argument),
        };
        let pattern = raw_pattern.to_ascii_lowercase();
        let mut entries = Vec::new();
        if Self::config_pattern_matches(&pattern, "requirepass") {
            entries.push(RespFrame::BulkString(Some(b"requirepass".to_vec())));
            entries.push(RespFrame::BulkString(Some(
                self.auth_state.requirepass().unwrap_or_default().to_vec(),
            )));
        }
        if Self::config_pattern_matches(&pattern, "acllog-max-len") {
            entries.push(RespFrame::BulkString(Some(b"acllog-max-len".to_vec())));
            entries.push(RespFrame::BulkString(Some(
                self.acllog_max_len.to_string().into_bytes(),
            )));
        }
        // Dynamic maxmemory — override the static default
        if Self::config_pattern_matches(&pattern, "maxmemory") {
            entries.push(RespFrame::BulkString(Some(b"maxmemory".to_vec())));
            entries.push(RespFrame::BulkString(Some(
                self.maxmemory_bytes.to_string().into_bytes(),
            )));
        }
        // Static configuration parameters that clients commonly probe
        for &(name, value) in CONFIG_STATIC_PARAMS {
            // Skip maxmemory from statics — we already emitted the dynamic value above
            if name == "maxmemory" {
                continue;
            }
            if Self::config_pattern_matches(&pattern, name) {
                entries.push(RespFrame::BulkString(Some(name.as_bytes().to_vec())));
                entries.push(RespFrame::BulkString(Some(value.as_bytes().to_vec())));
            }
        }
        RespFrame::Array(Some(entries))
    }

    fn handle_config_set(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() < 4 || !argv.len().is_multiple_of(2) {
            return command_error_to_resp(CommandError::WrongArity("CONFIG"));
        }

        let mut next_requirepass: Option<Option<Vec<u8>>> = None;
        let mut next_acllog_max_len = self.acllog_max_len;
        let mut next_maxmemory: Option<usize> = None;

        for pair in argv[2..].chunks_exact(2) {
            let parameter = match std::str::from_utf8(&pair[0]) {
                Ok(parameter) => parameter,
                Err(_) => return command_error_to_resp(CommandError::InvalidUtf8Argument),
            };
            if parameter.eq_ignore_ascii_case("requirepass") {
                next_requirepass = Some(if pair[1].is_empty() {
                    None
                } else {
                    Some(pair[1].clone())
                });
                continue;
            }
            if parameter.eq_ignore_ascii_case("acllog-max-len") {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) if value >= 0 => value,
                    Ok(_) => {
                        return RespFrame::Error(
                            "ERR CONFIG SET acllog-max-len must be a non-negative integer"
                                .to_string(),
                        );
                    }
                    Err(err) => return command_error_to_resp(err),
                };
                next_acllog_max_len = parsed;
                continue;
            }
            if parameter.eq_ignore_ascii_case("maxmemory") {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) if value >= 0 => value as usize,
                    Ok(_) => {
                        return RespFrame::Error(
                            "ERR Invalid argument '?' for CONFIG SET 'maxmemory'".to_string(),
                        );
                    }
                    Err(err) => return command_error_to_resp(err),
                };
                next_maxmemory = Some(parsed);
                continue;
            }
            // Accept commonly-probed parameters as no-ops to avoid breaking clients
            if parameter.eq_ignore_ascii_case("maxmemory-policy")
                || parameter.eq_ignore_ascii_case("hz")
                || parameter.eq_ignore_ascii_case("timeout")
                || parameter.eq_ignore_ascii_case("tcp-keepalive")
                || parameter.eq_ignore_ascii_case("loglevel")
                || parameter.eq_ignore_ascii_case("save")
                || parameter.eq_ignore_ascii_case("appendonly")
                || parameter.eq_ignore_ascii_case("slowlog-log-slower-than")
                || parameter.eq_ignore_ascii_case("slowlog-max-len")
                || parameter.eq_ignore_ascii_case("latency-tracking")
                || parameter.eq_ignore_ascii_case("notify-keyspace-events")
                || parameter.eq_ignore_ascii_case("lazyfree-lazy-eviction")
                || parameter.eq_ignore_ascii_case("lazyfree-lazy-expire")
                || parameter.eq_ignore_ascii_case("lazyfree-lazy-server-del")
                || parameter.eq_ignore_ascii_case("lazyfree-lazy-user-del")
                || parameter.eq_ignore_ascii_case("activedefrag")
                || parameter.eq_ignore_ascii_case("protected-mode")
            {
                continue;
            }
            return RespFrame::Error(format!("ERR Unsupported CONFIG parameter '{parameter}'"));
        }

        if let Some(requirepass) = next_requirepass {
            // CONFIG SET requirepass should bridge ACL defaults without dropping this session.
            self.auth_state
                .set_requirepass_with_session_policy(requirepass, true);
        }
        self.acllog_max_len = next_acllog_max_len;
        if let Some(maxmemory) = next_maxmemory {
            self.maxmemory_bytes = maxmemory;
        }
        RespFrame::SimpleString("OK".to_string())
    }

    fn config_pattern_matches(pattern: &str, parameter: &str) -> bool {
        if pattern == "*" {
            return true;
        }
        if pattern == parameter {
            return true;
        }
        if let Some(prefix) = pattern.strip_suffix('*') {
            return parameter.starts_with(prefix);
        }
        false
    }

    fn handle_asking_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 1 {
            return command_error_to_resp(CommandError::WrongArity("ASKING"));
        }
        self.cluster_state.asking = true;
        RespFrame::SimpleString("OK".to_string())
    }

    fn handle_readonly_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 1 {
            return command_error_to_resp(CommandError::WrongArity("READONLY"));
        }
        self.cluster_state.mode = ClusterClientMode::ReadOnly;
        RespFrame::SimpleString("OK".to_string())
    }

    fn handle_readwrite_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 1 {
            return command_error_to_resp(CommandError::WrongArity("READWRITE"));
        }
        self.cluster_state.mode = ClusterClientMode::ReadWrite;
        self.cluster_state.asking = false;
        RespFrame::SimpleString("OK".to_string())
    }

    fn handle_client_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() < 2 {
            return command_error_to_resp(CommandError::WrongArity("CLIENT"));
        }
        let sub = match std::str::from_utf8(&argv[1]) {
            Ok(s) => s,
            Err(_) => return command_error_to_resp(CommandError::InvalidUtf8Argument),
        };
        if sub.eq_ignore_ascii_case("SETNAME") {
            if argv.len() != 3 {
                return command_error_to_resp(CommandError::WrongArity("CLIENT"));
            }
            // Redis validates: name must not contain spaces
            if argv[2].contains(&b' ') {
                return RespFrame::Error(
                    "ERR Client names cannot contain spaces, newlines or special characters."
                        .to_string(),
                );
            }
            if argv[2].is_empty() {
                self.client_name = None;
            } else {
                self.client_name = Some(argv[2].clone());
            }
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("GETNAME") {
            if argv.len() != 2 {
                return command_error_to_resp(CommandError::WrongArity("CLIENT"));
            }
            match &self.client_name {
                Some(name) => RespFrame::BulkString(Some(name.clone())),
                None => RespFrame::BulkString(None),
            }
        } else if sub.eq_ignore_ascii_case("ID") {
            if argv.len() != 2 {
                return command_error_to_resp(CommandError::WrongArity("CLIENT"));
            }
            RespFrame::Integer(self.client_id as i64)
        } else if sub.eq_ignore_ascii_case("LIST") || sub.eq_ignore_ascii_case("INFO") {
            // Build real client info line from tracked state
            let name_str = self
                .client_name
                .as_ref()
                .map(|n| String::from_utf8_lossy(n).to_string())
                .unwrap_or_default();
            let flags = if self.transaction_state.in_transaction {
                "x"
            } else {
                "N"
            };
            let multi_count = if self.transaction_state.in_transaction {
                self.transaction_state.command_queue.len() as i64
            } else {
                -1
            };
            let info_line = format!(
                "id={} addr=127.0.0.1:0 laddr=127.0.0.1:6379 fd=0 name={} db=0 sub=0 psub=0 ssub=0 multi={} watch={} qbuf=0 qbuf-free=0 obl=0 oll=0 omem=0 tot-mem=0 events=r cmd=client|{} user=default lib-name= lib-ver= resp=2 flags={}\r\n",
                self.client_id,
                name_str,
                multi_count,
                self.transaction_state.watched_keys.len(),
                sub.to_ascii_lowercase(),
                flags,
            );
            RespFrame::BulkString(Some(info_line.into_bytes()))
        } else if sub.eq_ignore_ascii_case("NO-EVICT") {
            if argv.len() != 3 {
                return command_error_to_resp(CommandError::WrongArity("CLIENT"));
            }
            let mode = match std::str::from_utf8(&argv[2]) {
                Ok(m) => m,
                Err(_) => return command_error_to_resp(CommandError::InvalidUtf8Argument),
            };
            if mode.eq_ignore_ascii_case("ON") {
                self.client_no_evict = true;
            } else if mode.eq_ignore_ascii_case("OFF") {
                self.client_no_evict = false;
            } else {
                return RespFrame::Error(
                    "ERR argument must be 'on' or 'off'".to_string(),
                );
            }
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("NO-TOUCH") {
            if argv.len() != 3 {
                return command_error_to_resp(CommandError::WrongArity("CLIENT"));
            }
            let mode = match std::str::from_utf8(&argv[2]) {
                Ok(m) => m,
                Err(_) => return command_error_to_resp(CommandError::InvalidUtf8Argument),
            };
            if mode.eq_ignore_ascii_case("ON") {
                self.client_no_touch = true;
            } else if mode.eq_ignore_ascii_case("OFF") {
                self.client_no_touch = false;
            } else {
                return RespFrame::Error(
                    "ERR argument must be 'on' or 'off'".to_string(),
                );
            }
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("REPLY") {
            // CLIENT REPLY ON|OFF|SKIP
            if argv.len() != 3 {
                return command_error_to_resp(CommandError::WrongArity("CLIENT"));
            }
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("KILL") {
            // CLIENT KILL [ip:port | ID client-id | ...]
            // Single-connection runtime: always return 0 (no other clients to kill)
            Ok::<i64, ()>(0).ok();
            RespFrame::Integer(0)
        } else if sub.eq_ignore_ascii_case("PAUSE") {
            // CLIENT PAUSE timeout [WRITE|ALL]
            if argv.len() < 3 {
                return command_error_to_resp(CommandError::WrongArity("CLIENT"));
            }
            if parse_i64_arg(&argv[2]).is_err() {
                return command_error_to_resp(CommandError::InvalidInteger);
            }
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("UNPAUSE") {
            if argv.len() != 2 {
                return command_error_to_resp(CommandError::WrongArity("CLIENT"));
            }
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("TRACKING") {
            // CLIENT TRACKING ON|OFF [REDIRECT id] [PREFIX prefix ...] [BCAST] [OPTIN] [OPTOUT] [NOLOOP]
            if argv.len() < 3 {
                return command_error_to_resp(CommandError::WrongArity("CLIENT"));
            }
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("CACHING") {
            // CLIENT CACHING YES|NO
            if argv.len() < 3 {
                return command_error_to_resp(CommandError::WrongArity("CLIENT"));
            }
            RespFrame::SimpleString("OK".to_string())
        } else {
            RespFrame::Error(format!(
                "ERR Unknown subcommand or wrong number of arguments for CLIENT {sub}",
            ))
        }
    }

    fn handle_reset_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 1 {
            return command_error_to_resp(CommandError::WrongArity("RESET"));
        }
        // Reset all per-client state to initial values
        self.client_name = None;
        self.client_no_evict = false;
        self.client_no_touch = false;
        self.transaction_state = TransactionState::default();
        // Re-authenticate as default user (deauth + implicit re-auth for no-password default)
        self.auth_state.authenticated_user = Some(DEFAULT_AUTH_USER.to_vec());
        // Redis returns +RESET\r\n (a simple string "RESET")
        RespFrame::SimpleString("RESET".to_string())
    }

    fn handle_slowlog_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() < 2 {
            return command_error_to_resp(CommandError::WrongArity("SLOWLOG"));
        }
        let sub = match std::str::from_utf8(&argv[1]) {
            Ok(s) => s,
            Err(_) => return command_error_to_resp(CommandError::InvalidUtf8Argument),
        };
        if sub.eq_ignore_ascii_case("GET") {
            let count = if argv.len() >= 3 {
                match parse_i64_arg(&argv[2]) {
                    Ok(c) if c >= 0 => c as usize,
                    _ => return command_error_to_resp(CommandError::InvalidInteger),
                }
            } else {
                self.slowlog_max_len
            };
            let entries: Vec<RespFrame> = self
                .slowlog
                .iter()
                .rev()
                .take(count)
                .map(|entry| {
                    let argv_frames: Vec<RespFrame> = entry
                        .argv
                        .iter()
                        .map(|a| RespFrame::BulkString(Some(a.clone())))
                        .collect();
                    RespFrame::Array(Some(vec![
                        RespFrame::Integer(entry.id as i64),
                        RespFrame::Integer(entry.timestamp_sec as i64),
                        RespFrame::Integer(entry.duration_us as i64),
                        RespFrame::Array(Some(argv_frames)),
                        RespFrame::BulkString(Some(b"".to_vec())), // client addr
                        RespFrame::BulkString(Some(b"".to_vec())), // client name
                    ]))
                })
                .collect();
            RespFrame::Array(Some(entries))
        } else if sub.eq_ignore_ascii_case("LEN") {
            RespFrame::Integer(self.slowlog.len() as i64)
        } else if sub.eq_ignore_ascii_case("RESET") {
            self.slowlog.clear();
            self.slowlog_next_id = 0;
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("HELP") {
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(
                    b"SLOWLOG <subcommand> [<arg> [value] ...]. Subcommands are:".to_vec(),
                )),
                RespFrame::BulkString(Some(
                    b"GET [<count>] - Return the slow log entries.".to_vec(),
                )),
                RespFrame::BulkString(Some(
                    b"LEN - Return the number of entries in the slow log.".to_vec(),
                )),
                RespFrame::BulkString(Some(b"RESET - Reset the slow log.".to_vec())),
                RespFrame::BulkString(Some(b"HELP - Return subcommand help summary.".to_vec())),
            ]))
        } else {
            RespFrame::Error(format!(
                "ERR unknown subcommand or wrong number of arguments for 'slowlog|{sub}'"
            ))
        }
    }

    fn handle_save_command(&mut self, argv: &[Vec<u8>], now_ms: u64) -> RespFrame {
        if argv.len() != 1 {
            return command_error_to_resp(CommandError::WrongArity("SAVE"));
        }
        // Record save time — in a full implementation this would persist data to disk
        self.last_save_time_sec = now_ms / 1000;
        RespFrame::SimpleString("OK".to_string())
    }

    fn handle_bgsave_command(&mut self, argv: &[Vec<u8>], now_ms: u64) -> RespFrame {
        if argv.len() > 2 {
            return command_error_to_resp(CommandError::WrongArity("BGSAVE"));
        }
        // BGSAVE [SCHEDULE] — record save time
        self.last_save_time_sec = now_ms / 1000;
        RespFrame::SimpleString("Background saving started".to_string())
    }

    fn handle_lastsave_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 1 {
            return command_error_to_resp(CommandError::WrongArity("LASTSAVE"));
        }
        RespFrame::Integer(self.last_save_time_sec as i64)
    }

    fn handle_bgrewriteaof_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 1 {
            return command_error_to_resp(CommandError::WrongArity("BGREWRITEAOF"));
        }
        RespFrame::SimpleString("Background append only file rewriting started".to_string())
    }

    fn handle_shutdown_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() > 3 {
            return command_error_to_resp(CommandError::WrongArity("SHUTDOWN"));
        }
        // Validate flags: NOSAVE, SAVE, NOW, FORCE
        for arg in &argv[1..] {
            let s = match std::str::from_utf8(arg) {
                Ok(s) => s,
                Err(_) => return command_error_to_resp(CommandError::InvalidUtf8Argument),
            };
            if !s.eq_ignore_ascii_case("NOSAVE")
                && !s.eq_ignore_ascii_case("SAVE")
                && !s.eq_ignore_ascii_case("NOW")
                && !s.eq_ignore_ascii_case("FORCE")
            {
                return RespFrame::Error(format!(
                    "ERR unrecognized option or bad number of args for SHUTDOWN: '{s}'"
                ));
            }
        }
        // In standalone mode, acknowledge but don't actually shut down
        RespFrame::SimpleString("OK".to_string())
    }

    fn handle_cluster_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() < 2 {
            return command_error_to_resp(CommandError::WrongArity("CLUSTER"));
        }
        let subcommand = match classify_cluster_subcommand(&argv[1]) {
            Ok(subcommand) => subcommand,
            Err(err) => return command_error_to_resp(err),
        };

        if subcommand == ClusterSubcommand::Help {
            if argv.len() != 2 {
                return RespFrame::Error(CLUSTER_UNKNOWN_SUBCOMMAND_ERROR.to_string());
            }
            return RespFrame::Array(Some(vec![
                hello_bulk("CLUSTER HELP"),
                hello_bulk("CLUSTER subcommand dispatch scaffold (FR-P2C-007 D1)."),
                hello_bulk("Supported subcommands in this stage: HELP."),
            ]));
        }

        RespFrame::Error(CLUSTER_UNKNOWN_SUBCOMMAND_ERROR.to_string())
    }

    fn handle_wait_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 3 {
            return command_error_to_resp(CommandError::WrongArity("WAIT"));
        }
        let required_replicas = match parse_i64_arg(&argv[1]) {
            Ok(value) if value >= 0 => usize::try_from(value).unwrap_or(usize::MAX),
            _ => return command_error_to_resp(CommandError::InvalidInteger),
        };
        if !matches!(parse_i64_arg(&argv[2]), Ok(value) if value >= 0) {
            return command_error_to_resp(CommandError::InvalidInteger);
        }

        let outcome = evaluate_wait(
            &self.replication_ack_state.replica_ack_offsets,
            WaitThreshold {
                required_offset: self.replication_ack_state.primary_offset,
                required_replicas,
            },
        );
        let acked_replicas = i64::try_from(outcome.acked_replicas).unwrap_or(i64::MAX);
        RespFrame::Integer(acked_replicas)
    }

    fn handle_waitaof_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 4 {
            return command_error_to_resp(CommandError::WrongArity("WAITAOF"));
        }
        let required_local = match parse_i64_arg(&argv[1]) {
            Ok(value) if value >= 0 => usize::try_from(value).unwrap_or(usize::MAX),
            _ => return command_error_to_resp(CommandError::InvalidInteger),
        };
        let required_replicas = match parse_i64_arg(&argv[2]) {
            Ok(value) if value >= 0 => usize::try_from(value).unwrap_or(usize::MAX),
            _ => return command_error_to_resp(CommandError::InvalidInteger),
        };
        if !matches!(parse_i64_arg(&argv[3]), Ok(value) if value >= 0) {
            return command_error_to_resp(CommandError::InvalidInteger);
        }

        let required_local_offset = if required_local == 0 {
            ReplOffset(0)
        } else {
            self.replication_ack_state.primary_offset
        };
        let required_replica_offset = if required_replicas == 0 {
            ReplOffset(0)
        } else {
            self.replication_ack_state.primary_offset
        };

        let outcome = evaluate_waitaof(
            self.replication_ack_state.local_fsync_offset,
            &self.replication_ack_state.replica_fsync_offsets,
            WaitAofThreshold {
                required_local_offset,
                required_replica_offset,
                required_replicas,
            },
        );
        let local_ack = if outcome.local_satisfied { 1 } else { 0 };
        let replica_acks = i64::try_from(outcome.acked_replicas).unwrap_or(i64::MAX);
        RespFrame::Array(Some(vec![
            RespFrame::Integer(local_ack),
            RespFrame::Integer(replica_acks),
        ]))
    }

    fn handle_multi_command(&mut self) -> RespFrame {
        if self.transaction_state.in_transaction {
            return RespFrame::Error("ERR MULTI calls can not be nested".to_string());
        }
        self.transaction_state.in_transaction = true;
        self.transaction_state.command_queue.clear();
        RespFrame::SimpleString("OK".to_string())
    }

    fn handle_exec_command(
        &mut self,
        now_ms: u64,
        packet_id: u64,
        input_digest: &str,
        state_before: &str,
    ) -> RespFrame {
        if !self.transaction_state.in_transaction {
            return RespFrame::Error("ERR EXEC without MULTI".to_string());
        }
        let queued = std::mem::take(&mut self.transaction_state.command_queue);
        self.transaction_state.in_transaction = false;

        // Check watched keys: if any were modified, abort the transaction
        let watch_failed = self.transaction_state.watch_dirty || {
            let mut dirty = false;
            for (key, original_fp) in &self.transaction_state.watched_keys {
                let current_fp = self.store.key_fingerprint(key, now_ms);
                if current_fp != *original_fp {
                    dirty = true;
                    break;
                }
            }
            dirty
        };
        self.transaction_state.watched_keys.clear();
        self.transaction_state.watch_dirty = false;

        if watch_failed {
            return RespFrame::Array(None);
        }

        let mut results = Vec::with_capacity(queued.len());
        for argv in &queued {
            if let Some(reply) = self.enforce_maxmemory_before_dispatch(
                argv,
                now_ms,
                packet_id,
                input_digest,
                state_before,
            ) {
                results.push(reply);
                continue;
            }

            let _ = self.run_active_expire_cycle(now_ms, ActiveExpireCycleKind::Fast);
            match dispatch_argv(argv, &mut self.store, now_ms) {
                Ok(reply) => {
                    self.capture_aof_record(argv);
                    results.push(reply);
                }
                Err(err) => results.push(command_error_to_resp(err)),
            }
        }
        RespFrame::Array(Some(results))
    }

    fn handle_discard_command(&mut self) -> RespFrame {
        if !self.transaction_state.in_transaction {
            return RespFrame::Error("ERR DISCARD without MULTI".to_string());
        }
        self.transaction_state.in_transaction = false;
        self.transaction_state.command_queue.clear();
        self.transaction_state.watched_keys.clear();
        self.transaction_state.watch_dirty = false;
        RespFrame::SimpleString("OK".to_string())
    }

    fn handle_watch_command(&mut self, argv: &[Vec<u8>], now_ms: u64) -> RespFrame {
        if argv.len() < 2 {
            return RespFrame::Error(
                "ERR wrong number of arguments for 'watch' command".to_string(),
            );
        }
        if self.transaction_state.in_transaction {
            return RespFrame::Error("ERR WATCH inside MULTI is not allowed".to_string());
        }
        for key in &argv[1..] {
            let fp = self.store.key_fingerprint(key, now_ms);
            self.transaction_state.watched_keys.push((key.clone(), fp));
        }
        RespFrame::SimpleString("OK".to_string())
    }

    fn handle_unwatch_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 1 {
            return RespFrame::Error(
                "ERR wrong number of arguments for 'unwatch' command".to_string(),
            );
        }
        self.transaction_state.watched_keys.clear();
        self.transaction_state.watch_dirty = false;
        RespFrame::SimpleString("OK".to_string())
    }

    fn preflight_gate(
        &mut self,
        frame: &RespFrame,
        now_ms: u64,
        packet_id: u64,
        input_digest: &str,
        state_before: &str,
    ) -> Option<RespFrame> {
        let RespFrame::Array(Some(items)) = frame else {
            return None;
        };
        if items.len() > self.policy.gate.max_array_len {
            let reply = RespFrame::Error(
                "ERR Protocol error: command array exceeds compatibility gate".to_string(),
            );
            self.record_threat_event(ThreatEventInput {
                now_ms,
                packet_id,
                threat_class: ThreatClass::ResourceExhaustion,
                preferred_deviation: Some(HardenedDeviationCategory::ResourceClamp),
                subsystem: "compatibility_gate",
                action: "fail_closed_array_len",
                reason_code: "compat_array_len_exceeded",
                reason: format!(
                    "array length {} exceeded {}",
                    items.len(),
                    self.policy.gate.max_array_len
                ),
                input_digest: input_digest.to_string(),
                state_before,
                output: &reply,
            });
            return Some(reply);
        }

        for item in items {
            if let RespFrame::BulkString(Some(bytes)) = item
                && bytes.len() > self.policy.gate.max_bulk_len
            {
                let reply = RespFrame::Error(
                    "ERR Protocol error: bulk payload exceeds compatibility gate".to_string(),
                );
                self.record_threat_event(ThreatEventInput {
                    now_ms,
                    packet_id,
                    threat_class: ThreatClass::ResourceExhaustion,
                    preferred_deviation: Some(HardenedDeviationCategory::ResourceClamp),
                    subsystem: "compatibility_gate",
                    action: "fail_closed_bulk_len",
                    reason_code: "compat_bulk_len_exceeded",
                    reason: format!(
                        "bulk len {} exceeded {}",
                        bytes.len(),
                        self.policy.gate.max_bulk_len
                    ),
                    input_digest: input_digest.to_string(),
                    state_before,
                    output: &reply,
                });
                return Some(reply);
            }
        }
        None
    }

    fn record_threat_event(&mut self, input: ThreatEventInput<'_>) {
        if !self.policy.emit_evidence_ledger {
            return;
        }

        let (decision_action, severity) = self
            .policy
            .decide(input.threat_class, input.preferred_deviation);
        let state_after = self.store.state_digest();
        let output_digest = digest_bytes(&input.output.to_bytes());
        self.evidence.record(EvidenceEvent {
            ts_utc: format_ts_utc(input.now_ms),
            ts_ms: input.now_ms,
            packet_id: input.packet_id,
            mode: self.policy.mode,
            severity,
            threat_class: input.threat_class,
            decision_action,
            subsystem: input.subsystem,
            action: input.action,
            reason_code: input.reason_code,
            reason: input.reason,
            input_digest: input.input_digest,
            output_digest,
            state_digest_before: input.state_before.to_string(),
            state_digest_after: state_after,
            replay_cmd: format!(
                "cargo test -p fr-runtime -- --nocapture packet_{}",
                input.packet_id
            ),
            artifact_refs: vec![
                "SECURITY_COMPATIBILITY_THREAT_MATRIX_V1.md".to_string(),
                "PORTING_TO_RUST_ESSENCE_EXTRACTION_LEDGER_V1.md".to_string(),
            ],
            confidence: Some(1.0),
        });
    }

    fn gate_tls_error_for_mode(
        &self,
        error: TlsCfgError,
        preferred_deviation: HardenedDeviationCategory,
    ) -> TlsCfgError {
        if self.policy.mode != Mode::Hardened {
            return error;
        }
        match evaluate_tls_hardened_deviation(&self.policy, preferred_deviation) {
            Ok(_) => error,
            Err(gated_error) => gated_error,
        }
    }

    fn record_tls_config_event(
        &mut self,
        now_ms: u64,
        packet_id: u64,
        input_digest: &str,
        state_before: &str,
        error: &TlsCfgError,
        preferred_deviation: HardenedDeviationCategory,
    ) {
        let reply = RespFrame::Error(format!(
            "ERR TLS/config boundary violation ({})",
            error.reason_code()
        ));
        self.record_threat_event(ThreatEventInput {
            now_ms,
            packet_id,
            threat_class: ThreatClass::ConfigDowngradeAbuse,
            preferred_deviation: Some(preferred_deviation),
            subsystem: "tls_config",
            action: "reject_runtime_apply",
            reason_code: error.reason_code(),
            reason: error.to_string(),
            input_digest: input_digest.to_string(),
            state_before,
            output: &reply,
        });
    }
}

fn preferred_tls_deviation_for_error(error: &TlsCfgError) -> HardenedDeviationCategory {
    match error {
        TlsCfgError::OperationalKnobContractViolation(_) => {
            HardenedDeviationCategory::ResourceClamp
        }
        _ => HardenedDeviationCategory::MetadataSanitization,
    }
}

fn next_packet_id() -> u64 {
    PACKET_COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn format_ts_utc(now_ms: u64) -> String {
    format!("unix_ms:{now_ms}")
}

fn digest_bytes(bytes: &[u8]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn parse_i64_arg(arg: &[u8]) -> Result<i64, CommandError> {
    let text = std::str::from_utf8(arg).map_err(|_| CommandError::InvalidUtf8Argument)?;
    text.parse::<i64>()
        .map_err(|_| CommandError::InvalidInteger)
}

fn hello_bulk(value: &str) -> RespFrame {
    RespFrame::BulkString(Some(value.as_bytes().to_vec()))
}

fn build_hello_response(protocol_version: i64) -> RespFrame {
    RespFrame::Array(Some(vec![
        hello_bulk("server"),
        hello_bulk("frankenredis"),
        hello_bulk("version"),
        hello_bulk(env!("CARGO_PKG_VERSION")),
        hello_bulk("proto"),
        RespFrame::Integer(protocol_version),
    ]))
}

fn command_error_to_resp(error: CommandError) -> RespFrame {
    match error {
        CommandError::InvalidCommandFrame => {
            RespFrame::Error("ERR invalid command frame".to_string())
        }
        CommandError::InvalidUtf8Argument => {
            RespFrame::Error("ERR invalid UTF-8 argument".to_string())
        }
        CommandError::UnknownCommand {
            command,
            args_preview,
        } => {
            let mut out = format!("ERR unknown command '{}'", command);
            if let Some(args_preview) = args_preview {
                out.push_str(", with args beginning with: ");
                out.push_str(&args_preview);
            }
            RespFrame::Error(out)
        }
        CommandError::WrongArity(cmd) => RespFrame::Error(format!(
            "ERR wrong number of arguments for '{}' command",
            cmd
        )),
        CommandError::InvalidInteger => {
            RespFrame::Error("ERR value is not an integer or out of range".to_string())
        }
        CommandError::SyntaxError => RespFrame::Error("ERR syntax error".to_string()),
        CommandError::NoSuchKey => RespFrame::Error("ERR no such key".to_string()),
        CommandError::Store(store_error) => match store_error {
            fr_store::StoreError::ValueNotInteger => {
                RespFrame::Error("ERR value is not an integer or out of range".to_string())
            }
            fr_store::StoreError::HashValueNotInteger => {
                RespFrame::Error("ERR hash value is not an integer".to_string())
            }
            fr_store::StoreError::ValueNotFloat => {
                RespFrame::Error("ERR value is not a valid float".to_string())
            }
            fr_store::StoreError::IntegerOverflow => {
                RespFrame::Error("ERR increment or decrement would overflow".to_string())
            }
            fr_store::StoreError::KeyNotFound => RespFrame::Error("ERR no such key".to_string()),
            fr_store::StoreError::WrongType => RespFrame::Error(
                "WRONGTYPE Operation against a key holding the wrong kind of value".to_string(),
            ),
            fr_store::StoreError::InvalidHllValue => RespFrame::Error(
                "WRONGTYPE Key is not a valid HyperLogLog string value.".to_string(),
            ),
            fr_store::StoreError::IndexOutOfRange => {
                RespFrame::Error("ERR index out of range".to_string())
            }
            fr_store::StoreError::InvalidDumpPayload => {
                RespFrame::Error("ERR DUMP payload version or checksum are wrong".to_string())
            }
            fr_store::StoreError::BusyKey => {
                RespFrame::Error("BUSYKEY Target key name already exists.".to_string())
            }
        },
    }
}

fn protocol_error_to_resp(error: RespParseError) -> RespFrame {
    match error {
        RespParseError::InvalidBulkLength => {
            RespFrame::Error("ERR Protocol error: invalid bulk length".to_string())
        }
        RespParseError::InvalidMultibulkLength => {
            RespFrame::Error("ERR Protocol error: invalid multibulk length".to_string())
        }
        RespParseError::Incomplete => {
            RespFrame::Error("ERR Protocol error: unexpected EOF while reading request".to_string())
        }
        RespParseError::InvalidPrefix(ch) => RespFrame::Error(format!(
            "ERR Protocol error: invalid RESP type prefix '{}'",
            char::from(ch)
        )),
        RespParseError::UnsupportedResp3Type(ch) => RespFrame::Error(format!(
            "ERR Protocol error: unsupported RESP3 type prefix '{}'",
            char::from(ch)
        )),
        RespParseError::InvalidInteger => {
            RespFrame::Error("ERR Protocol error: invalid integer payload".to_string())
        }
        RespParseError::InvalidUtf8 => {
            RespFrame::Error("ERR Protocol error: invalid UTF-8 payload".to_string())
        }
    }
}

pub mod ecosystem {
    /// Adapter boundary for Asupersync integration.
    /// This keeps `fr-runtime` decoupled while enabling project-level runtime wiring.
    pub trait AsyncRuntimeAdapter {
        fn spawn_named(&self, name: &str, task: Box<dyn FnOnce() + Send>);
    }

    /// Adapter boundary for FrankenTUI evidence and operator dashboards.
    pub trait OperatorUiAdapter {
        fn push_evidence_line(&self, line: &str);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use fr_command::CommandError;
    use fr_config::{
        DecisionAction, DriftSeverity, HardenedDeviationCategory, Mode, RuntimePolicy, ThreatClass,
        TlsAuthClients, TlsConfig, TlsProtocol,
    };
    use fr_eventloop::{
        AcceptPathError, ActiveExpireCycleKind, BarrierOrderError, EVENT_LOOP_PHASE_ORDER,
        EventLoopMode, EventLoopPhase, FdRegistrationError, LoopBootstrap, PendingWriteError,
        ReadPathError, ReadinessCallback, TickBudget,
    };
    use fr_persist::{PersistError, decode_aof_stream};
    use fr_protocol::{RespFrame, parse_frame};

    use super::{
        ClusterSubcommand, Runtime, classify_cluster_subcommand,
        classify_cluster_subcommand_linear, classify_runtime_special_command,
        classify_runtime_special_command_linear,
    };

    fn command(parts: &[&[u8]]) -> RespFrame {
        RespFrame::Array(Some(
            parts
                .iter()
                .map(|part| RespFrame::BulkString(Some((*part).to_vec())))
                .collect(),
        ))
    }

    #[test]
    fn fr_p2c_001_u001_runtime_exposes_deterministic_phase_order() {
        let plan =
            Runtime::plan_event_loop_tick(1, 3, TickBudget::default(), EventLoopMode::Normal);
        assert_eq!(plan.phase_order, EVENT_LOOP_PHASE_ORDER);
    }

    #[test]
    fn fr_p2c_001_u003_runtime_no_sleep_when_backlog_present() {
        let plan =
            Runtime::plan_event_loop_tick(0, 1, TickBudget::default(), EventLoopMode::Normal);
        assert_eq!(plan.poll_timeout_ms, 0);
    }

    #[test]
    fn fr_p2c_001_u005_runtime_blocked_mode_is_bounded() {
        let plan = Runtime::plan_event_loop_tick(
            50,
            10_000,
            TickBudget::default(),
            EventLoopMode::Blocked,
        );
        assert_eq!(plan.poll_timeout_ms, 0);
        assert_eq!(plan.stats.accepted, TickBudget::BLOCKED_MODE_MAX_ACCEPTS);
        assert_eq!(
            plan.stats.processed_commands,
            TickBudget::BLOCKED_MODE_MAX_COMMANDS
        );
    }

    #[test]
    fn fr_p2c_001_u002_runtime_barrier_order_preserves_contract() {
        let observed = Runtime::plan_event_loop_readiness_order(true, true, true);
        assert_eq!(observed.first, Some(ReadinessCallback::Writable));
        assert_eq!(observed.second, Some(ReadinessCallback::Readable));
        Runtime::validate_event_loop_barrier_order(true, true, true, observed)
            .expect("barrier order must validate");
    }

    #[test]
    fn fr_p2c_001_u002_runtime_barrier_violation_returns_reason_code() {
        let err = Runtime::validate_event_loop_barrier_order(
            true,
            true,
            true,
            super::CallbackDispatchOrder {
                first: Some(ReadinessCallback::Readable),
                second: Some(ReadinessCallback::Writable),
            },
        )
        .expect_err("barrier violation");
        assert_eq!(err, BarrierOrderError::AeBarrierViolation);
        assert_eq!(err.reason_code(), "eventloop.ae_barrier_violation");
    }

    #[test]
    fn fr_p2c_001_u004_runtime_fd_registration_bounds_are_enforced() {
        let err = Runtime::validate_event_loop_fd_registration(32, 32)
            .expect_err("out-of-range fd should fail");
        assert_eq!(
            err,
            FdRegistrationError::FdOutOfRange {
                fd: 32,
                setsize: 32
            }
        );
        assert_eq!(err.reason_code(), "eventloop.fd_out_of_range");
    }

    #[test]
    fn fr_p2c_001_u004_runtime_fd_resize_growth_is_deterministic() {
        let grown = Runtime::plan_event_loop_fd_resize(64, 120, 1_024).expect("fd resize");
        assert_eq!(grown, 128);
    }

    #[test]
    fn fr_p2c_001_u006_runtime_accept_path_rejects_maxclients_overflow() {
        let err = Runtime::validate_event_loop_accept_path(5_000, 5_000, true)
            .expect_err("maxclients rejection");
        assert_eq!(
            err,
            AcceptPathError::MaxClientsReached {
                current_clients: 5_000,
                max_clients: 5_000
            }
        );
        assert_eq!(err.reason_code(), "eventloop.accept.maxclients_reached");
    }

    #[test]
    fn fr_p2c_001_u007_runtime_read_path_enforces_query_buffer_limit() {
        let err =
            Runtime::validate_event_loop_read_path(6, 5, 10, false).expect_err("limit exceeded");
        assert_eq!(
            err,
            ReadPathError::QueryBufferLimitExceeded {
                observed: 11,
                limit: 10
            }
        );
        assert_eq!(err.reason_code(), "eventloop.read.querybuf_limit_exceeded");
    }

    #[test]
    fn fr_p2c_001_u008_runtime_read_path_closes_on_fatal_error() {
        let err =
            Runtime::validate_event_loop_read_path(0, 0, 128, true).expect_err("fatal read path");
        assert_eq!(err, ReadPathError::FatalErrorDisconnect);
        assert_eq!(err.reason_code(), "eventloop.read.fatal_error_disconnect");
    }

    #[test]
    fn fr_p2c_001_u009_runtime_pending_write_delivery_rejects_losses() {
        let queued = [3_u64, 5, 8];
        let err = Runtime::validate_event_loop_pending_write_delivery(&queued, &[3], &[8])
            .expect_err("missing pending reply must fail");
        assert_eq!(err, PendingWriteError::PendingReplyLost { client_id: 5 });
        assert_eq!(err.reason_code(), "eventloop.write.pending_reply_lost");
    }

    #[test]
    fn fr_p2c_001_u009_runtime_pending_write_delivery_rejects_reordering() {
        let queued = [3_u64, 5, 8];
        let err = Runtime::validate_event_loop_pending_write_delivery(&queued, &[5, 3], &[8])
            .expect_err("flush reordering must fail");
        assert_eq!(err, PendingWriteError::FlushOrderViolation { client_id: 3 });
        assert_eq!(err.reason_code(), "eventloop.write.flush_order_violation");
    }

    #[test]
    fn fr_p2c_001_unit_contract_smoke() {
        let plan =
            Runtime::plan_event_loop_tick(1, 1, TickBudget::default(), EventLoopMode::Normal);
        assert_eq!(plan.phase_order, EVENT_LOOP_PHASE_ORDER);
        assert_eq!(plan.poll_timeout_ms, 0);

        let barrier = Runtime::plan_event_loop_readiness_order(true, true, true);
        Runtime::validate_event_loop_barrier_order(true, true, true, barrier)
            .expect("barrier order");

        Runtime::validate_event_loop_fd_registration(31, 32).expect("fd bounds");
        Runtime::plan_event_loop_fd_resize(64, 120, 1_024).expect("fd growth");

        Runtime::validate_event_loop_accept_path(999, 1_000, true).expect("accept path");
        Runtime::validate_event_loop_read_path(1, 2, 16, false).expect("read path");
        Runtime::validate_event_loop_pending_write_delivery(&[1, 2, 3], &[1], &[2, 3])
            .expect("pending writes");

        Runtime::validate_event_loop_bootstrap(LoopBootstrap::fully_wired())
            .expect("bootstrap wiring");
        Runtime::replay_event_loop_phase_trace(&EVENT_LOOP_PHASE_ORDER).expect("phase replay");
    }

    #[test]
    fn fr_p2c_009_u011_runtime_tls_accept_limit_clamps_tls_accepts() {
        let plan = Runtime::plan_event_loop_tick_with_tls_budget(
            15,
            50,
            12,
            4,
            TickBudget {
                max_accepts: 10,
                max_commands: 100,
            },
            EventLoopMode::Normal,
        );
        assert_eq!(plan.stats.accepted, 7);
        assert_eq!(plan.stats.accept_backlog_remaining, 8);
    }

    #[test]
    fn fr_p2c_009_u011_runtime_tls_accept_limit_never_exceeds_total_budget() {
        let plan = Runtime::plan_event_loop_tick_with_tls_budget(
            20,
            1,
            20,
            64,
            TickBudget {
                max_accepts: 5,
                max_commands: 10,
            },
            EventLoopMode::Normal,
        );
        assert_eq!(plan.stats.accepted, 5);
        assert_eq!(plan.stats.accept_backlog_remaining, 15);
    }

    #[test]
    fn fr_p2c_001_u011_runtime_phase_replay_accepts_contract_order() {
        let ticks = Runtime::replay_event_loop_phase_trace(&EVENT_LOOP_PHASE_ORDER)
            .expect("valid phase trace");
        assert_eq!(ticks, 1);
    }

    #[test]
    fn fr_p2c_001_u011_runtime_phase_replay_rejects_invalid_start() {
        let err = Runtime::replay_event_loop_phase_trace(&[EventLoopPhase::Poll])
            .expect_err("invalid start");
        assert_eq!(err.reason_code(), "eventloop.main_loop_entry_missing");
    }

    #[test]
    fn fr_p2c_001_u010_runtime_bootstrap_validation_accepts_fully_wired() {
        Runtime::validate_event_loop_bootstrap(LoopBootstrap::fully_wired())
            .expect("fully wired bootstrap");
    }

    #[test]
    fn fr_p2c_001_u010_runtime_bootstrap_validation_rejects_missing_hook() {
        let err = Runtime::validate_event_loop_bootstrap(LoopBootstrap {
            before_sleep_hook_installed: false,
            after_sleep_hook_installed: true,
            server_cron_timer_installed: true,
        })
        .expect_err("missing hook");
        assert_eq!(err.reason_code(), "eventloop.hook_install_missing");
    }

    #[test]
    fn strict_ping_path() {
        let mut rt = Runtime::default_strict();
        let in_frame = RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"PING".to_vec()))]));
        let out = rt.execute_frame(in_frame, 100);
        assert_eq!(out, RespFrame::SimpleString("PONG".to_string()));
    }

    #[test]
    fn fr_p2c_008_u010_runtime_keys_glob_class_edge_passthrough() {
        let mut rt = Runtime::default_strict();
        for key in [
            b"!".as_slice(),
            b"a",
            b"b",
            b"c",
            b"m",
            b"z",
            b"-",
            b"]",
            b"[abc",
        ] {
            let set = rt.execute_frame(command(&[b"SET", key, b"1"]), 0);
            assert_eq!(set, RespFrame::SimpleString("OK".to_string()));
        }

        let range = rt.execute_frame(command(&[b"KEYS", b"[z-a]"]), 1);
        assert_eq!(
            range,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"a".to_vec())),
                RespFrame::BulkString(Some(b"b".to_vec())),
                RespFrame::BulkString(Some(b"c".to_vec())),
                RespFrame::BulkString(Some(b"m".to_vec())),
                RespFrame::BulkString(Some(b"z".to_vec())),
            ]))
        );

        let escaped = rt.execute_frame(command(&[b"KEYS", b"[\\-]"]), 2);
        assert_eq!(
            escaped,
            RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"-".to_vec()))]))
        );

        let trailing_dash = rt.execute_frame(command(&[b"KEYS", b"[a-]"]), 3);
        assert_eq!(
            trailing_dash,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"]".to_vec())),
                RespFrame::BulkString(Some(b"a".to_vec())),
            ]))
        );

        let literal_bang = rt.execute_frame(command(&[b"KEYS", b"[!a]"]), 4);
        assert_eq!(
            literal_bang,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"!".to_vec())),
                RespFrame::BulkString(Some(b"a".to_vec())),
            ]))
        );

        let malformed = rt.execute_frame(command(&[b"KEYS", b"[abc"]), 5);
        assert_eq!(
            malformed,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"a".to_vec())),
                RespFrame::BulkString(Some(b"b".to_vec())),
                RespFrame::BulkString(Some(b"c".to_vec())),
            ]))
        );
    }

    #[test]
    fn fr_p2c_008_u001_runtime_command_path_fast_cycle_evicts_expired_keys() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"fr:p2c:008:exp", b"x", b"PX", b"1"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"fr:p2c:008:live", b"v"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );

        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"fr:p2c:008:live"]), 10),
            RespFrame::BulkString(Some(b"v".to_vec()))
        );

        let stats = rt
            .last_active_expire_cycle_stats()
            .expect("fast active-expire cycle should run on command path");
        assert_eq!(stats.plan.kind, ActiveExpireCycleKind::Fast);
        assert!(stats.sampled_keys >= 1);
        assert!(stats.evicted_keys >= 1);

        assert_eq!(
            rt.execute_frame(command(&[b"TTL", b"fr:p2c:008:exp"]), 10),
            RespFrame::Integer(-2)
        );
    }

    #[test]
    fn fr_p2c_008_u002_runtime_fast_and_slow_cycle_budgets_are_deterministic() {
        let mut rt = Runtime::default_strict();
        for idx in 0..20 {
            let key = format!("fr:p2c:008:ttl:{idx}");
            assert_eq!(
                rt.execute_frame(command(&[b"SET", key.as_bytes(), b"v", b"PX", b"1"]), 0),
                RespFrame::SimpleString("OK".to_string())
            );
        }

        let fast = rt.run_active_expire_cycle(10, ActiveExpireCycleKind::Fast);
        assert_eq!(fast.plan.kind, ActiveExpireCycleKind::Fast);
        assert_eq!(fast.plan.sample_limit, 16);
        assert_eq!(fast.sampled_keys, 16);
        assert_eq!(fast.evicted_keys, 16);

        let slow = rt.run_server_cron_active_expire_cycle(10);
        assert_eq!(slow.plan.kind, ActiveExpireCycleKind::Slow);
        assert_eq!(slow.plan.sample_limit, 4);
        assert_eq!(slow.sampled_keys, 4);
        assert_eq!(slow.evicted_keys, 4);

        assert_eq!(
            rt.execute_frame(command(&[b"DBSIZE"]), 10),
            RespFrame::Integer(0)
        );
    }

    #[test]
    fn fr_p2c_004_u001_default_bootstrap_is_authenticated() {
        let rt = Runtime::default_strict();
        assert!(rt.is_authenticated());
    }

    #[test]
    fn fr_p2c_004_u002_auth_success_transitions_state() {
        let mut rt = Runtime::default_strict();
        rt.set_requirepass(Some(b"secret".to_vec()));
        assert!(!rt.is_authenticated());

        let out = rt.execute_frame(command(&[b"AUTH", b"secret"]), 0);
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));
        assert!(rt.is_authenticated());
    }

    #[test]
    fn fr_p2c_004_u003_auth_wrongpass_rejected_without_state_promotion() {
        let mut rt = Runtime::default_strict();
        rt.set_requirepass(Some(b"secret".to_vec()));

        let wrong = rt.execute_frame(command(&[b"AUTH", b"bad"]), 0);
        assert_eq!(
            wrong,
            RespFrame::Error(
                "WRONGPASS invalid username-password pair or user is disabled.".to_string()
            )
        );
        assert!(!rt.is_authenticated());
    }

    #[test]
    fn fr_p2c_004_u004_hello_auth_early_fails_and_success_path_authenticates() {
        let mut rt = Runtime::default_strict();
        rt.set_requirepass(Some(b"secret".to_vec()));

        let wrong = rt.execute_frame(command(&[b"HELLO", b"3", b"AUTH", b"default", b"bad"]), 0);
        assert_eq!(
            wrong,
            RespFrame::Error(
                "WRONGPASS invalid username-password pair or user is disabled.".to_string()
            )
        );
        assert!(!rt.is_authenticated());

        let ok = rt.execute_frame(
            command(&[b"HELLO", b"3", b"AUTH", b"default", b"secret"]),
            0,
        );
        assert_eq!(
            ok,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"server".to_vec())),
                RespFrame::BulkString(Some(b"frankenredis".to_vec())),
                RespFrame::BulkString(Some(b"version".to_vec())),
                RespFrame::BulkString(Some(env!("CARGO_PKG_VERSION").as_bytes().to_vec())),
                RespFrame::BulkString(Some(b"proto".to_vec())),
                RespFrame::Integer(3),
            ]))
        );
        assert!(rt.is_authenticated());
    }

    #[test]
    fn fr_p2c_004_u005_noauth_gate_runs_before_dispatch() {
        let mut rt = Runtime::default_strict();
        rt.set_requirepass(Some(b"secret".to_vec()));

        let gated = rt.execute_frame(command(&[b"GET", b"k"]), 0);
        assert_eq!(
            gated,
            RespFrame::Error("NOAUTH Authentication required.".to_string())
        );
    }

    #[test]
    fn fr_p2c_004_u008_auth_user_password_success() {
        let mut rt = Runtime::default_strict();
        rt.add_user(b"alice".to_vec(), b"secret2".to_vec());
        assert!(!rt.is_authenticated());

        let out = rt.execute_frame(command(&[b"AUTH", b"alice", b"secret2"]), 0);
        assert_eq!(out, RespFrame::SimpleString("OK".to_string()));
        assert!(rt.is_authenticated());
    }

    #[test]
    fn fr_p2c_004_u009_hello_auth_user_password_success() {
        let mut rt = Runtime::default_strict();
        rt.add_user(b"alice".to_vec(), b"secret2".to_vec());

        let out = rt.execute_frame(command(&[b"HELLO", b"3", b"AUTH", b"alice", b"secret2"]), 0);
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"server".to_vec())),
                RespFrame::BulkString(Some(b"frankenredis".to_vec())),
                RespFrame::BulkString(Some(b"version".to_vec())),
                RespFrame::BulkString(Some(env!("CARGO_PKG_VERSION").as_bytes().to_vec())),
                RespFrame::BulkString(Some(b"proto".to_vec())),
                RespFrame::Integer(3),
            ]))
        );
        assert!(rt.is_authenticated());
    }

    #[test]
    fn fr_p2c_004_u009a_hello_auth_missing_credentials_returns_syntax_error() {
        let mut rt = Runtime::default_strict();
        rt.add_user(b"alice".to_vec(), b"secret2".to_vec());

        let missing_password = rt.execute_frame(command(&[b"HELLO", b"3", b"AUTH", b"alice"]), 0);
        assert_eq!(
            missing_password,
            RespFrame::Error("ERR syntax error".to_string())
        );

        let missing_username_and_password =
            rt.execute_frame(command(&[b"HELLO", b"3", b"AUTH"]), 0);
        assert_eq!(
            missing_username_and_password,
            RespFrame::Error("ERR syntax error".to_string())
        );
    }

    #[test]
    fn fr_p2c_004_u010_user_auth_requires_authentication_before_dispatch() {
        let mut rt = Runtime::default_strict();
        rt.add_user(b"alice".to_vec(), b"secret2".to_vec());

        let gated = rt.execute_frame(command(&[b"GET", b"k"]), 0);
        assert_eq!(
            gated,
            RespFrame::Error("NOAUTH Authentication required.".to_string())
        );

        let wrong = rt.execute_frame(command(&[b"AUTH", b"alice", b"bad"]), 1);
        assert_eq!(
            wrong,
            RespFrame::Error(
                "WRONGPASS invalid username-password pair or user is disabled.".to_string()
            )
        );

        let ok = rt.execute_frame(command(&[b"AUTH", b"alice", b"secret2"]), 2);
        assert_eq!(ok, RespFrame::SimpleString("OK".to_string()));

        let after_auth = rt.execute_frame(command(&[b"GET", b"k"]), 3);
        assert_eq!(after_auth, RespFrame::BulkString(None));
    }

    #[test]
    fn fr_p2c_004_u011_config_set_requirepass_bridge_preserves_authenticated_session() {
        let mut rt = Runtime::default_strict();
        let set = rt.execute_frame(command(&[b"CONFIG", b"SET", b"requirepass", b"secret"]), 0);
        assert_eq!(set, RespFrame::SimpleString("OK".to_string()));
        assert!(rt.is_authenticated());

        let get = rt.execute_frame(command(&[b"CONFIG", b"GET", b"requirepass"]), 1);
        assert_eq!(
            get,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"requirepass".to_vec())),
                RespFrame::BulkString(Some(b"secret".to_vec())),
            ]))
        );

        let clear = rt.execute_frame(command(&[b"CONFIG", b"SET", b"requirepass", b""]), 2);
        assert_eq!(clear, RespFrame::SimpleString("OK".to_string()));
        assert!(rt.is_authenticated());

        let cleared = rt.execute_frame(command(&[b"CONFIG", b"GET", b"requirepass"]), 3);
        assert_eq!(
            cleared,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"requirepass".to_vec())),
                RespFrame::BulkString(Some(Vec::new())),
            ]))
        );
    }

    #[test]
    fn fr_p2c_004_u012_config_acllog_max_len_round_trips() {
        let mut rt = Runtime::default_strict();
        let default_get = rt.execute_frame(command(&[b"CONFIG", b"GET", b"acllog-max-len"]), 0);
        assert_eq!(
            default_get,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"acllog-max-len".to_vec())),
                RespFrame::BulkString(Some(b"128".to_vec())),
            ]))
        );

        let set = rt.execute_frame(command(&[b"CONFIG", b"SET", b"acllog-max-len", b"256"]), 1);
        assert_eq!(set, RespFrame::SimpleString("OK".to_string()));

        let wildcard_get = rt.execute_frame(command(&[b"CONFIG", b"GET", b"acl*"]), 2);
        assert_eq!(
            wildcard_get,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"acllog-max-len".to_vec())),
                RespFrame::BulkString(Some(b"256".to_vec())),
            ]))
        );

        let invalid = rt.execute_frame(command(&[b"CONFIG", b"SET", b"acllog-max-len", b"-1"]), 3);
        assert_eq!(
            invalid,
            RespFrame::Error(
                "ERR CONFIG SET acllog-max-len must be a non-negative integer".to_string()
            )
        );
    }

    #[test]
    fn fr_p2c_005_u001_runtime_captures_successful_dispatched_commands_for_aof() {
        let mut rt = Runtime::default_strict();

        let set = rt.execute_frame(command(&[b"SET", b"k", b"v"]), 0);
        assert_eq!(set, RespFrame::SimpleString("OK".to_string()));

        let del = rt.execute_frame(command(&[b"DEL", b"k"]), 1);
        assert_eq!(del, RespFrame::Integer(1));

        let unknown = rt.execute_frame(command(&[b"NOPE"]), 2);
        assert!(matches!(unknown, RespFrame::Error(_)));

        let records = rt.aof_records();
        assert_eq!(records.len(), 2);
        assert_eq!(
            records[0].argv,
            vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]
        );
        assert_eq!(records[1].argv, vec![b"DEL".to_vec(), b"k".to_vec()]);
    }

    #[test]
    fn fr_p2c_005_u002_runtime_aof_stream_export_round_trips() {
        let mut rt = Runtime::default_strict();
        let _ = rt.execute_frame(command(&[b"SET", b"k", b"v"]), 0);
        let _ = rt.execute_frame(command(&[b"DEL", b"k"]), 1);

        let encoded = rt.encoded_aof_stream();
        let decoded = decode_aof_stream(&encoded).expect("decode aof stream");
        assert_eq!(decoded.len(), 2);
        assert_eq!(
            decoded[0].argv,
            vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]
        );
        assert_eq!(decoded[1].argv, vec![b"DEL".to_vec(), b"k".to_vec()]);
    }

    #[test]
    fn fr_p2c_005_u003_runtime_replay_aof_stream_applies_records() {
        let mut source = Runtime::default_strict();
        let _ = source.execute_frame(command(&[b"SET", b"k", b"v"]), 0);
        let _ = source.execute_frame(command(&[b"INCR", b"counter"]), 1);
        let encoded = source.encoded_aof_stream();
        let decoded = decode_aof_stream(&encoded).expect("decode source stream");

        let mut target = Runtime::default_strict();
        let replies = target
            .replay_aof_stream(&encoded, 10)
            .expect("replay aof stream");
        assert_eq!(
            replies,
            vec![
                RespFrame::SimpleString("OK".to_string()),
                RespFrame::Integer(1)
            ]
        );
        assert_eq!(target.aof_records(), decoded.as_slice());

        let get = target.execute_frame(command(&[b"GET", b"k"]), 100);
        assert_eq!(get, RespFrame::BulkString(Some(b"v".to_vec())));
        let get_counter = target.execute_frame(command(&[b"GET", b"counter"]), 101);
        assert_eq!(get_counter, RespFrame::BulkString(Some(b"1".to_vec())));
    }

    #[test]
    fn fr_p2c_005_u004_runtime_replay_aof_stream_rejects_invalid_payload() {
        let mut rt = Runtime::default_strict();
        let err = rt
            .replay_aof_stream(b"$3\r\nbad\r\n", 0)
            .expect_err("invalid stream must fail");
        assert_eq!(err, PersistError::InvalidFrame);
        assert!(rt.aof_records().is_empty());
    }

    #[test]
    fn fr_p2c_006_u005_wait_requires_arity_and_integer_args() {
        let mut rt = Runtime::default_strict();

        let wrong_arity = rt.execute_frame(command(&[b"WAIT", b"1"]), 0);
        assert_eq!(
            wrong_arity,
            RespFrame::Error("ERR wrong number of arguments for 'WAIT' command".to_string())
        );

        let invalid_integer = rt.execute_frame(command(&[b"WAIT", b"nope", b"0"]), 1);
        assert_eq!(
            invalid_integer,
            RespFrame::Error("ERR value is not an integer or out of range".to_string())
        );

        let invalid_timeout = rt.execute_frame(command(&[b"WAIT", b"1", b"-1"]), 2);
        assert_eq!(
            invalid_timeout,
            RespFrame::Error("ERR value is not an integer or out of range".to_string())
        );
    }

    #[test]
    fn fr_p2c_006_u005_wait_returns_acked_replica_count_at_primary_offset() {
        let mut rt = Runtime::default_strict();
        let _ = rt.execute_frame(command(&[b"SET", b"fr:p2c:006:key", b"value"]), 0);
        rt.set_replication_ack_state_for_tests(1, 1, &[1, 0, 2], &[1, 0, 2]);

        let two = rt.execute_frame(command(&[b"WAIT", b"2", b"0"]), 1);
        assert_eq!(two, RespFrame::Integer(2));

        let still_two = rt.execute_frame(command(&[b"WAIT", b"3", b"0"]), 2);
        assert_eq!(still_two, RespFrame::Integer(2));
    }

    #[test]
    fn fr_p2c_006_u006_waitaof_requires_local_and_replica_thresholds() {
        let mut rt = Runtime::default_strict();
        let _ = rt.execute_frame(command(&[b"SET", b"fr:p2c:006:aof", b"value"]), 0);

        rt.set_replication_ack_state_for_tests(1, 0, &[1, 0], &[1, 0]);
        let local_not_ready = rt.execute_frame(command(&[b"WAITAOF", b"1", b"1", b"0"]), 1);
        assert_eq!(
            local_not_ready,
            RespFrame::Array(Some(vec![RespFrame::Integer(0), RespFrame::Integer(1)]))
        );

        rt.set_replication_ack_state_for_tests(1, 1, &[1, 0, 2], &[1, 0, 2]);
        let local_and_replica_ready = rt.execute_frame(command(&[b"WAITAOF", b"1", b"2", b"0"]), 2);
        assert_eq!(
            local_and_replica_ready,
            RespFrame::Array(Some(vec![RespFrame::Integer(1), RespFrame::Integer(2)]))
        );
    }

    #[test]
    fn fr_p2c_006_u006_waitaof_rejects_invalid_integer_arguments() {
        let mut rt = Runtime::default_strict();

        let wrong_arity = rt.execute_frame(command(&[b"WAITAOF", b"1", b"0"]), 0);
        assert_eq!(
            wrong_arity,
            RespFrame::Error("ERR wrong number of arguments for 'WAITAOF' command".to_string())
        );

        let invalid_local = rt.execute_frame(command(&[b"WAITAOF", b"nope", b"1", b"0"]), 0);
        assert_eq!(
            invalid_local,
            RespFrame::Error("ERR value is not an integer or out of range".to_string())
        );

        let invalid_replica = rt.execute_frame(command(&[b"WAITAOF", b"1", b"nope", b"0"]), 1);
        assert_eq!(
            invalid_replica,
            RespFrame::Error("ERR value is not an integer or out of range".to_string())
        );

        let invalid_timeout = rt.execute_frame(command(&[b"WAITAOF", b"1", b"0", b"-1"]), 1);
        assert_eq!(
            invalid_timeout,
            RespFrame::Error("ERR value is not an integer or out of range".to_string())
        );
    }

    #[test]
    fn fr_p2c_004_runtime_special_command_classifier_matches_linear_reference() {
        let samples: &[&[u8]] = &[
            b"AUTH",
            b"auth",
            b"HeLlO",
            b"ASKING",
            b"CONFIG",
            b"readonly",
            b"READWRITE",
            b"cluster",
            b"WAIT",
            b"WAITAOF",
            b"QUIT",
            b"quit",
            b"PING",
            b"GET",
            b"SET",
            b"UNKNOWN",
            b"post",
            b"host:",
            &[0xFF],
        ];
        for sample in samples {
            let optimized = classify_runtime_special_command(sample);
            let linear = classify_runtime_special_command_linear(sample);
            assert_eq!(
                optimized,
                linear,
                "special command classifier mismatch for {:?}",
                String::from_utf8_lossy(sample)
            );
        }
    }

    #[test]
    fn fr_p2c_007_cluster_subcommand_classifier_matches_linear_reference() {
        let samples: &[&[u8]] = &[
            b"HELP",
            b"help",
            b"HeLp",
            b"NOPE",
            b"SLOTS",
            b"NODES",
            b"SETSLOT",
            b"FAILOVER",
            b"myid",
            &[0xFF],
        ];
        for sample in samples {
            let optimized = classify_cluster_subcommand(sample);
            let linear = classify_cluster_subcommand_linear(sample);
            assert_eq!(
                optimized,
                linear,
                "cluster subcommand classifier mismatch for {:?}",
                String::from_utf8_lossy(sample)
            );
        }
    }

    #[test]
    #[ignore = "profiling helper for FR-P2C-007-H"]
    fn fr_p2c_007_cluster_subcommand_route_profile_snapshot() {
        let workload: &[&[u8]] = &[
            b"HELP",
            b"help",
            b"HeLp",
            b"HELP",
            b"NOPE",
            b"SLOTS",
            b"NODES",
            b"SETSLOT",
            b"FAILOVER",
            b"myid",
            &[0xFF],
        ];

        let rounds = 300_000usize;
        let total_lookups = rounds.saturating_mul(workload.len());

        let mut linear_help_hits = 0usize;
        let mut linear_invalid_utf8 = 0usize;
        let linear_start = Instant::now();
        for _ in 0..rounds {
            for cmd in workload {
                match classify_cluster_subcommand_linear(cmd) {
                    Ok(ClusterSubcommand::Help) => {
                        linear_help_hits = linear_help_hits.saturating_add(1)
                    }
                    Ok(ClusterSubcommand::Unknown) => {}
                    Err(CommandError::InvalidUtf8Argument) => {
                        linear_invalid_utf8 = linear_invalid_utf8.saturating_add(1)
                    }
                    Err(err) => panic!("unexpected linear classifier error: {err:?}"),
                }
            }
        }
        let linear_ns = linear_start.elapsed().as_nanos();

        let mut optimized_help_hits = 0usize;
        let mut optimized_invalid_utf8 = 0usize;
        let optimized_start = Instant::now();
        for _ in 0..rounds {
            for cmd in workload {
                match classify_cluster_subcommand(cmd) {
                    Ok(ClusterSubcommand::Help) => {
                        optimized_help_hits = optimized_help_hits.saturating_add(1)
                    }
                    Ok(ClusterSubcommand::Unknown) => {}
                    Err(CommandError::InvalidUtf8Argument) => {
                        optimized_invalid_utf8 = optimized_invalid_utf8.saturating_add(1)
                    }
                    Err(err) => panic!("unexpected optimized classifier error: {err:?}"),
                }
            }
        }
        let optimized_ns = optimized_start.elapsed().as_nanos();

        assert_eq!(linear_help_hits, optimized_help_hits);
        assert_eq!(linear_invalid_utf8, optimized_invalid_utf8);
        assert!(total_lookups > 0);

        let linear_ns_per_lookup = linear_ns as f64 / total_lookups as f64;
        let optimized_ns_per_lookup = optimized_ns as f64 / total_lookups as f64;
        let speedup_ratio = if optimized_ns > 0 {
            linear_ns as f64 / optimized_ns as f64
        } else {
            0.0
        };

        println!("profile.packet_id=FR-P2C-007");
        println!("profile.benchmark=cluster_subcommand_classifier");
        println!("profile.total_lookups={total_lookups}");
        println!("profile.linear_total_ns={linear_ns}");
        println!("profile.optimized_total_ns={optimized_ns}");
        println!("profile.linear_help_hits={linear_help_hits}");
        println!("profile.optimized_help_hits={optimized_help_hits}");
        println!("profile.linear_invalid_utf8={linear_invalid_utf8}");
        println!("profile.optimized_invalid_utf8={optimized_invalid_utf8}");
        println!("profile.linear_ns_per_lookup={linear_ns_per_lookup:.6}");
        println!("profile.optimized_ns_per_lookup={optimized_ns_per_lookup:.6}");
        println!("profile.speedup_ratio={speedup_ratio:.6}");
    }

    #[test]
    #[ignore = "profiling helper for FR-P2C-004-H"]
    fn fr_p2c_004_runtime_special_route_profile_snapshot() {
        let workload: &[&[u8]] = &[
            b"PING",
            b"SET",
            b"GET",
            b"CONFIG",
            b"AUTH",
            b"HELLO",
            b"READONLY",
            b"READWRITE",
            b"CLUSTER",
            b"ASKING",
            b"WAIT",
            b"WAITAOF",
            b"DEL",
            b"MGET",
            b"MSET",
            b"QUIT",
            b"UNKNOWN",
            b"host:",
            b"post",
        ];

        let rounds = 300_000usize;
        let total_lookups = rounds.saturating_mul(workload.len());

        let mut linear_hits = 0usize;
        let linear_start = Instant::now();
        for _ in 0..rounds {
            for cmd in workload {
                if classify_runtime_special_command_linear(cmd).is_some() {
                    linear_hits = linear_hits.saturating_add(1);
                }
            }
        }
        let linear_ns = linear_start.elapsed().as_nanos();

        let mut optimized_hits = 0usize;
        let optimized_start = Instant::now();
        for _ in 0..rounds {
            for cmd in workload {
                if classify_runtime_special_command(cmd).is_some() {
                    optimized_hits = optimized_hits.saturating_add(1);
                }
            }
        }
        let optimized_ns = optimized_start.elapsed().as_nanos();

        assert_eq!(linear_hits, optimized_hits);
        assert!(total_lookups > 0);

        let linear_ns_per_lookup = linear_ns as f64 / total_lookups as f64;
        let optimized_ns_per_lookup = optimized_ns as f64 / total_lookups as f64;
        let speedup_ratio = if optimized_ns > 0 {
            linear_ns as f64 / optimized_ns as f64
        } else {
            0.0
        };

        println!("profile.packet_id=FR-P2C-004");
        println!("profile.benchmark=runtime_special_route_classifier");
        println!("profile.total_lookups={total_lookups}");
        println!("profile.linear_total_ns={linear_ns}");
        println!("profile.optimized_total_ns={optimized_ns}");
        println!("profile.linear_ns_per_lookup={linear_ns_per_lookup:.6}");
        println!("profile.optimized_ns_per_lookup={optimized_ns_per_lookup:.6}");
        println!("profile.speedup_ratio={speedup_ratio:.6}");
    }

    #[test]
    fn fr_p2c_007_u001_cluster_subcommand_router_is_deterministic() {
        let mut rt = Runtime::default_strict();

        let wrong_arity = rt.execute_frame(command(&[b"CLUSTER"]), 0);
        assert_eq!(
            wrong_arity,
            RespFrame::Error("ERR wrong number of arguments for 'CLUSTER' command".to_string())
        );

        let help = rt.execute_frame(command(&[b"CLUSTER", b"HELP"]), 0);
        assert_eq!(
            help,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"CLUSTER HELP".to_vec())),
                RespFrame::BulkString(Some(
                    b"CLUSTER subcommand dispatch scaffold (FR-P2C-007 D1).".to_vec(),
                )),
                RespFrame::BulkString(
                    Some(b"Supported subcommands in this stage: HELP.".to_vec(),)
                ),
            ]))
        );

        let unknown = rt.execute_frame(command(&[b"CLUSTER", b"NOPE"]), 0);
        assert_eq!(
            unknown,
            RespFrame::Error(
                "ERR Unknown subcommand or wrong number of arguments for 'CLUSTER'. Try CLUSTER HELP."
                    .to_string(),
            )
        );
    }

    #[test]
    fn fr_p2c_007_u007_client_cluster_mode_flags_transition_cleanly() {
        let mut rt = Runtime::default_strict();
        assert!(!rt.is_cluster_read_only());
        assert!(!rt.is_cluster_asking());

        let readonly = rt.execute_frame(command(&[b"READONLY"]), 0);
        assert_eq!(readonly, RespFrame::SimpleString("OK".to_string()));
        assert!(rt.is_cluster_read_only());
        assert!(!rt.is_cluster_asking());

        let asking = rt.execute_frame(command(&[b"ASKING"]), 0);
        assert_eq!(asking, RespFrame::SimpleString("OK".to_string()));
        assert!(rt.is_cluster_read_only());
        assert!(rt.is_cluster_asking());

        let readwrite = rt.execute_frame(command(&[b"READWRITE"]), 0);
        assert_eq!(readwrite, RespFrame::SimpleString("OK".to_string()));
        assert!(!rt.is_cluster_read_only());
        assert!(!rt.is_cluster_asking());
    }

    #[test]
    fn compatibility_gate_trips_on_large_array() {
        let mut policy = RuntimePolicy::default();
        policy.gate.max_array_len = 1;
        let mut rt = Runtime::new(policy);
        let in_frame = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"PING".to_vec())),
            RespFrame::BulkString(Some(b"x".to_vec())),
        ]));
        let out = rt.execute_frame(in_frame, 100);
        assert!(matches!(out, RespFrame::Error(_)));
        assert_eq!(rt.evidence().events().len(), 1);
        let event = &rt.evidence().events()[0];
        assert_eq!(event.mode, Mode::Strict);
        assert_eq!(event.threat_class, ThreatClass::ResourceExhaustion);
        assert_eq!(event.severity, DriftSeverity::S0);
        assert_eq!(event.decision_action, DecisionAction::FailClosed);
        assert_eq!(event.reason_code, "compat_array_len_exceeded");
        assert!(!event.input_digest.is_empty());
        assert!(!event.output_digest.is_empty());
        assert!(!event.state_digest_before.is_empty());
        assert!(!event.state_digest_after.is_empty());
    }

    #[test]
    fn unknown_command_error_includes_args_preview() {
        let mut rt = Runtime::default_strict();
        let frame = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"NOPE".to_vec())),
            RespFrame::BulkString(Some(b"a".to_vec())),
            RespFrame::BulkString(Some(b"b".to_vec())),
        ]));
        let out = rt.execute_frame(frame, 0);
        assert_eq!(
            out,
            RespFrame::Error(
                "ERR unknown command 'NOPE', with args beginning with: 'a' 'b' ".to_string()
            )
        );
    }

    #[test]
    fn protocol_invalid_bulk_length_error_string() {
        let mut rt = Runtime::default_strict();
        let raw = b"$-2\r\n";
        let encoded = rt.execute_bytes(raw, 0);
        let parsed = parse_frame(&encoded).expect("parse");
        assert_eq!(
            parsed.frame,
            RespFrame::Error("ERR Protocol error: invalid bulk length".to_string())
        );
        let event = rt.evidence().events().last().expect("event");
        assert_eq!(event.threat_class, ThreatClass::ParserAbuse);
        assert_eq!(event.severity, DriftSeverity::S0);
        assert_eq!(event.decision_action, DecisionAction::FailClosed);
        assert_eq!(event.reason_code, "protocol_parse_failure");
    }

    #[test]
    fn protocol_unsupported_resp3_type_error_string() {
        let mut rt = Runtime::default_strict();
        let raw = b"~1\r\n";
        let encoded = rt.execute_bytes(raw, 0);
        let parsed = parse_frame(&encoded).expect("parse");
        assert_eq!(
            parsed.frame,
            RespFrame::Error("ERR Protocol error: unsupported RESP3 type prefix '~'".to_string())
        );
        let event = rt.evidence().events().last().expect("event");
        assert_eq!(event.threat_class, ThreatClass::ParserAbuse);
        assert_eq!(event.severity, DriftSeverity::S0);
        assert_eq!(event.decision_action, DecisionAction::FailClosed);
        assert_eq!(event.reason_code, "protocol_parse_failure");
    }

    #[test]
    fn hardened_mode_allowlisted_gate_uses_bounded_defense() {
        let mut policy = RuntimePolicy::hardened();
        policy.gate.max_array_len = 1;
        let mut rt = Runtime::new(policy);
        let in_frame = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"PING".to_vec())),
            RespFrame::BulkString(Some(b"x".to_vec())),
        ]));
        let _ = rt.execute_frame(in_frame, 42);
        let event = rt.evidence().events().last().expect("event");
        assert_eq!(event.mode, Mode::Hardened);
        assert_eq!(event.decision_action, DecisionAction::BoundedDefense);
        assert_eq!(event.severity, DriftSeverity::S1);
    }

    #[test]
    fn hardened_mode_without_allowlist_rejects_non_allowlisted() {
        let mut policy = RuntimePolicy::hardened();
        policy.gate.max_array_len = 1;
        policy
            .hardened_allowlist
            .retain(|c| *c != HardenedDeviationCategory::ResourceClamp);
        let mut rt = Runtime::new(policy);
        let in_frame = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"PING".to_vec())),
            RespFrame::BulkString(Some(b"x".to_vec())),
        ]));
        let _ = rt.execute_frame(in_frame, 42);
        let event = rt.evidence().events().last().expect("event");
        assert_eq!(event.mode, Mode::Hardened);
        assert_eq!(event.decision_action, DecisionAction::RejectNonAllowlisted);
        assert_eq!(event.severity, DriftSeverity::S2);
    }

    fn valid_tls_config() -> TlsConfig {
        TlsConfig {
            tls_port: Some(6380),
            cert_file: Some("cert.pem".to_string()),
            key_file: Some("key.pem".to_string()),
            ca_file: Some("ca.pem".to_string()),
            protocols: vec![TlsProtocol::TlsV1_2, TlsProtocol::TlsV1_3],
            ciphers: Some("HIGH:!aNULL".to_string()),
            auth_clients: TlsAuthClients::Required,
            cluster_announce_tls_port: Some(16380),
            max_new_tls_connections_per_cycle: 64,
        }
    }

    #[test]
    fn fr_p2c_009_u010_runtime_apply_updates_tls_state() {
        let mut runtime = Runtime::default_strict();
        runtime
            .apply_tls_config(valid_tls_config(), 123)
            .expect("valid TLS config");
        let tls_state = runtime.tls_runtime_state();
        assert!(tls_state.tls_listener_enabled);
        assert!(tls_state.connection_type_configured);
        assert!(tls_state.active_config.is_some());
    }

    #[test]
    fn fr_p2c_009_u013_strict_mode_rejects_unsafe_tls_config_and_records_event() {
        let mut runtime = Runtime::default_strict();
        let mut invalid = valid_tls_config();
        invalid.tls_port = None;
        invalid.cluster_announce_tls_port = None;
        let err = runtime
            .apply_tls_config(invalid, 321)
            .expect_err("must fail closed");
        assert_eq!(err.reason_code(), "tlscfg.safety_gate_contract_violation");

        let event = runtime.evidence().events().last().expect("event");
        assert_eq!(event.threat_class, ThreatClass::ConfigDowngradeAbuse);
        assert_eq!(event.reason_code, "tlscfg.safety_gate_contract_violation");
        assert_eq!(event.decision_action, DecisionAction::FailClosed);
        assert_eq!(event.severity, DriftSeverity::S0);
    }

    #[test]
    fn fr_p2c_009_u013_hardened_non_allowlisted_tls_deviation_is_rejected() {
        let mut policy = RuntimePolicy::hardened();
        policy
            .hardened_allowlist
            .retain(|category| *category != HardenedDeviationCategory::MetadataSanitization);
        let mut runtime = Runtime::new(policy);

        let mut invalid = valid_tls_config();
        invalid.tls_port = None;
        invalid.cluster_announce_tls_port = None;
        let err = runtime
            .apply_tls_config(invalid, 456)
            .expect_err("must reject non-allowlisted");
        assert_eq!(err.reason_code(), "tlscfg.hardened_nonallowlisted_rejected");

        let event = runtime.evidence().events().last().expect("event");
        assert_eq!(event.reason_code, "tlscfg.hardened_nonallowlisted_rejected");
        assert_eq!(event.decision_action, DecisionAction::RejectNonAllowlisted);
        assert_eq!(event.severity, DriftSeverity::S2);
    }

    // ── WATCH / UNWATCH tests ──

    #[test]
    fn watch_exec_succeeds_when_key_unchanged() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"k", b"v"]), 0),
            RespFrame::SimpleString("OK".to_string()),
        );
        // WATCH k
        assert_eq!(
            rt.execute_frame(command(&[b"WATCH", b"k"]), 1),
            RespFrame::SimpleString("OK".to_string()),
        );
        // MULTI
        assert_eq!(
            rt.execute_frame(command(&[b"MULTI"]), 2),
            RespFrame::SimpleString("OK".to_string()),
        );
        // Queue a SET inside the transaction
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"k", b"new"]), 3),
            RespFrame::SimpleString("QUEUED".to_string()),
        );
        // EXEC should succeed (key not modified between WATCH and EXEC)
        let result = rt.execute_frame(command(&[b"EXEC"]), 4);
        assert_eq!(
            result,
            RespFrame::Array(Some(vec![RespFrame::SimpleString("OK".to_string())])),
        );
    }

    #[test]
    fn watch_exec_aborts_when_key_modified() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"k", b"v"]), 0),
            RespFrame::SimpleString("OK".to_string()),
        );
        // WATCH k
        assert_eq!(
            rt.execute_frame(command(&[b"WATCH", b"k"]), 1),
            RespFrame::SimpleString("OK".to_string()),
        );
        // Modify k outside the transaction (simulates another client)
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"k", b"changed"]), 2),
            RespFrame::SimpleString("OK".to_string()),
        );
        // MULTI
        assert_eq!(
            rt.execute_frame(command(&[b"MULTI"]), 3),
            RespFrame::SimpleString("OK".to_string()),
        );
        // Queue a GET
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"k"]), 4),
            RespFrame::SimpleString("QUEUED".to_string()),
        );
        // EXEC should return null array (transaction aborted)
        let result = rt.execute_frame(command(&[b"EXEC"]), 5);
        assert_eq!(result, RespFrame::Array(None));
    }

    #[test]
    fn watch_nonexistent_key_aborts_on_creation() {
        let mut rt = Runtime::default_strict();
        // WATCH a key that doesn't exist
        assert_eq!(
            rt.execute_frame(command(&[b"WATCH", b"missing"]), 0),
            RespFrame::SimpleString("OK".to_string()),
        );
        // Create the key
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"missing", b"now_exists"]), 1),
            RespFrame::SimpleString("OK".to_string()),
        );
        // MULTI + EXEC
        assert_eq!(
            rt.execute_frame(command(&[b"MULTI"]), 2),
            RespFrame::SimpleString("OK".to_string()),
        );
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"missing"]), 3),
            RespFrame::SimpleString("QUEUED".to_string()),
        );
        // Should abort because the watched key was created
        let result = rt.execute_frame(command(&[b"EXEC"]), 4);
        assert_eq!(result, RespFrame::Array(None));
    }

    #[test]
    fn watch_multiple_keys() {
        let mut rt = Runtime::default_strict();
        rt.execute_frame(command(&[b"SET", b"a", b"1"]), 0);
        rt.execute_frame(command(&[b"SET", b"b", b"2"]), 0);
        // WATCH multiple keys in one command
        assert_eq!(
            rt.execute_frame(command(&[b"WATCH", b"a", b"b"]), 1),
            RespFrame::SimpleString("OK".to_string()),
        );
        // Only modify b
        rt.execute_frame(command(&[b"SET", b"b", b"changed"]), 2);
        // MULTI + EXEC
        rt.execute_frame(command(&[b"MULTI"]), 3);
        rt.execute_frame(command(&[b"GET", b"a"]), 4);
        let result = rt.execute_frame(command(&[b"EXEC"]), 5);
        assert_eq!(result, RespFrame::Array(None));
    }

    #[test]
    fn unwatch_clears_watched_keys() {
        let mut rt = Runtime::default_strict();
        rt.execute_frame(command(&[b"SET", b"k", b"v"]), 0);
        rt.execute_frame(command(&[b"WATCH", b"k"]), 1);
        // Modify k
        rt.execute_frame(command(&[b"SET", b"k", b"changed"]), 2);
        // UNWATCH clears the watch
        assert_eq!(
            rt.execute_frame(command(&[b"UNWATCH"]), 3),
            RespFrame::SimpleString("OK".to_string()),
        );
        // MULTI + EXEC should succeed now
        rt.execute_frame(command(&[b"MULTI"]), 4);
        rt.execute_frame(command(&[b"GET", b"k"]), 5);
        let result = rt.execute_frame(command(&[b"EXEC"]), 6);
        assert_eq!(
            result,
            RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"changed".to_vec()))])),
        );
    }

    #[test]
    fn watch_inside_multi_returns_error() {
        let mut rt = Runtime::default_strict();
        rt.execute_frame(command(&[b"MULTI"]), 0);
        let result = rt.execute_frame(command(&[b"WATCH", b"k"]), 1);
        assert!(matches!(result, RespFrame::Error(ref msg) if msg.contains("WATCH inside MULTI")));
    }

    #[test]
    fn watch_wrong_arity() {
        let mut rt = Runtime::default_strict();
        let result = rt.execute_frame(command(&[b"WATCH"]), 0);
        assert!(
            matches!(result, RespFrame::Error(ref msg) if msg.contains("wrong number of arguments"))
        );
    }

    #[test]
    fn unwatch_wrong_arity() {
        let mut rt = Runtime::default_strict();
        let result = rt.execute_frame(command(&[b"UNWATCH", b"extra"]), 0);
        assert!(
            matches!(result, RespFrame::Error(ref msg) if msg.contains("wrong number of arguments"))
        );
    }

    #[test]
    fn discard_clears_watched_keys() {
        let mut rt = Runtime::default_strict();
        rt.execute_frame(command(&[b"SET", b"k", b"v"]), 0);
        rt.execute_frame(command(&[b"WATCH", b"k"]), 1);
        rt.execute_frame(command(&[b"SET", b"k", b"changed"]), 2);
        rt.execute_frame(command(&[b"MULTI"]), 3);
        // DISCARD clears both transaction and watch state
        assert_eq!(
            rt.execute_frame(command(&[b"DISCARD"]), 4),
            RespFrame::SimpleString("OK".to_string()),
        );
        // A new MULTI+EXEC without WATCH should succeed
        rt.execute_frame(command(&[b"MULTI"]), 5);
        rt.execute_frame(command(&[b"GET", b"k"]), 6);
        let result = rt.execute_frame(command(&[b"EXEC"]), 7);
        assert_eq!(
            result,
            RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"changed".to_vec()))])),
        );
    }

    #[test]
    fn exec_clears_watch_state() {
        let mut rt = Runtime::default_strict();
        rt.execute_frame(command(&[b"SET", b"k", b"v"]), 0);
        // WATCH + successful EXEC
        rt.execute_frame(command(&[b"WATCH", b"k"]), 1);
        rt.execute_frame(command(&[b"MULTI"]), 2);
        rt.execute_frame(command(&[b"SET", b"k", b"new"]), 3);
        let r1 = rt.execute_frame(command(&[b"EXEC"]), 4);
        assert!(matches!(r1, RespFrame::Array(Some(_))));
        // Now modify k and do another MULTI+EXEC without re-WATCHing
        rt.execute_frame(command(&[b"SET", b"k", b"changed_again"]), 5);
        rt.execute_frame(command(&[b"MULTI"]), 6);
        rt.execute_frame(command(&[b"GET", b"k"]), 7);
        // Should succeed - previous WATCH was cleared by EXEC
        let r2 = rt.execute_frame(command(&[b"EXEC"]), 8);
        assert_eq!(
            r2,
            RespFrame::Array(Some(vec![RespFrame::BulkString(Some(
                b"changed_again".to_vec()
            ))])),
        );
    }

    #[test]
    fn watch_key_deleted_aborts_transaction() {
        let mut rt = Runtime::default_strict();
        rt.execute_frame(command(&[b"SET", b"k", b"v"]), 0);
        rt.execute_frame(command(&[b"WATCH", b"k"]), 1);
        // Delete the watched key
        rt.execute_frame(command(&[b"DEL", b"k"]), 2);
        // MULTI + EXEC
        rt.execute_frame(command(&[b"MULTI"]), 3);
        rt.execute_frame(command(&[b"SET", b"k", b"new"]), 4);
        let result = rt.execute_frame(command(&[b"EXEC"]), 5);
        assert_eq!(result, RespFrame::Array(None));
    }

    #[test]
    fn config_set_maxmemory_updates_and_get_returns_dynamic_value() {
        let mut rt = Runtime::default_strict();
        // Default is 0
        let get = rt.execute_frame(command(&[b"CONFIG", b"GET", b"maxmemory"]), 0);
        assert_eq!(
            get,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"maxmemory".to_vec())),
                RespFrame::BulkString(Some(b"0".to_vec())),
            ]))
        );
        // Set to 1GB
        let set = rt.execute_frame(
            command(&[b"CONFIG", b"SET", b"maxmemory", b"1073741824"]),
            1,
        );
        assert_eq!(set, RespFrame::SimpleString("OK".to_string()));
        // Verify the dynamic value
        let get2 = rt.execute_frame(command(&[b"CONFIG", b"GET", b"maxmemory"]), 2);
        assert_eq!(
            get2,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"maxmemory".to_vec())),
                RespFrame::BulkString(Some(b"1073741824".to_vec())),
            ]))
        );
    }

    #[test]
    fn config_set_common_params_accepted() {
        let mut rt = Runtime::default_strict();
        // Commonly-probed parameters should be accepted without error
        let set = rt.execute_frame(
            command(&[b"CONFIG", b"SET", b"hz", b"100"]),
            0,
        );
        assert_eq!(set, RespFrame::SimpleString("OK".to_string()));
        let set = rt.execute_frame(
            command(&[b"CONFIG", b"SET", b"timeout", b"300"]),
            1,
        );
        assert_eq!(set, RespFrame::SimpleString("OK".to_string()));
        let set = rt.execute_frame(
            command(&[b"CONFIG", b"SET", b"loglevel", b"warning"]),
            2,
        );
        assert_eq!(set, RespFrame::SimpleString("OK".to_string()));
    }
}
