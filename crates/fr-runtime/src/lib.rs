#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use fr_command::{
    CommandError, MigrateKeySpec, commands_in_acl_category, dispatch_argv, execute_migrate,
    frame_to_argv, parse_migrate_request,
};
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
use fr_persist::{
    AofRecord, PersistError, RdbEntry, RdbValue, argv_to_aof_records, decode_aof_stream,
    decode_rdb, encode_aof_stream, encode_rdb, write_aof_file, write_rdb_file,
};
use fr_protocol::{RespFrame, RespParseError};
use fr_repl::{
    BacklogWindow, PsyncReply, ReplOffset, WaitAofThreshold, WaitThreshold, decide_psync,
    evaluate_wait, evaluate_waitaof, parse_psync_reply,
};
use fr_store::{
    EvictionLoopFailure, EvictionLoopResult, EvictionLoopStatus, EvictionSafetyGateState,
    MaxmemoryPolicy, Store, decode_db_key, encode_db_key, glob_match,
};

/// Return the ACL categories a command belongs to, using the COMMAND_TABLE metadata.
fn command_categories(cmd_name: &str) -> Vec<String> {
    let Some(flags) = fr_command::get_command_flags(cmd_name.as_bytes()) else {
        return Vec::new();
    };
    // The ACL category names used by Redis map to flag words and data-type names.
    // We check against the known category set.
    const ACL_CATEGORIES: &[&str] = &[
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
    let flag_list: Vec<&str> = flags.split_whitespace().collect();
    let mut cats = Vec::new();
    for &cat in ACL_CATEGORIES {
        // Check if the command's flags contain the category name.
        if flag_list.iter().any(|f| f.eq_ignore_ascii_case(cat)) {
            cats.push(cat.to_string());
        }
    }
    // Also check if the command appears in category listing (handles edge cases where
    // flag names differ from category names, e.g., "generic" vs "keyspace").
    for &cat in ACL_CATEGORIES {
        if !cats.iter().any(|c| c == cat) {
            let cat_cmds = commands_in_acl_category(cat);
            if cat_cmds.iter().any(|c| c.eq_ignore_ascii_case(cmd_name)) {
                cats.push(cat.to_string());
            }
        }
    }
    cats
}

static PACKET_COUNTER: AtomicU64 = AtomicU64::new(1);
static CLIENT_ID_COUNTER: AtomicU64 = AtomicU64::new(1);
const DEFAULT_AUTH_USER: &[u8] = b"default";
const NOAUTH_ERROR: &str = "NOAUTH Authentication required.";
const WRONGPASS_ERROR: &str = "WRONGPASS invalid username-password pair or user is disabled.";
const AUTH_NOT_CONFIGURED_ERROR: &str = "ERR AUTH <password> called without any password configured for the default user. Are you sure your configuration is correct?";
const CLUSTER_UNKNOWN_SUBCOMMAND_ERROR: &str =
    "ERR Unknown subcommand or wrong number of arguments for 'CLUSTER'. Try CLUSTER HELP.";
#[allow(dead_code)]
const ACL_UNKNOWN_SUBCOMMAND_ERROR: &str =
    "ERR unknown subcommand or wrong number of arguments for 'ACL'. Try ACL HELP.";
const DEFAULT_ACLLOG_MAX_LEN: i64 = 128;
const DEFAULT_REPL_BACKLOG_SIZE: u64 = 1_048_576;

/// Static configuration parameters returned by CONFIG GET.
/// These represent sensible defaults for a standalone FrankenRedis instance.
const CONFIG_STATIC_PARAMS: &[(&str, &str)] = &[
    // Network
    ("bind", "127.0.0.1"),
    ("bind-source-addr", ""),
    ("port", "6379"),
    ("tcp-backlog", "511"),
    ("unixsocket", ""),
    ("unixsocketperm", "0"),
    ("timeout", "0"),
    ("tcp-keepalive", "300"),
    ("protected-mode", "yes"),
    // General
    ("daemonize", "no"),
    ("pidfile", ""),
    ("loglevel", "notice"),
    ("logfile", ""),
    ("databases", "16"),
    ("always-show-logo", "yes"),
    ("set-proc-title", "yes"),
    ("proc-title-template", "{title} {laddr} {server-mode}"),
    // Memory
    ("maxmemory", "0"),
    ("maxmemory-policy", "noeviction"),
    ("maxmemory-samples", "5"),
    ("maxmemory-eviction-tenacity", "10"),
    ("maxclients", "10000"),
    // Persistence - RDB
    ("save", ""),
    ("stop-writes-on-bgsave-error", "yes"),
    ("rdbcompression", "yes"),
    ("rdbchecksum", "yes"),
    ("dbfilename", "dump.rdb"),
    ("rdb-del-sync-files", "no"),
    ("dir", "."),
    // Persistence - AOF
    ("appendonly", "no"),
    ("appendfilename", "appendonly.aof"),
    ("appenddirname", "appendonlydir"),
    ("appendfsync", "everysec"),
    ("no-appendfsync-on-rewrite", "no"),
    ("auto-aof-rewrite-percentage", "100"),
    ("auto-aof-rewrite-min-size", "67108864"),
    ("aof-load-truncated", "yes"),
    ("aof-use-rdb-preamble", "yes"),
    ("aof-timestamp-enabled", "no"),
    // Replication
    ("replicaof", ""),
    ("masterauth", ""),
    ("masteruser", ""),
    ("replica-serve-stale-data", "yes"),
    ("replica-read-only", "yes"),
    ("replica-lazy-flush", "no"),
    ("repl-diskless-sync", "yes"),
    ("repl-diskless-sync-delay", "5"),
    ("repl-diskless-sync-period", "0"),
    ("repl-diskless-load", "disabled"),
    ("repl-ping-replica-period", "10"),
    ("repl-timeout", "60"),
    ("repl-backlog-size", "1048576"),
    ("repl-backlog-ttl", "3600"),
    ("min-replicas-to-write", "0"),
    ("min-replicas-max-lag", "10"),
    // Lua scripting
    ("lua-time-limit", "5000"),
    ("busy-reply-threshold", "5000"),
    // Cluster
    ("cluster-enabled", "no"),
    ("cluster-config-file", "nodes.conf"),
    ("cluster-node-timeout", "15000"),
    ("cluster-migration-barrier", "1"),
    ("cluster-allow-reads-when-down", "no"),
    ("cluster-allow-pubsubshard-when-down", "yes"),
    ("cluster-link-sendbuf-limit", "0"),
    ("cluster-announce-hostname", ""),
    ("cluster-announce-human-nodename", ""),
    ("cluster-preferred-endpoint-type", "ip"),
    // Slow log
    ("slowlog-log-slower-than", "10000"),
    ("slowlog-max-len", "128"),
    // Latency monitor
    ("latency-tracking", "yes"),
    ("latency-tracking-info-percentiles", "50 99 99.9"),
    ("latency-monitor-threshold", "0"),
    // Keyspace notifications
    ("notify-keyspace-events", ""),
    // Advanced
    ("hz", "10"),
    ("dynamic-hz", "yes"),
    ("activedefrag", "no"),
    ("active-defrag-enabled", "no"),
    ("active-defrag-threshold-lower", "10"),
    ("active-defrag-threshold-upper", "100"),
    ("active-defrag-cycle-min", "1"),
    ("active-defrag-cycle-max", "25"),
    ("active-expire-enabled", "yes"),
    ("active-expire-effort", "1"),
    ("lfu-log-factor", "10"),
    ("lfu-decay-time", "1"),
    // Lazy free
    ("lazyfree-lazy-eviction", "no"),
    ("lazyfree-lazy-expire", "no"),
    ("lazyfree-lazy-server-del", "no"),
    ("lazyfree-lazy-user-del", "no"),
    ("lazyfree-lazy-user-flush", "no"),
    // Encoding thresholds
    ("list-max-listpack-size", "-2"),
    ("list-max-ziplist-size", "-2"),
    ("list-max-listpack-entries", "128"),
    ("list-max-listpack-value", "64"),
    ("list-compress-depth", "0"),
    ("set-max-intset-entries", "512"),
    ("set-max-listpack-entries", "128"),
    ("hash-max-listpack-entries", "128"),
    ("hash-max-listpack-value", "64"),
    ("hash-max-ziplist-entries", "128"),
    ("hash-max-ziplist-value", "64"),
    ("zset-max-listpack-entries", "128"),
    ("zset-max-listpack-value", "64"),
    ("zset-max-ziplist-entries", "128"),
    ("zset-max-ziplist-value", "64"),
    ("stream-node-max-bytes", "4096"),
    ("stream-node-max-entries", "100"),
    // Protocol
    ("proto-max-bulk-len", "512000000"),
    ("close-on-oom", "no"),
    // I/O threads
    ("io-threads", "1"),
    ("io-threads-do-reads", "no"),
    // Memory allocator
    ("jemalloc-bg-thread", "yes"),
    // Misc
    ("rename-command", ""),
    ("enable-debug-command", "no"),
    ("hide-user-data-from-log", "no"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct AclUser {
    passwords: Vec<Vec<u8>>,
    enabled: bool,
    /// True when "nopass" rule is set (allow auth without password).
    nopass: bool,
    /// Per-command and per-category permission model.
    /// When `all_commands` is true, all commands are allowed unless individually denied.
    /// When `all_commands` is false, only individually allowed commands/categories are permitted.
    all_commands: bool,
    /// Explicitly allowed individual commands (lowercase).
    allowed_commands: HashSet<String>,
    /// Explicitly denied individual commands (lowercase).
    denied_commands: HashSet<String>,
    /// Explicitly allowed categories (lowercase).
    allowed_categories: HashSet<String>,
    /// Explicitly denied categories (lowercase).
    denied_categories: HashSet<String>,
    /// Key patterns. Empty means no keys allowed unless all_keys is true.
    all_keys: bool,
    /// Channel patterns. Empty means no channels allowed unless all_channels is true.
    all_channels: bool,
}

impl AclUser {
    fn new_default() -> Self {
        Self {
            passwords: Vec::new(),
            enabled: true,
            nopass: true, // default user allows passwordless access
            all_commands: true,
            allowed_commands: HashSet::new(),
            denied_commands: HashSet::new(),
            allowed_categories: HashSet::new(),
            denied_categories: HashSet::new(),
            all_keys: true,
            all_channels: true,
        }
    }

    fn check_password(&self, password: &[u8]) -> bool {
        if self.nopass {
            return true;
        }
        self.passwords.iter().any(|p| p.as_slice() == password)
    }

    /// Check if a specific command is allowed for this user.
    fn is_command_allowed(&self, cmd_name: &str) -> bool {
        let cmd_lower = cmd_name.to_ascii_lowercase();

        // Explicit per-command deny always wins.
        if self.denied_commands.contains(&cmd_lower) {
            return false;
        }

        // Explicit per-command allow always wins (after deny check).
        if self.allowed_commands.contains(&cmd_lower) {
            return true;
        }

        // Check category-level permissions.
        // Get the categories this command belongs to.
        let cmd_categories = command_categories(&cmd_lower);

        // If any denied category contains this command, deny it.
        for denied_cat in &self.denied_categories {
            if cmd_categories.iter().any(|c| c == denied_cat) {
                return false;
            }
        }

        // If we have allowed categories, check if any match.
        if !self.allowed_categories.is_empty() {
            for allowed_cat in &self.allowed_categories {
                if cmd_categories.iter().any(|c| c == allowed_cat) {
                    return true;
                }
            }
        }

        // Fall back to base permission.
        self.all_commands
    }

    fn commands_string(&self) -> String {
        if self.all_commands && self.denied_commands.is_empty() && self.denied_categories.is_empty()
        {
            return "+@all".to_string();
        }

        let mut parts: Vec<String> = Vec::new();

        if self.all_commands {
            parts.push("+@all".to_string());
        } else if self.allowed_categories.is_empty() && self.allowed_commands.is_empty() {
            parts.push("-@all".to_string());
        }

        // Add allowed categories.
        let mut sorted_cats: Vec<&String> = self.allowed_categories.iter().collect();
        sorted_cats.sort();
        for cat in sorted_cats {
            parts.push(format!("+@{cat}"));
        }

        // Add denied categories.
        let mut sorted_denied_cats: Vec<&String> = self.denied_categories.iter().collect();
        sorted_denied_cats.sort();
        for cat in sorted_denied_cats {
            parts.push(format!("-@{cat}"));
        }

        // Add allowed commands.
        let mut sorted_cmds: Vec<&String> = self.allowed_commands.iter().collect();
        sorted_cmds.sort();
        for cmd in sorted_cmds {
            parts.push(format!("+{cmd}"));
        }

        // Add denied commands.
        let mut sorted_denied: Vec<&String> = self.denied_commands.iter().collect();
        sorted_denied.sort();
        for cmd in sorted_denied {
            parts.push(format!("-{cmd}"));
        }

        if parts.is_empty() {
            "-@all".to_string()
        } else {
            parts.join(" ")
        }
    }

    fn acl_list_line(&self, username: &[u8]) -> String {
        let username_str = String::from_utf8_lossy(username);
        let on_off = if self.enabled { "on" } else { "off" };
        let pass_part = if self.nopass {
            " nopass".to_string()
        } else {
            self.passwords
                .iter()
                .map(|_| " #<hidden>".to_string())
                .collect::<String>()
        };
        let keys_part = if self.all_keys { " ~*" } else { "" };
        let channels_part = if self.all_channels { " &*" } else { "" };
        let commands_part = self.commands_string();
        format!("user {username_str} {on_off}{pass_part}{keys_part}{channels_part} {commands_part}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AuthState {
    requirepass: Option<Vec<u8>>,
    acl_users: BTreeMap<Vec<u8>, AclUser>,
}

impl Default for AuthState {
    fn default() -> Self {
        let mut acl_users = BTreeMap::new();
        acl_users.insert(DEFAULT_AUTH_USER.to_vec(), AclUser::new_default());
        Self {
            requirepass: None,
            acl_users,
        }
    }
}

impl AuthState {
    fn set_requirepass(&mut self, requirepass: Option<Vec<u8>>) {
        self.requirepass = requirepass.clone();
        let default_user = self
            .acl_users
            .entry(DEFAULT_AUTH_USER.to_vec())
            .or_insert_with(AclUser::new_default);
        if let Some(pass) = requirepass {
            default_user.passwords = vec![pass];
            default_user.nopass = false;
        } else {
            // Redis bridge behavior: empty requirepass maps back to default-user nopass.
            default_user.passwords.clear();
            default_user.nopass = true;
        }
    }

    fn add_user(&mut self, username: Vec<u8>, password: Vec<u8>) {
        let user = self
            .acl_users
            .entry(username)
            .or_insert_with(AclUser::new_default);
        user.passwords = vec![password];
        user.nopass = false;
    }

    fn auth_required(&self) -> bool {
        self.requirepass.is_some() || self.acl_users.values().any(|u| !u.passwords.is_empty())
    }

    fn requirepass(&self) -> Option<&[u8]> {
        self.requirepass.as_deref()
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
                user.nopass = true;
            } else if rule_str.eq_ignore_ascii_case("resetpass") {
                // resetpass clears all passwords AND clears nopass flag.
                // The user becomes unauthenticatable until a password is set.
                user.passwords.clear();
                user.nopass = false;
            } else if rule_str.eq_ignore_ascii_case("allcommands") || rule_str == "+@all" {
                user.all_commands = true;
                user.allowed_commands.clear();
                user.denied_commands.clear();
                user.allowed_categories.clear();
                user.denied_categories.clear();
            } else if rule_str.eq_ignore_ascii_case("allkeys") || rule_str == "~*" {
                user.all_keys = true;
            } else if rule_str.eq_ignore_ascii_case("allchannels") || rule_str == "&*" {
                user.all_channels = true;
            } else if rule_str == "-@all" || rule_str.eq_ignore_ascii_case("nocommands") {
                user.all_commands = false;
                user.allowed_commands.clear();
                user.denied_commands.clear();
                user.allowed_categories.clear();
                user.denied_categories.clear();
            } else if let Some(cat) = rule_str.strip_prefix("+@") {
                // +@category — allow all commands in this category.
                let cat_lower = cat.to_ascii_lowercase();
                user.allowed_categories.insert(cat_lower.clone());
                user.denied_categories.remove(&cat_lower);
            } else if let Some(cat) = rule_str.strip_prefix("-@") {
                // -@category — deny all commands in this category.
                let cat_lower = cat.to_ascii_lowercase();
                user.denied_categories.insert(cat_lower.clone());
                user.allowed_categories.remove(&cat_lower);
            } else if let Some(cmd) = rule_str.strip_prefix('+') {
                // +command — allow this specific command.
                let cmd_lower = cmd.to_ascii_lowercase();
                user.allowed_commands.insert(cmd_lower.clone());
                user.denied_commands.remove(&cmd_lower);
            } else if let Some(cmd) = rule_str.strip_prefix('-') {
                // -command — deny this specific command.
                let cmd_lower = cmd.to_ascii_lowercase();
                user.denied_commands.insert(cmd_lower.clone());
                user.allowed_commands.remove(&cmd_lower);
            } else if let Some(pass) = rule_str.strip_prefix('>') {
                user.passwords.push(pass.as_bytes().to_vec());
                user.nopass = false; // adding a password disables nopass
            } else if let Some(pass) = rule_str.strip_prefix('<') {
                user.passwords.retain(|p| p.as_slice() != pass.as_bytes());
            } else if rule_str.starts_with('~') {
                // Key pattern (e.g., ~user:*) — for now, accept but only ~* grants full access.
                if rule_str == "~*" {
                    user.all_keys = true;
                }
                // Non-wildcard key patterns accepted silently (future: store and enforce).
            } else if rule_str.starts_with('&') {
                // Channel pattern (e.g., &channel:*).
                if rule_str == "&*" {
                    user.all_channels = true;
                }
            } else if rule_str.eq_ignore_ascii_case("reset") {
                // Reset user to default state (no passwords, disabled, no commands).
                user.passwords.clear();
                user.nopass = false;
                user.all_commands = false;
                user.allowed_commands.clear();
                user.denied_commands.clear();
                user.allowed_categories.clear();
                user.denied_categories.clear();
                user.all_keys = false;
                user.all_channels = false;
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

    fn can_authenticate_as(&self, username: &[u8]) -> bool {
        self.acl_users
            .get(username)
            .is_some_and(|user| user.enabled)
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
    Role,
    Replicaof,
    Slaveof,
    Sync,
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
    Pubsub,
    Subscribe,
    Unsubscribe,
    Psubscribe,
    Punsubscribe,
    Publish,
    Ssubscribe,
    Sunsubscribe,
    Spublish,
    Replconf,
    Psync,
    Select,
    Swapdb,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClusterSubcommand {
    Help,
    Dispatch,
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
            } else if eq_ascii_token(cmd, b"ROLE") {
                Some(RuntimeSpecialCommand::Role)
            } else if eq_ascii_token(cmd, b"SYNC") {
                Some(RuntimeSpecialCommand::Sync)
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
            } else if eq_ascii_token(cmd, b"PSYNC") {
                Some(RuntimeSpecialCommand::Psync)
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
            } else if eq_ascii_token(cmd, b"PUBSUB") {
                Some(RuntimeSpecialCommand::Pubsub)
            } else if eq_ascii_token(cmd, b"BGSAVE") {
                Some(RuntimeSpecialCommand::Bgsave)
            } else if eq_ascii_token(cmd, b"SELECT") {
                Some(RuntimeSpecialCommand::Select)
            } else if eq_ascii_token(cmd, b"SWAPDB") {
                Some(RuntimeSpecialCommand::Swapdb)
            } else {
                None
            }
        }
        7 => {
            if eq_ascii_token(cmd, b"CLUSTER") {
                Some(RuntimeSpecialCommand::Cluster)
            } else if eq_ascii_token(cmd, b"SLAVEOF") {
                Some(RuntimeSpecialCommand::Slaveof)
            } else if eq_ascii_token(cmd, b"WAITAOF") {
                Some(RuntimeSpecialCommand::Waitaof)
            } else if eq_ascii_token(cmd, b"DISCARD") {
                Some(RuntimeSpecialCommand::Discard)
            } else if eq_ascii_token(cmd, b"UNWATCH") {
                Some(RuntimeSpecialCommand::Unwatch)
            } else if eq_ascii_token(cmd, b"SLOWLOG") {
                Some(RuntimeSpecialCommand::Slowlog)
            } else if eq_ascii_token(cmd, b"PUBLISH") {
                Some(RuntimeSpecialCommand::Publish)
            } else {
                None
            }
        }
        8 => {
            if eq_ascii_token(cmd, b"READONLY") {
                Some(RuntimeSpecialCommand::Readonly)
            } else if eq_ascii_token(cmd, b"LASTSAVE") {
                Some(RuntimeSpecialCommand::Lastsave)
            } else if eq_ascii_token(cmd, b"REPLCONF") {
                Some(RuntimeSpecialCommand::Replconf)
            } else if eq_ascii_token(cmd, b"SHUTDOWN") {
                Some(RuntimeSpecialCommand::Shutdown)
            } else if eq_ascii_token(cmd, b"SPUBLISH") {
                Some(RuntimeSpecialCommand::Spublish)
            } else {
                None
            }
        }
        9 => {
            if eq_ascii_token(cmd, b"READWRITE") {
                Some(RuntimeSpecialCommand::Readwrite)
            } else if eq_ascii_token(cmd, b"REPLICAOF") {
                Some(RuntimeSpecialCommand::Replicaof)
            } else if eq_ascii_token(cmd, b"SUBSCRIBE") {
                Some(RuntimeSpecialCommand::Subscribe)
            } else {
                None
            }
        }
        10 => {
            if eq_ascii_token(cmd, b"PSUBSCRIBE") {
                Some(RuntimeSpecialCommand::Psubscribe)
            } else if eq_ascii_token(cmd, b"SSUBSCRIBE") {
                Some(RuntimeSpecialCommand::Ssubscribe)
            } else {
                None
            }
        }
        11 => {
            if eq_ascii_token(cmd, b"UNSUBSCRIBE") {
                Some(RuntimeSpecialCommand::Unsubscribe)
            } else {
                None
            }
        }
        12 => {
            if eq_ascii_token(cmd, b"BGREWRITEAOF") {
                Some(RuntimeSpecialCommand::Bgrewriteaof)
            } else if eq_ascii_token(cmd, b"PUNSUBSCRIBE") {
                Some(RuntimeSpecialCommand::Punsubscribe)
            } else if eq_ascii_token(cmd, b"SUNSUBSCRIBE") {
                Some(RuntimeSpecialCommand::Sunsubscribe)
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
    } else if command.eq_ignore_ascii_case("REPLCONF") {
        Some(RuntimeSpecialCommand::Replconf)
    } else if command.eq_ignore_ascii_case("PSYNC") {
        Some(RuntimeSpecialCommand::Psync)
    } else if command.eq_ignore_ascii_case("HELLO") {
        Some(RuntimeSpecialCommand::Hello)
    } else if command.eq_ignore_ascii_case("ROLE") {
        Some(RuntimeSpecialCommand::Role)
    } else if command.eq_ignore_ascii_case("REPLICAOF") {
        Some(RuntimeSpecialCommand::Replicaof)
    } else if command.eq_ignore_ascii_case("SLAVEOF") {
        Some(RuntimeSpecialCommand::Slaveof)
    } else if command.eq_ignore_ascii_case("SYNC") {
        Some(RuntimeSpecialCommand::Sync)
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
    } else if command.eq_ignore_ascii_case("SUBSCRIBE") {
        Some(RuntimeSpecialCommand::Subscribe)
    } else if command.eq_ignore_ascii_case("UNSUBSCRIBE") {
        Some(RuntimeSpecialCommand::Unsubscribe)
    } else if command.eq_ignore_ascii_case("PSUBSCRIBE") {
        Some(RuntimeSpecialCommand::Psubscribe)
    } else if command.eq_ignore_ascii_case("PUNSUBSCRIBE") {
        Some(RuntimeSpecialCommand::Punsubscribe)
    } else if command.eq_ignore_ascii_case("PUBLISH") {
        Some(RuntimeSpecialCommand::Publish)
    } else if command.eq_ignore_ascii_case("SSUBSCRIBE") {
        Some(RuntimeSpecialCommand::Ssubscribe)
    } else if command.eq_ignore_ascii_case("SUNSUBSCRIBE") {
        Some(RuntimeSpecialCommand::Sunsubscribe)
    } else if command.eq_ignore_ascii_case("SPUBLISH") {
        Some(RuntimeSpecialCommand::Spublish)
    } else if command.eq_ignore_ascii_case("SELECT") {
        Some(RuntimeSpecialCommand::Select)
    } else if command.eq_ignore_ascii_case("SWAPDB") {
        Some(RuntimeSpecialCommand::Swapdb)
    } else {
        None
    }
}

#[inline]
fn classify_cluster_subcommand(cmd: &[u8]) -> Result<ClusterSubcommand, CommandError> {
    if cmd.len() == 4 && eq_ascii_token(cmd, b"HELP") {
        return Ok(ClusterSubcommand::Help);
    }
    if (cmd.len() == 4 && (eq_ascii_token(cmd, b"INFO") || eq_ascii_token(cmd, b"MYID")))
        || (cmd.len() == 5
            && (eq_ascii_token(cmd, b"SLOTS")
                || eq_ascii_token(cmd, b"NODES")
                || eq_ascii_token(cmd, b"RESET")))
        || (cmd.len() == 6 && eq_ascii_token(cmd, b"SHARDS"))
        || (cmd.len() == 7 && eq_ascii_token(cmd, b"KEYSLOT"))
        || (cmd.len() == 13 && eq_ascii_token(cmd, b"GETKEYSINSLOT"))
        || (cmd.len() == 15 && eq_ascii_token(cmd, b"COUNTKEYSINSLOT"))
    {
        return Ok(ClusterSubcommand::Dispatch);
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
    } else if subcommand.eq_ignore_ascii_case("INFO")
        || subcommand.eq_ignore_ascii_case("MYID")
        || subcommand.eq_ignore_ascii_case("SLOTS")
        || subcommand.eq_ignore_ascii_case("SHARDS")
        || subcommand.eq_ignore_ascii_case("NODES")
        || subcommand.eq_ignore_ascii_case("KEYSLOT")
        || subcommand.eq_ignore_ascii_case("GETKEYSINSLOT")
        || subcommand.eq_ignore_ascii_case("COUNTKEYSINSLOT")
        || subcommand.eq_ignore_ascii_case("RESET")
    {
        Ok(ClusterSubcommand::Dispatch)
    } else {
        Ok(ClusterSubcommand::Unknown)
    }
}

#[inline]
fn eq_ascii_token(lhs: &[u8], rhs: &[u8]) -> bool {
    lhs.eq_ignore_ascii_case(rhs)
}

#[allow(dead_code)]
fn increment_run_id_hex(current: &str) -> String {
    let parsed = u128::from_str_radix(current, 16).unwrap_or(0);
    format!("{:040x}", parsed.saturating_add(1))
}

#[inline]
fn parse_db_index_arg(
    arg: &[u8],
    out_of_range_message: &'static str,
    database_count: usize,
) -> Result<usize, RespFrame> {
    let parsed = std::str::from_utf8(arg)
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .ok_or_else(|| {
            RespFrame::Error("ERR value is not an integer or out of range".to_string())
        })?;
    if (0..database_count as i64).contains(&parsed) {
        Ok(parsed as usize)
    } else {
        Err(RespFrame::Error(out_of_range_message.to_string()))
    }
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ReplicaState {
    ack_offset: ReplOffset,
    fsync_offset: ReplOffset,
    listening_port: u16,
    ip_address: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum ReplicationRoleState {
    Master,
    Replica {
        host: String,
        port: u16,
        state: &'static str,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReplicationRuntimeState {
    role: ReplicationRoleState,
    backlog: BacklogWindow,
    replicas: BTreeMap<u64, ReplicaState>,
}

impl ReplicationRuntimeState {
    fn new(replid: String) -> Self {
        Self {
            role: ReplicationRoleState::Master,
            backlog: BacklogWindow {
                replid,
                start_offset: ReplOffset(0),
                end_offset: ReplOffset(0),
            },
            replicas: BTreeMap::new(),
        }
    }

    fn ensure_replica(&mut self, client_id: u64) -> &mut ReplicaState {
        self.replicas.entry(client_id).or_default()
    }

    fn update_backlog_window(&mut self, end_offset: ReplOffset, backlog_size: u64) {
        let start = if end_offset.0 == 0 {
            ReplOffset(0)
        } else if end_offset.0 >= backlog_size {
            ReplOffset(end_offset.0 - backlog_size + 1)
        } else {
            ReplOffset(1)
        };
        self.backlog.start_offset = start;
        self.backlog.end_offset = end_offset;
    }

    fn replica_ack_offsets(&self) -> Vec<ReplOffset> {
        self.replicas
            .values()
            .map(|replica| replica.ack_offset)
            .collect()
    }

    fn replica_fsync_offsets(&self) -> Vec<ReplOffset> {
        self.replicas
            .values()
            .map(|replica| replica.fsync_offset)
            .collect()
    }

    fn backlog_histlen(&self) -> u64 {
        self.backlog
            .end_offset
            .0
            .saturating_sub(self.backlog.start_offset.0)
            .saturating_add(u64::from(self.backlog.end_offset.0 > 0))
    }

    #[allow(dead_code)]
    fn rotate_backlog_identity(&mut self) {
        let next_replid = increment_run_id_hex(&self.backlog.replid);
        let current_offset = self.backlog.end_offset;
        self.backlog
            .rotate(next_replid, current_offset, current_offset);
    }
}

#[derive(Debug, Clone, Default)]
struct TransactionState {
    in_transaction: bool,
    command_queue: Vec<Vec<Vec<u8>>>,
    /// (key, fingerprint, dirty_counter_at_watch_time)
    watched_keys: Vec<(Vec<u8>, u64, u64)>,
    watch_dirty: bool,
    exec_abort: bool,
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

/// State that belongs to the long-lived server process rather than a client.
#[derive(Debug)]
pub struct ServerState {
    store: Store,
    aof_records: Vec<AofRecord>,
    aof_selected_db: usize,
    replication_runtime_state: ReplicationRuntimeState,
    evidence: EvidenceLedger,
    auth_state: AuthState,
    tls_state: TlsRuntimeState,
    acllog_max_len: i64,
    replication_ack_state: ReplicationAckState,
    maxmemory_bytes: usize,
    maxmemory_not_counted_bytes: usize,
    maxmemory_eviction_sample_limit: usize,
    maxmemory_eviction_max_cycles: usize,
    eviction_safety_gate: EvictionSafetyGateState,
    last_eviction_loop: Option<EvictionLoopResult>,
    active_expire_db_cursor: usize,
    active_expire_key_cursor: Option<Vec<u8>>,
    active_expire_budget: ActiveExpireCycleBudget,
    last_active_expire_cycle: Option<ActiveExpireCycleStats>,
    /// Server hz (timer interrupt frequency).
    hz: u64,
    /// Maximum number of connected clients (CONFIG SET maxclients).
    pub max_clients: usize,
    /// Replication backlog size in bytes (CONFIG SET repl-backlog-size). Default 1 MiB.
    pub repl_backlog_size: u64,
    /// Replication timeout in seconds (CONFIG SET repl-timeout). Default 60.
    pub repl_timeout_sec: u64,
    /// Client output buffer hard limit (CONFIG SET client-output-buffer-limit). Default 256 MiB.
    pub output_buffer_limit: usize,
    /// Client query buffer limit (CONFIG SET client-query-buffer-limit). Default 1 GiB.
    pub query_buffer_limit: usize,
    /// Maximum bulk string length in RESP protocol (CONFIG SET proto-max-bulk-len). Default 512 MiB.
    pub proto_max_bulk_len: usize,
    /// Set to true when SHUTDOWN is requested. Server event loop checks this.
    pub shutdown_requested: bool,
    /// If true, skip the final SAVE on shutdown.
    pub shutdown_nosave: bool,
    /// Command time budget in ms (CONFIG SET busy-reply-threshold / lua-time-limit).
    command_time_budget_ms: u64,
    /// Slow log: ring buffer of slow queries.
    slowlog: std::collections::VecDeque<SlowlogEntry>,
    /// Slow log entry ID counter.
    slowlog_next_id: u64,
    /// Slow log threshold in microseconds (slowlog-log-slower-than config).
    slowlog_log_slower_than_us: i64,
    /// Slow log maximum length.
    slowlog_max_len: usize,
    /// Last successful save timestamp (seconds since epoch).
    last_save_time_sec: u64,
    /// Path for AOF persistence file (used by SAVE/BGSAVE).
    aof_path: Option<std::path::PathBuf>,
    /// Configured AOF target path, preserved even when appendonly is disabled.
    aof_config_path: Option<std::path::PathBuf>,
    /// Path for RDB persistence file (used by SAVE/BGSAVE).
    rdb_path: Option<std::path::PathBuf>,
    /// Dynamically overridden CONFIG parameters (set via CONFIG SET, returned by CONFIG GET).
    config_overrides: HashMap<String, String>,

    // ── Pub/Sub global state ────────────────────────────────────────
    /// Channel → set of subscribed client IDs.
    pubsub_channel_subs: HashMap<Vec<u8>, HashSet<u64>>,
    /// Pattern → set of subscribed client IDs.
    pubsub_pattern_subs: HashMap<Vec<u8>, HashSet<u64>>,
    /// Shard channel → set of subscribed client IDs.
    pubsub_shard_subs: HashMap<Vec<u8>, HashSet<u64>>,
    /// Per-client outbox: client_id → pending messages for delivery.
    pubsub_outbox: HashMap<u64, Vec<fr_store::PubSubMessage>>,
    /// Inverse mapping: client_id → set of channels they are subscribed to.
    pubsub_client_channels: HashMap<u64, HashSet<Vec<u8>>>,
    /// Inverse mapping: client_id → set of patterns they are subscribed to.
    pubsub_client_patterns: HashMap<u64, HashSet<Vec<u8>>>,
    /// Inverse mapping: client_id → set of shard channels they are subscribed to.
    pubsub_client_shard_channels: HashMap<u64, HashSet<Vec<u8>>>,
    /// Keys that were modified in the current tick and may unblock clients.
    pub ready_keys: HashSet<Vec<u8>>,
    /// Set of client IDs that are in MONITOR mode.
    pub monitor_clients: HashSet<u64>,
    /// Pending monitor output lines to deliver to monitor clients.
    pub monitor_output: Vec<(u64, Vec<u8>)>,
    /// CLIENT PAUSE: deadline in ms when pause expires. 0 = not paused.
    pub client_pause_deadline_ms: u64,
    /// CLIENT PAUSE mode: true = ALL (block all commands), false = WRITE only.
    pub client_pause_all: bool,
}

impl Default for ServerState {
    fn default() -> Self {
        let store = Store::new();
        Self {
            replication_runtime_state: ReplicationRuntimeState::new(
                "0000000000000000000000000000000000000000".to_string(),
            ),
            store,
            aof_records: Vec::new(),
            aof_selected_db: 0,
            evidence: EvidenceLedger::default(),
            auth_state: AuthState::default(),
            tls_state: TlsRuntimeState::default(),
            acllog_max_len: DEFAULT_ACLLOG_MAX_LEN,
            replication_ack_state: ReplicationAckState::default(),
            maxmemory_bytes: 0,
            maxmemory_not_counted_bytes: 0,
            maxmemory_eviction_sample_limit: 5,
            maxmemory_eviction_max_cycles: 4,
            eviction_safety_gate: EvictionSafetyGateState::default(),
            last_eviction_loop: None,
            active_expire_db_cursor: 0,
            active_expire_key_cursor: None,
            active_expire_budget: ActiveExpireCycleBudget::default(),
            last_active_expire_cycle: None,
            hz: 10,
            max_clients: 10_000,
            repl_backlog_size: DEFAULT_REPL_BACKLOG_SIZE,
            repl_timeout_sec: 60,
            output_buffer_limit: 256 * 1024 * 1024, // 256 MiB (reasonable default)
            query_buffer_limit: 1024 * 1024 * 1024, // 1 GiB (Redis default)
            proto_max_bulk_len: 512_000_000,        // Redis default (512 MB, not 512 MiB)
            shutdown_requested: false,
            shutdown_nosave: false,
            command_time_budget_ms: 5000,
            slowlog: std::collections::VecDeque::new(),
            slowlog_next_id: 0,
            slowlog_log_slower_than_us: 10_000,
            slowlog_max_len: 128,
            last_save_time_sec: 0,
            aof_path: None,
            aof_config_path: None,
            rdb_path: None,
            config_overrides: HashMap::new(),
            pubsub_channel_subs: HashMap::new(),
            pubsub_pattern_subs: HashMap::new(),
            pubsub_shard_subs: HashMap::new(),
            pubsub_outbox: HashMap::new(),
            pubsub_client_channels: HashMap::new(),
            pubsub_client_patterns: HashMap::new(),
            pubsub_client_shard_channels: HashMap::new(),
            ready_keys: HashSet::new(),
            monitor_clients: HashSet::new(),
            monitor_output: Vec::new(),
            client_pause_deadline_ms: 0,
            client_pause_all: false,
        }
    }
}

impl ServerState {
    pub fn set_aof_path(&mut self, path: std::path::PathBuf) {
        self.aof_config_path = Some(path.clone());
        self.aof_path = Some(path);
    }

    pub fn set_rdb_path(&mut self, path: std::path::PathBuf) {
        self.rdb_path = Some(path);
    }

    pub fn load_aof(&mut self, now_ms: u64) -> Result<usize, PersistError> {
        let path = match &self.aof_path {
            Some(path) => path.clone(),
            None => return Ok(0),
        };
        let records = fr_persist::read_aof_file(&path)?;
        let count = records.len();
        let mut replayed_store = Store::new();
        for (index, record) in records.iter().enumerate() {
            let replay_now_ms = now_ms.saturating_add(index as u64);
            dispatch_argv(&record.argv, &mut replayed_store, replay_now_ms)
                .map_err(|_| PersistError::InvalidFrame)?;
        }
        self.store = replayed_store;
        self.aof_records = records;
        Ok(count)
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
            self.active_expire_key_cursor.clone(),
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
    pub fn last_active_expire_cycle_stats(&self) -> Option<ActiveExpireCycleStats> {
        self.last_active_expire_cycle
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
        self.replication_runtime_state.replicas.clear();
        let replica_count = replica_ack_offsets.len().max(replica_fsync_offsets.len());
        for index in 0..replica_count {
            let replica = self
                .replication_runtime_state
                .replicas
                .entry(index as u64)
                .or_default();
            replica.ack_offset = ReplOffset(*replica_ack_offsets.get(index).unwrap_or(&0));
            replica.fsync_offset = ReplOffset(*replica_fsync_offsets.get(index).unwrap_or(&0));
        }
        self.refresh_replica_ack_snapshots();
    }

    fn refresh_replica_ack_snapshots(&mut self) {
        self.replication_ack_state.replica_ack_offsets =
            self.replication_runtime_state.replica_ack_offsets();
        self.replication_ack_state.replica_fsync_offsets =
            self.replication_runtime_state.replica_fsync_offsets();
    }

    fn reset_slowlog(&mut self) {
        self.slowlog.clear();
        self.slowlog_next_id = 0;
    }

    fn record_slowlog(&mut self, argv: &[Vec<u8>], duration_us: u64, now_ms: u64) {
        if self.slowlog_log_slower_than_us < 0 {
            return;
        }
        if (duration_us as i64) < self.slowlog_log_slower_than_us {
            return;
        }
        let entry = SlowlogEntry {
            id: self.slowlog_next_id,
            timestamp_sec: now_ms / 1000,
            duration_us,
            argv: argv.to_vec(),
        };
        self.slowlog_next_id += 1;
        self.slowlog.push_back(entry);
        while self.slowlog.len() > self.slowlog_max_len {
            self.slowlog.pop_front();
        }
    }

    fn capture_aof_record(&mut self, argv: &[Vec<u8>]) {
        if !Runtime::command_advances_replication_offset(argv) {
            return;
        }
        let record = AofRecord {
            argv: argv.to_vec(),
        };
        let encoded_len =
            u64::try_from(record.to_resp_frame().to_bytes().len()).unwrap_or(u64::MAX);
        self.aof_records.push(record);
        self.replication_ack_state.primary_offset.0 = self
            .replication_ack_state
            .primary_offset
            .0
            .saturating_add(encoded_len);
        self.replication_ack_state.local_fsync_offset = self.replication_ack_state.primary_offset;
        self.replication_runtime_state.update_backlog_window(
            self.replication_ack_state.primary_offset,
            self.repl_backlog_size,
        );
    }
}

/// State that is clearly scoped to a single client session.
#[derive(Debug)]
pub struct ClientSession {
    cluster_state: ClusterClientState,
    transaction_state: TransactionState,
    authenticated_user: Option<Vec<u8>>,
    /// Per-client selected database. SELECT support lands later, but the state
    /// must already be session-scoped before multiple concurrent clients exist.
    selected_db: usize,
    /// RESP protocol negotiated via HELLO. Redis defaults new connections to RESP2.
    resp_protocol_version: i64,
    /// Per-client connection ID (monotonically increasing).
    pub client_id: u64,
    /// Per-client name set via CLIENT SETNAME.
    client_name: Option<Vec<u8>>,
    /// Client library name set via CLIENT SETINFO LIB-NAME (Redis 7.2+).
    client_lib_name: Option<String>,
    /// Client library version set via CLIENT SETINFO LIB-VER (Redis 7.2+).
    client_lib_ver: Option<String>,
    /// Flags: client no-evict mode.
    client_no_evict: bool,
    /// Flags: client no-touch mode.
    client_no_touch: bool,
    /// Client peer address (set on connection accept).
    pub peer_addr: Option<std::net::SocketAddr>,
}

impl Default for ClientSession {
    fn default() -> Self {
        Self {
            cluster_state: ClusterClientState::default(),
            transaction_state: TransactionState::default(),
            authenticated_user: None,
            selected_db: 0,
            resp_protocol_version: 2,
            client_id: CLIENT_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
            client_name: None,
            client_lib_name: None,
            client_lib_ver: None,
            client_no_evict: false,
            client_no_touch: false,
            peer_addr: None,
        }
    }
}

impl ClientSession {
    pub fn new_for_server(server: &ServerState) -> Self {
        let mut session = Self::default();
        session.refresh_authentication_for_server(&server.auth_state, false);
        session
    }

    fn is_authenticated(&self) -> bool {
        self.authenticated_user.is_some()
    }

    fn requires_auth(&self, auth_state: &AuthState) -> bool {
        auth_state.auth_required() && !self.is_authenticated()
    }

    fn current_user_name(&self) -> &[u8] {
        self.authenticated_user
            .as_deref()
            .unwrap_or(DEFAULT_AUTH_USER)
    }

    fn refresh_authentication_for_server(
        &mut self,
        auth_state: &AuthState,
        preserve_authenticated_user: bool,
    ) {
        let previous_authenticated_user = preserve_authenticated_user
            .then(|| self.authenticated_user.clone())
            .flatten();
        if !auth_state.auth_required() {
            self.authenticated_user = Some(DEFAULT_AUTH_USER.to_vec());
        } else if let Some(username) = previous_authenticated_user
            && auth_state.can_authenticate_as(&username)
        {
            self.authenticated_user = Some(username);
        } else {
            self.authenticated_user = None;
        }
    }

    fn reset_connection_state(&mut self, auth_state: &AuthState) {
        self.cluster_state = ClusterClientState::default();
        self.transaction_state = TransactionState::default();
        self.selected_db = 0;
        self.resp_protocol_version = 2;
        self.client_name = None;
        self.client_lib_name = None;
        self.client_lib_ver = None;
        self.client_no_evict = false;
        self.client_no_touch = false;
        self.refresh_authentication_for_server(auth_state, false);
    }
}

#[derive(Debug)]
pub struct Runtime {
    policy: RuntimePolicy,
    pub server: ServerState,
    session: ClientSession,
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

const MAX_COMMAND_ARITY: usize = 1024 * 1024;

impl Runtime {
    #[must_use]
    pub fn new(policy: RuntimePolicy) -> Self {
        let server = ServerState::default();
        let session = ClientSession::new_for_server(&server);
        Self {
            policy,
            server,
            session,
        }
    }

    /// Set the AOF persistence file path. When set, SAVE/BGSAVE will write
    /// a full AOF rewrite to this path.
    pub fn set_aof_path(&mut self, path: std::path::PathBuf) {
        self.server.set_aof_path(path);
    }

    /// Set the RDB persistence file path. When set, SAVE/BGSAVE will write
    /// an RDB snapshot to this path.
    pub fn set_rdb_path(&mut self, path: std::path::PathBuf) {
        self.server.set_rdb_path(path);
    }

    /// Set the server listen port (for INFO server section).
    pub fn set_server_port(&mut self, port: u16) {
        self.server.store.server_port = port;
    }

    #[must_use]
    pub fn server_port(&self) -> u16 {
        self.server.store.server_port
    }

    /// Load and replay AOF records from the configured path, restoring store state.
    ///
    /// Each AOF record is dispatched through the command router as if it were
    /// a client command. Returns the number of records replayed, or an error.
    pub fn load_aof(&mut self, now_ms: u64) -> Result<usize, PersistError> {
        let path = match &self.server.aof_path {
            Some(path) => path.clone(),
            None => return Ok(0),
        };
        let records = fr_persist::read_aof_file(&path)?;
        let count = records.len();
        let original_store = std::mem::replace(&mut self.server.store, Store::new());
        let original_records = std::mem::take(&mut self.server.aof_records);
        let original_aof_db = self.server.aof_selected_db;
        let original_db = self.session.selected_db;

        self.server.aof_selected_db = 0;
        self.session.selected_db = 0;
        for (index, record) in records.iter().enumerate() {
            let replay_now_ms = now_ms.saturating_add(index as u64);
            let reply = self.execute_frame(record.to_resp_frame(), replay_now_ms);
            if matches!(reply, RespFrame::Error(_)) {
                self.server.store = original_store;
                self.server.aof_records = original_records;
                self.server.aof_selected_db = original_aof_db;
                self.session.selected_db = original_db;
                return Err(PersistError::InvalidFrame);
            }
        }

        self.server.aof_records = records;
        self.server.aof_selected_db = self.session.selected_db;
        self.session.selected_db = original_db;
        Ok(count)
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
        self.server.run_active_expire_cycle(now_ms, cycle_kind)
    }

    #[must_use]
    pub fn run_server_cron_active_expire_cycle(&mut self, now_ms: u64) -> ActiveExpireCycleStats {
        self.run_active_expire_cycle(now_ms, ActiveExpireCycleKind::Slow)
    }

    #[must_use]
    pub fn last_active_expire_cycle_stats(&self) -> Option<ActiveExpireCycleStats> {
        self.server.last_active_expire_cycle_stats()
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
        &self.server.evidence
    }

    #[must_use]
    pub fn aof_records(&self) -> &[AofRecord] {
        &self.server.aof_records
    }

    #[must_use]
    pub fn encoded_aof_stream(&self) -> Vec<u8> {
        encode_aof_stream(&self.server.aof_records)
    }

    #[must_use]
    pub fn encoded_aof_stream_from_offset(&self, offset: u64) -> Vec<u8> {
        let stream = self.encoded_aof_stream();
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        stream.get(start..).unwrap_or(&[]).to_vec()
    }

    #[must_use]
    pub fn encoded_rdb_snapshot(&mut self, now_ms: u64) -> Vec<u8> {
        let entries = store_to_rdb_entries(&mut self.server.store, now_ms);
        encode_rdb(
            &entries,
            &[
                ("redis-ver", fr_store::REDIS_COMPAT_VERSION),
                ("frankenredis", "true"),
            ],
        )
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

    pub fn apply_replication_sync_payload(
        &mut self,
        reply_line: &str,
        payload: &[u8],
        now_ms: u64,
    ) -> Result<(), PersistError> {
        match parse_psync_reply(reply_line).map_err(|_| PersistError::InvalidFrame)? {
            PsyncReply::Continue => {
                self.replay_aof_stream(payload, now_ms)?;
            }
            PsyncReply::FullResync { replid, offset } => {
                let (entries, _aux) = decode_rdb(payload)?;
                let mut store = Store::new();
                apply_rdb_entries_to_store(&mut store, &entries, now_ms)?;
                self.server.store = store;
                self.server.aof_records.clear();
                self.server.aof_selected_db = 0;
                self.session.selected_db = 0;
                self.server
                    .replication_runtime_state
                    .backlog
                    .rotate(replid, offset, offset);
                self.server.replication_ack_state.primary_offset = offset;
                self.server.replication_ack_state.local_fsync_offset = offset;
            }
        }
        if let ReplicationRoleState::Replica { ref mut state, .. } =
            self.server.replication_runtime_state.role
        {
            *state = "connected";
        }
        self.server.refresh_replica_ack_snapshots();
        Ok(())
    }

    #[must_use]
    pub fn replica_sync_target(&self) -> Option<(String, u16)> {
        match &self.server.replication_runtime_state.role {
            ReplicationRoleState::Replica { host, port, state } if *state != "connected" => {
                Some((host.clone(), *port))
            }
            _ => None,
        }
    }

    #[must_use]
    pub fn replica_psync_request(&self) -> Option<(String, i64)> {
        let ReplicationRoleState::Replica { state, .. } =
            &self.server.replication_runtime_state.role
        else {
            return None;
        };
        if *state == "reconnect" {
            return Some((
                self.server.replication_runtime_state.backlog.replid.clone(),
                i64::try_from(self.server.replication_ack_state.primary_offset.0)
                    .unwrap_or(i64::MAX),
            ));
        }
        Some(("?".to_string(), -1))
    }

    pub fn set_replica_connection_state(&mut self, state: &'static str) {
        if let ReplicationRoleState::Replica {
            state: role_state, ..
        } = &mut self.server.replication_runtime_state.role
        {
            *role_state = state;
        }
    }

    #[must_use]
    pub fn replication_primary_offset(&self) -> ReplOffset {
        self.server.replication_ack_state.primary_offset
    }

    #[must_use]
    pub fn is_replica(&self, client_id: u64) -> bool {
        self.server
            .replication_runtime_state
            .replicas
            .contains_key(&client_id)
    }

    #[must_use]
    pub fn replica_ack_frame(&self) -> Option<RespFrame> {
        let ReplicationRoleState::Replica { .. } = &self.server.replication_runtime_state.role
        else {
            return None;
        };
        Some(RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"REPLCONF".to_vec())),
            RespFrame::BulkString(Some(b"ACK".to_vec())),
            RespFrame::BulkString(Some(
                self.server
                    .replication_ack_state
                    .primary_offset
                    .0
                    .to_string()
                    .into_bytes(),
            )),
        ])))
    }

    #[must_use]
    pub fn tls_runtime_state(&self) -> &TlsRuntimeState {
        self.server.tls_runtime_state()
    }

    #[must_use]
    pub fn server_state(&self) -> &ServerState {
        &self.server
    }

    #[must_use]
    pub fn server_state_mut(&mut self) -> &mut ServerState {
        &mut self.server
    }

    #[must_use]
    pub fn client_session(&self) -> &ClientSession {
        &self.session
    }

    /// Create a new `ClientSession` for this runtime's server state.
    /// Used by multi-client servers to create per-connection sessions.
    #[must_use]
    pub fn new_session(&self) -> ClientSession {
        ClientSession::new_for_server(&self.server)
    }

    /// Swap the active client session, returning the previous one.
    /// Used by multi-client servers to switch between per-connection sessions
    /// before executing commands.
    pub fn swap_session(&mut self, mut session: ClientSession) -> ClientSession {
        // Ensure the new session is correctly associated with this server's state.
        session.refresh_authentication_for_server(&self.server.auth_state, true);
        std::mem::replace(&mut self.session, session)
    }

    /// Track a new client connection for INFO stats.
    pub fn track_connection_opened(&mut self) {
        self.server.store.stat_total_connections_received += 1;
        self.server.store.stat_connected_clients += 1;
    }

    /// Track a client disconnection for INFO stats.
    pub fn track_connection_closed(&mut self) {
        self.server.store.stat_connected_clients =
            self.server.store.stat_connected_clients.saturating_sub(1);
    }

    /// Drain pending pub/sub messages for the current session's client.
    pub fn drain_pending_pubsub(&mut self) -> Vec<fr_store::PubSubMessage> {
        // First drain any messages from the per-client Store (legacy path for
        // single-session tests), then drain from the global outbox.
        let mut msgs = self.server.store.drain_pending_pubsub();
        let client_id = self.session.client_id;
        if let Some(outbox) = self.server.pubsub_outbox.remove(&client_id) {
            msgs.extend(outbox);
        }
        msgs
    }

    /// Drain pending pub/sub messages for a specific client by ID.
    /// Used by the server event loop to deliver messages to non-active clients.
    pub fn drain_pubsub_for_client(&mut self, client_id: u64) -> Vec<fr_store::PubSubMessage> {
        self.server
            .pubsub_outbox
            .remove(&client_id)
            .unwrap_or_default()
    }

    /// Return all client IDs that have pending pub/sub messages.
    pub fn pubsub_clients_with_pending(&self) -> HashSet<u64> {
        self.server
            .pubsub_outbox
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(&id, _)| id)
            .collect()
    }

    // ── Pub/Sub global registry operations ──────────────────────────

    /// Subscribe the current client to a channel. Returns total subscription count.
    pub fn pubsub_subscribe(&mut self, channel: Vec<u8>) -> usize {
        let client_id = self.session.client_id;
        self.server.store.subscribe(channel.clone());
        self.server
            .pubsub_channel_subs
            .entry(channel.clone())
            .or_default()
            .insert(client_id);
        self.server
            .pubsub_client_channels
            .entry(client_id)
            .or_default()
            .insert(channel);
        self.pubsub_sub_count(client_id)
    }

    /// Unsubscribe the current client from a channel. Returns total subscription count.
    pub fn pubsub_unsubscribe(&mut self, channel: &[u8]) -> usize {
        let client_id = self.session.client_id;
        if let Some(subs) = self.server.pubsub_channel_subs.get_mut(channel) {
            subs.remove(&client_id);
            if subs.is_empty() {
                self.server.pubsub_channel_subs.remove(channel);
            }
        }
        if let Some(channels) = self.server.pubsub_client_channels.get_mut(&client_id) {
            channels.remove(channel);
            if channels.is_empty() {
                self.server.pubsub_client_channels.remove(&client_id);
            }
        }
        if !self.server.pubsub_channel_subs.contains_key(channel) {
            self.server.store.unsubscribe(channel);
        }
        self.pubsub_sub_count(client_id)
    }

    /// Subscribe the current client to a pattern. Returns total subscription count.
    pub fn pubsub_psubscribe(&mut self, pattern: Vec<u8>) -> usize {
        let client_id = self.session.client_id;
        self.server.store.psubscribe(pattern.clone());
        self.server
            .pubsub_pattern_subs
            .entry(pattern.clone())
            .or_default()
            .insert(client_id);
        self.server
            .pubsub_client_patterns
            .entry(client_id)
            .or_default()
            .insert(pattern);
        self.pubsub_sub_count(client_id)
    }

    /// Unsubscribe the current client from a pattern. Returns total subscription count.
    pub fn pubsub_punsubscribe(&mut self, pattern: &[u8]) -> usize {
        let client_id = self.session.client_id;
        if let Some(subs) = self.server.pubsub_pattern_subs.get_mut(pattern) {
            subs.remove(&client_id);
            if subs.is_empty() {
                self.server.pubsub_pattern_subs.remove(pattern);
            }
        }
        if let Some(patterns) = self.server.pubsub_client_patterns.get_mut(&client_id) {
            patterns.remove(pattern);
            if patterns.is_empty() {
                self.server.pubsub_client_patterns.remove(&client_id);
            }
        }
        if !self.server.pubsub_pattern_subs.contains_key(pattern) {
            self.server.store.punsubscribe(pattern);
        }
        self.pubsub_sub_count(client_id)
    }

    /// Subscribe the current client to a shard channel. Returns shard sub count.
    pub fn pubsub_ssubscribe(&mut self, channel: Vec<u8>) -> usize {
        let client_id = self.session.client_id;
        self.server.store.ssubscribe(channel.clone());
        self.server
            .pubsub_shard_subs
            .entry(channel.clone())
            .or_default()
            .insert(client_id);
        self.server
            .pubsub_client_shard_channels
            .entry(client_id)
            .or_default()
            .insert(channel);
        self.pubsub_shard_sub_count(client_id)
    }

    /// Unsubscribe the current client from a shard channel. Returns shard sub count.
    pub fn pubsub_sunsubscribe(&mut self, channel: &[u8]) -> usize {
        let client_id = self.session.client_id;
        if let Some(subs) = self.server.pubsub_shard_subs.get_mut(channel) {
            subs.remove(&client_id);
            if subs.is_empty() {
                self.server.pubsub_shard_subs.remove(channel);
            }
        }
        if let Some(channels) = self.server.pubsub_client_shard_channels.get_mut(&client_id) {
            channels.remove(channel);
            if channels.is_empty() {
                self.server.pubsub_client_shard_channels.remove(&client_id);
            }
        }
        if !self.server.pubsub_shard_subs.contains_key(channel) {
            self.server.store.sunsubscribe(channel);
        }
        self.pubsub_shard_sub_count(client_id)
    }

    /// Publish a message to a channel. Queues messages in each subscriber's
    /// outbox. Returns the number of clients that received the message.
    pub fn pubsub_publish(&mut self, channel: &[u8], message: &[u8]) -> usize {
        let mut receivers = 0;

        // Direct channel subscribers
        if let Some(client_ids) = self.server.pubsub_channel_subs.get(channel) {
            for &client_id in client_ids {
                self.server
                    .pubsub_outbox
                    .entry(client_id)
                    .or_default()
                    .push(fr_store::PubSubMessage::Message {
                        channel: channel.to_vec(),
                        data: message.to_vec(),
                    });
                receivers += 1;
            }
        }

        // Pattern subscribers — each matching pattern produces a pmessage
        // OPTIMIZATION: Avoid intermediate allocations and excessive cloning.
        for (pattern, client_ids) in &self.server.pubsub_pattern_subs {
            if fr_store::glob_match(pattern, channel) {
                for &client_id in client_ids {
                    self.server
                        .pubsub_outbox
                        .entry(client_id)
                        .or_default()
                        .push(fr_store::PubSubMessage::PMessage {
                            pattern: pattern.clone(),
                            channel: channel.to_vec(),
                            data: message.to_vec(),
                        });
                    receivers += 1;
                }
            }
        }

        receivers
    }

    /// Publish a message to a shard channel. Returns receiver count.
    pub fn pubsub_spublish(&mut self, channel: &[u8], message: &[u8]) -> usize {
        let mut receivers = 0;
        if let Some(client_ids) = self.server.pubsub_shard_subs.get(channel) {
            for &client_id in client_ids {
                self.server
                    .pubsub_outbox
                    .entry(client_id)
                    .or_default()
                    .push(fr_store::PubSubMessage::SMessage {
                        channel: channel.to_vec(),
                        data: message.to_vec(),
                    });
                receivers += 1;
            }
        }
        receivers
    }

    /// Remove all subscriptions for a client (called on disconnect).
    pub fn pubsub_cleanup_client(&mut self, client_id: u64) {
        if let Some(channels) = self.server.pubsub_client_channels.remove(&client_id) {
            for ch in channels {
                if let Some(subs) = self.server.pubsub_channel_subs.get_mut(&ch) {
                    subs.remove(&client_id);
                    if subs.is_empty() {
                        self.server.pubsub_channel_subs.remove(&ch);
                        self.server.store.unsubscribe(&ch);
                    }
                }
            }
        }
        if let Some(patterns) = self.server.pubsub_client_patterns.remove(&client_id) {
            for pat in patterns {
                if let Some(subs) = self.server.pubsub_pattern_subs.get_mut(&pat) {
                    subs.remove(&client_id);
                    if subs.is_empty() {
                        self.server.pubsub_pattern_subs.remove(&pat);
                        self.server.store.punsubscribe(&pat);
                    }
                }
            }
        }
        if let Some(shard_channels) = self.server.pubsub_client_shard_channels.remove(&client_id) {
            for ch in shard_channels {
                if let Some(subs) = self.server.pubsub_shard_subs.get_mut(&ch) {
                    subs.remove(&client_id);
                    if subs.is_empty() {
                        self.server.pubsub_shard_subs.remove(&ch);
                        self.server.store.sunsubscribe(&ch);
                    }
                }
            }
        }
        self.server.pubsub_outbox.remove(&client_id);
    }

    fn pubsub_sub_count(&self, client_id: u64) -> usize {
        self.server
            .pubsub_client_channels
            .get(&client_id)
            .map_or(0, HashSet::len)
            + self
                .server
                .pubsub_client_patterns
                .get(&client_id)
                .map_or(0, HashSet::len)
    }

    fn pubsub_shard_sub_count(&self, client_id: u64) -> usize {
        self.server
            .pubsub_client_shard_channels
            .get(&client_id)
            .map_or(0, HashSet::len)
    }

    /// Returns true if the current client has any active pub/sub subscriptions.
    pub fn is_in_subscription_mode(&self) -> bool {
        self.pubsub_sub_count(self.session.client_id) > 0
            || self.pubsub_shard_sub_count(self.session.client_id) > 0
    }

    pub fn configure_maxmemory_enforcement(
        &mut self,
        maxmemory_bytes: usize,
        not_counted_bytes: usize,
        sample_limit: usize,
        max_cycles: usize,
    ) {
        self.server.configure_maxmemory_enforcement(
            maxmemory_bytes,
            not_counted_bytes,
            sample_limit,
            max_cycles,
        );
    }

    pub fn set_eviction_safety_gate(&mut self, safety_gate: EvictionSafetyGateState) {
        self.server.set_eviction_safety_gate(safety_gate);
    }

    #[must_use]
    pub fn maxmemory_pressure_state(&self) -> fr_store::MaxmemoryPressureState {
        self.server.maxmemory_pressure_state()
    }

    #[must_use]
    pub fn last_eviction_loop_result(&self) -> Option<EvictionLoopResult> {
        self.server.last_eviction_loop_result()
    }

    pub fn set_requirepass(&mut self, requirepass: Option<Vec<u8>>) {
        self.apply_requirepass_update(requirepass, false);
    }

    pub fn add_user(&mut self, username: Vec<u8>, password: Vec<u8>) {
        self.server.auth_state.add_user(username, password);
        self.session
            .refresh_authentication_for_server(&self.server.auth_state, false);
    }

    #[must_use]
    pub fn is_authenticated(&self) -> bool {
        self.session.is_authenticated()
    }

    fn is_command_authorized(&self, argv: &[Vec<u8>]) -> bool {
        let username = self.session.current_user_name();
        let Some(user) = self.server.auth_state.get_user(username) else {
            // User was deleted while session is still active. In Redis, the
            // connection would be killed asynchronously. For compatibility
            // in single-session mode, allow continued access.
            return true;
        };

        let Some(cmd) = argv.first() else {
            return false;
        };

        let cmd_name = String::from_utf8_lossy(cmd);
        user.is_command_allowed(&cmd_name)
    }

    #[must_use]
    pub fn is_cluster_read_only(&self) -> bool {
        self.session.cluster_state.mode == ClusterClientMode::ReadOnly
    }

    #[must_use]
    pub fn parser_config(&self) -> fr_protocol::ParserConfig {
        fr_protocol::ParserConfig {
            max_bulk_len: self
                .policy
                .gate
                .max_bulk_len
                .min(self.server.proto_max_bulk_len),
            max_array_len: self.policy.gate.max_array_len,
            max_recursion_depth: 128,
        }
    }

    #[must_use]
    pub fn is_cluster_asking(&self) -> bool {
        self.session.cluster_state.asking
    }

    #[cfg(test)]
    fn set_replication_ack_state_for_tests(
        &mut self,
        primary_offset: u64,
        local_fsync_offset: u64,
        replica_ack_offsets: &[u64],
        replica_fsync_offsets: &[u64],
    ) {
        self.server.set_replication_ack_state_for_tests(
            primary_offset,
            local_fsync_offset,
            replica_ack_offsets,
            replica_fsync_offsets,
        );
    }

    fn apply_requirepass_update(
        &mut self,
        requirepass: Option<Vec<u8>>,
        preserve_authenticated_user: bool,
    ) {
        self.server.auth_state.set_requirepass(requirepass);
        self.session.refresh_authentication_for_server(
            &self.server.auth_state,
            preserve_authenticated_user,
        );
    }

    pub fn apply_tls_config(
        &mut self,
        candidate: TlsConfig,
        now_ms: u64,
    ) -> Result<(), TlsCfgError> {
        let packet_id = next_packet_id();
        let input_digest = digest_bytes(format!("{candidate:?}").as_bytes());
        let state_before = self.server.store.state_digest();

        let plan = match plan_tls_runtime_apply(&self.server.tls_state, candidate) {
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

        let mut next_state = self.server.tls_state.clone();
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
        self.server.tls_state = next_state;
        Ok(())
    }

    pub fn execute_frame(&mut self, frame: RespFrame, now_ms: u64) -> RespFrame {
        self.server.store.stat_total_commands_processed += 1;
        let packet_id = next_packet_id();
        let input_digest = digest_bytes(&frame.to_bytes());
        let state_before = self.server.store.state_digest();
        self.execute_frame_internal(frame, now_ms, packet_id, input_digest, state_before)
    }

    fn execute_frame_internal(
        &mut self,
        frame: RespFrame,
        now_ms: u64,
        packet_id: u64,
        input_digest: String,
        state_before: String,
    ) -> RespFrame {
        if let Some(reply) =
            self.preflight_gate(&frame, now_ms, packet_id, &input_digest, &state_before)
        {
            return reply;
        }

        let argv = match frame_to_argv(&frame) {
            Ok(argv) => {
                if argv.len() > MAX_COMMAND_ARITY {
                    let reply = RespFrame::Error(format!(
                        "ERR Protocol error: too many arguments (limit: {})",
                        MAX_COMMAND_ARITY
                    ));
                    self.record_threat_event(ThreatEventInput {
                        now_ms,
                        packet_id,
                        threat_class: ThreatClass::ResourceExhaustion,
                        preferred_deviation: Some(HardenedDeviationCategory::ResourceClamp),
                        subsystem: "router",
                        action: "reject_large_command",
                        reason_code: "too_many_arguments",
                        reason: format!(
                            "command arity {} exceeds limit {}",
                            argv.len(),
                            MAX_COMMAND_ARITY
                        ),
                        input_digest,
                        state_before: &state_before,
                        output: &reply,
                    });
                    return reply;
                }
                argv
            }
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

        let special_command = classify_runtime_special_command(&argv[0]);
        let command_name_lossy = String::from_utf8_lossy(&argv[0]);
        let command_name = &command_name_lossy;

        match special_command {
            Some(RuntimeSpecialCommand::Auth) => return self.handle_auth_command(&argv),
            Some(RuntimeSpecialCommand::Hello) => return self.handle_hello_command(&argv),
            _ => {}
        }

        if self.session.requires_auth(&self.server.auth_state) {
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

        if !self.is_command_authorized(&argv) {
            let reply = RespFrame::Error(format!(
                "NOPERM this user has no permissions to run the '{}' command",
                command_name
            ));
            self.record_threat_event(ThreatEventInput {
                now_ms,
                packet_id,
                threat_class: ThreatClass::AuthPolicyConfusion,
                preferred_deviation: None,
                subsystem: "admission_gate",
                action: "reject_unauthorized_command",
                reason_code: "auth.noperm_gate_violation",
                reason: format!(
                    "rejected '{}' prior to dispatch due to insufficient ACL permissions",
                    command_name
                ),
                input_digest,
                state_before: &state_before,
                output: &reply,
            });
            return reply;
        }

        // When inside MULTI, queue commands that can be deferred to EXEC.
        // Only transaction-control commands (MULTI/EXEC/DISCARD/WATCH/UNWATCH)
        // and connection commands (QUIT/RESET) execute immediately.
        if self.session.transaction_state.in_transaction {
            let must_execute_now = matches!(
                special_command,
                Some(RuntimeSpecialCommand::Multi)
                    | Some(RuntimeSpecialCommand::Exec)
                    | Some(RuntimeSpecialCommand::Discard)
                    | Some(RuntimeSpecialCommand::Watch)
                    | Some(RuntimeSpecialCommand::Unwatch)
                    | Some(RuntimeSpecialCommand::Quit)
                    | Some(RuntimeSpecialCommand::Reset)
            );
            // Subscribe/Unsubscribe family is not allowed inside MULTI
            let is_sub_cmd = matches!(
                special_command,
                Some(RuntimeSpecialCommand::Subscribe)
                    | Some(RuntimeSpecialCommand::Unsubscribe)
                    | Some(RuntimeSpecialCommand::Psubscribe)
                    | Some(RuntimeSpecialCommand::Punsubscribe)
                    | Some(RuntimeSpecialCommand::Ssubscribe)
                    | Some(RuntimeSpecialCommand::Sunsubscribe)
            );
            if is_sub_cmd {
                return RespFrame::Error(
                    "ERR Command not allowed inside a transaction".to_string(),
                );
            }
            if !must_execute_now {
                let cmd_bytes = match argv.first() {
                    Some(cmd) => cmd,
                    None => {
                        self.session.transaction_state.exec_abort = true;
                        return CommandError::InvalidCommandFrame.to_resp();
                    }
                };
                if !fr_command::is_known_command(cmd_bytes) {
                    self.session.transaction_state.exec_abort = true;
                    let cmd_str = std::str::from_utf8(cmd_bytes).unwrap_or("");
                    return RespFrame::Error(format!(
                        "ERR unknown command '{}'",
                        fr_command::trim_and_cap_string(cmd_str, 128)
                    ));
                }
                // Validate arity before queueing (Redis rejects wrong arity
                // immediately and sets EXECABORT).
                if let Err(cmd_name) = fr_command::check_command_arity(cmd_bytes, argv.len()) {
                    self.session.transaction_state.exec_abort = true;
                    return RespFrame::Error(format!(
                        "ERR wrong number of arguments for '{}' command",
                        cmd_name
                    ));
                }
                self.session.transaction_state.command_queue.push(argv);
                return RespFrame::SimpleString("QUEUED".to_string());
            }
        }

        // Dispatch runtime-special commands that execute outside of MULTI,
        // or the subset that must execute immediately inside MULTI.
        match special_command {
            Some(RuntimeSpecialCommand::Acl) => return self.handle_acl_command(&argv),
            Some(RuntimeSpecialCommand::Config) => return self.handle_config_command(&argv),
            Some(RuntimeSpecialCommand::Client) => {
                return self.handle_client_command(&argv, now_ms);
            }
            Some(RuntimeSpecialCommand::Role) => return self.handle_role_command(&argv),
            Some(RuntimeSpecialCommand::Replconf) => return self.handle_replconf_command(&argv),
            Some(RuntimeSpecialCommand::Psync) | Some(RuntimeSpecialCommand::Sync) => {
                return self.handle_psync_command(&argv);
            }
            Some(RuntimeSpecialCommand::Replicaof) | Some(RuntimeSpecialCommand::Slaveof) => {
                return self.handle_replicaof_command(&argv);
            }
            Some(RuntimeSpecialCommand::Asking) => return self.handle_asking_command(&argv),
            Some(RuntimeSpecialCommand::Readonly) => return self.handle_readonly_command(&argv),
            Some(RuntimeSpecialCommand::Readwrite) => return self.handle_readwrite_command(&argv),
            Some(RuntimeSpecialCommand::Cluster) => {
                return self.handle_cluster_command(&argv, now_ms);
            }
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
                return self.handle_bgrewriteaof_command(&argv, now_ms);
            }
            Some(RuntimeSpecialCommand::Shutdown) => {
                return self.handle_shutdown_command(&argv);
            }
            Some(RuntimeSpecialCommand::Pubsub) => {
                return self.handle_pubsub_command(&argv);
            }
            Some(RuntimeSpecialCommand::Subscribe) => {
                return self.handle_subscribe_command(&argv);
            }
            Some(RuntimeSpecialCommand::Unsubscribe) => {
                return self.handle_unsubscribe_command(&argv);
            }
            Some(RuntimeSpecialCommand::Psubscribe) => {
                return self.handle_psubscribe_command(&argv);
            }
            Some(RuntimeSpecialCommand::Punsubscribe) => {
                return self.handle_punsubscribe_command(&argv);
            }
            Some(RuntimeSpecialCommand::Publish) => {
                return self.handle_publish_command(&argv);
            }
            Some(RuntimeSpecialCommand::Ssubscribe) => {
                return self.handle_ssubscribe_command(&argv);
            }
            Some(RuntimeSpecialCommand::Sunsubscribe) => {
                return self.handle_sunsubscribe_command(&argv);
            }
            Some(RuntimeSpecialCommand::Spublish) => {
                return self.handle_spublish_command(&argv);
            }
            Some(RuntimeSpecialCommand::Select) => {
                return self.handle_select_command(&argv);
            }
            Some(RuntimeSpecialCommand::Swapdb) => {
                return self.handle_swapdb_command(&argv);
            }
            _ => {}
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
        let dirty_before = self.server.store.dirty;
        let start = Instant::now();
        let handled_migrate = argv
            .first()
            .is_some_and(|cmd| eq_ascii_token(cmd, b"MIGRATE"));
        let result = if handled_migrate {
            self.handle_migrate_command(&argv, now_ms)
        } else {
            self.execute_db_scoped_command(&argv, now_ms)
        };
        let elapsed_us = start.elapsed().as_micros() as u64;
        let dirty_after = self.server.store.dirty;
        self.record_slowlog(&argv, elapsed_us, now_ms);

        if elapsed_us > (self.server.command_time_budget_ms * 1000) {
            self.record_threat_event(ThreatEventInput {
                now_ms,
                packet_id,
                threat_class: ThreatClass::ResourceExhaustion,
                preferred_deviation: Some(HardenedDeviationCategory::ResourceClamp),
                subsystem: "router",
                action: "slow_command_detected",
                reason_code: "command_time_budget_exceeded",
                reason: format!(
                    "command '{}' took {}us, exceeding budget {}ms",
                    command_name, elapsed_us, self.server.command_time_budget_ms
                ),
                input_digest,
                state_before: &state_before,
                output: &RespFrame::SimpleString("OK".to_string()), // Dummy for logging
            });
        }

        // Feed MONITOR clients before returning
        self.feed_monitors(&argv, now_ms, self.session.selected_db);

        // Check if this was a MONITOR command — flag the client
        if argv
            .first()
            .is_some_and(|cmd| cmd.eq_ignore_ascii_case(b"MONITOR"))
        {
            self.enable_monitor();
        }

        match result {
            Ok(reply) => {
                if dirty_after > dirty_before {
                    // Record AOF only for non-special commands (special ones record themselves)
                    if special_command.is_none() && !handled_migrate {
                        self.capture_aof_record(&argv);
                    }

                    // Optimized blocking: track keys modified by write commands
                    // so the event loop only checks clients waiting on these keys.
                    if !handled_migrate {
                        let cmd_keys = fr_command::command_keys(&argv);
                        for key in &cmd_keys {
                            self.server.ready_keys.insert(key.clone());
                        }
                        // Keyspace notifications: publish events for modified keys.
                        if self.server.store.notify_keyspace_events != 0 {
                            let event = Self::command_to_keyspace_event(&argv);
                            let event_type = Self::command_to_notify_type(&argv);
                            let db = self.session.selected_db;
                            let is_rename = argv
                                .first()
                                .map(|c| {
                                    c.eq_ignore_ascii_case(b"RENAME")
                                        || c.eq_ignore_ascii_case(b"RENAMENX")
                                })
                                .unwrap_or(false);
                            if is_rename && cmd_keys.len() >= 2 {
                                // RENAME emits rename_from on source, rename_to on dest
                                self.server.store.notify_keyspace_event(
                                    event_type,
                                    "rename_from",
                                    &cmd_keys[0],
                                    db,
                                );
                                self.server.store.notify_keyspace_event(
                                    event_type,
                                    "rename_to",
                                    &cmd_keys[1],
                                    db,
                                );
                            } else {
                                for key in &cmd_keys {
                                    self.server
                                        .store
                                        .notify_keyspace_event(event_type, event, key, db);
                                }
                            }
                            // Deliver queued keyspace notifications via pub/sub
                            self.deliver_keyspace_notifications();
                        }
                    }
                }
                reply
            }
            Err(err) => (err).to_resp(),
        }
    }

    /// Map a command name to its keyspace notification event name.
    #[allow(clippy::if_same_then_else)]
    fn command_to_keyspace_event(argv: &[Vec<u8>]) -> &'static str {
        let Some(cmd) = argv.first() else {
            return "unknown";
        };
        // Match common commands to their Redis event names (lowercase).
        if cmd.eq_ignore_ascii_case(b"SET")
            || cmd.eq_ignore_ascii_case(b"SETEX")
            || cmd.eq_ignore_ascii_case(b"PSETEX")
            || cmd.eq_ignore_ascii_case(b"MSET")
            || cmd.eq_ignore_ascii_case(b"MSETNX")
            || cmd.eq_ignore_ascii_case(b"SETNX")
            || cmd.eq_ignore_ascii_case(b"GETSET")
            || cmd.eq_ignore_ascii_case(b"SETRANGE")
        {
            "set"
        } else if cmd.eq_ignore_ascii_case(b"DEL") || cmd.eq_ignore_ascii_case(b"UNLINK") {
            "del"
        } else if cmd.eq_ignore_ascii_case(b"APPEND") {
            "append"
        } else if cmd.eq_ignore_ascii_case(b"INCR") || cmd.eq_ignore_ascii_case(b"INCRBY") {
            "incrby"
        } else if cmd.eq_ignore_ascii_case(b"DECR") || cmd.eq_ignore_ascii_case(b"DECRBY") {
            "decrby"
        } else if cmd.eq_ignore_ascii_case(b"INCRBYFLOAT") {
            "incrbyfloat"
        } else if cmd.eq_ignore_ascii_case(b"EXPIRE")
            || cmd.eq_ignore_ascii_case(b"PEXPIRE")
            || cmd.eq_ignore_ascii_case(b"EXPIREAT")
            || cmd.eq_ignore_ascii_case(b"PEXPIREAT")
        {
            "expire"
        } else if cmd.eq_ignore_ascii_case(b"PERSIST") {
            "persist"
        } else if cmd.eq_ignore_ascii_case(b"RENAME") {
            "rename_from"
        } else if cmd.eq_ignore_ascii_case(b"LPUSH") || cmd.eq_ignore_ascii_case(b"LPUSHX") {
            "lpush"
        } else if cmd.eq_ignore_ascii_case(b"RPUSH") || cmd.eq_ignore_ascii_case(b"RPUSHX") {
            "rpush"
        } else if cmd.eq_ignore_ascii_case(b"LPOP") {
            "lpop"
        } else if cmd.eq_ignore_ascii_case(b"RPOP") {
            "rpop"
        } else if cmd.eq_ignore_ascii_case(b"LSET") {
            "lset"
        } else if cmd.eq_ignore_ascii_case(b"LINSERT") {
            "linsert"
        } else if cmd.eq_ignore_ascii_case(b"LTRIM") {
            "ltrim"
        } else if cmd.eq_ignore_ascii_case(b"LREM") {
            "lrem"
        } else if cmd.eq_ignore_ascii_case(b"HSET")
            || cmd.eq_ignore_ascii_case(b"HMSET")
            || cmd.eq_ignore_ascii_case(b"HSETNX")
        {
            "hset"
        } else if cmd.eq_ignore_ascii_case(b"HDEL") {
            "hdel"
        } else if cmd.eq_ignore_ascii_case(b"HINCRBY") {
            "hincrby"
        } else if cmd.eq_ignore_ascii_case(b"HINCRBYFLOAT") {
            "hincrbyfloat"
        } else if cmd.eq_ignore_ascii_case(b"SADD") {
            "sadd"
        } else if cmd.eq_ignore_ascii_case(b"SREM") {
            "srem"
        } else if cmd.eq_ignore_ascii_case(b"SPOP") {
            "spop"
        } else if cmd.eq_ignore_ascii_case(b"SMOVE") {
            "smove"
        } else if cmd.eq_ignore_ascii_case(b"ZADD") {
            "zadd"
        } else if cmd.eq_ignore_ascii_case(b"ZREM") {
            "zrem"
        } else if cmd.eq_ignore_ascii_case(b"ZINCRBY") {
            "zincrby"
        } else if cmd.eq_ignore_ascii_case(b"ZPOPMIN") {
            "zpopmin"
        } else if cmd.eq_ignore_ascii_case(b"ZPOPMAX") {
            "zpopmax"
        } else if cmd.eq_ignore_ascii_case(b"XADD") {
            "xadd"
        } else if cmd.eq_ignore_ascii_case(b"XDEL") {
            "xdel"
        } else if cmd.eq_ignore_ascii_case(b"XTRIM") {
            "xtrim"
        } else if cmd.eq_ignore_ascii_case(b"GETDEL") {
            "getdel"
        } else if cmd.eq_ignore_ascii_case(b"COPY") {
            "copy_to"
        } else if cmd.eq_ignore_ascii_case(b"RESTORE") {
            "restore"
        } else if cmd.eq_ignore_ascii_case(b"SETBIT") {
            "setbit"
        } else if cmd.eq_ignore_ascii_case(b"GETEX") {
            "getex"
        } else {
            "generic"
        }
    }

    /// Map a command to its notification type flag.
    #[allow(clippy::if_same_then_else)]
    fn command_to_notify_type(argv: &[Vec<u8>]) -> u32 {
        let Some(cmd) = argv.first() else {
            return fr_store::NOTIFY_GENERIC;
        };
        if cmd.eq_ignore_ascii_case(b"SET")
            || cmd.eq_ignore_ascii_case(b"SETEX")
            || cmd.eq_ignore_ascii_case(b"PSETEX")
            || cmd.eq_ignore_ascii_case(b"MSET")
            || cmd.eq_ignore_ascii_case(b"MSETNX")
            || cmd.eq_ignore_ascii_case(b"SETNX")
            || cmd.eq_ignore_ascii_case(b"GETSET")
            || cmd.eq_ignore_ascii_case(b"SETRANGE")
            || cmd.eq_ignore_ascii_case(b"APPEND")
            || cmd.eq_ignore_ascii_case(b"INCR")
            || cmd.eq_ignore_ascii_case(b"INCRBY")
            || cmd.eq_ignore_ascii_case(b"DECR")
            || cmd.eq_ignore_ascii_case(b"DECRBY")
            || cmd.eq_ignore_ascii_case(b"INCRBYFLOAT")
            || cmd.eq_ignore_ascii_case(b"GETDEL")
            || cmd.eq_ignore_ascii_case(b"GETEX")
            || cmd.eq_ignore_ascii_case(b"SETBIT")
        {
            fr_store::NOTIFY_STRING
        } else if cmd.eq_ignore_ascii_case(b"LPUSH")
            || cmd.eq_ignore_ascii_case(b"RPUSH")
            || cmd.eq_ignore_ascii_case(b"LPUSHX")
            || cmd.eq_ignore_ascii_case(b"RPUSHX")
            || cmd.eq_ignore_ascii_case(b"LPOP")
            || cmd.eq_ignore_ascii_case(b"RPOP")
            || cmd.eq_ignore_ascii_case(b"LSET")
            || cmd.eq_ignore_ascii_case(b"LINSERT")
            || cmd.eq_ignore_ascii_case(b"LTRIM")
            || cmd.eq_ignore_ascii_case(b"LREM")
            || cmd.eq_ignore_ascii_case(b"LMOVE")
            || cmd.eq_ignore_ascii_case(b"BLMOVE")
            || cmd.eq_ignore_ascii_case(b"RPOPLPUSH")
            || cmd.eq_ignore_ascii_case(b"BRPOPLPUSH")
            || cmd.eq_ignore_ascii_case(b"BLPOP")
            || cmd.eq_ignore_ascii_case(b"BRPOP")
            || cmd.eq_ignore_ascii_case(b"LMPOP")
            || cmd.eq_ignore_ascii_case(b"BLMPOP")
        {
            fr_store::NOTIFY_LIST
        } else if cmd.eq_ignore_ascii_case(b"SADD")
            || cmd.eq_ignore_ascii_case(b"SREM")
            || cmd.eq_ignore_ascii_case(b"SPOP")
            || cmd.eq_ignore_ascii_case(b"SMOVE")
            || cmd.eq_ignore_ascii_case(b"SINTERSTORE")
            || cmd.eq_ignore_ascii_case(b"SUNIONSTORE")
            || cmd.eq_ignore_ascii_case(b"SDIFFSTORE")
        {
            fr_store::NOTIFY_SET
        } else if cmd.eq_ignore_ascii_case(b"HSET")
            || cmd.eq_ignore_ascii_case(b"HMSET")
            || cmd.eq_ignore_ascii_case(b"HSETNX")
            || cmd.eq_ignore_ascii_case(b"HDEL")
            || cmd.eq_ignore_ascii_case(b"HINCRBY")
            || cmd.eq_ignore_ascii_case(b"HINCRBYFLOAT")
        {
            fr_store::NOTIFY_HASH
        } else if cmd.eq_ignore_ascii_case(b"ZADD")
            || cmd.eq_ignore_ascii_case(b"ZREM")
            || cmd.eq_ignore_ascii_case(b"ZINCRBY")
            || cmd.eq_ignore_ascii_case(b"ZPOPMIN")
            || cmd.eq_ignore_ascii_case(b"ZPOPMAX")
            || cmd.eq_ignore_ascii_case(b"ZRANGESTORE")
            || cmd.eq_ignore_ascii_case(b"ZINTERSTORE")
            || cmd.eq_ignore_ascii_case(b"ZUNIONSTORE")
            || cmd.eq_ignore_ascii_case(b"ZDIFFSTORE")
            || cmd.eq_ignore_ascii_case(b"ZMPOP")
            || cmd.eq_ignore_ascii_case(b"BZMPOP")
            || cmd.eq_ignore_ascii_case(b"BZPOPMIN")
            || cmd.eq_ignore_ascii_case(b"BZPOPMAX")
        {
            fr_store::NOTIFY_ZSET
        } else if cmd.eq_ignore_ascii_case(b"XADD")
            || cmd.eq_ignore_ascii_case(b"XDEL")
            || cmd.eq_ignore_ascii_case(b"XTRIM")
        {
            fr_store::NOTIFY_STREAM
        } else {
            fr_store::NOTIFY_GENERIC
        }
    }

    /// Deliver queued keyspace notifications through the pub/sub system.
    fn deliver_keyspace_notifications(&mut self) {
        let notifications = self.server.store.drain_keyspace_notifications();
        for (channel, message) in notifications {
            self.pubsub_publish(&channel, &message);
        }
    }

    fn strip_db_prefixes_from_frame(&self, frame: &mut RespFrame) {
        match frame {
            RespFrame::BulkString(Some(bytes)) => {
                if let Some((_, logical)) = decode_db_key(bytes) {
                    *bytes = logical.to_vec();
                }
            }
            RespFrame::SimpleString(s) => {
                if let Some((_, logical)) = decode_db_key(s.as_bytes()) {
                    *s = String::from_utf8_lossy(logical).to_string();
                }
            }
            RespFrame::Array(Some(frames)) => {
                for f in frames {
                    self.strip_db_prefixes_from_frame(f);
                }
            }
            _ => {}
        }
    }

    /// Clear the set of keys that were modified in the current tick.
    pub fn clear_ready_keys(&mut self) {
        self.server.ready_keys.clear();
    }

    /// Return and clear the set of keys that were modified in the current tick.
    pub fn drain_ready_keys(&mut self) -> HashSet<Vec<u8>> {
        std::mem::take(&mut self.server.ready_keys)
    }

    // ── MONITOR support ─────────────────────────────────────────────

    /// Register the current client as a MONITOR client.
    pub fn enable_monitor(&mut self) {
        self.server.monitor_clients.insert(self.session.client_id);
    }

    /// Remove a client from the monitor set.
    pub fn disable_monitor(&mut self, client_id: u64) {
        self.server.monitor_clients.remove(&client_id);
    }

    /// Feed a command to all monitor clients, formatted as Redis does.
    pub fn feed_monitors(&mut self, argv: &[Vec<u8>], now_ms: u64, db: usize) {
        if self.server.monitor_clients.is_empty() {
            return;
        }
        let secs = now_ms / 1000;
        let usecs = (now_ms % 1000) * 1000;
        let mut line = format!("+{secs}.{usecs:06} [{} 127.0.0.1:0]", db);
        for arg in argv {
            line.push(' ');
            line.push('"');
            for &b in arg.iter() {
                if b == b'"' || b == b'\\' {
                    line.push('\\');
                    line.push(b as char);
                } else if !(32..=126).contains(&b) {
                    line.push_str(&format!("\\x{b:02x}"));
                } else {
                    line.push(b as char);
                }
            }
            line.push('"');
        }
        line.push_str("\r\n");
        let line_bytes = line.into_bytes();
        for &client_id in &self.server.monitor_clients {
            self.server
                .monitor_output
                .push((client_id, line_bytes.clone()));
        }
    }

    /// Drain pending monitor output for delivery.
    pub fn drain_monitor_output(&mut self) -> Vec<(u64, Vec<u8>)> {
        std::mem::take(&mut self.server.monitor_output)
    }

    /// Check if clients are paused. Returns true if the pause is active.
    /// If the deadline has passed, auto-unpause and return false.
    pub fn is_client_paused(&mut self, now_ms: u64) -> bool {
        if self.server.client_pause_deadline_ms == 0 {
            return false;
        }
        if now_ms >= self.server.client_pause_deadline_ms {
            self.server.client_pause_deadline_ms = 0;
            self.server.client_pause_all = false;
            return false;
        }
        true
    }

    /// Check if a specific command should be blocked by CLIENT PAUSE.
    pub fn is_command_paused(&mut self, argv: &[Vec<u8>], now_ms: u64) -> bool {
        if !self.is_client_paused(now_ms) {
            return false;
        }
        if self.server.client_pause_all {
            return true;
        }
        // WRITE mode: only block write commands
        argv.first()
            .is_some_and(|cmd| fr_command::is_write_command(cmd))
    }

    pub fn execute_bytes(&mut self, input: &[u8], now_ms: u64) -> Vec<u8> {
        let packet_id = next_packet_id();
        let input_digest = digest_bytes(input);
        let state_before = self.server.store.state_digest();

        let parser_config = self.parser_config();

        match fr_protocol::parse_frame_with_config(input, &parser_config) {
            Ok(parsed) => self
                .execute_frame_internal(parsed.frame, now_ms, packet_id, input_digest, state_before)
                .to_bytes(),
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
        if self.server.maxmemory_bytes == 0 {
            self.server.last_eviction_loop = None;
            return None;
        }

        let loop_result = self.server.store.run_bounded_eviction_loop(
            now_ms,
            self.server.maxmemory_bytes,
            self.server.maxmemory_not_counted_bytes,
            self.server.maxmemory_eviction_sample_limit,
            self.server.maxmemory_eviction_max_cycles,
            self.server.eviction_safety_gate,
        );
        self.server.last_eviction_loop = Some(loop_result);

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
        if Runtime::command_advances_replication_offset(argv)
            && self.server.aof_selected_db != self.session.selected_db
        {
            self.server.capture_aof_record(&[
                b"SELECT".to_vec(),
                self.session.selected_db.to_string().into_bytes(),
            ]);
            self.server.aof_selected_db = self.session.selected_db;
        }
        self.server.capture_aof_record(argv);
    }

    fn command_advances_replication_offset(argv: &[Vec<u8>]) -> bool {
        let Some(command) = argv.first() else {
            return false;
        };
        if eq_ascii_token(command, b"MULTI")
            || eq_ascii_token(command, b"EXEC")
            || eq_ascii_token(command, b"SELECT")
            || eq_ascii_token(command, b"SWAPDB")
        {
            return true;
        }
        fr_command::is_write_command(command)
    }

    fn namespace_argv_for_selected_db(&self, argv: &[Vec<u8>]) -> Vec<Vec<u8>> {
        let mut rewritten = argv.to_vec();
        for idx in fr_command::command_key_indexes(argv) {
            if let Some(arg) = rewritten.get_mut(idx) {
                *arg = encode_db_key(self.session.selected_db, arg);
            }
        }
        rewritten
    }

    fn execute_db_scoped_command(
        &mut self,
        argv: &[Vec<u8>],
        now_ms: u64,
    ) -> Result<RespFrame, CommandError> {
        let Some(command) = argv.first() else {
            return Err(CommandError::InvalidCommandFrame);
        };
        if eq_ascii_token(command, b"KEYS") {
            return self.handle_db_keys_command(argv, now_ms);
        }
        if eq_ascii_token(command, b"DBSIZE") {
            return self.handle_dbsize_command(argv);
        }
        if eq_ascii_token(command, b"FLUSHDB") {
            return self.handle_flushdb_command(argv);
        }
        if eq_ascii_token(command, b"RANDOMKEY") {
            return self.handle_randomkey_command(argv, now_ms);
        }
        if eq_ascii_token(command, b"SCAN") {
            return self.handle_scan_command(argv, now_ms);
        }
        if eq_ascii_token(command, b"MOVE") {
            return self.handle_move_command(argv, now_ms);
        }
        if eq_ascii_token(command, b"COPY") {
            return self.handle_copy_command(argv, now_ms);
        }
        if eq_ascii_token(command, b"INFO") {
            return self.handle_info_command(argv, now_ms);
        }
        let namespaced = self.namespace_argv_for_selected_db(argv);
        let mut reply = dispatch_argv(&namespaced, &mut self.server.store, now_ms)?;
        self.strip_db_prefixes_from_frame(&mut reply);
        Ok(reply)
    }

    fn handle_migrate_command(
        &mut self,
        argv: &[Vec<u8>],
        now_ms: u64,
    ) -> Result<RespFrame, CommandError> {
        let request = parse_migrate_request(argv)?;
        let key_specs: Vec<MigrateKeySpec> = request
            .keys
            .iter()
            .map(|key| MigrateKeySpec {
                source_key: encode_db_key(self.session.selected_db, key),
                target_key: key.clone(),
            })
            .collect();
        let outcome = execute_migrate(&request, &key_specs, &mut self.server.store, now_ms)?;

        if !outcome.deleted_keys.is_empty() {
            let mut del_argv = vec![b"DEL".to_vec()];
            del_argv.extend(outcome.deleted_keys.iter().cloned());
            self.capture_aof_record(&del_argv);

            for key in &outcome.deleted_keys {
                self.server.ready_keys.insert(key.clone());
            }

            if self.server.store.notify_keyspace_events != 0 {
                for key in &outcome.deleted_keys {
                    self.server.store.notify_keyspace_event(
                        fr_store::NOTIFY_GENERIC,
                        "del",
                        key,
                        self.session.selected_db,
                    );
                }
                self.deliver_keyspace_notifications();
            }
        }

        Ok(outcome.reply)
    }

    fn handle_auth_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 2 && argv.len() != 3 {
            return CommandError::WrongArity("AUTH").to_resp();
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
        // HELLO with no args: return server info using current protocol (Redis 7+)
        if argv.len() == 1 {
            return build_hello_response(
                self.session.resp_protocol_version,
                self.session.client_id,
            );
        }

        let protocol_version = match parse_i64_arg(&argv[1]) {
            Ok(version) => version,
            Err(err) => return err.to_resp(),
        };

        if protocol_version != 2 && protocol_version != 3 {
            return RespFrame::Error(format!(
                "NOPROTO unsupported protocol version '{}'",
                protocol_version
            ));
        }

        let mut auth_credentials: Option<(&[u8], &[u8])> = None;
        let mut next_client_name: Option<Option<Vec<u8>>> = None;
        let mut options = argv[2..].iter();
        while let Some(option_arg) = options.next() {
            let option = match std::str::from_utf8(option_arg) {
                Ok(option) => option,
                Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
            };
            if option.eq_ignore_ascii_case("AUTH") {
                let Some(username) = options.next() else {
                    return CommandError::SyntaxError.to_resp();
                };
                let Some(password) = options.next() else {
                    return CommandError::SyntaxError.to_resp();
                };
                auth_credentials = Some((username.as_slice(), password.as_slice()));
                continue;
            }
            if option.eq_ignore_ascii_case("SETNAME") {
                let Some(name) = options.next() else {
                    return CommandError::SyntaxError.to_resp();
                };
                if name.contains(&b' ') {
                    return RespFrame::Error(
                        "ERR Client names cannot contain spaces, newlines or special characters."
                            .to_string(),
                    );
                }
                next_client_name = Some(if name.is_empty() {
                    None
                } else {
                    Some(name.clone())
                });
                continue;
            }
            return CommandError::SyntaxError.to_resp();
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
        } else if self.session.requires_auth(&self.server.auth_state) {
            return RespFrame::Error(NOAUTH_ERROR.to_string());
        }

        if let Some(client_name) = next_client_name {
            self.session.client_name = client_name;
        }
        self.session.resp_protocol_version = protocol_version;
        build_hello_response(protocol_version, self.session.client_id)
    }

    fn authenticate_user(&mut self, username: &[u8], password: &[u8]) -> Result<(), AuthFailure> {
        if !self.server.auth_state.auth_required() {
            return Err(AuthFailure::NotConfigured);
        }

        let Some(acl_user) = self.server.auth_state.acl_users.get(username) else {
            return Err(AuthFailure::WrongPass);
        };

        if !acl_user.enabled {
            return Err(AuthFailure::WrongPass);
        }

        if !acl_user.check_password(password) {
            return Err(AuthFailure::WrongPass);
        }

        self.session.authenticated_user = Some(username.to_vec());
        Ok(())
    }

    fn handle_acl_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() < 2 {
            return CommandError::WrongArity("ACL").to_resp();
        }
        let sub = match std::str::from_utf8(&argv[1]) {
            Ok(s) => s,
            Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
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
                return CommandError::WrongSubcommandArity {
                    command: "ACL",
                    subcommand: sub.to_string(),
                }
                .to_resp();
            }
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("HELP") {
            if argv.len() != 2 {
                return CommandError::WrongSubcommandArity {
                    command: "ACL",
                    subcommand: "HELP".to_string(),
                }
                .to_resp();
            }
            self.handle_acl_help()
        } else if sub.eq_ignore_ascii_case("DRYRUN") {
            // ACL DRYRUN <username> <command> [<arg> ...]
            if argv.len() < 4 {
                return CommandError::WrongSubcommandArity {
                    command: "ACL",
                    subcommand: sub.to_string(),
                }
                .to_resp();
            }
            let username = &argv[2];
            let user = self.server.auth_state.get_user(username);
            match user {
                Some(u) => {
                    let cmd_name = String::from_utf8_lossy(&argv[3]);
                    if u.is_command_allowed(&cmd_name) {
                        RespFrame::SimpleString("OK".to_string())
                    } else {
                        RespFrame::Error(format!(
                            "ERR User '{}' has no permissions to run the '{}' command",
                            String::from_utf8_lossy(username),
                            cmd_name
                        ))
                    }
                }
                None => RespFrame::Error(format!(
                    "ERR User '{}' not found",
                    String::from_utf8_lossy(username)
                )),
            }
        } else {
            CommandError::UnknownSubcommand {
                command: "ACL",
                subcommand: sub.to_string(),
            }
            .to_resp()
        }
    }

    fn handle_acl_whoami(&self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 2 {
            return CommandError::WrongSubcommandArity {
                command: "ACL",
                subcommand: "WHOAMI".to_string(),
            }
            .to_resp();
        }
        let username = self.session.current_user_name();
        RespFrame::BulkString(Some(username.to_vec()))
    }

    fn handle_acl_list(&self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 2 {
            return CommandError::WrongSubcommandArity {
                command: "ACL",
                subcommand: "LIST".to_string(),
            }
            .to_resp();
        }
        let entries = self.server.auth_state.acl_list_entries();
        RespFrame::Array(Some(
            entries
                .into_iter()
                .map(|e| RespFrame::BulkString(Some(e.into_bytes())))
                .collect(),
        ))
    }

    fn handle_acl_users(&self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 2 {
            return CommandError::WrongSubcommandArity {
                command: "ACL",
                subcommand: "USERS".to_string(),
            }
            .to_resp();
        }
        let names = self.server.auth_state.user_names();
        RespFrame::Array(Some(
            names
                .into_iter()
                .map(|n| RespFrame::BulkString(Some(n.to_vec())))
                .collect(),
        ))
    }

    fn handle_acl_setuser(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() < 3 {
            return CommandError::WrongSubcommandArity {
                command: "ACL",
                subcommand: "SETUSER".to_string(),
            }
            .to_resp();
        }
        let username = argv[2].clone();
        let rules: Vec<&[u8]> = argv[3..].iter().map(Vec::as_slice).collect();
        match self.server.auth_state.set_user(username, &rules) {
            Ok(()) => RespFrame::SimpleString("OK".to_string()),
            Err(msg) => RespFrame::Error(msg),
        }
    }

    fn handle_acl_deluser(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() < 3 {
            return CommandError::WrongSubcommandArity {
                command: "ACL",
                subcommand: "DELUSER".to_string(),
            }
            .to_resp();
        }
        let mut deleted = 0i64;
        for username in &argv[2..] {
            if username.as_slice() == DEFAULT_AUTH_USER {
                return RespFrame::Error("ERR The 'default' user cannot be removed".to_string());
            }
            if self.server.auth_state.del_user(username) {
                deleted += 1;
            }
        }
        RespFrame::Integer(deleted)
    }

    fn handle_acl_getuser(&self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 3 {
            return CommandError::WrongSubcommandArity {
                command: "ACL",
                subcommand: "GETUSER".to_string(),
            }
            .to_resp();
        }
        let Some(user) = self.server.auth_state.get_user(&argv[2]) else {
            return RespFrame::BulkString(None);
        };
        let flags_str = if user.enabled {
            if user.nopass { "on nopass" } else { "on" }
        } else {
            "off"
        };
        let commands_str = user.commands_string();
        let keys_str = if user.all_keys { "~*" } else { "" };
        let channels_str = if user.all_channels { "&*" } else { "" };
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
            RespFrame::BulkString(Some(commands_str.into_bytes())),
            RespFrame::BulkString(Some(b"keys".to_vec())),
            RespFrame::BulkString(Some(keys_str.as_bytes().to_vec())),
            RespFrame::BulkString(Some(b"channels".to_vec())),
            RespFrame::BulkString(Some(channels_str.as_bytes().to_vec())),
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
                Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
            };
            if CATEGORIES.iter().any(|c| c.eq_ignore_ascii_case(cat)) {
                let cmds = commands_in_acl_category(cat);
                RespFrame::Array(Some(
                    cmds.iter()
                        .map(|c| RespFrame::BulkString(Some(c.as_bytes().to_vec())))
                        .collect(),
                ))
            } else {
                RespFrame::Error(format!("ERR Unknown ACL cat category '{cat}'"))
            }
        } else {
            CommandError::WrongSubcommandArity {
                command: "ACL",
                subcommand: "CAT".to_string(),
            }
            .to_resp()
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
            return CommandError::WrongSubcommandArity {
                command: "ACL",
                subcommand: "GENPASS".to_string(),
            }
            .to_resp();
        };

        let hex_chars = bits.div_ceil(4);
        let bytes_needed = hex_chars.div_ceil(2);
        let mut buf = vec![0u8; bytes_needed];
        // Read from /dev/urandom for cryptographic-quality randomness.
        // Fall back to PRNG seeded from packet counter + timestamp if unavailable.
        if std::fs::File::open("/dev/urandom")
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
            .is_err()
        {
            let seed = PACKET_COUNTER.load(Ordering::Relaxed).wrapping_add(
                std::time::SystemTime::UNIX_EPOCH
                    .elapsed()
                    .map_or(0, |d| d.as_nanos() as u64),
            );
            let mut state = seed.wrapping_mul(0x5851_f42d_4c95_7f2d).wrapping_add(1);
            for byte in &mut buf {
                state = state.wrapping_mul(0x5851_f42d_4c95_7f2d).wrapping_add(1);
                *byte = (state >> 33) as u8;
            }
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
                Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
            };
            if sub.eq_ignore_ascii_case("RESET") {
                RespFrame::SimpleString("OK".to_string())
            } else {
                match sub.parse::<i64>() {
                    Ok(_) => RespFrame::Array(Some(Vec::new())),
                    Err(_) => CommandError::UnknownSubcommand {
                        command: "ACL",
                        subcommand: "LOG".to_string(),
                    }
                    .to_resp(),
                }
            }
        } else {
            CommandError::WrongSubcommandArity {
                command: "ACL",
                subcommand: "LOG".to_string(),
            }
            .to_resp()
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
            return CommandError::WrongArity("CONFIG").to_resp();
        }
        let sub = match std::str::from_utf8(&argv[1]) {
            Ok(sub) => sub,
            Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
        };
        if sub.eq_ignore_ascii_case("GET") {
            return self.handle_config_get(argv);
        }
        if sub.eq_ignore_ascii_case("SET") {
            return self.handle_config_set(argv);
        }
        if sub.eq_ignore_ascii_case("HELP") {
            return self.handle_config_help(argv);
        }
        if sub.eq_ignore_ascii_case("RESETSTAT") {
            if argv.len() != 2 {
                return CommandError::WrongSubcommandArity {
                    command: "CONFIG",
                    subcommand: "RESETSTAT".to_string(),
                }
                .to_resp();
            }
            // Reset tracked statistics: slowlog, command counters, connection counters
            self.server.reset_slowlog();
            self.server.store.stat_total_commands_processed = 0;
            self.server.store.stat_total_connections_received = 0;
            return RespFrame::SimpleString("OK".to_string());
        }
        if sub.eq_ignore_ascii_case("REWRITE") {
            if argv.len() != 2 {
                return CommandError::WrongSubcommandArity {
                    command: "CONFIG",
                    subcommand: "REWRITE".to_string(),
                }
                .to_resp();
            }
            return RespFrame::SimpleString("OK".to_string());
        }
        CommandError::UnknownSubcommand {
            command: "CONFIG",
            subcommand: sub.to_string(),
        }
        .to_resp()
    }

    fn handle_config_help(&self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 2 {
            return CommandError::WrongSubcommandArity {
                command: "CONFIG",
                subcommand: "HELP".to_string(),
            }
            .to_resp();
        }
        RespFrame::Array(Some(Self::config_help_lines()))
    }

    fn config_help_lines() -> Vec<RespFrame> {
        fn bulk(text: &str) -> RespFrame {
            RespFrame::BulkString(Some(text.as_bytes().to_vec()))
        }

        vec![
            bulk("CONFIG <subcommand> [<arg> [value] [opt] ...]. Subcommands are:"),
            bulk("GET <pattern> [<pattern> ...]"),
            bulk("    Return configuration parameters matching the specified patterns."),
            bulk("SET <parameter> <value> [<parameter> <value> ...]"),
            bulk("    Set configuration parameters to the specified values."),
            bulk("RESETSTAT"),
            bulk("    Reset statistics reported by INFO."),
            bulk("REWRITE"),
            bulk("    Rewrite the configuration file with the current in-memory settings."),
            bulk("HELP"),
            bulk("    Print this help."),
        ]
    }

    fn handle_config_get(&self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() < 3 {
            return CommandError::WrongSubcommandArity {
                command: "CONFIG",
                subcommand: "GET".to_string(),
            }
            .to_resp();
        }
        let mut entries = Vec::new();
        // Redis 7+ supports multiple patterns: CONFIG GET pattern1 pattern2 ...
        for arg in &argv[2..] {
            let raw_pattern = match std::str::from_utf8(arg) {
                Ok(pattern) => pattern,
                Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
            };
            let pattern = raw_pattern.to_ascii_lowercase();
            self.collect_config_entries(&pattern, &mut entries);
        }
        RespFrame::Array(Some(entries))
    }

    /// Collect all config parameter entries matching a single pattern.
    fn collect_config_entries(&self, pattern: &str, entries: &mut Vec<RespFrame>) {
        if Self::config_pattern_matches(pattern, "requirepass") {
            entries.push(RespFrame::BulkString(Some(b"requirepass".to_vec())));
            entries.push(RespFrame::BulkString(Some(
                self.server
                    .auth_state
                    .requirepass()
                    .unwrap_or_default()
                    .to_vec(),
            )));
        }
        if Self::config_pattern_matches(pattern, "acllog-max-len") {
            entries.push(RespFrame::BulkString(Some(b"acllog-max-len".to_vec())));
            entries.push(RespFrame::BulkString(Some(
                self.server.acllog_max_len.to_string().into_bytes(),
            )));
        }
        // Dynamic maxmemory — override the static default
        if Self::config_pattern_matches(pattern, "maxmemory") {
            entries.push(RespFrame::BulkString(Some(b"maxmemory".to_vec())));
            entries.push(RespFrame::BulkString(Some(
                self.server.maxmemory_bytes.to_string().into_bytes(),
            )));
        }
        // Dynamic slowlog params — override the static defaults
        if Self::config_pattern_matches(pattern, "slowlog-log-slower-than") {
            entries.push(RespFrame::BulkString(Some(
                b"slowlog-log-slower-than".to_vec(),
            )));
            entries.push(RespFrame::BulkString(Some(
                self.server
                    .slowlog_log_slower_than_us
                    .to_string()
                    .into_bytes(),
            )));
        }
        if Self::config_pattern_matches(pattern, "slowlog-max-len") {
            entries.push(RespFrame::BulkString(Some(b"slowlog-max-len".to_vec())));
            entries.push(RespFrame::BulkString(Some(
                self.server.slowlog_max_len.to_string().into_bytes(),
            )));
        }
        if Self::config_pattern_matches(pattern, "maxclients") {
            entries.push(RespFrame::BulkString(Some(b"maxclients".to_vec())));
            entries.push(RespFrame::BulkString(Some(
                self.server.max_clients.to_string().into_bytes(),
            )));
        }
        if Self::config_pattern_matches(pattern, "repl-backlog-size") {
            entries.push(RespFrame::BulkString(Some(b"repl-backlog-size".to_vec())));
            entries.push(RespFrame::BulkString(Some(
                self.server.repl_backlog_size.to_string().into_bytes(),
            )));
        }
        if Self::config_pattern_matches(pattern, "repl-timeout") {
            entries.push(RespFrame::BulkString(Some(b"repl-timeout".to_vec())));
            entries.push(RespFrame::BulkString(Some(
                self.server.repl_timeout_sec.to_string().into_bytes(),
            )));
        }
        if Self::config_pattern_matches(pattern, "maxmemory-samples") {
            entries.push(RespFrame::BulkString(Some(b"maxmemory-samples".to_vec())));
            entries.push(RespFrame::BulkString(Some(
                self.server
                    .maxmemory_eviction_sample_limit
                    .to_string()
                    .into_bytes(),
            )));
        }
        if Self::config_pattern_matches(pattern, "busy-reply-threshold") {
            entries.push(RespFrame::BulkString(Some(
                b"busy-reply-threshold".to_vec(),
            )));
            entries.push(RespFrame::BulkString(Some(
                self.server.command_time_budget_ms.to_string().into_bytes(),
            )));
        }
        if Self::config_pattern_matches(pattern, "lua-time-limit") {
            entries.push(RespFrame::BulkString(Some(b"lua-time-limit".to_vec())));
            entries.push(RespFrame::BulkString(Some(
                self.server.command_time_budget_ms.to_string().into_bytes(),
            )));
        }
        if Self::config_pattern_matches(pattern, "client-query-buffer-limit") {
            entries.push(RespFrame::BulkString(Some(
                b"client-query-buffer-limit".to_vec(),
            )));
            entries.push(RespFrame::BulkString(Some(
                self.server.query_buffer_limit.to_string().into_bytes(),
            )));
        }
        if Self::config_pattern_matches(pattern, "proto-max-bulk-len") {
            entries.push(RespFrame::BulkString(Some(b"proto-max-bulk-len".to_vec())));
            entries.push(RespFrame::BulkString(Some(
                self.server.proto_max_bulk_len.to_string().into_bytes(),
            )));
        }
        if Self::config_pattern_matches(pattern, "client-output-buffer-limit") {
            entries.push(RespFrame::BulkString(Some(
                b"client-output-buffer-limit".to_vec(),
            )));
            entries.push(RespFrame::BulkString(Some(
                self.server.output_buffer_limit.to_string().into_bytes(),
            )));
        }
        // Dynamic hz — override the static default
        if Self::config_pattern_matches(pattern, "hz") {
            entries.push(RespFrame::BulkString(Some(b"hz".to_vec())));
            entries.push(RespFrame::BulkString(Some(
                self.server.hz.to_string().into_bytes(),
            )));
        }
        // Dynamic encoding thresholds — live values from Store
        let encoding_params: &[(&str, usize)] = &[
            (
                "hash-max-listpack-entries",
                self.server.store.hash_max_listpack_entries,
            ),
            (
                "hash-max-listpack-value",
                self.server.store.hash_max_listpack_value,
            ),
            (
                "list-max-listpack-entries",
                self.server.store.list_max_listpack_entries,
            ),
            (
                "list-max-listpack-value",
                self.server.store.list_max_listpack_value,
            ),
            (
                "set-max-intset-entries",
                self.server.store.set_max_intset_entries,
            ),
            (
                "set-max-listpack-entries",
                self.server.store.set_max_listpack_entries,
            ),
            (
                "zset-max-listpack-entries",
                self.server.store.zset_max_listpack_entries,
            ),
            (
                "zset-max-listpack-value",
                self.server.store.zset_max_listpack_value,
            ),
        ];
        for &(name, value) in encoding_params {
            if Self::config_pattern_matches(pattern, name) {
                entries.push(RespFrame::BulkString(Some(name.as_bytes().to_vec())));
                entries.push(RespFrame::BulkString(Some(value.to_string().into_bytes())));
            }
        }
        // Dynamic list encoding threshold — live value from Store (signed)
        if Self::config_pattern_matches(pattern, "list-max-listpack-size") {
            entries.push(RespFrame::BulkString(Some(
                b"list-max-listpack-size".to_vec(),
            )));
            entries.push(RespFrame::BulkString(Some(
                self.server
                    .store
                    .list_max_listpack_size
                    .to_string()
                    .into_bytes(),
            )));
        }
        if Self::config_pattern_matches(pattern, "list-max-ziplist-size") {
            entries.push(RespFrame::BulkString(Some(
                b"list-max-ziplist-size".to_vec(),
            )));
            entries.push(RespFrame::BulkString(Some(
                self.server
                    .store
                    .list_max_listpack_size
                    .to_string()
                    .into_bytes(),
            )));
        }
        // Dynamic maxmemory-policy — live value from Store
        if Self::config_pattern_matches(pattern, "maxmemory-policy") {
            entries.push(RespFrame::BulkString(Some(b"maxmemory-policy".to_vec())));
            entries.push(RespFrame::BulkString(Some(
                self.server
                    .store
                    .maxmemory_policy
                    .as_config_str()
                    .as_bytes()
                    .to_vec(),
            )));
        }
        if Self::config_pattern_matches(pattern, "appendonly") {
            entries.push(RespFrame::BulkString(Some(b"appendonly".to_vec())));
            entries.push(RespFrame::BulkString(Some(
                if self.server.aof_path.is_some() {
                    b"yes".to_vec()
                } else {
                    b"no".to_vec()
                },
            )));
        }
        if Self::config_pattern_matches(pattern, "appendfilename") {
            entries.push(RespFrame::BulkString(Some(b"appendfilename".to_vec())));
            let filename = self
                .server
                .aof_config_path
                .as_ref()
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "appendonly.aof".to_string());
            entries.push(RespFrame::BulkString(Some(filename.into_bytes())));
        }
        if Self::config_pattern_matches(pattern, "appenddirname") {
            entries.push(RespFrame::BulkString(Some(b"appenddirname".to_vec())));
            let dirname = self
                .server
                .aof_config_path
                .as_ref()
                .and_then(|path| path.parent())
                .map(|path| path.to_string_lossy().into_owned())
                .filter(|path| !path.is_empty())
                .or_else(|| {
                    self.server
                        .aof_config_path
                        .as_ref()
                        .map(|_| ".".to_string())
                })
                .unwrap_or_else(|| "appendonlydir".to_string());
            entries.push(RespFrame::BulkString(Some(dirname.into_bytes())));
        }
        if Self::config_pattern_matches(pattern, "dbfilename") {
            entries.push(RespFrame::BulkString(Some(b"dbfilename".to_vec())));
            let filename = self
                .server
                .rdb_path
                .as_ref()
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "dump.rdb".to_string());
            entries.push(RespFrame::BulkString(Some(filename.into_bytes())));
        }
        if Self::config_pattern_matches(pattern, "dir") {
            entries.push(RespFrame::BulkString(Some(b"dir".to_vec())));
            let dirname = self
                .server
                .rdb_path
                .as_ref()
                .and_then(|path| path.parent())
                .map(|path| path.to_string_lossy().into_owned())
                .filter(|path| !path.is_empty())
                .unwrap_or_else(|| ".".to_string());
            entries.push(RespFrame::BulkString(Some(dirname.into_bytes())));
        }
        // Also emit ziplist aliases from Store (they alias the same live values).
        let ziplist_aliases: &[(&str, usize)] = &[
            (
                "hash-max-ziplist-entries",
                self.server.store.hash_max_listpack_entries,
            ),
            (
                "hash-max-ziplist-value",
                self.server.store.hash_max_listpack_value,
            ),
            (
                "zset-max-ziplist-entries",
                self.server.store.zset_max_listpack_entries,
            ),
            (
                "zset-max-ziplist-value",
                self.server.store.zset_max_listpack_value,
            ),
        ];
        for &(name, value) in ziplist_aliases {
            if Self::config_pattern_matches(pattern, name) {
                entries.push(RespFrame::BulkString(Some(name.as_bytes().to_vec())));
                entries.push(RespFrame::BulkString(Some(value.to_string().into_bytes())));
            }
        }
        // Static configuration parameters that clients commonly probe.
        // If a parameter has been overridden via CONFIG SET, use the override.
        for &(name, default_value) in CONFIG_STATIC_PARAMS {
            // Skip dynamically-managed params — we already emitted live values above
            if name == "maxmemory"
                || name == "maxmemory-policy"
                || name == "slowlog-log-slower-than"
                || name == "slowlog-max-len"
                || name == "hz"
                || name == "hash-max-listpack-entries"
                || name == "hash-max-listpack-value"
                || name == "hash-max-ziplist-entries"
                || name == "hash-max-ziplist-value"
                || name == "set-max-intset-entries"
                || name == "set-max-listpack-entries"
                || name == "zset-max-listpack-entries"
                || name == "zset-max-listpack-value"
                || name == "zset-max-ziplist-entries"
                || name == "zset-max-ziplist-value"
                || name == "list-max-listpack-size"
                || name == "list-max-ziplist-size"
                || name == "list-max-listpack-entries"
                || name == "list-max-listpack-value"
                || name == "appendonly"
                || name == "appendfilename"
                || name == "appenddirname"
                || name == "dbfilename"
                || name == "dir"
                || name == "maxclients"
                || name == "busy-reply-threshold"
                || name == "lua-time-limit"
                || name == "maxmemory-samples"
                || name == "repl-backlog-size"
                || name == "repl-timeout"
                || name == "client-query-buffer-limit"
                || name == "proto-max-bulk-len"
                || name == "client-output-buffer-limit"
            {
                continue;
            }
            if Self::config_pattern_matches(pattern, name) {
                entries.push(RespFrame::BulkString(Some(name.as_bytes().to_vec())));
                let value = self
                    .server
                    .config_overrides
                    .get(name)
                    .map(|v| v.as_bytes().to_vec())
                    .unwrap_or_else(|| default_value.as_bytes().to_vec());
                entries.push(RespFrame::BulkString(Some(value)));
            }
        }
    }

    fn handle_config_set(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() < 4 || !argv.len().is_multiple_of(2) {
            return CommandError::WrongSubcommandArity {
                command: "CONFIG",
                subcommand: "SET".to_string(),
            }
            .to_resp();
        }

        let mut next_requirepass: Option<Option<Vec<u8>>> = None;
        let mut next_acllog_max_len = self.server.acllog_max_len;
        let mut next_maxmemory: Option<usize> = None;
        let mut next_maxmemory_policy: Option<MaxmemoryPolicy> = None;
        let mut next_slowlog_slower_than: Option<i64> = None;
        let mut next_slowlog_max_len: Option<usize> = None;
        let mut next_hz: Option<u64> = None;
        let mut next_maxclients: Option<usize> = None;
        let mut next_repl_backlog_size: Option<u64> = None;
        let mut next_repl_timeout: Option<u64> = None;
        let mut next_query_buffer_limit: Option<usize> = None;
        let mut next_proto_max_bulk_len: Option<usize> = None;
        let mut next_output_buffer_limit: Option<usize> = None;
        let mut next_maxmemory_samples: Option<usize> = None;
        let mut next_command_time_budget: Option<u64> = None;
        let mut next_appendonly: Option<bool> = None;
        let mut next_keyspace_events: Option<u32> = None;
        let mut next_list_max_listpack_size: Option<i64> = None;
        let mut next_rdb_path = self
            .server
            .rdb_path
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from(".").join("dump.rdb"));
        let mut rdb_path_changed = false;
        let mut encoding_threshold_updates: Vec<(&str, usize)> = Vec::new();
        let mut static_override_updates: Vec<(String, String)> = Vec::new();

        for pair in argv[2..].chunks_exact(2) {
            let parameter = match std::str::from_utf8(&pair[0]) {
                Ok(parameter) => parameter,
                Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
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
                    Err(err) => return err.to_resp(),
                };
                next_acllog_max_len = parsed;
                continue;
            }
            if parameter.eq_ignore_ascii_case("maxmemory") {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) if value >= 0 => value as usize,
                    Ok(_) => {
                        return RespFrame::Error(format!(
                            "ERR Invalid argument '{}' for CONFIG SET 'maxmemory'",
                            String::from_utf8_lossy(&pair[1])
                        ));
                    }
                    Err(err) => return err.to_resp(),
                };
                next_maxmemory = Some(parsed);
                continue;
            }
            if parameter.eq_ignore_ascii_case("maxmemory-policy") {
                let value_str = match std::str::from_utf8(&pair[1]) {
                    Ok(value) => value,
                    Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
                };
                match MaxmemoryPolicy::from_config_str(value_str) {
                    Some(policy) => {
                        next_maxmemory_policy = Some(policy);
                    }
                    None => {
                        return RespFrame::Error(format!(
                            "ERR Invalid argument '{value_str}' for CONFIG SET 'maxmemory-policy'"
                        ));
                    }
                }
                continue;
            }
            if parameter.eq_ignore_ascii_case("slowlog-log-slower-than") {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) => value,
                    Err(err) => return err.to_resp(),
                };
                next_slowlog_slower_than = Some(parsed);
                continue;
            }
            if parameter.eq_ignore_ascii_case("slowlog-max-len") {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) if value >= 0 => value as usize,
                    Ok(_) => {
                        return RespFrame::Error(format!(
                            "ERR Invalid argument '{}' for CONFIG SET 'slowlog-max-len'",
                            String::from_utf8_lossy(&pair[1])
                        ));
                    }
                    Err(err) => return err.to_resp(),
                };
                next_slowlog_max_len = Some(parsed);
                continue;
            }
            if parameter.eq_ignore_ascii_case("hz") {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) if (1..=500).contains(&value) => value as u64,
                    Ok(_) => {
                        return RespFrame::Error(format!(
                            "ERR Invalid argument '{}' for CONFIG SET 'hz'",
                            String::from_utf8_lossy(&pair[1])
                        ));
                    }
                    Err(err) => return err.to_resp(),
                };
                next_hz = Some(parsed);
                continue;
            }
            if parameter.eq_ignore_ascii_case("maxclients") {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) if value >= 1 => value as usize,
                    Ok(_) => {
                        return RespFrame::Error(
                            "ERR Invalid argument for CONFIG SET 'maxclients'".to_string(),
                        );
                    }
                    Err(err) => return err.to_resp(),
                };
                next_maxclients = Some(parsed);
                continue;
            }
            if parameter.eq_ignore_ascii_case("repl-backlog-size") {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) if value >= 0 => value as u64,
                    Ok(_) => {
                        return RespFrame::Error(
                            "ERR Invalid argument for CONFIG SET 'repl-backlog-size'".to_string(),
                        );
                    }
                    Err(err) => return err.to_resp(),
                };
                next_repl_backlog_size = Some(parsed);
                static_override_updates.push(("repl-backlog-size".to_string(), parsed.to_string()));
                continue;
            }
            if parameter.eq_ignore_ascii_case("repl-timeout") {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) if value >= 1 => value as u64,
                    Ok(_) => {
                        return RespFrame::Error(
                            "ERR Invalid argument for CONFIG SET 'repl-timeout'".to_string(),
                        );
                    }
                    Err(err) => return err.to_resp(),
                };
                next_repl_timeout = Some(parsed);
                static_override_updates.push(("repl-timeout".to_string(), parsed.to_string()));
                continue;
            }
            if parameter.eq_ignore_ascii_case("client-query-buffer-limit") {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) if value >= 0 => value as usize,
                    Ok(_) => {
                        return RespFrame::Error(
                            "ERR Invalid argument for CONFIG SET 'client-query-buffer-limit'"
                                .to_string(),
                        );
                    }
                    Err(err) => return err.to_resp(),
                };
                next_query_buffer_limit = Some(parsed);
                static_override_updates
                    .push(("client-query-buffer-limit".to_string(), parsed.to_string()));
                continue;
            }
            if parameter.eq_ignore_ascii_case("proto-max-bulk-len") {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) if value >= 0 => value as usize,
                    Ok(_) => {
                        return RespFrame::Error(
                            "ERR Invalid argument for CONFIG SET 'proto-max-bulk-len'".to_string(),
                        );
                    }
                    Err(err) => return err.to_resp(),
                };
                next_proto_max_bulk_len = Some(parsed);
                static_override_updates
                    .push(("proto-max-bulk-len".to_string(), parsed.to_string()));
                continue;
            }
            if parameter.eq_ignore_ascii_case("client-output-buffer-limit") {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) if value >= 0 => value as usize,
                    Ok(_) => {
                        return RespFrame::Error(
                            "ERR Invalid argument for CONFIG SET 'client-output-buffer-limit'"
                                .to_string(),
                        );
                    }
                    Err(err) => return err.to_resp(),
                };
                next_output_buffer_limit = Some(parsed);
                static_override_updates
                    .push(("client-output-buffer-limit".to_string(), parsed.to_string()));
                continue;
            }
            if parameter.eq_ignore_ascii_case("maxmemory-samples") {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) if value >= 1 => value as usize,
                    Ok(_) => {
                        return RespFrame::Error(
                            "ERR Invalid argument for CONFIG SET 'maxmemory-samples'".to_string(),
                        );
                    }
                    Err(err) => return err.to_resp(),
                };
                next_maxmemory_samples = Some(parsed);
                static_override_updates.push(("maxmemory-samples".to_string(), parsed.to_string()));
                continue;
            }
            if parameter.eq_ignore_ascii_case("busy-reply-threshold")
                || parameter.eq_ignore_ascii_case("lua-time-limit")
            {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) if value >= 0 => value as u64,
                    Ok(_) => {
                        return RespFrame::Error(
                            "ERR Invalid argument for CONFIG SET 'busy-reply-threshold'"
                                .to_string(),
                        );
                    }
                    Err(err) => return err.to_resp(),
                };
                next_command_time_budget = Some(parsed);
                continue;
            }
            if parameter.eq_ignore_ascii_case("appendonly") {
                let value_str = match std::str::from_utf8(&pair[1]) {
                    Ok(value) => value,
                    Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
                };
                next_appendonly = Some(if value_str.eq_ignore_ascii_case("yes") {
                    true
                } else if value_str.eq_ignore_ascii_case("no") {
                    false
                } else {
                    return RespFrame::Error(format!(
                        "ERR Invalid argument '{value_str}' for CONFIG SET 'appendonly'"
                    ));
                });
                continue;
            }
            if parameter.eq_ignore_ascii_case("appendfilename")
                || parameter.eq_ignore_ascii_case("appenddirname")
                || parameter.eq_ignore_ascii_case("bind")
                || parameter.eq_ignore_ascii_case("port")
            {
                return RespFrame::Error(format!(
                    "ERR CONFIG SET failed (possibly related to argument '{parameter}') - can't set immutable config"
                ));
            }
            if parameter.eq_ignore_ascii_case("dir") {
                let value_str = match std::str::from_utf8(&pair[1]) {
                    Ok(value) => value,
                    Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
                };
                let dir_path = std::path::Path::new(value_str);
                match std::fs::metadata(dir_path) {
                    Ok(metadata) if metadata.is_dir() => {}
                    Ok(_) => {
                        return RespFrame::Error(
                            "ERR CONFIG SET failed (possibly related to argument 'dir') - Not a directory"
                                .to_string(),
                        );
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        return RespFrame::Error(
                            "ERR CONFIG SET failed (possibly related to argument 'dir') - No such file or directory"
                                .to_string(),
                        );
                    }
                    Err(err) => {
                        return RespFrame::Error(format!(
                            "ERR CONFIG SET failed (possibly related to argument 'dir') - {err}"
                        ));
                    }
                }
                let filename = next_rdb_path
                    .file_name()
                    .map(|name| name.to_os_string())
                    .unwrap_or_else(|| std::ffi::OsString::from("dump.rdb"));
                next_rdb_path = std::path::PathBuf::from(value_str).join(filename);
                rdb_path_changed = true;
                continue;
            }
            if parameter.eq_ignore_ascii_case("dbfilename") {
                let value_str = match std::str::from_utf8(&pair[1]) {
                    Ok(value) => value,
                    Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
                };
                let value_path = std::path::Path::new(value_str);
                if value_path.components().count() != 1 || value_path.file_name().is_none() {
                    return RespFrame::Error(
                        "ERR CONFIG SET failed (possibly related to argument 'dbfilename') - dbfilename can't be a path, just a filename"
                            .to_string(),
                    );
                }
                let parent = next_rdb_path
                    .parent()
                    .map(std::path::Path::to_path_buf)
                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                next_rdb_path = parent.join(value_str);
                rdb_path_changed = true;
                continue;
            }
            // List encoding threshold — accepts negative values (-1 to -5 for byte limits).
            // Keyspace notifications config
            if parameter.eq_ignore_ascii_case("notify-keyspace-events") {
                let value_str = std::str::from_utf8(&pair[1]).unwrap_or("");
                match fr_store::keyspace_events_parse(value_str) {
                    Some(flags) => {
                        // Defer application to after all params are validated
                        static_override_updates
                            .push(("notify-keyspace-events".to_string(), value_str.to_string()));
                        // Store flags for deferred application
                        next_keyspace_events = Some(flags);
                    }
                    None => {
                        return RespFrame::Error(
                            "ERR Invalid argument for CONFIG SET 'notify-keyspace-events'"
                                .to_string(),
                        );
                    }
                }
                continue;
            }
            if parameter.eq_ignore_ascii_case("list-max-listpack-size")
                || parameter.eq_ignore_ascii_case("list-max-ziplist-size")
            {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) if (-5..=i64::MAX).contains(&value) => value,
                    Ok(_) | Err(_) => {
                        return RespFrame::Error(format!(
                            "ERR Invalid argument '{}' for CONFIG SET '{parameter}'",
                            String::from_utf8_lossy(&pair[1])
                        ));
                    }
                };
                next_list_max_listpack_size = Some(parsed);
                continue;
            }
            // Encoding threshold parameters — update Store fields for live effect.
            if parameter.eq_ignore_ascii_case("hash-max-listpack-entries")
                || parameter.eq_ignore_ascii_case("hash-max-listpack-value")
                || parameter.eq_ignore_ascii_case("hash-max-ziplist-entries")
                || parameter.eq_ignore_ascii_case("hash-max-ziplist-value")
                || parameter.eq_ignore_ascii_case("list-max-listpack-entries")
                || parameter.eq_ignore_ascii_case("list-max-listpack-value")
                || parameter.eq_ignore_ascii_case("set-max-intset-entries")
                || parameter.eq_ignore_ascii_case("set-max-listpack-entries")
                || parameter.eq_ignore_ascii_case("zset-max-listpack-entries")
                || parameter.eq_ignore_ascii_case("zset-max-listpack-value")
                || parameter.eq_ignore_ascii_case("zset-max-ziplist-entries")
                || parameter.eq_ignore_ascii_case("zset-max-ziplist-value")
            {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) if value >= 0 => value as usize,
                    Ok(_) | Err(_) => {
                        return RespFrame::Error(format!(
                            "ERR Invalid argument '{}' for CONFIG SET '{parameter}'",
                            String::from_utf8_lossy(&pair[1])
                        ));
                    }
                };
                // Normalize ziplist aliases to their listpack equivalents.
                let canonical: &'static str = if parameter
                    .eq_ignore_ascii_case("hash-max-listpack-entries")
                    || parameter.eq_ignore_ascii_case("hash-max-ziplist-entries")
                {
                    "hash-max-listpack-entries"
                } else if parameter.eq_ignore_ascii_case("hash-max-listpack-value")
                    || parameter.eq_ignore_ascii_case("hash-max-ziplist-value")
                {
                    "hash-max-listpack-value"
                } else if parameter.eq_ignore_ascii_case("list-max-listpack-entries") {
                    "list-max-listpack-entries"
                } else if parameter.eq_ignore_ascii_case("list-max-listpack-value") {
                    "list-max-listpack-value"
                } else if parameter.eq_ignore_ascii_case("set-max-intset-entries") {
                    "set-max-intset-entries"
                } else if parameter.eq_ignore_ascii_case("set-max-listpack-entries") {
                    "set-max-listpack-entries"
                } else if parameter.eq_ignore_ascii_case("zset-max-listpack-entries")
                    || parameter.eq_ignore_ascii_case("zset-max-ziplist-entries")
                {
                    "zset-max-listpack-entries"
                } else {
                    // zset-max-listpack-value or zset-max-ziplist-value
                    "zset-max-listpack-value"
                };
                encoding_threshold_updates.push((canonical, parsed));
                continue;
            }
            // Accept known CONFIG parameters and store the overridden value so
            // CONFIG GET returns the SET value rather than the compiled-in default.
            let is_known_param = CONFIG_STATIC_PARAMS
                .iter()
                .any(|&(name, _)| name.eq_ignore_ascii_case(parameter));
            if is_known_param {
                // Safety: is_known_param guarantees the find will succeed, but
                // use expect() to make intent explicit if invariant breaks.
                let canonical = CONFIG_STATIC_PARAMS
                    .iter()
                    .find(|&&(name, _)| name.eq_ignore_ascii_case(parameter))
                    .map(|&(name, _)| name.to_string())
                    .expect("is_known_param was true but find returned None");
                let value = String::from_utf8_lossy(&pair[1]).to_string();
                static_override_updates.push((canonical, value));
                continue;
            }
            return RespFrame::Error(format!("ERR Unsupported CONFIG parameter '{parameter}'"));
        }

        if let Some(requirepass) = next_requirepass {
            // CONFIG SET requirepass should bridge ACL defaults without dropping this session.
            self.apply_requirepass_update(requirepass, true);
        }
        self.server.acllog_max_len = next_acllog_max_len;
        if let Some(maxmemory) = next_maxmemory {
            self.server.maxmemory_bytes = maxmemory;
        }
        if let Some(maxmemory_policy) = next_maxmemory_policy {
            self.server.store.maxmemory_policy = maxmemory_policy;
        }
        if let Some(threshold) = next_slowlog_slower_than {
            self.server.slowlog_log_slower_than_us = threshold;
        }
        if let Some(max_len) = next_slowlog_max_len {
            self.server.slowlog_max_len = max_len;
            // Trim existing entries if the new max is smaller.
            while self.server.slowlog.len() > self.server.slowlog_max_len {
                self.server.slowlog.pop_front();
            }
        }
        if let Some(hz) = next_hz {
            self.server.hz = hz;
            self.server.store.server_hz = hz;
        }
        if let Some(mc) = next_maxclients {
            self.server.max_clients = mc;
            self.server.store.server_maxclients = mc as u64;
        }
        if let Some(backlog_size) = next_repl_backlog_size {
            self.server.repl_backlog_size = backlog_size;
            self.server.store.server_repl_backlog_size = backlog_size;
        }
        if let Some(repl_timeout) = next_repl_timeout {
            self.server.repl_timeout_sec = repl_timeout;
        }
        if let Some(query_buffer_limit) = next_query_buffer_limit {
            self.server.query_buffer_limit = query_buffer_limit;
        }
        if let Some(proto_max_bulk_len) = next_proto_max_bulk_len {
            self.server.proto_max_bulk_len = proto_max_bulk_len;
        }
        if let Some(output_buffer_limit) = next_output_buffer_limit {
            self.server.output_buffer_limit = output_buffer_limit;
        }
        if let Some(maxmemory_samples) = next_maxmemory_samples {
            self.server.maxmemory_eviction_sample_limit = maxmemory_samples;
        }
        if let Some(budget) = next_command_time_budget {
            self.server.command_time_budget_ms = budget;
        }
        if let Some(flags) = next_keyspace_events {
            self.server.store.notify_keyspace_events = flags;
        }
        if rdb_path_changed {
            self.server.rdb_path = Some(next_rdb_path);
        }
        if let Some(appendonly) = next_appendonly {
            if appendonly {
                let configured_path = self.server.aof_config_path.clone().unwrap_or_else(|| {
                    std::path::PathBuf::from("appendonlydir").join("appendonly.aof")
                });
                self.server.aof_config_path = Some(configured_path.clone());
                self.server.aof_path = Some(configured_path);
            } else {
                self.server.aof_path = None;
            }
        }
        if let Some(list_max_listpack_size) = next_list_max_listpack_size {
            self.server.store.list_max_listpack_size = list_max_listpack_size;
        }
        // Apply encoding threshold updates to Store and CONFIG GET state.
        for (param, value) in encoding_threshold_updates {
            match param {
                "hash-max-listpack-entries" => self.server.store.hash_max_listpack_entries = value,
                "hash-max-listpack-value" => self.server.store.hash_max_listpack_value = value,
                "list-max-listpack-entries" => self.server.store.list_max_listpack_entries = value,
                "list-max-listpack-value" => self.server.store.list_max_listpack_value = value,
                "set-max-intset-entries" => self.server.store.set_max_intset_entries = value,
                "set-max-listpack-entries" => self.server.store.set_max_listpack_entries = value,
                "zset-max-listpack-entries" => self.server.store.zset_max_listpack_entries = value,
                "zset-max-listpack-value" => self.server.store.zset_max_listpack_value = value,
                _ => {}
            }
            self.server
                .config_overrides
                .insert(param.to_string(), value.to_string());
        }
        for (param, value) in static_override_updates {
            self.server.config_overrides.insert(param, value);
        }
        RespFrame::SimpleString("OK".to_string())
    }

    fn config_pattern_matches(pattern: &str, parameter: &str) -> bool {
        glob_match(pattern.as_bytes(), parameter.as_bytes())
    }

    fn handle_asking_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 1 {
            return CommandError::WrongArity("ASKING").to_resp();
        }
        self.session.cluster_state.asking = true;
        RespFrame::SimpleString("OK".to_string())
    }

    fn handle_readonly_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 1 {
            return CommandError::WrongArity("READONLY").to_resp();
        }
        self.session.cluster_state.mode = ClusterClientMode::ReadOnly;
        RespFrame::SimpleString("OK".to_string())
    }

    fn handle_readwrite_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 1 {
            return CommandError::WrongArity("READWRITE").to_resp();
        }
        self.session.cluster_state.mode = ClusterClientMode::ReadWrite;
        self.session.cluster_state.asking = false;
        RespFrame::SimpleString("OK".to_string())
    }

    fn handle_client_command(&mut self, argv: &[Vec<u8>], now_ms: u64) -> RespFrame {
        if argv.len() < 2 {
            return CommandError::WrongArity("CLIENT").to_resp();
        }
        let sub = match std::str::from_utf8(&argv[1]) {
            Ok(s) => s,
            Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
        };
        if sub.eq_ignore_ascii_case("SETNAME") {
            if argv.len() != 3 {
                return CommandError::WrongArity("CLIENT").to_resp();
            }
            // Redis validates: name must not contain spaces or control chars (< 0x20)
            if argv[2].iter().any(|&b| b <= b' ') {
                return RespFrame::Error(
                    "ERR Client names cannot contain spaces, newlines or special characters."
                        .to_string(),
                );
            }
            if argv[2].is_empty() {
                self.session.client_name = None;
            } else {
                self.session.client_name = Some(argv[2].clone());
            }
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("GETNAME") {
            if argv.len() != 2 {
                return CommandError::WrongArity("CLIENT").to_resp();
            }
            match &self.session.client_name {
                Some(name) => RespFrame::BulkString(Some(name.clone())),
                None => RespFrame::BulkString(None),
            }
        } else if sub.eq_ignore_ascii_case("ID") {
            if argv.len() != 2 {
                return CommandError::WrongArity("CLIENT").to_resp();
            }
            RespFrame::Integer(self.session.client_id as i64)
        } else if sub.eq_ignore_ascii_case("LIST") || sub.eq_ignore_ascii_case("INFO") {
            if sub.eq_ignore_ascii_case("INFO") && argv.len() != 2 {
                return CommandError::WrongArity("CLIENT").to_resp();
            }
            let name_str = self
                .session
                .client_name
                .as_ref()
                .map(|n| String::from_utf8_lossy(n).to_string())
                .unwrap_or_default();
            let flags = if self.session.transaction_state.in_transaction {
                "x"
            } else {
                "N"
            };
            let multi_count = if self.session.transaction_state.in_transaction {
                self.session.transaction_state.command_queue.len() as i64
            } else {
                -1
            };
            let client_id = self.session.client_id;
            let channel_subs = self
                .server
                .pubsub_client_channels
                .get(&client_id)
                .map_or(0, HashSet::len);
            let pattern_subs = self
                .server
                .pubsub_client_patterns
                .get(&client_id)
                .map_or(0, HashSet::len);
            let shard_subs = self
                .server
                .pubsub_client_shard_channels
                .get(&client_id)
                .map_or(0, HashSet::len);
            let lib_name = self.session.client_lib_name.as_deref().unwrap_or("");
            let lib_ver = self.session.client_lib_ver.as_deref().unwrap_or("");
            let peer = self
                .session
                .peer_addr
                .map(|a| a.to_string())
                .unwrap_or_else(|| "127.0.0.1:0".to_string());
            let info_line = format!(
                "id={} addr={} laddr=127.0.0.1:{} fd=0 name={} db={} sub={} psub={} ssub={} multi={} watch={} qbuf=0 qbuf-free=0 obl=0 oll=0 omem=0 tot-mem=0 events=r cmd=client|{} user={} lib-name={} lib-ver={} resp={} flags={}\r\n",
                client_id,
                peer,
                self.server.store.server_port,
                name_str,
                self.session.selected_db,
                channel_subs,
                pattern_subs,
                shard_subs,
                multi_count,
                self.session.transaction_state.watched_keys.len(),
                sub.to_ascii_lowercase(),
                String::from_utf8_lossy(self.session.current_user_name()),
                lib_name,
                lib_ver,
                self.session.resp_protocol_version,
                flags,
            );
            if sub.eq_ignore_ascii_case("LIST") {
                let payload = if argv.len() == 2 {
                    info_line.into_bytes()
                } else if argv.len() == 4 && eq_ascii_token(&argv[2], b"TYPE") {
                    let client_id = self.session.client_id;
                    let has_subs = self
                        .server
                        .pubsub_channel_subs
                        .values()
                        .any(|clients| clients.contains(&client_id))
                        || self
                            .server
                            .pubsub_pattern_subs
                            .values()
                            .any(|clients| clients.contains(&client_id))
                        || self
                            .server
                            .pubsub_shard_subs
                            .values()
                            .any(|clients| clients.contains(&client_id));
                    let client_type = if has_subs { "pubsub" } else { "normal" };
                    let include_self = match std::str::from_utf8(&argv[3]) {
                        Ok(kind) if kind.eq_ignore_ascii_case(client_type) => true,
                        Ok(kind)
                            if kind.eq_ignore_ascii_case("NORMAL")
                                || kind.eq_ignore_ascii_case("MASTER")
                                || kind.eq_ignore_ascii_case("REPLICA")
                                || kind.eq_ignore_ascii_case("PUBSUB") =>
                        {
                            false
                        }
                        Ok(kind) => {
                            return RespFrame::Error(format!("ERR Unknown client type '{kind}'"));
                        }
                        Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
                    };
                    if include_self {
                        info_line.into_bytes()
                    } else {
                        Vec::new()
                    }
                } else if argv.len() >= 4 && eq_ascii_token(&argv[2], b"ID") {
                    let mut payload = Vec::new();
                    for id_arg in &argv[3..] {
                        let parsed_id = match parse_i64_arg(id_arg) {
                            Ok(id) if id > 0 => id as u64,
                            _ => return RespFrame::Error("ERR Invalid client ID".to_string()),
                        };
                        if parsed_id == client_id {
                            payload.extend_from_slice(info_line.as_bytes());
                        }
                    }
                    payload
                } else {
                    return RespFrame::Error("ERR syntax error".to_string());
                };
                return RespFrame::BulkString(Some(payload));
            }
            RespFrame::BulkString(Some(info_line.into_bytes()))
        } else if sub.eq_ignore_ascii_case("NO-EVICT") {
            if argv.len() != 3 {
                return CommandError::WrongArity("CLIENT").to_resp();
            }
            let mode = match std::str::from_utf8(&argv[2]) {
                Ok(m) => m,
                Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
            };
            if mode.eq_ignore_ascii_case("ON") {
                self.session.client_no_evict = true;
            } else if mode.eq_ignore_ascii_case("OFF") {
                self.session.client_no_evict = false;
            } else {
                return RespFrame::Error("ERR argument must be 'on' or 'off'".to_string());
            }
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("NO-TOUCH") {
            if argv.len() != 3 {
                return CommandError::WrongArity("CLIENT").to_resp();
            }
            let mode = match std::str::from_utf8(&argv[2]) {
                Ok(m) => m,
                Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
            };
            if mode.eq_ignore_ascii_case("ON") {
                self.session.client_no_touch = true;
            } else if mode.eq_ignore_ascii_case("OFF") {
                self.session.client_no_touch = false;
            } else {
                return RespFrame::Error("ERR argument must be 'on' or 'off'".to_string());
            }
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("SETINFO") {
            // CLIENT SETINFO <attr> <value> (Redis 7.2+)
            if argv.len() != 4 {
                return CommandError::WrongArity("CLIENT").to_resp();
            }
            let attr = match std::str::from_utf8(&argv[2]) {
                Ok(a) => a,
                Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
            };
            let val = match std::str::from_utf8(&argv[3]) {
                Ok(v) => v.to_string(),
                Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
            };
            if attr.eq_ignore_ascii_case("LIB-NAME") || attr.eq_ignore_ascii_case("lib-name") {
                // Redis validates: lib-name must not contain spaces or newlines
                if val.bytes().any(|b| b <= b' ') {
                    return RespFrame::Error(
                        "ERR lib-name can only contain characters that are allowed in CLIENT SETNAME"
                            .to_string(),
                    );
                }
                self.session.client_lib_name = if val.is_empty() { None } else { Some(val) };
            } else if attr.eq_ignore_ascii_case("LIB-VER") || attr.eq_ignore_ascii_case("lib-ver") {
                if val.bytes().any(|b| b <= b' ') {
                    return RespFrame::Error(
                        "ERR lib-ver can only contain characters that are allowed in CLIENT SETNAME"
                            .to_string(),
                    );
                }
                self.session.client_lib_ver = if val.is_empty() { None } else { Some(val) };
            } else {
                return RespFrame::Error(format!(
                    "ERR Unrecognized option '{attr}' for CLIENT SETINFO"
                ));
            }
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("REPLY") {
            // CLIENT REPLY ON|OFF|SKIP
            if argv.len() != 3 {
                return CommandError::WrongArity("CLIENT").to_resp();
            }
            let mode = match std::str::from_utf8(&argv[2]) {
                Ok(mode) => mode,
                Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
            };
            if !mode.eq_ignore_ascii_case("ON")
                && !mode.eq_ignore_ascii_case("OFF")
                && !mode.eq_ignore_ascii_case("SKIP")
            {
                return RespFrame::Error("ERR syntax error".to_string());
            }
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("KILL") {
            // CLIENT KILL [ip:port | ID client-id | ...]
            // Single-connection runtime: always return 0 (no other clients to kill)
            Ok::<i64, ()>(0).ok();
            RespFrame::Integer(0)
        } else if sub.eq_ignore_ascii_case("PAUSE") {
            // CLIENT PAUSE timeout [WRITE|ALL]
            if argv.len() < 3 || argv.len() > 4 {
                return CommandError::WrongArity("CLIENT").to_resp();
            }
            let timeout_ms = match parse_i64_arg(&argv[2]) {
                Ok(ms) => ms.max(0) as u64,
                _ => return CommandError::InvalidInteger.to_resp(),
            };
            let pause_all = if argv.len() == 4 {
                let mode =
                    std::str::from_utf8(&argv[3]).map_err(|_| CommandError::InvalidUtf8Argument);
                match mode {
                    Ok(m) if m.eq_ignore_ascii_case("ALL") => true,
                    Ok(m) if m.eq_ignore_ascii_case("WRITE") => false,
                    _ => return RespFrame::Error("ERR syntax error".to_string()),
                }
            } else {
                true // default is ALL
            };
            if timeout_ms == 0 {
                // timeout 0 = unpause
                self.server.client_pause_deadline_ms = 0;
                self.server.client_pause_all = false;
            } else {
                // Use the runtime's logical clock so pause behavior stays deterministic.
                self.server.client_pause_deadline_ms = now_ms.saturating_add(timeout_ms);
                self.server.client_pause_all = pause_all;
            }
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("UNPAUSE") {
            if argv.len() != 2 {
                return CommandError::WrongArity("CLIENT").to_resp();
            }
            self.server.client_pause_deadline_ms = 0;
            self.server.client_pause_all = false;
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("TRACKING") {
            // CLIENT TRACKING ON|OFF [REDIRECT id] [PREFIX prefix ...] [BCAST] [OPTIN] [OPTOUT] [NOLOOP]
            if argv.len() < 3 {
                return CommandError::WrongArity("CLIENT").to_resp();
            }
            let mode = match std::str::from_utf8(&argv[2]) {
                Ok(mode) => mode,
                Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
            };
            if !mode.eq_ignore_ascii_case("ON") && !mode.eq_ignore_ascii_case("OFF") {
                return RespFrame::Error("ERR syntax error".to_string());
            }
            // PARITY RULE: Redis 7.2 returns OK for CLIENT TRACKING ON/OFF.
            // Client libraries (redis-py, jedis, lettuce, ioredis) depend on this.
            // Conformance test core_client_conformance verifies this behavior.
            // DO NOT change to return an error — this has been reverted 3 times.
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("CACHING") {
            // CLIENT CACHING YES|NO — Redis 7.2 returns OK.
            if argv.len() != 3 {
                return CommandError::WrongArity("CLIENT").to_resp();
            }
            let mode = match std::str::from_utf8(&argv[2]) {
                Ok(mode) => mode,
                Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
            };
            if !mode.eq_ignore_ascii_case("YES") && !mode.eq_ignore_ascii_case("NO") {
                return RespFrame::Error("ERR syntax error".to_string());
            }
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("GETREDIR") {
            // CLIENT GETREDIR — returns redirect ID for tracking (-1 = not tracking)
            if argv.len() != 2 {
                return CommandError::WrongArity("CLIENT").to_resp();
            }
            RespFrame::Integer(-1) // not tracking
        } else if sub.eq_ignore_ascii_case("TRACKINGINFO") {
            // CLIENT TRACKINGINFO — returns tracking info
            if argv.len() != 2 {
                return CommandError::WrongArity("CLIENT").to_resp();
            }
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"flags".to_vec())),
                RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"off".to_vec()))])),
                RespFrame::BulkString(Some(b"redirect".to_vec())),
                RespFrame::Integer(-1),
                RespFrame::BulkString(Some(b"prefixes".to_vec())),
                RespFrame::Array(Some(Vec::new())),
            ]))
        } else if sub.eq_ignore_ascii_case("UNBLOCK") {
            // CLIENT UNBLOCK client-id [TIMEOUT|ERROR]
            if argv.len() != 3 && argv.len() != 4 {
                return CommandError::WrongArity("CLIENT").to_resp();
            }
            match parse_i64_arg(&argv[2]) {
                Ok(id) if id > 0 => {}
                _ => return CommandError::InvalidInteger.to_resp(),
            }
            if argv.len() == 4 {
                let mode = match std::str::from_utf8(&argv[3]) {
                    Ok(mode) => mode,
                    Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
                };
                if !mode.eq_ignore_ascii_case("TIMEOUT") && !mode.eq_ignore_ascii_case("ERROR") {
                    return RespFrame::Error("ERR syntax error".to_string());
                }
            }
            // In single-runtime mode, return 0 (no clients unblocked)
            RespFrame::Integer(0)
        } else if sub.eq_ignore_ascii_case("HELP") {
            if argv.len() != 2 {
                return CommandError::WrongArity("CLIENT").to_resp();
            }
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(
                    b"CLIENT <subcommand> [<arg> [value] [opt] ...]. Subcommands are:".to_vec(),
                )),
                RespFrame::BulkString(Some(b"CACHING (YES|NO)".to_vec())),
                RespFrame::BulkString(Some(b"GETNAME".to_vec())),
                RespFrame::BulkString(Some(b"GETREDIR".to_vec())),
                RespFrame::BulkString(Some(b"ID".to_vec())),
                RespFrame::BulkString(Some(b"INFO".to_vec())),
                RespFrame::BulkString(Some(b"KILL <option> ...".to_vec())),
                RespFrame::BulkString(Some(
                    b"LIST [TYPE (NORMAL|MASTER|REPLICA|PUBSUB)] [ID <client-id> ...]".to_vec(),
                )),
                RespFrame::BulkString(Some(b"NO-EVICT (ON|OFF)".to_vec())),
                RespFrame::BulkString(Some(b"NO-TOUCH (ON|OFF)".to_vec())),
                RespFrame::BulkString(Some(b"PAUSE <timeout> [WRITE|ALL]".to_vec())),
                RespFrame::BulkString(Some(b"REPLY (ON|OFF|SKIP)".to_vec())),
                RespFrame::BulkString(Some(b"SETINFO <option> <value>".to_vec())),
                RespFrame::BulkString(Some(b"SETNAME <connection-name>".to_vec())),
                RespFrame::BulkString(Some(b"TRACKING (ON|OFF) ...".to_vec())),
                RespFrame::BulkString(Some(b"TRACKINGINFO".to_vec())),
                RespFrame::BulkString(Some(b"UNBLOCK <client-id> [TIMEOUT|ERROR]".to_vec())),
                RespFrame::BulkString(Some(b"UNPAUSE".to_vec())),
                RespFrame::BulkString(Some(b"HELP".to_vec())),
            ]))
        } else {
            CommandError::UnknownSubcommand {
                command: "CLIENT",
                subcommand: sub.to_string(),
            }
            .to_resp()
        }
    }

    fn handle_reset_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 1 {
            return CommandError::WrongArity("RESET").to_resp();
        }
        // Clear pub/sub subscriptions from the global registry before resetting.
        self.pubsub_cleanup_client(self.session.client_id);
        self.session.reset_connection_state(&self.server.auth_state);
        // Redis returns +RESET\r\n (a simple string "RESET")
        RespFrame::SimpleString("RESET".to_string())
    }

    fn handle_slowlog_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() < 2 {
            return CommandError::WrongArity("SLOWLOG").to_resp();
        }
        let sub = match std::str::from_utf8(&argv[1]) {
            Ok(s) => s,
            Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
        };
        if sub.eq_ignore_ascii_case("GET") {
            if argv.len() > 3 {
                return CommandError::WrongSubcommandArity {
                    command: "SLOWLOG",
                    subcommand: "GET".to_string(),
                }
                .to_resp();
            }
            let count = if argv.len() == 3 {
                match parse_i64_arg(&argv[2]) {
                    Ok(-1) => self.server.slowlog.len(),
                    Ok(c) if c >= 0 => c as usize,
                    Ok(_) => {
                        return RespFrame::Error(
                            "ERR count should be greater than or equal to -1".to_string(),
                        );
                    }
                    Err(_) => return CommandError::InvalidInteger.to_resp(),
                }
            } else {
                10
            };
            let entries: Vec<RespFrame> = self
                .server
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
            if argv.len() != 2 {
                return CommandError::WrongSubcommandArity {
                    command: "SLOWLOG",
                    subcommand: "LEN".to_string(),
                }
                .to_resp();
            }
            RespFrame::Integer(self.server.slowlog.len() as i64)
        } else if sub.eq_ignore_ascii_case("RESET") {
            if argv.len() != 2 {
                return CommandError::WrongSubcommandArity {
                    command: "SLOWLOG",
                    subcommand: "RESET".to_string(),
                }
                .to_resp();
            }
            self.server.reset_slowlog();
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("HELP") {
            if argv.len() != 2 {
                return CommandError::WrongSubcommandArity {
                    command: "SLOWLOG",
                    subcommand: "HELP".to_string(),
                }
                .to_resp();
            }
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
            CommandError::UnknownSubcommand {
                command: "SLOWLOG",
                subcommand: sub.to_string(),
            }
            .to_resp()
        }
    }

    /// Record a command execution in the slow log if it exceeded the threshold.
    fn record_slowlog(&mut self, argv: &[Vec<u8>], duration_us: u64, now_ms: u64) {
        self.server.record_slowlog(argv, duration_us, now_ms);
    }

    fn handle_save_command(&mut self, argv: &[Vec<u8>], now_ms: u64) -> RespFrame {
        if argv.len() != 1 {
            return CommandError::WrongArity("SAVE").to_resp();
        }
        if let Err(reply) = self.persist_snapshot_to_disk(now_ms) {
            return reply;
        }
        self.server.store.mark_saved_at(now_ms);
        self.server.last_save_time_sec = now_ms / 1000;
        RespFrame::SimpleString("OK".to_string())
    }

    fn handle_bgsave_command(&mut self, argv: &[Vec<u8>], now_ms: u64) -> RespFrame {
        if argv.len() > 2 {
            return CommandError::SyntaxError.to_resp();
        }
        if argv.len() == 2 {
            let option = match std::str::from_utf8(&argv[1]) {
                Ok(option) => option,
                Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
            };
            if !option.eq_ignore_ascii_case("SCHEDULE") {
                return CommandError::SyntaxError.to_resp();
            }
        }
        // In a single-threaded context, BGSAVE behaves like SAVE.
        if let Err(reply) = self.persist_snapshot_to_disk(now_ms) {
            return reply;
        }
        self.server.store.mark_saved_at(now_ms);
        self.server.last_save_time_sec = now_ms / 1000;
        RespFrame::SimpleString("Background saving started".to_string())
    }

    fn persist_snapshot_to_disk(&mut self, now_ms: u64) -> Result<(), RespFrame> {
        if let Some(path) = &self.server.aof_path {
            let commands = self.server.store.to_aof_commands(now_ms);
            let records = argv_to_aof_records(commands);
            if write_aof_file(path, &records).is_err() {
                return Err(RespFrame::Error(
                    "ERR error saving dataset to disk".to_string(),
                ));
            }
        }

        if let Some(path) = &self.server.rdb_path {
            let entries = store_to_rdb_entries(&mut self.server.store, now_ms);
            let aux = [
                ("redis-ver", fr_store::REDIS_COMPAT_VERSION),
                ("frankenredis", "true"),
            ];
            if write_rdb_file(path, &entries, &aux).is_err() {
                return Err(RespFrame::Error(
                    "ERR error saving RDB snapshot to disk".to_string(),
                ));
            }
        }

        Ok(())
    }

    fn handle_lastsave_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 1 {
            return CommandError::WrongArity("LASTSAVE").to_resp();
        }
        RespFrame::Integer(self.server.last_save_time_sec as i64)
    }

    fn handle_bgrewriteaof_command(&mut self, argv: &[Vec<u8>], now_ms: u64) -> RespFrame {
        if argv.len() != 1 {
            return CommandError::WrongArity("BGREWRITEAOF").to_resp();
        }
        let Some(path) = &self.server.aof_path else {
            return RespFrame::Error(
                "ERR appendonly is disabled, cannot rewrite append only file".to_string(),
            );
        };
        // Rewrite the AOF file with a snapshot of the current store state.
        let commands = self.server.store.to_aof_commands(now_ms);
        let records = argv_to_aof_records(commands);
        if let Err(_e) = write_aof_file(path, &records) {
            return RespFrame::Error("ERR error rewriting AOF file".to_string());
        }
        RespFrame::SimpleString("Background append only file rewriting started".to_string())
    }

    fn handle_shutdown_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        let mut flags = 0u8;
        let mut abort = false;
        for arg in &argv[1..] {
            let option = match std::str::from_utf8(arg) {
                Ok(option) => option,
                Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
            };
            if option.eq_ignore_ascii_case("NOSAVE") {
                flags |= 0b0001;
            } else if option.eq_ignore_ascii_case("SAVE") {
                flags |= 0b0010;
            } else if option.eq_ignore_ascii_case("NOW") {
                flags |= 0b0100;
            } else if option.eq_ignore_ascii_case("FORCE") {
                flags |= 0b1000;
            } else if option.eq_ignore_ascii_case("ABORT") {
                abort = true;
            } else {
                return CommandError::SyntaxError.to_resp();
            }
        }
        if (flags & 0b0001 != 0 && flags & 0b0010 != 0) || (abort && flags != 0) {
            return CommandError::SyntaxError.to_resp();
        }
        if abort {
            if self.server.shutdown_requested {
                self.server.shutdown_requested = false;
                self.server.shutdown_nosave = false;
                return RespFrame::SimpleString("OK".to_string());
            }
            return RespFrame::Error("ERR No shutdown in progress.".to_string());
        }
        // Signal the server event loop to initiate graceful shutdown
        self.server.shutdown_requested = true;
        self.server.shutdown_nosave = flags & 0b0001 != 0;
        RespFrame::SimpleString("OK".to_string())
    }

    // ── Pub/Sub command handlers (use global registry) ─────────────

    fn handle_subscribe_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() < 2 {
            return CommandError::WrongArity("SUBSCRIBE").to_resp();
        }
        let mut replies = Vec::new();
        for channel in &argv[1..] {
            let count = self.pubsub_subscribe(channel.clone());
            replies.push(RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"subscribe".to_vec())),
                RespFrame::BulkString(Some(channel.clone())),
                RespFrame::Integer(count as i64),
            ])));
        }
        if replies.len() == 1
            && let Some(frame) = replies.pop()
        {
            frame
        } else {
            RespFrame::Sequence(replies)
        }
    }

    fn handle_unsubscribe_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() < 2 {
            // Unsubscribe from all channels
            let mut channels: Vec<Vec<u8>> = self
                .server
                .pubsub_client_channels
                .get(&self.session.client_id)
                .into_iter()
                .flat_map(|channels| channels.iter().cloned())
                .collect();
            channels.sort();
            if channels.is_empty() {
                return RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"unsubscribe".to_vec())),
                    RespFrame::BulkString(None),
                    RespFrame::Integer(0),
                ]));
            }
            let mut replies = Vec::new();
            for ch in channels {
                let count = self.pubsub_unsubscribe(&ch);
                replies.push(RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"unsubscribe".to_vec())),
                    RespFrame::BulkString(Some(ch)),
                    RespFrame::Integer(count as i64),
                ])));
            }
            if replies.len() == 1 {
                return replies.into_iter().next().expect("single reply");
            }
            return RespFrame::Sequence(replies);
        }
        let mut replies = Vec::new();
        for channel in &argv[1..] {
            let count = self.pubsub_unsubscribe(channel);
            replies.push(RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"unsubscribe".to_vec())),
                RespFrame::BulkString(Some(channel.clone())),
                RespFrame::Integer(count as i64),
            ])));
        }
        if replies.len() == 1 {
            return replies.into_iter().next().expect("single reply");
        }
        RespFrame::Sequence(replies)
    }

    fn handle_psubscribe_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() < 2 {
            return CommandError::WrongArity("PSUBSCRIBE").to_resp();
        }
        let mut replies = Vec::new();
        for pattern in &argv[1..] {
            let count = self.pubsub_psubscribe(pattern.clone());
            replies.push(RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"psubscribe".to_vec())),
                RespFrame::BulkString(Some(pattern.clone())),
                RespFrame::Integer(count as i64),
            ])));
        }
        if replies.len() == 1 {
            return replies.into_iter().next().expect("single reply");
        }
        RespFrame::Sequence(replies)
    }

    fn handle_punsubscribe_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() < 2 {
            let mut patterns: Vec<Vec<u8>> = self
                .server
                .pubsub_client_patterns
                .get(&self.session.client_id)
                .into_iter()
                .flat_map(|patterns| patterns.iter().cloned())
                .collect();
            patterns.sort();
            if patterns.is_empty() {
                return RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"punsubscribe".to_vec())),
                    RespFrame::BulkString(None),
                    RespFrame::Integer(0),
                ]));
            }
            let mut replies = Vec::new();
            for pat in patterns {
                let count = self.pubsub_punsubscribe(&pat);
                replies.push(RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"punsubscribe".to_vec())),
                    RespFrame::BulkString(Some(pat)),
                    RespFrame::Integer(count as i64),
                ])));
            }
            if replies.len() == 1 {
                return replies.into_iter().next().expect("single reply");
            }
            return RespFrame::Sequence(replies);
        }
        let mut replies = Vec::new();
        for pattern in &argv[1..] {
            let count = self.pubsub_punsubscribe(pattern);
            replies.push(RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"punsubscribe".to_vec())),
                RespFrame::BulkString(Some(pattern.clone())),
                RespFrame::Integer(count as i64),
            ])));
        }
        if replies.len() == 1 {
            return replies.into_iter().next().expect("single reply");
        }
        RespFrame::Sequence(replies)
    }

    fn handle_publish_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 3 {
            return CommandError::WrongArity("PUBLISH").to_resp();
        }
        let receivers = self.pubsub_publish(&argv[1], &argv[2]);
        RespFrame::Integer(receivers as i64)
    }

    fn handle_pubsub_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() < 2 {
            return CommandError::WrongArity("PUBSUB").to_resp();
        }
        let sub = match std::str::from_utf8(&argv[1]) {
            Ok(sub) => sub,
            Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
        };

        if sub.eq_ignore_ascii_case("HELP") {
            if argv.len() != 2 {
                return CommandError::SyntaxError.to_resp();
            }
            return RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(
                    b"PUBSUB <subcommand> [<arg> [value] ...]. Subcommands are:".to_vec(),
                )),
                RespFrame::BulkString(Some(
                    b"CHANNELS [<pattern>] - Return the currently active channels matching a <pattern> (default: '*').".to_vec(),
                )),
                RespFrame::BulkString(Some(
                    b"NUMPAT - Return number of subscriptions to patterns.".to_vec(),
                )),
                RespFrame::BulkString(Some(
                    b"NUMSUB [<channel> ...] - Return the number of subscribers for the specified channels, excluding pattern subscriptions(default: no channels).".to_vec(),
                )),
                RespFrame::BulkString(Some(
                    b"SHARDCHANNELS [<pattern>] - Return the currently active shard level channels matching a <pattern> (default: '*').".to_vec(),
                )),
                RespFrame::BulkString(Some(
                    b"SHARDNUMSUB [<shardchannel> ...] - Return the number of subscribers for the specified shard level channel(s)".to_vec(),
                )),
            ]));
        }

        if sub.eq_ignore_ascii_case("CHANNELS") {
            if argv.len() != 2 && argv.len() != 3 {
                return CommandError::SyntaxError.to_resp();
            }
            let mut channels: Vec<Vec<u8>> =
                self.server.pubsub_channel_subs.keys().cloned().collect();
            if let Some(pattern) = argv.get(2) {
                channels.retain(|channel| fr_store::glob_match(pattern, channel));
            }
            channels.sort();
            return RespFrame::Array(Some(
                channels
                    .into_iter()
                    .map(|channel| RespFrame::BulkString(Some(channel)))
                    .collect(),
            ));
        }

        if sub.eq_ignore_ascii_case("NUMSUB") {
            let mut result = Vec::with_capacity(argv.len().saturating_sub(2) * 2);
            for channel in &argv[2..] {
                result.push(RespFrame::BulkString(Some(channel.clone())));
                result.push(RespFrame::Integer(
                    self.server
                        .pubsub_channel_subs
                        .get(channel)
                        .map_or(0, HashSet::len) as i64,
                ));
            }
            return RespFrame::Array(Some(result));
        }

        if sub.eq_ignore_ascii_case("NUMPAT") {
            if argv.len() != 2 {
                return CommandError::SyntaxError.to_resp();
            }
            return RespFrame::Integer(
                self.server
                    .pubsub_pattern_subs
                    .values()
                    .map(|clients| clients.len())
                    .sum::<usize>() as i64,
            );
        }

        if sub.eq_ignore_ascii_case("SHARDCHANNELS") {
            if argv.len() != 2 && argv.len() != 3 {
                return CommandError::SyntaxError.to_resp();
            }
            let mut channels: Vec<Vec<u8>> =
                self.server.pubsub_shard_subs.keys().cloned().collect();
            if let Some(pattern) = argv.get(2) {
                channels.retain(|channel| fr_store::glob_match(pattern, channel));
            }
            channels.sort();
            return RespFrame::Array(Some(
                channels
                    .into_iter()
                    .map(|channel| RespFrame::BulkString(Some(channel)))
                    .collect(),
            ));
        }

        if sub.eq_ignore_ascii_case("SHARDNUMSUB") {
            let mut result = Vec::with_capacity(argv.len().saturating_sub(2) * 2);
            for channel in &argv[2..] {
                result.push(RespFrame::BulkString(Some(channel.clone())));
                result.push(RespFrame::Integer(
                    self.server
                        .pubsub_shard_subs
                        .get(channel)
                        .map_or(0, HashSet::len) as i64,
                ));
            }
            return RespFrame::Array(Some(result));
        }

        CommandError::SyntaxError.to_resp()
    }

    fn handle_ssubscribe_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() < 2 {
            return CommandError::WrongArity("SSUBSCRIBE").to_resp();
        }
        let mut replies = Vec::new();
        for channel in &argv[1..] {
            let count = self.pubsub_ssubscribe(channel.clone());
            replies.push(RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"ssubscribe".to_vec())),
                RespFrame::BulkString(Some(channel.clone())),
                RespFrame::Integer(count as i64),
            ])));
        }
        if replies.len() == 1 {
            return replies.into_iter().next().expect("single reply");
        }
        RespFrame::Sequence(replies)
    }

    fn handle_sunsubscribe_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() < 2 {
            let mut channels: Vec<Vec<u8>> = self
                .server
                .pubsub_client_shard_channels
                .get(&self.session.client_id)
                .into_iter()
                .flat_map(|channels| channels.iter().cloned())
                .collect();
            channels.sort();
            if channels.is_empty() {
                return RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"sunsubscribe".to_vec())),
                    RespFrame::BulkString(None),
                    RespFrame::Integer(0),
                ]));
            }
            let mut replies = Vec::new();
            for ch in channels {
                let count = self.pubsub_sunsubscribe(&ch);
                replies.push(RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"sunsubscribe".to_vec())),
                    RespFrame::BulkString(Some(ch)),
                    RespFrame::Integer(count as i64),
                ])));
            }
            if replies.len() == 1 {
                return replies.into_iter().next().expect("single reply");
            }
            return RespFrame::Sequence(replies);
        }
        let mut replies = Vec::new();
        for channel in &argv[1..] {
            let count = self.pubsub_sunsubscribe(channel);
            replies.push(RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"sunsubscribe".to_vec())),
                RespFrame::BulkString(Some(channel.clone())),
                RespFrame::Integer(count as i64),
            ])));
        }
        if replies.len() == 1 {
            return replies.into_iter().next().expect("single reply");
        }
        RespFrame::Sequence(replies)
    }

    fn handle_spublish_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 3 {
            return CommandError::WrongArity("SPUBLISH").to_resp();
        }
        let receivers = self.pubsub_spublish(&argv[1], &argv[2]);
        RespFrame::Integer(receivers as i64)
    }

    fn handle_select_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 2 {
            return CommandError::WrongArity("SELECT").to_resp();
        }
        let db = match parse_db_index_arg(
            &argv[1],
            "ERR DB index is out of range",
            self.server.store.database_count,
        ) {
            Ok(db) => db,
            Err(reply) => return reply,
        };
        self.session.selected_db = db;
        RespFrame::SimpleString("OK".to_string())
    }

    fn handle_swapdb_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 3 {
            return CommandError::WrongArity("SWAPDB").to_resp();
        }
        let dbc = self.server.store.database_count;
        let db1 = match parse_db_index_arg(&argv[1], "ERR invalid DB index", dbc) {
            Ok(n) => n,
            Err(e) => return e,
        };
        let db2 = match parse_db_index_arg(&argv[2], "ERR invalid DB index", dbc) {
            Ok(n) => n,
            Err(e) => return e,
        };
        self.server.store.swap_databases(db1, db2);
        self.capture_aof_record(argv);
        RespFrame::SimpleString("OK".to_string())
    }

    fn handle_db_keys_command(
        &mut self,
        argv: &[Vec<u8>],
        now_ms: u64,
    ) -> Result<RespFrame, CommandError> {
        if argv.len() != 2 {
            return Err(CommandError::WrongArity("KEYS"));
        }
        let matched =
            self.server
                .store
                .keys_matching_in_db(self.session.selected_db, &argv[1], now_ms);
        let frames = matched
            .into_iter()
            .map(|key| RespFrame::BulkString(Some(key)))
            .collect();
        Ok(RespFrame::Array(Some(frames)))
    }

    fn handle_dbsize_command(&mut self, argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
        if argv.len() != 1 {
            return Err(CommandError::WrongArity("DBSIZE"));
        }
        let size = self.server.store.dbsize_in_db(self.session.selected_db);
        Ok(RespFrame::Integer(i64::try_from(size).unwrap_or(i64::MAX)))
    }

    fn handle_flushdb_command(&mut self, argv: &[Vec<u8>]) -> Result<RespFrame, CommandError> {
        if argv.len() > 2 {
            return Err(CommandError::WrongArity("FLUSHDB"));
        }
        self.server.store.flush_database(self.session.selected_db);
        Ok(RespFrame::SimpleString("OK".to_string()))
    }

    fn handle_randomkey_command(
        &mut self,
        argv: &[Vec<u8>],
        now_ms: u64,
    ) -> Result<RespFrame, CommandError> {
        if argv.len() != 1 {
            return Err(CommandError::WrongArity("RANDOMKEY"));
        }
        let Some(physical) = self
            .server
            .store
            .randomkey_in_db(self.session.selected_db, now_ms)
        else {
            return Ok(RespFrame::BulkString(None));
        };
        let logical = decode_db_key(&physical)
            .map(|(_, logical)| logical.to_vec())
            .unwrap_or(physical);
        Ok(RespFrame::BulkString(Some(logical)))
    }

    fn handle_scan_command(
        &mut self,
        argv: &[Vec<u8>],
        now_ms: u64,
    ) -> Result<RespFrame, CommandError> {
        if argv.len() < 2 {
            return Err(CommandError::WrongArity("SCAN"));
        }
        let cursor = std::str::from_utf8(&argv[1])
            .map_err(|_| CommandError::InvalidInteger)?
            .parse::<usize>()
            .map_err(|_| CommandError::InvalidInteger)?;

        let mut pattern: Option<&[u8]> = None;
        let mut count: usize = 10;
        let mut type_filter: Option<&[u8]> = None;
        let mut i = 2;
        while i < argv.len() {
            let keyword =
                std::str::from_utf8(&argv[i]).map_err(|_| CommandError::InvalidUtf8Argument)?;
            if keyword.eq_ignore_ascii_case("MATCH") {
                if i + 1 >= argv.len() {
                    return Err(CommandError::SyntaxError);
                }
                pattern = Some(argv[i + 1].as_slice());
                i += 2;
            } else if keyword.eq_ignore_ascii_case("COUNT") {
                if i + 1 >= argv.len() {
                    return Err(CommandError::SyntaxError);
                }
                let parsed = std::str::from_utf8(&argv[i + 1])
                    .map_err(|_| CommandError::InvalidInteger)?
                    .parse::<i64>()
                    .map_err(|_| CommandError::InvalidInteger)?;
                if parsed <= 0 {
                    return Err(CommandError::InvalidInteger);
                }
                count = parsed as usize;
                i += 2;
            } else if keyword.eq_ignore_ascii_case("TYPE") {
                if i + 1 >= argv.len() {
                    return Err(CommandError::SyntaxError);
                }
                type_filter = Some(argv[i + 1].as_slice());
                i += 2;
            } else {
                return Err(CommandError::SyntaxError);
            }
        }

        let logical_keys = self
            .server
            .store
            .keys_in_db(self.session.selected_db, now_ms);
        let mut filtered = Vec::new();
        for logical_key in logical_keys {
            if let Some(expected_pattern) = pattern
                && !glob_match(expected_pattern, &logical_key)
            {
                continue;
            }
            if let Some(expected_type) = type_filter {
                let physical = encode_db_key(self.session.selected_db, &logical_key);
                let expected = std::str::from_utf8(expected_type).unwrap_or("");
                if self
                    .server
                    .store
                    .key_type(&physical, now_ms)
                    .is_none_or(|actual| !actual.eq_ignore_ascii_case(expected))
                {
                    continue;
                }
            }
            filtered.push(logical_key);
        }
        filtered.sort();

        if cursor >= filtered.len() {
            return Ok(RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"0".to_vec())),
                RespFrame::Array(Some(Vec::new())),
            ])));
        }

        let end = cursor.saturating_add(count.max(1)).min(filtered.len());
        let next_cursor = if end >= filtered.len() { 0 } else { end };
        let batch = filtered[cursor..end]
            .iter()
            .cloned()
            .map(|key| RespFrame::BulkString(Some(key)))
            .collect();
        Ok(RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(next_cursor.to_string().into_bytes())),
            RespFrame::Array(Some(batch)),
        ])))
    }

    fn handle_move_command(
        &mut self,
        argv: &[Vec<u8>],
        now_ms: u64,
    ) -> Result<RespFrame, CommandError> {
        if argv.len() != 3 {
            return Err(CommandError::WrongArity("MOVE"));
        }
        let target_db = parse_db_index_arg(
            &argv[2],
            "ERR DB index is out of range",
            self.server.store.database_count,
        )
        .map_err(|reply| match reply {
            RespFrame::Error(message) => CommandError::Custom(message),
            _ => CommandError::InvalidInteger,
        })?;
        if target_db == self.session.selected_db {
            return Ok(RespFrame::Integer(0));
        }
        let source = encode_db_key(self.session.selected_db, &argv[1]);
        let destination = encode_db_key(target_db, &argv[1]);
        if !self.server.store.exists(&source, now_ms)
            || self.server.store.exists(&destination, now_ms)
        {
            return Ok(RespFrame::Integer(0));
        }
        self.server
            .store
            .copy(&source, &destination, false, now_ms)
            .map_err(CommandError::Store)?;
        self.server.store.del(&[source], now_ms);
        Ok(RespFrame::Integer(1))
    }

    fn handle_copy_command(
        &mut self,
        argv: &[Vec<u8>],
        now_ms: u64,
    ) -> Result<RespFrame, CommandError> {
        if argv.len() < 3 {
            return Err(CommandError::WrongArity("COPY"));
        }
        let source = encode_db_key(self.session.selected_db, &argv[1]);
        let mut destination_db = self.session.selected_db;
        let mut replace = false;
        let mut i = 3;
        while i < argv.len() {
            let arg = std::str::from_utf8(&argv[i]).unwrap_or("");
            if arg.eq_ignore_ascii_case("REPLACE") {
                replace = true;
                i += 1;
                continue;
            }
            if arg.eq_ignore_ascii_case("DB") {
                if i + 1 >= argv.len() {
                    return Err(CommandError::SyntaxError);
                }
                destination_db = parse_db_index_arg(
                    &argv[i + 1],
                    "ERR DB index is out of range",
                    self.server.store.database_count,
                )
                .map_err(|reply| match reply {
                    RespFrame::Error(message) => CommandError::Custom(message),
                    _ => CommandError::InvalidInteger,
                })?;
                i += 2;
                continue;
            }
            return Err(CommandError::SyntaxError);
        }
        let destination = encode_db_key(destination_db, &argv[2]);
        let copied = self
            .server
            .store
            .copy(&source, &destination, replace, now_ms)
            .map_err(CommandError::Store)?;
        Ok(RespFrame::Integer(i64::from(copied)))
    }

    fn handle_info_command(
        &mut self,
        argv: &[Vec<u8>],
        now_ms: u64,
    ) -> Result<RespFrame, CommandError> {
        let sections: Vec<&str> = argv[1..]
            .iter()
            .map(|arg| std::str::from_utf8(arg).map_err(|_| CommandError::InvalidUtf8Argument))
            .collect::<Result<_, _>>()?;
        let is_all = sections.is_empty()
            || sections.iter().any(|section| {
                section.eq_ignore_ascii_case("all")
                    || section.eq_ignore_ascii_case("everything")
                    || section.eq_ignore_ascii_case("default")
            });
        let section_requested = |name: &str| {
            is_all
                || sections
                    .iter()
                    .any(|section| section.eq_ignore_ascii_case(name))
        };
        let is_persistence = section_requested("persistence");
        let is_replication = section_requested("replication");
        let is_keyspace = section_requested("keyspace");

        let mut info = Vec::new();
        if is_persistence
            && let RespFrame::BulkString(Some(bytes)) = self.handle_info_persistence_section()
        {
            info.extend_from_slice(&bytes);
        }
        if is_replication
            && let RespFrame::BulkString(Some(bytes)) = self.handle_info_replication_section()
        {
            info.extend_from_slice(&bytes);
        }
        if is_keyspace
            && let RespFrame::BulkString(Some(bytes)) = self.handle_info_keyspace_section(now_ms)
        {
            info.extend_from_slice(&bytes);
        }

        const COMMAND_INFO_SECTIONS: &[&str] = &[
            "server",
            "clients",
            "memory",
            "stats",
            "cpu",
            "modules",
            "errorstats",
            "cluster",
        ];

        let mut delegated = vec![b"INFO".to_vec()];
        if is_all {
            delegated.extend(
                COMMAND_INFO_SECTIONS
                    .iter()
                    .map(|section| section.as_bytes().to_vec()),
            );
        } else {
            for section in &sections {
                if COMMAND_INFO_SECTIONS
                    .iter()
                    .any(|known| section.eq_ignore_ascii_case(known))
                    && !delegated
                        .iter()
                        .skip(1)
                        .any(|arg| eq_ascii_token(arg, section.as_bytes()))
                {
                    delegated.push(section.as_bytes().to_vec());
                }
            }
        }

        if delegated.len() > 1 {
            let namespaced = self.namespace_argv_for_selected_db(&delegated);
            let mut reply = dispatch_argv(&namespaced, &mut self.server.store, now_ms)?;
            self.strip_db_prefixes_from_frame(&mut reply);
            if let RespFrame::BulkString(Some(bytes)) = reply {
                info.extend_from_slice(&bytes);
            }
        }

        if info.is_empty() {
            return Ok(RespFrame::BulkString(Some(Vec::new())));
        }

        Ok(RespFrame::BulkString(Some(info)))
    }

    fn handle_info_keyspace_section(&mut self, _now_ms: u64) -> RespFrame {
        let mut info = String::from("# Keyspace\r\n");
        for db in 0..self.server.store.database_count {
            let keys = self.server.store.dbsize_in_db(db);
            if keys > 0 {
                let expires = self.server.store.expires_in_db(db);
                info.push_str(&format!(
                    "db{db}:keys={keys},expires={expires},avg_ttl=0\r\n"
                ));
            }
        }
        info.push_str("\r\n");
        RespFrame::BulkString(Some(info.into_bytes()))
    }

    fn handle_info_persistence_section(&mut self) -> RespFrame {
        let mut info = String::from("# Persistence\r\n");
        info.push_str("loading:0\r\n");
        info.push_str("async_loading:0\r\n");
        info.push_str("current_cow_peak:0\r\n");
        info.push_str("current_cow_size:0\r\n");
        info.push_str("current_cow_size_age:0\r\n");
        info.push_str("current_fork_perc:0.00\r\n");
        info.push_str("current_save_keys_processed:0\r\n");
        info.push_str("current_save_keys_total:0\r\n");
        info.push_str(&format!(
            "rdb_changes_since_last_save:{}\r\n",
            self.server.store.dirty
        ));
        info.push_str("rdb_bgsave_in_progress:0\r\n");
        info.push_str(&format!(
            "rdb_last_save_time:{}\r\n",
            self.server.store.last_save_time_sec
        ));
        info.push_str("rdb_last_bgsave_status:ok\r\n");
        info.push_str("rdb_last_bgsave_time_sec:-1\r\n");
        info.push_str("rdb_current_bgsave_time_sec:-1\r\n");
        info.push_str("rdb_saves:0\r\n");
        info.push_str("rdb_last_cow_size:0\r\n");
        info.push_str(&format!(
            "aof_enabled:{}\r\n",
            usize::from(self.server.aof_path.is_some())
        ));
        info.push_str("aof_rewrite_in_progress:0\r\n");
        info.push_str("aof_rewrite_scheduled:0\r\n");
        info.push_str("aof_last_rewrite_time_sec:-1\r\n");
        info.push_str("aof_current_rewrite_time_sec:-1\r\n");
        info.push_str("aof_last_bgrewrite_status:ok\r\n");
        info.push_str("aof_last_write_status:ok\r\n");
        info.push_str("aof_last_cow_size:0\r\n");
        info.push_str("\r\n");
        RespFrame::BulkString(Some(info.into_bytes()))
    }

    fn handle_info_replication_section(&mut self) -> RespFrame {
        self.server.refresh_replica_ack_snapshots();
        let backlog = &self.server.replication_runtime_state.backlog;
        let connected_replicas = self.server.replication_runtime_state.replicas.len();
        let backlog_active = usize::from(connected_replicas > 0);
        let backlog_histlen = self.server.replication_runtime_state.backlog_histlen();
        let role = match self.server.replication_runtime_state.role {
            ReplicationRoleState::Master => "master",
            ReplicationRoleState::Replica { .. } => "slave",
        };

        let mut info = String::from("# Replication\r\n");
        info.push_str(&format!("role:{role}\r\n"));

        if matches!(
            self.server.replication_runtime_state.role,
            ReplicationRoleState::Master
        ) {
            for (i, replica) in self
                .server
                .replication_runtime_state
                .replicas
                .values()
                .enumerate()
            {
                let ip = replica.ip_address.as_deref().unwrap_or("127.0.0.1");
                let port = replica.listening_port;
                let state = "online"; // We only track registered replicas
                let offset = replica.ack_offset.0;
                let lag = self
                    .server
                    .replication_ack_state
                    .primary_offset
                    .0
                    .saturating_sub(offset);
                info.push_str(&format!(
                    "slave{i}:ip={ip},port={port},state={state},offset={offset},lag={lag}\r\n"
                ));
            }
        }

        info.push_str(&format!("connected_slaves:{connected_replicas}\r\n"));
        info.push_str("master_failover_state:no-failover\r\n");
        info.push_str(&format!("master_replid:{}\r\n", backlog.replid));
        info.push_str("master_replid2:0000000000000000000000000000000000000000\r\n");
        info.push_str(&format!(
            "master_repl_offset:{}\r\n",
            self.server.replication_ack_state.primary_offset.0
        ));
        info.push_str("second_repl_offset:-1\r\n");
        info.push_str(&format!("repl_backlog_active:{backlog_active}\r\n"));
        info.push_str(&format!(
            "repl_backlog_size:{}\r\n",
            self.server.repl_backlog_size
        ));
        info.push_str(&format!(
            "repl_backlog_first_byte_offset:{}\r\n",
            backlog.start_offset.0
        ));
        info.push_str(&format!("repl_backlog_histlen:{backlog_histlen}\r\n"));
        info.push_str("\r\n");
        RespFrame::BulkString(Some(info.into_bytes()))
    }

    fn handle_cluster_command(&mut self, argv: &[Vec<u8>], now_ms: u64) -> RespFrame {
        if argv.len() < 2 {
            return CommandError::WrongArity("CLUSTER").to_resp();
        }
        let subcommand = match classify_cluster_subcommand(&argv[1]) {
            Ok(subcommand) => subcommand,
            Err(err) => return err.to_resp(),
        };

        if subcommand == ClusterSubcommand::Help {
            if argv.len() != 2 {
                return RespFrame::Error(CLUSTER_UNKNOWN_SUBCOMMAND_ERROR.to_string());
            }
            return RespFrame::Array(Some(vec![
                hello_bulk("CLUSTER HELP"),
                hello_bulk("CLUSTER subcommand dispatch scaffold (FR-P2C-007 D1)."),
                hello_bulk(
                    "Supported subcommands in this stage: HELP, INFO, MYID, SLOTS, SHARDS, NODES, KEYSLOT, GETKEYSINSLOT, COUNTKEYSINSLOT, RESET.",
                ),
            ]));
        }

        if subcommand == ClusterSubcommand::Dispatch {
            return match dispatch_argv(argv, &mut self.server.store, now_ms) {
                Ok(reply) => reply,
                Err(err) => (err).to_resp(),
            };
        }

        RespFrame::Error(CLUSTER_UNKNOWN_SUBCOMMAND_ERROR.to_string())
    }

    fn handle_wait_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 3 {
            return CommandError::WrongArity("WAIT").to_resp();
        }
        let required_replicas = match parse_i64_arg(&argv[1]) {
            Ok(value) if value >= 0 => usize::try_from(value).unwrap_or(usize::MAX),
            _ => return CommandError::InvalidInteger.to_resp(),
        };
        if !matches!(parse_i64_arg(&argv[2]), Ok(value) if value >= 0) {
            return CommandError::InvalidInteger.to_resp();
        }

        let outcome = evaluate_wait(
            &self.server.replication_ack_state.replica_ack_offsets,
            WaitThreshold {
                required_offset: self.server.replication_ack_state.primary_offset,
                required_replicas,
            },
        );
        let acked_replicas = i64::try_from(outcome.acked_replicas).unwrap_or(i64::MAX);
        RespFrame::Integer(acked_replicas)
    }

    fn handle_waitaof_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 4 {
            return CommandError::WrongArity("WAITAOF").to_resp();
        }
        let required_local = match parse_i64_arg(&argv[1]) {
            Ok(value) if value >= 0 => usize::try_from(value).unwrap_or(usize::MAX),
            _ => return CommandError::InvalidInteger.to_resp(),
        };
        let required_replicas = match parse_i64_arg(&argv[2]) {
            Ok(value) if value >= 0 => usize::try_from(value).unwrap_or(usize::MAX),
            _ => return CommandError::InvalidInteger.to_resp(),
        };
        if !matches!(parse_i64_arg(&argv[3]), Ok(value) if value >= 0) {
            return CommandError::InvalidInteger.to_resp();
        }
        if matches!(
            self.server.replication_runtime_state.role,
            ReplicationRoleState::Replica { .. }
        ) {
            return RespFrame::Error(
                "ERR WAITAOF cannot be used with replica instances. Please also note that writes to replicas are just local and are not propagated.".to_string(),
            );
        }
        if required_local > 0 && self.server.aof_path.is_none() {
            return RespFrame::Error(
                "ERR WAITAOF cannot be used when numlocal is set but appendonly is disabled."
                    .to_string(),
            );
        }

        let required_local_offset = if required_local == 0 {
            ReplOffset(0)
        } else {
            self.server.replication_ack_state.primary_offset
        };
        let required_replica_offset = if required_replicas == 0 {
            ReplOffset(0)
        } else {
            self.server.replication_ack_state.primary_offset
        };

        let outcome = evaluate_waitaof(
            self.server.replication_ack_state.local_fsync_offset,
            &self.server.replication_ack_state.replica_fsync_offsets,
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

    fn handle_role_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 1 {
            return CommandError::WrongArity("ROLE").to_resp();
        }
        self.server.refresh_replica_ack_snapshots();
        match &self.server.replication_runtime_state.role {
            ReplicationRoleState::Master => {
                let replicas = self
                    .server
                    .replication_runtime_state
                    .replicas
                    .values()
                    .map(|replica| {
                        RespFrame::Array(Some(vec![
                            hello_bulk(replica.ip_address.as_deref().unwrap_or("")),
                            RespFrame::Integer(i64::from(replica.listening_port)),
                            RespFrame::Integer(
                                i64::try_from(replica.ack_offset.0).unwrap_or(i64::MAX),
                            ),
                        ]))
                    })
                    .collect();
                RespFrame::Array(Some(vec![
                    hello_bulk("master"),
                    RespFrame::Integer(self.server.replication_ack_state.primary_offset.0 as i64),
                    RespFrame::Array(Some(replicas)),
                ]))
            }
            ReplicationRoleState::Replica { host, port, state } => RespFrame::Array(Some(vec![
                hello_bulk("slave"),
                hello_bulk(host),
                RespFrame::Integer(i64::from(*port)),
                hello_bulk(state),
                RespFrame::Integer(
                    i64::try_from(self.server.replication_ack_state.primary_offset.0)
                        .unwrap_or(i64::MAX),
                ),
            ])),
        }
    }

    fn handle_replicaof_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 3 {
            return CommandError::WrongArity("REPLICAOF").to_resp();
        }
        let host = String::from_utf8_lossy(&argv[1]).into_owned();
        let port = String::from_utf8_lossy(&argv[2]).into_owned();
        if host.eq_ignore_ascii_case("NO") && port.eq_ignore_ascii_case("ONE") {
            if matches!(
                self.server.replication_runtime_state.role,
                ReplicationRoleState::Master
            ) {
                return RespFrame::SimpleString("OK Already a master".to_string());
            }
            self.server.replication_runtime_state.role = ReplicationRoleState::Master;
            self.server
                .replication_runtime_state
                .rotate_backlog_identity();
            self.server.replication_runtime_state.replicas.clear();
            self.server.refresh_replica_ack_snapshots();
            return RespFrame::SimpleString("OK".to_string());
        }
        let Ok(port) = port.parse::<u16>() else {
            return CommandError::InvalidInteger.to_resp();
        };
        if matches!(
            &self.server.replication_runtime_state.role,
            ReplicationRoleState::Replica {
                host: current_host,
                port: current_port,
                ..
            } if current_host.eq_ignore_ascii_case(&host) && *current_port == port
        ) {
            return RespFrame::SimpleString("OK Already connected to specified master".to_string());
        }
        self.server.replication_runtime_state.role = ReplicationRoleState::Replica {
            host,
            port,
            state: "connect",
        };
        RespFrame::SimpleString("OK".to_string())
    }

    fn handle_replconf_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() < 2 {
            return CommandError::WrongArity("REPLCONF").to_resp();
        }
        let sub = std::str::from_utf8(&argv[1]).unwrap_or("");
        if sub.eq_ignore_ascii_case("ACK") {
            if argv.len() != 3 {
                return CommandError::WrongArity("REPLCONF").to_resp();
            }
            if matches!(
                self.server.replication_runtime_state.role,
                ReplicationRoleState::Replica { .. }
            ) {
                // Slaves don't accept ACKs from master
                return RespFrame::SimpleString("OK".to_string());
            }
            let offset = match parse_i64_arg(&argv[2]) {
                Ok(value) if value >= 0 => value as u64,
                _ => return CommandError::InvalidInteger.to_resp(),
            };
            if let Some(replica) = self
                .server
                .replication_runtime_state
                .replicas
                .get_mut(&self.session.client_id)
            {
                let offset = ReplOffset(offset);
                if offset > replica.ack_offset {
                    replica.ack_offset = offset;
                }
                if offset > replica.fsync_offset {
                    replica.fsync_offset = offset;
                }
            }
            self.server.refresh_replica_ack_snapshots();
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("FACK") {
            if argv.len() != 3 {
                return CommandError::WrongArity("REPLCONF").to_resp();
            }
            if matches!(
                self.server.replication_runtime_state.role,
                ReplicationRoleState::Replica { .. }
            ) {
                return RespFrame::SimpleString("OK".to_string());
            }
            let offset = match parse_i64_arg(&argv[2]) {
                Ok(value) if value >= 0 => ReplOffset(value as u64),
                _ => return CommandError::InvalidInteger.to_resp(),
            };
            if let Some(replica) = self
                .server
                .replication_runtime_state
                .replicas
                .get_mut(&self.session.client_id)
            {
                if offset > replica.ack_offset {
                    replica.ack_offset = offset;
                }
                if offset > replica.fsync_offset {
                    replica.fsync_offset = offset;
                }
            }
            self.server.refresh_replica_ack_snapshots();
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("GETACK") {
            if argv.len() != 3 || !eq_ascii_token(&argv[2], b"*") {
                return CommandError::WrongArity("REPLCONF").to_resp();
            }
            self.server.refresh_replica_ack_snapshots();
            RespFrame::Array(Some(vec![
                hello_bulk("REPLCONF"),
                hello_bulk("ACK"),
                hello_bulk(
                    &self
                        .server
                        .replication_ack_state
                        .primary_offset
                        .0
                        .to_string(),
                ),
            ]))
        } else if sub.eq_ignore_ascii_case("listening-port") {
            if argv.len() == 3
                && let Ok(value) = parse_i64_arg(&argv[2])
                && let Ok(port) = u16::try_from(value)
            {
                self.server
                    .replication_runtime_state
                    .ensure_replica(self.session.client_id)
                    .listening_port = port;
            }
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("ip-address") {
            if argv.len() == 3 {
                self.server
                    .replication_runtime_state
                    .ensure_replica(self.session.client_id)
                    .ip_address = Some(String::from_utf8_lossy(&argv[2]).into_owned());
            }
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("capa") {
            // Acknowledge capabilities, e.g. "psync2", "eof"
            RespFrame::SimpleString("OK".to_string())
        } else {
            // Unknown REPLCONF options are ignored for compatibility
            RespFrame::SimpleString("OK".to_string())
        }
    }

    fn handle_psync_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        let is_sync = argv.first().is_some_and(|cmd| eq_ascii_token(cmd, b"SYNC"));
        if is_sync {
            if argv.len() != 1 {
                return CommandError::WrongArity("SYNC").to_resp();
            }
        } else if argv.len() != 3 {
            return CommandError::WrongArity("PSYNC").to_resp();
        }
        let (requested_replid, requested_offset) = if is_sync {
            ("?", -1)
        } else {
            let requested_offset = match parse_i64_arg(&argv[2]) {
                Ok(value) => value,
                Err(_) => return CommandError::InvalidInteger.to_resp(),
            };
            (
                std::str::from_utf8(&argv[1]).unwrap_or(""),
                requested_offset,
            )
        };

        let primary_offset = self.server.replication_ack_state.primary_offset;
        let backlog = self.server.replication_runtime_state.backlog.clone();
        let continued_offset = if requested_replid == "?" || requested_offset < 0 {
            None
        } else {
            match decide_psync(
                &backlog,
                requested_replid,
                ReplOffset(requested_offset as u64),
            ) {
                fr_repl::PsyncDecision::Continue { requested_offset } => Some(requested_offset),
                fr_repl::PsyncDecision::FullResync { .. } => None,
            }
        };

        let response = if continued_offset.is_some() {
            RespFrame::SimpleString("CONTINUE".to_string())
        } else {
            RespFrame::SimpleString(format!(
                "FULLRESYNC {} {}",
                backlog.replid, primary_offset.0
            ))
        };

        let replica = self
            .server
            .replication_runtime_state
            .ensure_replica(self.session.client_id);
        if let Some(offset) = continued_offset {
            if offset > replica.ack_offset {
                replica.ack_offset = offset;
            }
            if offset > replica.fsync_offset {
                replica.fsync_offset = offset;
            }
        }
        self.server.refresh_replica_ack_snapshots();
        response
    }

    fn handle_multi_command(&mut self) -> RespFrame {
        if self.session.transaction_state.in_transaction {
            return RespFrame::Error("ERR MULTI calls can not be nested".to_string());
        }
        self.session.transaction_state.in_transaction = true;
        self.session.transaction_state.exec_abort = false;
        self.session.transaction_state.command_queue.clear();
        RespFrame::SimpleString("OK".to_string())
    }

    fn handle_exec_command(
        &mut self,
        now_ms: u64,
        packet_id: u64,
        input_digest: &str,
        state_before: &str,
    ) -> RespFrame {
        if !self.session.transaction_state.in_transaction {
            return RespFrame::Error("ERR EXEC without MULTI".to_string());
        }
        let queued = std::mem::take(&mut self.session.transaction_state.command_queue);
        let exec_abort = self.session.transaction_state.exec_abort;
        self.session.transaction_state.in_transaction = false;
        self.session.transaction_state.exec_abort = false;

        if exec_abort {
            self.session.transaction_state.watched_keys.clear();
            self.session.transaction_state.watch_dirty = false;
            return RespFrame::Error(
                "EXECABORT Transaction discarded because of previous errors.".to_string(),
            );
        }

        // Check watched keys: if any were modified, abort the transaction
        let watch_failed = self.session.transaction_state.watch_dirty
            || {
                let mut dirty = false;
                for (key, original_fp, original_mod_count) in
                    &self.session.transaction_state.watched_keys
                {
                    let current_fp = self.server.store.key_fingerprint(key, now_ms);
                    let current_dirty = self.server.store.key_modification_count(key, now_ms);
                    if current_fp != *original_fp || current_dirty != *original_mod_count {
                        println!(
                            "WATCH FAILED: key={:?}, current_fp={}, original_fp={}, current_dirty={}, original_mod_count={}",
                            key, current_fp, original_fp, current_dirty, original_mod_count
                        );
                        dirty = true;
                        break;
                    }
                }
                dirty
            };
        self.session.transaction_state.watched_keys.clear();
        self.session.transaction_state.watch_dirty = false;

        if watch_failed {
            return RespFrame::Array(None);
        }

        let mut results = Vec::with_capacity(queued.len());
        let mut transaction_dirty = false;
        let mut transaction_aof = Vec::new();

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
            let dirty_before = self.server.store.dirty;
            let start = Instant::now();
            let result = self.execute_db_scoped_command(argv, now_ms);
            let elapsed_us = start.elapsed().as_micros() as u64;
            let dirty_after = self.server.store.dirty;
            self.record_slowlog(argv, elapsed_us, now_ms);

            match result {
                Ok(mut reply) => {
                    self.strip_db_prefixes_from_frame(&mut reply);
                    if dirty_after > dirty_before {
                        transaction_dirty = true;
                        transaction_aof.push(argv.clone());

                        // Optimized blocking: track keys modified by write commands
                        let cmd_keys = fr_command::command_keys(argv);
                        for key in &cmd_keys {
                            self.server.ready_keys.insert(key.clone());
                        }
                        // Keyspace notifications
                        if self.server.store.notify_keyspace_events != 0 {
                            let event = Self::command_to_keyspace_event(argv);
                            let event_type = Self::command_to_notify_type(argv);
                            let db = self.session.selected_db;
                            for key in &cmd_keys {
                                self.server
                                    .store
                                    .notify_keyspace_event(event_type, event, key, db);
                            }
                            self.deliver_keyspace_notifications();
                        }
                    }
                    results.push(reply);
                }
                Err(err) => results.push(err.to_resp()),
            }
        }

        if transaction_dirty {
            self.capture_aof_record(&[b"MULTI".to_vec()]);
            for argv in transaction_aof {
                self.capture_aof_record(&argv);
            }
            self.capture_aof_record(&[b"EXEC".to_vec()]);
        }

        RespFrame::Array(Some(results))
    }

    fn handle_discard_command(&mut self) -> RespFrame {
        if !self.session.transaction_state.in_transaction {
            return RespFrame::Error("ERR DISCARD without MULTI".to_string());
        }
        self.session.transaction_state.in_transaction = false;
        self.session.transaction_state.exec_abort = false;
        self.session.transaction_state.command_queue.clear();
        self.session.transaction_state.watched_keys.clear();
        self.session.transaction_state.watch_dirty = false;
        RespFrame::SimpleString("OK".to_string())
    }

    fn handle_watch_command(&mut self, argv: &[Vec<u8>], now_ms: u64) -> RespFrame {
        if argv.len() < 2 {
            return RespFrame::Error(
                "ERR wrong number of arguments for 'watch' command".to_string(),
            );
        }
        if self.session.transaction_state.in_transaction {
            return RespFrame::Error("ERR WATCH inside MULTI is not allowed".to_string());
        }
        for key in &argv[1..] {
            let physical = encode_db_key(self.session.selected_db, key);
            let fp = self.server.store.key_fingerprint(&physical, now_ms);
            let mod_count = self.server.store.key_modification_count(&physical, now_ms);
            self.session
                .transaction_state
                .watched_keys
                .push((physical, fp, mod_count));
        }
        RespFrame::SimpleString("OK".to_string())
    }

    fn handle_unwatch_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 1 {
            return RespFrame::Error(
                "ERR wrong number of arguments for 'unwatch' command".to_string(),
            );
        }
        self.session.transaction_state.watched_keys.clear();
        self.session.transaction_state.watch_dirty = false;
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
        let state_after = self.server.store.state_digest();
        let output_digest = digest_bytes(&input.output.to_bytes());
        self.server.evidence.record(EvidenceEvent {
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

fn build_hello_response(protocol_version: i64, client_id: u64) -> RespFrame {
    RespFrame::Array(Some(vec![
        hello_bulk("server"),
        hello_bulk("redis"),
        hello_bulk("version"),
        hello_bulk(fr_store::REDIS_COMPAT_VERSION),
        hello_bulk("proto"),
        RespFrame::Integer(protocol_version),
        hello_bulk("id"),
        RespFrame::Integer(client_id as i64),
        hello_bulk("mode"),
        hello_bulk("standalone"),
        hello_bulk("role"),
        hello_bulk("master"),
        hello_bulk("modules"),
        RespFrame::Array(Some(Vec::new())),
    ]))
}

/// Convert Store entries to RDB entries for snapshot persistence.
fn store_to_rdb_entries(store: &mut Store, now_ms: u64) -> Vec<RdbEntry> {
    use fr_store::Value;

    // Expire stale keys first.
    let all_keys = store.all_keys();
    for key in &all_keys {
        store.expire_key_if_stale(key, now_ms);
    }

    let mut entries = Vec::new();
    for key in store.all_keys() {
        let Some((value, expires_at_ms)) = store.get_value_and_expiry(&key) else {
            continue;
        };
        let (db, logical_key) = decode_db_key(&key).unwrap_or((0, key.as_slice()));
        let rdb_value = match value {
            Value::String(v) => RdbValue::String(v.clone()),
            Value::List(l) => RdbValue::List(l.iter().cloned().collect()),
            Value::Set(s) => {
                let mut members: Vec<Vec<u8>> = s.iter().cloned().collect();
                members.sort();
                RdbValue::Set(members)
            }
            Value::Hash(h) => {
                let mut fields: Vec<(Vec<u8>, Vec<u8>)> =
                    h.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                fields.sort_by(|a, b| a.0.cmp(&b.0));
                RdbValue::Hash(fields)
            }
            Value::SortedSet(zs) => {
                let members: Vec<(Vec<u8>, f64)> =
                    zs.iter_asc().map(|(m, s)| (m.clone(), *s)).collect();
                RdbValue::SortedSet(members)
            }
            Value::Stream(entries_map) => {
                let stream_entries: Vec<fr_persist::StreamEntry> = entries_map
                    .iter()
                    .map(|((ms, seq), fields)| {
                        let field_pairs: Vec<(Vec<u8>, Vec<u8>)> =
                            fields.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                        (*ms, *seq, field_pairs)
                    })
                    .collect();
                let watermark = store.stream_watermark(&key).unwrap_or(None);
                RdbValue::Stream(stream_entries, watermark)
            }
        };
        entries.push(RdbEntry {
            db,
            key: logical_key.to_vec(),
            value: rdb_value,
            expire_ms: expires_at_ms,
        });
    }
    entries
}

fn apply_rdb_entries_to_store(
    store: &mut Store,
    entries: &[RdbEntry],
    now_ms: u64,
) -> Result<(), PersistError> {
    for entry in entries {
        let key = encode_db_key(entry.db, &entry.key);
        match &entry.value {
            RdbValue::String(value) => {
                store.set_with_abs_expiry(key, value.clone(), entry.expire_ms, now_ms);
            }
            RdbValue::List(items) => {
                store
                    .rpush(&key, items, now_ms)
                    .map_err(|_| PersistError::InvalidFrame)?;
                if let Some(expires_at_ms) = entry.expire_ms {
                    store.expire_at_milliseconds(
                        &key,
                        i64::try_from(expires_at_ms).unwrap_or(i64::MAX),
                        now_ms,
                    );
                }
            }
            RdbValue::Set(members) => {
                store
                    .sadd(&key, members, now_ms)
                    .map_err(|_| PersistError::InvalidFrame)?;
                if let Some(expires_at_ms) = entry.expire_ms {
                    store.expire_at_milliseconds(
                        &key,
                        i64::try_from(expires_at_ms).unwrap_or(i64::MAX),
                        now_ms,
                    );
                }
            }
            RdbValue::Hash(fields) => {
                for (field, value) in fields {
                    store
                        .hset(&key, field.clone(), value.clone(), now_ms)
                        .map_err(|_| PersistError::InvalidFrame)?;
                }
                if let Some(expires_at_ms) = entry.expire_ms {
                    store.expire_at_milliseconds(
                        &key,
                        i64::try_from(expires_at_ms).unwrap_or(i64::MAX),
                        now_ms,
                    );
                }
            }
            RdbValue::SortedSet(members) => {
                let zset_members: Vec<(f64, Vec<u8>)> = members
                    .iter()
                    .map(|(member, score)| (*score, member.clone()))
                    .collect();
                store
                    .zadd(&key, &zset_members, now_ms)
                    .map_err(|_| PersistError::InvalidFrame)?;
                if let Some(expires_at_ms) = entry.expire_ms {
                    store.expire_at_milliseconds(
                        &key,
                        i64::try_from(expires_at_ms).unwrap_or(i64::MAX),
                        now_ms,
                    );
                }
            }
            RdbValue::Stream(stream_entries, watermark) => {
                for (ms, seq, fields) in stream_entries {
                    let field_pairs: Vec<(Vec<u8>, Vec<u8>)> =
                        fields.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                    let _ = store.xadd(&key, (*ms, *seq), &field_pairs, now_ms);
                }
                if let Some((wm_ms, wm_seq)) = watermark {
                    let _ = store.xsetid(&key, (*wm_ms, *wm_seq), now_ms);
                }
                if let Some(expires_at_ms) = entry.expire_ms {
                    store.expire_at_milliseconds(
                        &key,
                        i64::try_from(expires_at_ms).unwrap_or(i64::MAX),
                        now_ms,
                    );
                }
            }
        }
    }
    Ok(())
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
        RespParseError::BulkLengthTooLarge => {
            RespFrame::Error("ERR Protocol error: bulk length exceeds limit".to_string())
        }
        RespParseError::MultibulkLengthTooLarge => {
            RespFrame::Error("ERR Protocol error: multibulk length exceeds limit".to_string())
        }
        RespParseError::RecursionLimitExceeded => {
            RespFrame::Error("ERR Protocol error: recursion depth limit exceeded".to_string())
        }
        _ => RespFrame::Error("ERR Protocol error: unknown parse error".to_string()),
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

    use fr_command::{CommandError, dispatch_argv};
    use fr_config::{
        DecisionAction, DriftSeverity, HardenedDeviationCategory, Mode, RuntimePolicy, ThreatClass,
        TlsAuthClients, TlsConfig, TlsProtocol,
    };
    use fr_eventloop::{
        AcceptPathError, ActiveExpireCycleKind, BarrierOrderError, EVENT_LOOP_PHASE_ORDER,
        EventLoopMode, EventLoopPhase, FdRegistrationError, LoopBootstrap, PendingWriteError,
        ReadPathError, ReadinessCallback, TickBudget,
    };
    use fr_persist::{AofRecord, PersistError, RdbValue, decode_aof_stream, write_aof_file};
    use fr_protocol::{RespFrame, parse_frame};

    use super::{
        ClientSession, ClusterClientMode, ClusterSubcommand, DEFAULT_AUTH_USER, Runtime,
        ServerState, classify_cluster_subcommand, classify_cluster_subcommand_linear,
        classify_runtime_special_command, classify_runtime_special_command_linear,
        store_to_rdb_entries,
    };

    fn command(parts: &[&[u8]]) -> RespFrame {
        RespFrame::Array(Some(
            parts
                .iter()
                .map(|part| RespFrame::BulkString(Some((*part).to_vec())))
                .collect(),
        ))
    }

    fn argv(parts: &[&[u8]]) -> Vec<Vec<u8>> {
        parts.iter().map(|part| (*part).to_vec()).collect()
    }

    #[test]
    fn fr_p2c_001_u001_runtime_exposes_deterministic_phase_order() {
        let plan =
            Runtime::plan_event_loop_tick(1, 3, TickBudget::default(), EventLoopMode::Normal);
        assert_eq!(plan.phase_order, EVENT_LOOP_PHASE_ORDER);
    }

    #[test]
    fn fr_p2c_001_u001a_runtime_command_paths_update_server_state() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"CONFIG",
                    b"SET",
                    b"maxmemory",
                    b"1024",
                    b"hz",
                    b"42",
                    b"timeout",
                    b"30",
                ]),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        assert_eq!(rt.server.maxmemory_bytes, 1024);
        assert_eq!(rt.server.hz, 42);
        assert_eq!(
            rt.server
                .config_overrides
                .get("timeout")
                .map(String::as_str),
            Some("30")
        );
    }

    #[test]
    fn fr_p2c_001_u001b_server_state_is_shared_while_sessions_remain_isolated() {
        let mut server = ServerState::default();
        let mut writer = ClientSession::new_for_server(&server);
        let reader = ClientSession::new_for_server(&server);

        writer.client_name = Some(b"writer".to_vec());
        server.configure_maxmemory_enforcement(64, 0, 16, 4);
        server
            .config_overrides
            .insert("timeout".to_string(), "60".to_string());

        assert_eq!(
            dispatch_argv(
                &argv(&[b"SET", b"shared:key", b"value"]),
                &mut server.store,
                0
            ),
            Ok(RespFrame::SimpleString("OK".to_string()))
        );
        assert_eq!(
            dispatch_argv(&argv(&[b"GET", b"shared:key"]), &mut server.store, 1),
            Ok(RespFrame::BulkString(Some(b"value".to_vec())))
        );
        assert_eq!(server.maxmemory_bytes, 64);
        assert_eq!(
            server.config_overrides.get("timeout").map(String::as_str),
            Some("60")
        );
        assert_eq!(writer.client_name.as_deref(), Some(b"writer".as_slice()));
        assert_eq!(reader.client_name, None);
        assert_ne!(writer.client_id, reader.client_id);
    }

    #[test]
    fn fr_p2c_001_u001c_client_sessions_isolate_auth_protocol_db_and_transaction_state() {
        let mut server = ServerState::default();
        server.auth_state.set_requirepass(Some(b"secret".to_vec()));

        let mut authenticated = ClientSession::new_for_server(&server);
        let isolated = ClientSession::new_for_server(&server);

        assert!(!authenticated.is_authenticated());
        assert!(!isolated.is_authenticated());

        authenticated.authenticated_user = Some(DEFAULT_AUTH_USER.to_vec());
        authenticated.selected_db = 5;
        authenticated.resp_protocol_version = 3;
        authenticated.client_name = Some(b"alpha".to_vec());
        authenticated.transaction_state.in_transaction = true;
        authenticated
            .transaction_state
            .watched_keys
            .push((b"watched".to_vec(), 7, 0));
        authenticated.cluster_state.mode = ClusterClientMode::ReadOnly;
        authenticated.cluster_state.asking = true;

        assert_eq!(isolated.authenticated_user, None);
        assert_eq!(isolated.selected_db, 0);
        assert_eq!(isolated.resp_protocol_version, 2);
        assert_eq!(isolated.client_name, None);
        assert!(!isolated.transaction_state.in_transaction);
        assert!(isolated.transaction_state.watched_keys.is_empty());
        assert_eq!(isolated.cluster_state.mode, ClusterClientMode::ReadWrite);
        assert!(!isolated.cluster_state.asking);
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
                RespFrame::BulkString(Some(b"redis".to_vec())),
                RespFrame::BulkString(Some(b"version".to_vec())),
                RespFrame::BulkString(Some(b"7.2.0".to_vec())),
                RespFrame::BulkString(Some(b"proto".to_vec())),
                RespFrame::Integer(3),
                RespFrame::BulkString(Some(b"id".to_vec())),
                RespFrame::Integer(rt.session.client_id as i64),
                RespFrame::BulkString(Some(b"mode".to_vec())),
                RespFrame::BulkString(Some(b"standalone".to_vec())),
                RespFrame::BulkString(Some(b"role".to_vec())),
                RespFrame::BulkString(Some(b"master".to_vec())),
                RespFrame::BulkString(Some(b"modules".to_vec())),
                RespFrame::Array(Some(Vec::new())),
            ]))
        );
        assert!(rt.is_authenticated());
    }

    #[test]
    fn fr_p2c_004_u004a_hello_failed_auth_does_not_leak_setname_or_protocol_state() {
        let mut rt = Runtime::default_strict();
        rt.set_requirepass(Some(b"secret".to_vec()));

        let wrong = rt.execute_frame(
            command(&[
                b"HELLO", b"3", b"SETNAME", b"leak", b"AUTH", b"default", b"bad",
            ]),
            0,
        );
        assert_eq!(
            wrong,
            RespFrame::Error(
                "WRONGPASS invalid username-password pair or user is disabled.".to_string()
            )
        );
        assert_eq!(rt.session.client_name, None);
        assert_eq!(rt.session.resp_protocol_version, 2);
        assert!(!rt.is_authenticated());
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
                RespFrame::BulkString(Some(b"redis".to_vec())),
                RespFrame::BulkString(Some(b"version".to_vec())),
                RespFrame::BulkString(Some(b"7.2.0".to_vec())),
                RespFrame::BulkString(Some(b"proto".to_vec())),
                RespFrame::Integer(3),
                RespFrame::BulkString(Some(b"id".to_vec())),
                RespFrame::Integer(rt.session.client_id as i64),
                RespFrame::BulkString(Some(b"mode".to_vec())),
                RespFrame::BulkString(Some(b"standalone".to_vec())),
                RespFrame::BulkString(Some(b"role".to_vec())),
                RespFrame::BulkString(Some(b"master".to_vec())),
                RespFrame::BulkString(Some(b"modules".to_vec())),
                RespFrame::Array(Some(Vec::new())),
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
    fn fr_p2c_004_u009b_client_list_reflects_session_protocol_db_and_user_state() {
        let mut rt = Runtime::default_strict();
        rt.session.selected_db = 5;

        let hello = rt.execute_frame(command(&[b"HELLO", b"3", b"SETNAME", b"alpha"]), 0);
        assert_eq!(
            hello,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"server".to_vec())),
                RespFrame::BulkString(Some(b"redis".to_vec())),
                RespFrame::BulkString(Some(b"version".to_vec())),
                RespFrame::BulkString(Some(b"7.2.0".to_vec())),
                RespFrame::BulkString(Some(b"proto".to_vec())),
                RespFrame::Integer(3),
                RespFrame::BulkString(Some(b"id".to_vec())),
                RespFrame::Integer(rt.session.client_id as i64),
                RespFrame::BulkString(Some(b"mode".to_vec())),
                RespFrame::BulkString(Some(b"standalone".to_vec())),
                RespFrame::BulkString(Some(b"role".to_vec())),
                RespFrame::BulkString(Some(b"master".to_vec())),
                RespFrame::BulkString(Some(b"modules".to_vec())),
                RespFrame::Array(Some(Vec::new())),
            ]))
        );

        let client_list = rt.execute_frame(command(&[b"CLIENT", b"LIST"]), 1);
        let info = match client_list {
            RespFrame::BulkString(Some(info)) => String::from_utf8(info).expect("client info utf8"),
            other => panic!("unexpected client list response: {other:?}"),
        };
        assert!(info.contains("name=alpha"));
        assert!(info.contains("db=5"));
        assert!(info.contains("user=default"));
        assert!(info.contains("resp=3"));
    }

    #[test]
    fn fr_p2c_004_u009c_reset_clears_session_protocol_and_deauths_when_auth_required() {
        let mut rt = Runtime::default_strict();
        rt.set_requirepass(Some(b"secret".to_vec()));

        assert_eq!(
            rt.execute_frame(command(&[b"AUTH", b"secret"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"HELLO", b"3", b"SETNAME", b"alpha"]), 1),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"server".to_vec())),
                RespFrame::BulkString(Some(b"redis".to_vec())),
                RespFrame::BulkString(Some(b"version".to_vec())),
                RespFrame::BulkString(Some(b"7.2.0".to_vec())),
                RespFrame::BulkString(Some(b"proto".to_vec())),
                RespFrame::Integer(3),
                RespFrame::BulkString(Some(b"id".to_vec())),
                RespFrame::Integer(rt.session.client_id as i64),
                RespFrame::BulkString(Some(b"mode".to_vec())),
                RespFrame::BulkString(Some(b"standalone".to_vec())),
                RespFrame::BulkString(Some(b"role".to_vec())),
                RespFrame::BulkString(Some(b"master".to_vec())),
                RespFrame::BulkString(Some(b"modules".to_vec())),
                RespFrame::Array(Some(Vec::new())),
            ]))
        );
        rt.session.selected_db = 7;

        assert_eq!(
            rt.execute_frame(command(&[b"RESET"]), 2),
            RespFrame::SimpleString("RESET".to_string())
        );
        assert!(!rt.is_authenticated());
        assert_eq!(rt.session.selected_db, 0);
        assert_eq!(rt.session.resp_protocol_version, 2);
        assert_eq!(rt.session.client_name, None);
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"k"]), 3),
            RespFrame::Error("NOAUTH Authentication required.".to_string())
        );
    }

    #[test]
    fn pubsub_subscription_mode_is_scoped_to_active_client() {
        let mut rt = Runtime::default_strict();
        let subscriber = rt.new_session();
        let other_client = rt.new_session();

        let previous = rt.swap_session(subscriber);
        assert_eq!(
            rt.execute_frame(command(&[b"SUBSCRIBE", b"alpha"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"subscribe".to_vec())),
                RespFrame::BulkString(Some(b"alpha".to_vec())),
                RespFrame::Integer(1),
            ]))
        );
        assert!(rt.is_in_subscription_mode());

        let subscriber = rt.swap_session(other_client);
        assert!(!rt.is_in_subscription_mode());
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"k"]), 1),
            RespFrame::BulkString(None)
        );

        let _other_client = rt.swap_session(subscriber);
        let _ = rt.swap_session(previous);
    }

    #[test]
    fn pubsub_multi_subscribe_encodes_as_resp_sequence() {
        let mut rt = Runtime::default_strict();
        let reply = rt.execute_frame(command(&[b"SUBSCRIBE", b"alpha", b"beta"]), 0);
        match &reply {
            RespFrame::Sequence(items) => {
                assert_eq!(items.len(), 2);
            }
            other => panic!("expected RESP sequence, got {other:?}"),
        }
        assert_eq!(
            reply.to_bytes(),
            b"*3\r\n$9\r\nsubscribe\r\n$5\r\nalpha\r\n:1\r\n*3\r\n$9\r\nsubscribe\r\n$4\r\nbeta\r\n:2\r\n"
                .to_vec()
        );
    }

    #[test]
    fn live_pubsub_introspection_uses_runtime_subscription_state() {
        let mut rt = Runtime::default_strict();
        let first_client = rt.new_session();
        let second_client = rt.new_session();

        let previous = rt.swap_session(first_client);
        assert_eq!(
            rt.execute_frame(command(&[b"SUBSCRIBE", b"alpha", b"beta"]), 0),
            RespFrame::Sequence(vec![
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"subscribe".to_vec())),
                    RespFrame::BulkString(Some(b"alpha".to_vec())),
                    RespFrame::Integer(1),
                ])),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"subscribe".to_vec())),
                    RespFrame::BulkString(Some(b"beta".to_vec())),
                    RespFrame::Integer(2),
                ])),
            ])
        );
        assert_eq!(
            rt.execute_frame(command(&[b"PSUBSCRIBE", b"a*"]), 1),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"psubscribe".to_vec())),
                RespFrame::BulkString(Some(b"a*".to_vec())),
                RespFrame::Integer(3),
            ]))
        );

        let first_client = rt.swap_session(second_client);
        assert_eq!(
            rt.execute_frame(command(&[b"SUBSCRIBE", b"alpha"]), 2),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"subscribe".to_vec())),
                RespFrame::BulkString(Some(b"alpha".to_vec())),
                RespFrame::Integer(1),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SSUBSCRIBE", b"shard-1"]), 3),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"ssubscribe".to_vec())),
                RespFrame::BulkString(Some(b"shard-1".to_vec())),
                RespFrame::Integer(1),
            ]))
        );

        let second_client = rt.swap_session(first_client);
        assert_eq!(
            rt.execute_frame(command(&[b"PUBSUB", b"CHANNELS"]), 4),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"alpha".to_vec())),
                RespFrame::BulkString(Some(b"beta".to_vec())),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"PUBSUB", b"CHANNELS", b"a*"]), 5),
            RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"alpha".to_vec()))]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"PUBSUB", b"NUMSUB", b"alpha", b"beta"]), 6),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"alpha".to_vec())),
                RespFrame::Integer(2),
                RespFrame::BulkString(Some(b"beta".to_vec())),
                RespFrame::Integer(1),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"PUBSUB", b"NUMPAT"]), 7),
            RespFrame::Integer(1)
        );
        assert_eq!(
            rt.execute_frame(command(&[b"PUBSUB", b"SHARDCHANNELS"]), 8),
            RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"shard-1".to_vec()))]))
        );
        assert_eq!(
            rt.execute_frame(
                command(&[b"PUBSUB", b"SHARDNUMSUB", b"shard-1", b"shard-2"]),
                9
            ),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"shard-1".to_vec())),
                RespFrame::Integer(1),
                RespFrame::BulkString(Some(b"shard-2".to_vec())),
                RespFrame::Integer(0),
            ]))
        );

        let _ = rt.swap_session(second_client);
        let _ = rt.swap_session(previous);
    }

    #[test]
    fn live_pubsub_help_and_arity_follow_redis_syntax() {
        let mut rt = Runtime::default_strict();
        let help = rt.execute_frame(command(&[b"PUBSUB", b"HELP"]), 0);
        let RespFrame::Array(Some(lines)) = help else {
            panic!("expected pubsub help array");
        };
        assert!(lines.len() >= 6);

        assert_eq!(
            rt.execute_frame(command(&[b"PUBSUB", b"HELP", b"extra"]), 1),
            RespFrame::Error("ERR syntax error".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"PUBSUB", b"CHANNELS", b"a*", b"extra"]), 2),
            RespFrame::Error("ERR syntax error".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"PUBSUB", b"NUMPAT", b"extra"]), 3),
            RespFrame::Error("ERR syntax error".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"PUBSUB", b"BOGUS"]), 4),
            RespFrame::Error("ERR syntax error".to_string())
        );
    }

    #[test]
    fn reset_only_clears_callers_pubsub_state() {
        let mut rt = Runtime::default_strict();
        let first_client = rt.new_session();
        let second_client = rt.new_session();

        let previous = rt.swap_session(first_client);
        assert_eq!(
            rt.execute_frame(command(&[b"SUBSCRIBE", b"alpha"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"subscribe".to_vec())),
                RespFrame::BulkString(Some(b"alpha".to_vec())),
                RespFrame::Integer(1),
            ]))
        );

        let first_client = rt.swap_session(second_client);
        assert_eq!(
            rt.execute_frame(command(&[b"SUBSCRIBE", b"beta"]), 1),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"subscribe".to_vec())),
                RespFrame::BulkString(Some(b"beta".to_vec())),
                RespFrame::Integer(1),
            ]))
        );
        assert!(
            rt.server
                .store
                .subscribed_channels
                .contains(b"alpha".as_slice())
        );
        assert!(
            rt.server
                .store
                .subscribed_channels
                .contains(b"beta".as_slice())
        );

        let second_client = rt.swap_session(first_client);
        assert_eq!(
            rt.execute_frame(command(&[b"RESET"]), 2),
            RespFrame::SimpleString("RESET".to_string())
        );
        assert!(
            !rt.server
                .store
                .subscribed_channels
                .contains(b"alpha".as_slice())
        );
        assert!(
            rt.server
                .store
                .subscribed_channels
                .contains(b"beta".as_slice())
        );

        let _first_client = rt.swap_session(second_client);
        assert!(rt.is_in_subscription_mode());
        assert_eq!(
            rt.execute_frame(command(&[b"UNSUBSCRIBE"]), 3),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"unsubscribe".to_vec())),
                RespFrame::BulkString(Some(b"beta".to_vec())),
                RespFrame::Integer(0),
            ]))
        );

        let _ = rt.swap_session(previous);
    }

    #[test]
    fn multi_db_select_scopes_keyspace_commands() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"shared", b"db0"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SELECT", b"2"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"shared", b"db2"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"other", b"x"]), 3),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"DBSIZE"]), 4),
            RespFrame::Integer(2)
        );
        assert_eq!(
            rt.execute_frame(command(&[b"KEYS", b"*"]), 5),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"other".to_vec())),
                RespFrame::BulkString(Some(b"shared".to_vec())),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SCAN", b"0"]), 6),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"0".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"other".to_vec())),
                    RespFrame::BulkString(Some(b"shared".to_vec())),
                ])),
            ]))
        );

        assert_eq!(
            rt.execute_frame(command(&[b"SELECT", b"0"]), 7),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"shared"]), 8),
            RespFrame::BulkString(Some(b"db0".to_vec()))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"DBSIZE"]), 9),
            RespFrame::Integer(1)
        );
    }

    #[test]
    fn multi_db_move_and_swapdb_preserve_logical_keys() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"moveme", b"value"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"MOVE", b"moveme", b"3"]), 1),
            RespFrame::Integer(1)
        );
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"moveme"]), 2),
            RespFrame::BulkString(None)
        );

        assert_eq!(
            rt.execute_frame(command(&[b"SELECT", b"3"]), 3),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"moveme"]), 4),
            RespFrame::BulkString(Some(b"value".to_vec()))
        );

        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"swap", b"db3"]), 5),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SELECT", b"0"]), 6),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"swap", b"db0"]), 7),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SWAPDB", b"0", b"3"]), 8),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"swap"]), 9),
            RespFrame::BulkString(Some(b"db3".to_vec()))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SELECT", b"3"]), 10),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"swap"]), 11),
            RespFrame::BulkString(Some(b"db0".to_vec()))
        );
    }

    #[test]
    fn multi_db_persistence_tracks_selected_db_boundaries() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"zero", b"0"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SELECT", b"4"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"four", b"4"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );

        assert_eq!(
            rt.aof_records(),
            [
                AofRecord {
                    argv: vec![b"SET".to_vec(), b"zero".to_vec(), b"0".to_vec()],
                },
                AofRecord {
                    argv: vec![b"SELECT".to_vec(), b"4".to_vec()],
                },
                AofRecord {
                    argv: vec![b"SET".to_vec(), b"four".to_vec(), b"4".to_vec()],
                },
            ]
            .as_slice()
        );

        let entries = store_to_rdb_entries(&mut rt.server.store, 3);
        assert!(entries.iter().any(|entry| {
            entry.db == 0 && entry.key == b"zero" && entry.value == RdbValue::String(b"0".to_vec())
        }));
        assert!(entries.iter().any(|entry| {
            entry.db == 4 && entry.key == b"four" && entry.value == RdbValue::String(b"4".to_vec())
        }));
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

    fn read_runtime_test_frame(
        stream: &mut std::net::TcpStream,
        read_buf: &mut Vec<u8>,
    ) -> RespFrame {
        let parser_config = fr_protocol::ParserConfig::default();
        loop {
            match fr_protocol::parse_frame_with_config(read_buf, &parser_config) {
                Ok(parsed) => {
                    read_buf.drain(..parsed.consumed);
                    return parsed.frame;
                }
                Err(fr_protocol::RespParseError::Incomplete) => {}
                Err(err) => panic!("failed to parse runtime test frame: {err:?}"),
            }

            let mut chunk = [0u8; 4096];
            let n = std::io::Read::read(stream, &mut chunk).expect("read runtime test frame");
            assert_ne!(
                n, 0,
                "connection closed before runtime test frame completed"
            );
            read_buf.extend_from_slice(&chunk[..n]);
        }
    }

    fn runtime_test_frame_to_argv(frame: RespFrame) -> Vec<Vec<u8>> {
        match frame {
            RespFrame::Array(Some(items)) => items
                .into_iter()
                .map(|item| match item {
                    RespFrame::BulkString(Some(bytes)) => bytes,
                    other => panic!("expected bulk string, got {other:?}"),
                })
                .collect(),
            other => panic!("expected array frame, got {other:?}"),
        }
    }

    #[test]
    fn migrate_in_selected_db_propagates_as_del_not_migrate() {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind migrate runtime listener");
        let addr = listener.local_addr().expect("runtime listener addr");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<Vec<u8>>>::new()));
        let seen_clone = std::sync::Arc::clone(&seen);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept runtime migrate client");
            let mut read_buf = Vec::new();
            for _ in 0..2 {
                let argv =
                    runtime_test_frame_to_argv(read_runtime_test_frame(&mut stream, &mut read_buf));
                seen_clone.lock().expect("lock seen").push(argv);
                std::io::Write::write_all(
                    &mut stream,
                    &RespFrame::SimpleString("OK".to_string()).to_bytes(),
                )
                .expect("write runtime migrate reply");
            }
        });

        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"k", b"db0"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SELECT", b"2"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"k", b"db2"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );

        let reply = rt.execute_frame(
            command(&[
                b"MIGRATE",
                addr.ip().to_string().as_bytes(),
                addr.port().to_string().as_bytes(),
                b"k",
                b"9",
                b"5000",
            ]),
            3,
        );
        assert_eq!(reply, RespFrame::SimpleString("OK".to_string()));

        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"k"]), 4),
            RespFrame::BulkString(None)
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SELECT", b"0"]), 5),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"k"]), 6),
            RespFrame::BulkString(Some(b"db0".to_vec()))
        );

        let migrate_records: Vec<&AofRecord> = rt
            .aof_records()
            .iter()
            .filter(|record| {
                record
                    .argv
                    .first()
                    .is_some_and(|cmd| cmd.eq_ignore_ascii_case(b"MIGRATE"))
            })
            .collect();
        assert!(migrate_records.is_empty(), "MIGRATE must not enter the AOF");
        assert_eq!(
            rt.aof_records().last().expect("last aof record").argv,
            vec![b"DEL".to_vec(), b"k".to_vec()]
        );

        server.join().expect("join runtime fake target");
        let seen = seen.lock().expect("lock seen");
        assert_eq!(seen[0], vec![b"SELECT".to_vec(), b"9".to_vec()]);
        assert_eq!(seen[1][0], b"RESTORE".to_vec());
        assert_eq!(seen[1][1], b"k".to_vec());
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
            RespFrame::Error("ERR wrong number of arguments for 'wait' command".to_string())
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
        rt.set_aof_path(std::path::PathBuf::from("appendonly.aof"));
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
            RespFrame::Error("ERR wrong number of arguments for 'waitaof' command".to_string())
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
    fn waitaof_rejects_local_threshold_when_appendonly_is_disabled() {
        let mut rt = Runtime::default_strict();

        let out = rt.execute_frame(command(&[b"WAITAOF", b"1", b"0", b"0"]), 0);
        assert_eq!(
            out,
            RespFrame::Error(
                "ERR WAITAOF cannot be used when numlocal is set but appendonly is disabled."
                    .to_string()
            )
        );
    }

    #[test]
    fn waitaof_rejects_replica_instances() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"REPLICAOF", b"127.0.0.1", b"6379"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );

        let out = rt.execute_frame(command(&[b"WAITAOF", b"0", b"0", b"0"]), 1);
        assert_eq!(
            out,
            RespFrame::Error(
                "ERR WAITAOF cannot be used with replica instances. Please also note that writes to replicas are just local and are not propagated.".to_string()
            )
        );
    }

    #[test]
    fn monitor_registers_client_and_streams_subsequent_commands() {
        let mut rt = Runtime::default_strict();
        rt.session.client_id = 42;

        assert_eq!(
            rt.execute_frame(command(&[b"MONITOR"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        assert!(rt.server.monitor_clients.contains(&42));
        assert!(rt.drain_monitor_output().is_empty());

        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"alpha", b"1"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );

        assert_eq!(
            rt.drain_monitor_output(),
            vec![(
                42,
                b"+0.002000 [0 127.0.0.1:0] \"SET\" \"alpha\" \"1\"\r\n".to_vec(),
            )]
        );
    }

    #[test]
    fn monitor_uses_selected_db_in_output() {
        let mut rt = Runtime::default_strict();
        rt.session.client_id = 7;

        assert_eq!(
            rt.execute_frame(command(&[b"MONITOR"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SELECT", b"2"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
        rt.drain_monitor_output();

        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"beta", b"2"]), 3),
            RespFrame::SimpleString("OK".to_string())
        );

        assert_eq!(
            rt.drain_monitor_output(),
            vec![(
                7,
                b"+0.003000 [2 127.0.0.1:0] \"SET\" \"beta\" \"2\"\r\n".to_vec(),
            )]
        );
    }

    #[test]
    fn client_tracking_returns_ok_matching_redis() {
        // Redis 7.2 returns OK for CLIENT TRACKING ON/OFF.
        // Client libraries depend on this — DO NOT change to expect error.
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"TRACKING", b"ON"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"TRACKING", b"OFF"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
    }

    #[test]
    fn client_caching_returns_ok_matching_redis() {
        // Redis 7.2 returns OK for CLIENT CACHING YES/NO.
        // Client libraries depend on this — DO NOT change to expect error.
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"CACHING", b"YES"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"CACHING", b"NO"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
    }

    #[test]
    fn client_list_reports_live_pubsub_counts() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"SUBSCRIBE", b"chan"]), 1),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"subscribe".to_vec())),
                RespFrame::BulkString(Some(b"chan".to_vec())),
                RespFrame::Integer(1),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"PSUBSCRIBE", b"pat:*"]), 2),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"psubscribe".to_vec())),
                RespFrame::BulkString(Some(b"pat:*".to_vec())),
                RespFrame::Integer(2),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SSUBSCRIBE", b"shard"]), 3),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"ssubscribe".to_vec())),
                RespFrame::BulkString(Some(b"shard".to_vec())),
                RespFrame::Integer(1),
            ]))
        );

        let client_info = rt.execute_frame(command(&[b"CLIENT", b"INFO"]), 4);
        let info = match client_info {
            RespFrame::BulkString(Some(info)) => String::from_utf8(info).expect("client info utf8"),
            other => panic!("unexpected client info response: {other:?}"),
        };
        assert!(info.contains("sub=1"));
        assert!(info.contains("psub=1"));
        assert!(info.contains("ssub=1"));
    }

    #[test]
    fn client_info_rejects_extra_arguments() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"INFO", b"extra"]), 1),
            RespFrame::Error("ERR wrong number of arguments for 'client' command".to_string())
        );
    }

    #[test]
    fn client_list_applies_single_client_type_and_id_filters() {
        let mut rt = Runtime::default_strict();
        // Get the actual client ID (varies based on test execution order)
        let id_frame = rt.execute_frame(command(&[b"CLIENT", b"ID"]), 0);
        let client_id = match id_frame {
            RespFrame::Integer(n) => n.to_string(),
            _ => panic!("CLIENT ID should return integer"),
        };
        let id_bytes = client_id.as_bytes().to_vec();

        let list_all = rt.execute_frame(command(&[b"CLIENT", b"LIST"]), 1);
        let list_normal = rt.execute_frame(command(&[b"CLIENT", b"LIST", b"TYPE", b"normal"]), 2);
        let list_replica = rt.execute_frame(command(&[b"CLIENT", b"LIST", b"TYPE", b"replica"]), 3);
        let list_id_match = rt.execute_frame(command(&[b"CLIENT", b"LIST", b"ID", &id_bytes]), 4);
        let list_id_miss = rt.execute_frame(command(&[b"CLIENT", b"LIST", b"ID", b"999999999"]), 5);

        assert_eq!(list_normal, list_all);
        assert_eq!(list_replica, RespFrame::BulkString(Some(Vec::new())));
        assert_eq!(list_id_match, list_all);
        assert_eq!(list_id_miss, RespFrame::BulkString(Some(Vec::new())));
    }

    #[test]
    fn client_list_rejects_invalid_filters() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"LIST", b"TYPE", b"bogus"]), 1),
            RespFrame::Error("ERR Unknown client type 'bogus'".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"LIST", b"ID", b"nope"]), 2),
            RespFrame::Error("ERR Invalid client ID".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"LIST", b"EXTRA"]), 3),
            RespFrame::Error("ERR syntax error".to_string())
        );
    }

    #[test]
    fn client_reply_and_help_reject_invalid_arguments() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"REPLY", b"MAYBE"]), 1),
            RespFrame::Error("ERR syntax error".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"HELP", b"extra"]), 2),
            RespFrame::Error("ERR wrong number of arguments for 'client' command".to_string())
        );
    }

    #[test]
    fn slowlog_get_uses_redis_default_count_and_minus_one_means_all() {
        let mut rt = Runtime::default_strict();
        rt.server.slowlog_max_len = 64;
        rt.server.slowlog_log_slower_than_us = 0;
        for idx in 0..12u64 {
            let key = format!("k{idx}");
            rt.record_slowlog(
                &[b"SET".to_vec(), key.into_bytes(), b"v".to_vec()],
                50,
                idx * 1000,
            );
        }

        let default = rt.execute_frame(command(&[b"SLOWLOG", b"GET"]), 99);
        let RespFrame::Array(Some(default_entries)) = default else {
            panic!("expected default slowlog array");
        };
        assert_eq!(default_entries.len(), 10);

        let all = rt.execute_frame(command(&[b"SLOWLOG", b"GET", b"-1"]), 100);
        let RespFrame::Array(Some(all_entries)) = all else {
            panic!("expected all slowlog array");
        };
        assert_eq!(all_entries.len(), 12);
    }

    #[test]
    fn slowlog_rejects_invalid_shapes_and_negative_count_below_minus_one() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"SLOWLOG", b"GET", b"-2"]), 1),
            RespFrame::Error("ERR count should be greater than or equal to -1".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SLOWLOG", b"GET", b"1", b"extra"]), 2),
            RespFrame::Error(
                "ERR wrong number of arguments for 'slowlog|get' subcommand".to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SLOWLOG", b"LEN", b"extra"]), 3),
            RespFrame::Error(
                "ERR wrong number of arguments for 'slowlog|len' subcommand".to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SLOWLOG", b"RESET", b"extra"]), 4),
            RespFrame::Error(
                "ERR wrong number of arguments for 'slowlog|reset' subcommand".to_string()
            )
        );
    }

    #[test]
    fn client_unblock_rejects_invalid_arguments() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"UNBLOCK", b"nope"]), 1),
            RespFrame::Error("ERR value is not an integer or out of range".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"UNBLOCK", b"1", b"wat"]), 1),
            RespFrame::Error("ERR syntax error".to_string())
        );
    }

    #[test]
    fn client_pause_uses_runtime_clock_for_expiry() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"PAUSE", b"50", b"ALL"]), 1_000),
            RespFrame::SimpleString("OK".to_string())
        );
        assert!(rt.is_client_paused(1_049));
        assert!(!rt.is_client_paused(1_050));
    }

    #[test]
    fn client_unpause_clears_pause_state_immediately() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"PAUSE", b"100", b"ALL"]), 500),
            RespFrame::SimpleString("OK".to_string())
        );
        assert!(rt.is_client_paused(550));

        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"UNPAUSE"]), 551),
            RespFrame::SimpleString("OK".to_string())
        );
        assert!(!rt.is_client_paused(551));
    }

    #[test]
    fn replication_role_transitions_change_role_shape() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"REPLICAOF", b"127.0.0.1", b"6380"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"ROLE"]), 1),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"slave".to_vec())),
                RespFrame::BulkString(Some(b"127.0.0.1".to_vec())),
                RespFrame::Integer(6380),
                RespFrame::BulkString(Some(b"connect".to_vec())),
                RespFrame::Integer(0),
            ]))
        );

        assert_eq!(
            rt.execute_frame(command(&[b"REPLICAOF", b"NO", b"ONE"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"ROLE"]), 3),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"master".to_vec())),
                RespFrame::Integer(0),
                RespFrame::Array(Some(Vec::new())),
            ]))
        );
    }

    #[test]
    fn replication_replicaof_same_master_is_idempotent() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"REPLICAOF", b"127.0.0.1", b"6380"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"REPLICAOF", b"127.0.0.1", b"6380"]), 1),
            RespFrame::SimpleString("OK Already connected to specified master".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"ROLE"]), 2),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"slave".to_vec())),
                RespFrame::BulkString(Some(b"127.0.0.1".to_vec())),
                RespFrame::Integer(6380),
                RespFrame::BulkString(Some(b"connect".to_vec())),
                RespFrame::Integer(0),
            ]))
        );
    }

    #[test]
    fn replication_psync_continue_uses_live_backlog_window() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"rep:key", b"value"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        let replid = rt.server.replication_runtime_state.backlog.replid.clone();
        let live_offset = rt.replication_primary_offset().0.to_string();

        assert_eq!(
            rt.execute_frame(
                command(&[b"PSYNC", replid.as_bytes(), live_offset.as_bytes()]),
                1
            ),
            RespFrame::SimpleString("CONTINUE".to_string())
        );
        assert_eq!(
            rt.execute_frame(
                command(&[b"PSYNC", b"wrong-replid", live_offset.as_bytes()]),
                2
            ),
            RespFrame::SimpleString(format!("FULLRESYNC {} {}", replid, live_offset))
        );
    }

    #[test]
    fn replication_fullresync_psync_does_not_credit_replica_with_requested_offset() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"rep:key", b"value"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"REPLCONF", b"listening-port", b"6380"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );

        let live_offset = rt.replication_primary_offset().0.to_string();
        let bogus_requested_offset = b"999999";
        assert_eq!(
            rt.execute_frame(
                command(&[b"PSYNC", b"wrong-replid", bogus_requested_offset]),
                2
            ),
            RespFrame::SimpleString(format!(
                "FULLRESYNC {} {}",
                rt.server.replication_runtime_state.backlog.replid, live_offset
            ))
        );

        let replica = rt
            .server
            .replication_runtime_state
            .replicas
            .get(&rt.session.client_id)
            .expect("replica state");
        assert_eq!(replica.ack_offset, fr_repl::ReplOffset(0));
        assert_eq!(replica.fsync_offset, fr_repl::ReplOffset(0));
    }

    #[test]
    fn replication_sync_alias_triggers_fullresync() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"SYNC"]), 0),
            RespFrame::SimpleString(
                "FULLRESYNC 0000000000000000000000000000000000000000 0".to_string()
            )
        );
    }

    #[test]
    fn replication_info_reports_byte_offsets() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"rep:key", b"value"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        let expected_offset = rt.replication_primary_offset().0;

        let info = rt.execute_frame(command(&[b"INFO", b"replication"]), 1);
        let RespFrame::BulkString(Some(info_bytes)) = info else {
            panic!("expected bulk INFO response");
        };
        let info = String::from_utf8(info_bytes).expect("utf8 info");
        assert!(info.contains(&format!("master_repl_offset:{expected_offset}\r\n")));
        assert!(info.contains(&format!("repl_backlog_histlen:{expected_offset}\r\n")));
        assert!(info.contains("repl_backlog_first_byte_offset:1\r\n"));
    }

    #[test]
    fn live_info_supports_multiple_requested_sections() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"key", b"value"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );

        let info = rt.execute_frame(
            command(&[b"INFO", b"replication", b"server", b"keyspace"]),
            1,
        );
        let RespFrame::BulkString(Some(info_bytes)) = info else {
            panic!("expected bulk INFO response");
        };
        let info = String::from_utf8(info_bytes).expect("utf8 info");
        assert!(info.contains("# Replication\r\n"), "{info}");
        assert!(info.contains("# Server\r\n"), "{info}");
        assert!(info.contains("# Keyspace\r\n"), "{info}");
    }

    #[test]
    fn config_set_repl_backlog_size_updates_live_replication_info_and_window() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"repl-backlog-size", b"1"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"key", b"value"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );

        let info = rt.execute_frame(command(&[b"INFO", b"replication"]), 2);
        let RespFrame::BulkString(Some(info_bytes)) = info else {
            panic!("expected bulk INFO response");
        };
        let info = String::from_utf8(info_bytes).expect("utf8 info");
        assert!(info.contains("repl_backlog_size:1\r\n"), "{info}");

        let primary_offset = rt.server.replication_ack_state.primary_offset.0;
        assert!(
            info.contains(&format!(
                "repl_backlog_first_byte_offset:{primary_offset}\r\n"
            )),
            "{info}"
        );
    }

    #[test]
    fn replication_replconf_ack_is_monotonic() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"REPLCONF", b"listening-port", b"6380"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"REPLCONF", b"ACK", b"10"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"REPLCONF", b"ACK", b"5"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );

        let replica = rt
            .server
            .replication_runtime_state
            .replicas
            .get(&rt.session.client_id)
            .expect("replica state");
        assert_eq!(replica.ack_offset, fr_repl::ReplOffset(10));
        assert_eq!(replica.fsync_offset, fr_repl::ReplOffset(10));
    }

    #[test]
    fn replication_fullresync_apply_replaces_store_and_tracks_offset() {
        let mut primary = Runtime::default_strict();
        assert_eq!(
            primary.execute_frame(command(&[b"SET", b"zero", b"0"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            primary.execute_frame(command(&[b"SELECT", b"2"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            primary.execute_frame(command(&[b"SET", b"two", b"2"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
        let reply = match primary.execute_frame(command(&[b"PSYNC", b"?", b"-1"]), 3) {
            RespFrame::SimpleString(line) => line,
            other => panic!("expected fullresync, got {other:?}"),
        };
        let snapshot = primary.encoded_rdb_snapshot(3);
        let fullresync_offset = primary.replication_primary_offset().0;

        let mut replica = Runtime::default_strict();
        assert_eq!(
            replica.execute_frame(command(&[b"SET", b"stale", b"value"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            replica.execute_frame(command(&[b"REPLICAOF", b"127.0.0.1", b"6380"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        replica
            .apply_replication_sync_payload(&reply, &snapshot, 4)
            .expect("apply fullresync");

        assert_eq!(
            replica.execute_frame(command(&[b"GET", b"stale"]), 5),
            RespFrame::BulkString(None)
        );
        assert_eq!(
            replica.execute_frame(command(&[b"GET", b"zero"]), 6),
            RespFrame::BulkString(Some(b"0".to_vec()))
        );
        assert_eq!(
            replica.execute_frame(command(&[b"SELECT", b"2"]), 7),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            replica.execute_frame(command(&[b"GET", b"two"]), 8),
            RespFrame::BulkString(Some(b"2".to_vec()))
        );
        assert_eq!(
            replica.execute_frame(command(&[b"ROLE"]), 9),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"slave".to_vec())),
                RespFrame::BulkString(Some(b"127.0.0.1".to_vec())),
                RespFrame::Integer(6380),
                RespFrame::BulkString(Some(b"connected".to_vec())),
                RespFrame::Integer(i64::try_from(fullresync_offset).unwrap_or(i64::MAX)),
            ]))
        );
    }

    #[test]
    fn replication_continue_apply_replays_backlog_tail() {
        let mut primary = Runtime::default_strict();
        assert_eq!(
            primary.execute_frame(command(&[b"SET", b"alpha", b"1"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        let fullresync = match primary.execute_frame(command(&[b"PSYNC", b"?", b"-1"]), 1) {
            RespFrame::SimpleString(line) => line,
            other => panic!("expected fullresync, got {other:?}"),
        };
        let snapshot = primary.encoded_rdb_snapshot(1);
        let fullresync_offset = primary.replication_primary_offset().0;

        let mut replica = Runtime::default_strict();
        assert_eq!(
            replica.execute_frame(command(&[b"REPLICAOF", b"127.0.0.1", b"6380"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
        replica
            .apply_replication_sync_payload(&fullresync, &snapshot, 3)
            .expect("apply fullresync");

        assert_eq!(
            primary.execute_frame(command(&[b"SET", b"beta", b"2"]), 4),
            RespFrame::SimpleString("OK".to_string())
        );
        let backlog = primary.encoded_aof_stream_from_offset(fullresync_offset);
        let continued_offset = primary.replication_primary_offset().0;
        replica
            .apply_replication_sync_payload("CONTINUE", &backlog, 5)
            .expect("apply backlog");

        assert_eq!(
            replica.execute_frame(command(&[b"GET", b"alpha"]), 6),
            RespFrame::BulkString(Some(b"1".to_vec()))
        );
        assert_eq!(
            replica.execute_frame(command(&[b"GET", b"beta"]), 7),
            RespFrame::BulkString(Some(b"2".to_vec()))
        );
        assert_eq!(
            replica.execute_frame(command(&[b"ROLE"]), 8),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"slave".to_vec())),
                RespFrame::BulkString(Some(b"127.0.0.1".to_vec())),
                RespFrame::Integer(6380),
                RespFrame::BulkString(Some(b"connected".to_vec())),
                RespFrame::Integer(i64::try_from(continued_offset).unwrap_or(i64::MAX)),
            ]))
        );
    }

    #[test]
    fn fr_p2c_004_runtime_special_command_classifier_matches_linear_reference() {
        let samples: &[&[u8]] = &[
            b"AUTH",
            b"auth",
            b"HeLlO",
            b"ROLE",
            b"REPLCONF",
            b"PSYNC",
            b"SYNC",
            b"REPLICAOF",
            b"SLAVEOF",
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
            b"INFO",
            b"MYID",
            b"NOPE",
            b"SLOTS",
            b"RESET",
            b"NODES",
            b"KEYSLOT",
            b"GETKEYSINSLOT",
            b"COUNTKEYSINSLOT",
            b"SETSLOT",
            b"FAILOVER",
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
            b"INFO",
            b"MYID",
            b"HELP",
            b"NOPE",
            b"SLOTS",
            b"RESET",
            b"NODES",
            b"KEYSLOT",
            b"GETKEYSINSLOT",
            b"COUNTKEYSINSLOT",
            b"SETSLOT",
            b"FAILOVER",
            &[0xFF],
        ];

        let rounds = 300_000usize;
        let total_lookups = rounds.saturating_mul(workload.len());

        let mut linear_help_hits = 0usize;
        let mut linear_dispatch_hits = 0usize;
        let mut linear_invalid_utf8 = 0usize;
        let linear_start = Instant::now();
        for _ in 0..rounds {
            for cmd in workload {
                match classify_cluster_subcommand_linear(cmd) {
                    Ok(ClusterSubcommand::Help) => {
                        linear_help_hits = linear_help_hits.saturating_add(1)
                    }
                    Ok(ClusterSubcommand::Dispatch) => {
                        linear_dispatch_hits = linear_dispatch_hits.saturating_add(1)
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
        let mut optimized_dispatch_hits = 0usize;
        let mut optimized_invalid_utf8 = 0usize;
        let optimized_start = Instant::now();
        for _ in 0..rounds {
            for cmd in workload {
                match classify_cluster_subcommand(cmd) {
                    Ok(ClusterSubcommand::Help) => {
                        optimized_help_hits = optimized_help_hits.saturating_add(1)
                    }
                    Ok(ClusterSubcommand::Dispatch) => {
                        optimized_dispatch_hits = optimized_dispatch_hits.saturating_add(1)
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
        assert_eq!(linear_dispatch_hits, optimized_dispatch_hits);
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
            RespFrame::Error("ERR wrong number of arguments for 'cluster' command".to_string())
        );

        let help = rt.execute_frame(command(&[b"CLUSTER", b"HELP"]), 0);
        assert_eq!(
            help,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"CLUSTER HELP".to_vec())),
                RespFrame::BulkString(Some(
                    b"CLUSTER subcommand dispatch scaffold (FR-P2C-007 D1).".to_vec(),
                )),
                RespFrame::BulkString(Some(
                    b"Supported subcommands in this stage: HELP, INFO, MYID, SLOTS, SHARDS, NODES, KEYSLOT, GETKEYSINSLOT, COUNTKEYSINSLOT, RESET.".to_vec(),
                )),
            ]))
        );

        let info = rt.execute_frame(command(&[b"CLUSTER", b"INFO"]), 0);
        assert_eq!(
            info,
            RespFrame::BulkString(Some(
                b"cluster_enabled:0\r\n\
                  cluster_state:ok\r\n\
                  cluster_slots_assigned:0\r\n\
                  cluster_slots_ok:0\r\n\
                  cluster_slots_pfail:0\r\n\
                  cluster_slots_fail:0\r\n\
                  cluster_known_nodes:0\r\n\
                  cluster_size:0\r\n\
                  cluster_current_epoch:0\r\n\
                  cluster_my_epoch:0\r\n\
                  cluster_stats_messages_sent:0\r\n\
                  cluster_stats_messages_received:0\r\n\
                  total_cluster_links_buffer_limit_exceeded:0\r\n"
                    .to_vec(),
            ))
        );

        let myid = rt.execute_frame(command(&[b"CLUSTER", b"MYID"]), 0);
        assert_eq!(
            myid,
            RespFrame::BulkString(Some(b"0000000000000000000000000000000000000000".to_vec(),))
        );

        let slots = rt.execute_frame(command(&[b"CLUSTER", b"SLOTS"]), 0);
        assert_eq!(slots, RespFrame::Array(Some(Vec::new())));

        let shards = rt.execute_frame(command(&[b"CLUSTER", b"SHARDS"]), 0);
        assert_eq!(shards, RespFrame::Array(Some(Vec::new())));

        let nodes = rt.execute_frame(command(&[b"CLUSTER", b"NODES"]), 0);
        assert_eq!(nodes, RespFrame::BulkString(Some(Vec::new())));

        let keyslot = rt.execute_frame(command(&[b"CLUSTER", b"KEYSLOT", b"foo"]), 0);
        assert_eq!(keyslot, RespFrame::Integer(12182));

        let getkeysinslot =
            rt.execute_frame(command(&[b"CLUSTER", b"GETKEYSINSLOT", b"12182", b"10"]), 0);
        assert_eq!(getkeysinslot, RespFrame::Array(Some(Vec::new())));

        let countkeysinslot =
            rt.execute_frame(command(&[b"CLUSTER", b"COUNTKEYSINSLOT", b"12182"]), 0);
        assert_eq!(countkeysinslot, RespFrame::Integer(0));

        let reset = rt.execute_frame(command(&[b"CLUSTER", b"RESET"]), 0);
        assert_eq!(reset, RespFrame::SimpleString("OK".to_string()));

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
    fn multi_rejects_wrong_arity_immediately() {
        let mut rt = Runtime::default_strict();
        // MULTI
        assert_eq!(
            rt.execute_frame(command(&[b"MULTI"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        // GET with wrong arity (too many args) should return error, not QUEUED
        let reply = rt.execute_frame(command(&[b"GET", b"k1", b"k2"]), 1);
        assert!(
            matches!(&reply, RespFrame::Error(e) if e.contains("wrong number of arguments")),
            "Wrong arity inside MULTI should return error immediately, got: {reply:?}"
        );
        // EXEC should return EXECABORT since the transaction is tainted
        let exec = rt.execute_frame(command(&[b"EXEC"]), 2);
        assert!(
            matches!(&exec, RespFrame::Error(e) if e.contains("EXECABORT")),
            "EXEC after arity error should return EXECABORT, got: {exec:?}"
        );
    }

    #[test]
    fn multi_queues_valid_commands_after_arity_rejection() {
        let mut rt = Runtime::default_strict();
        rt.execute_frame(command(&[b"SET", b"k", b"v"]), 0);
        rt.execute_frame(command(&[b"MULTI"]), 1);
        // Valid command should queue
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"k"]), 2),
            RespFrame::SimpleString("QUEUED".to_string())
        );
        // EXEC should succeed with result array
        let exec = rt.execute_frame(command(&[b"EXEC"]), 3);
        assert_eq!(
            exec,
            RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"v".to_vec()))]))
        );
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
        // hz should be persisted and retrievable
        let set = rt.execute_frame(command(&[b"CONFIG", b"SET", b"hz", b"100"]), 0);
        assert_eq!(set, RespFrame::SimpleString("OK".to_string()));
        let get = rt.execute_frame(command(&[b"CONFIG", b"GET", b"hz"]), 0);
        assert_eq!(
            get,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"hz".to_vec())),
                RespFrame::BulkString(Some(b"100".to_vec())),
            ]))
        );
        // Common static params should persist through CONFIG GET as well.
        let set = rt.execute_frame(command(&[b"CONFIG", b"SET", b"timeout", b"300"]), 1);
        assert_eq!(set, RespFrame::SimpleString("OK".to_string()));
        let get = rt.execute_frame(command(&[b"CONFIG", b"GET", b"timeout"]), 1);
        assert_eq!(
            get,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"timeout".to_vec())),
                RespFrame::BulkString(Some(b"300".to_vec())),
            ]))
        );
        let set = rt.execute_frame(command(&[b"CONFIG", b"SET", b"loglevel", b"warning"]), 2);
        assert_eq!(set, RespFrame::SimpleString("OK".to_string()));
        let get = rt.execute_frame(command(&[b"CONFIG", b"GET", b"loglevel"]), 2);
        assert_eq!(
            get,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"loglevel".to_vec())),
                RespFrame::BulkString(Some(b"warning".to_vec())),
            ]))
        );
    }

    #[test]
    fn config_get_reports_live_appendonly_path_state() {
        let mut rt = Runtime::default_strict();
        rt.set_aof_path(std::path::PathBuf::from("/tmp/fr-test/real-appendonly.aof"));

        let get = rt.execute_frame(
            command(&[
                b"CONFIG",
                b"GET",
                b"appendonly",
                b"appendfilename",
                b"appenddirname",
            ]),
            0,
        );
        assert_eq!(
            get,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"appendonly".to_vec())),
                RespFrame::BulkString(Some(b"yes".to_vec())),
                RespFrame::BulkString(Some(b"appendfilename".to_vec())),
                RespFrame::BulkString(Some(b"real-appendonly.aof".to_vec())),
                RespFrame::BulkString(Some(b"appenddirname".to_vec())),
                RespFrame::BulkString(Some(b"/tmp/fr-test".to_vec())),
            ]))
        );
    }

    #[test]
    fn config_get_reports_default_appenddirname_when_aof_is_disabled() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"GET", b"appendfilename", b"appenddirname"]),
                0,
            ),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"appendfilename".to_vec())),
                RespFrame::BulkString(Some(b"appendonly.aof".to_vec())),
                RespFrame::BulkString(Some(b"appenddirname".to_vec())),
                RespFrame::BulkString(Some(b"appendonlydir".to_vec())),
            ]))
        );
    }

    #[test]
    fn config_get_appenddirname_uses_dot_for_filename_only_aof_path() {
        let mut rt = Runtime::default_strict();
        rt.set_aof_path(std::path::PathBuf::from("custom.aof"));

        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"GET", b"appendfilename", b"appenddirname"]),
                0,
            ),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"appendfilename".to_vec())),
                RespFrame::BulkString(Some(b"custom.aof".to_vec())),
                RespFrame::BulkString(Some(b"appenddirname".to_vec())),
                RespFrame::BulkString(Some(b".".to_vec())),
            ]))
        );
    }

    #[test]
    fn config_get_reports_live_rdb_path_state() {
        let mut rt = Runtime::default_strict();
        rt.set_rdb_path(std::path::PathBuf::from("/tmp/fr-rdb/real-dump.rdb"));

        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"dbfilename", b"dir"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"dbfilename".to_vec())),
                RespFrame::BulkString(Some(b"real-dump.rdb".to_vec())),
                RespFrame::BulkString(Some(b"dir".to_vec())),
                RespFrame::BulkString(Some(b"/tmp/fr-rdb".to_vec())),
            ]))
        );
    }

    #[test]
    fn config_get_dir_uses_dot_for_filename_only_rdb_path() {
        let mut rt = Runtime::default_strict();
        rt.set_rdb_path(std::path::PathBuf::from("custom.rdb"));

        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"dir", b"dbfilename"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"dir".to_vec())),
                RespFrame::BulkString(Some(b".".to_vec())),
                RespFrame::BulkString(Some(b"dbfilename".to_vec())),
                RespFrame::BulkString(Some(b"custom.rdb".to_vec())),
            ]))
        );
    }

    #[test]
    fn config_set_appendonly_updates_live_runtime_state() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"appendonly", b"yes"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"CONFIG",
                    b"GET",
                    b"appendonly",
                    b"appendfilename",
                    b"appenddirname",
                ]),
                0,
            ),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"appendonly".to_vec())),
                RespFrame::BulkString(Some(b"yes".to_vec())),
                RespFrame::BulkString(Some(b"appendfilename".to_vec())),
                RespFrame::BulkString(Some(b"appendonly.aof".to_vec())),
                RespFrame::BulkString(Some(b"appenddirname".to_vec())),
                RespFrame::BulkString(Some(b"appendonlydir".to_vec())),
            ]))
        );

        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"appendonly", b"no"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"appendonly"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"appendonly".to_vec())),
                RespFrame::BulkString(Some(b"no".to_vec())),
            ]))
        );
    }

    #[test]
    fn config_set_rejects_immutable_append_paths() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"SET", b"appendfilename", b"other.aof"]),
                0
            ),
            RespFrame::Error(
                "ERR CONFIG SET failed (possibly related to argument 'appendfilename') - can't set immutable config"
                    .to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"SET", b"appenddirname", b"otherdir"]),
                0
            ),
            RespFrame::Error(
                "ERR CONFIG SET failed (possibly related to argument 'appenddirname') - can't set immutable config"
                    .to_string()
            )
        );
    }

    #[test]
    fn config_set_appendonly_preserves_existing_configured_aof_path() {
        let mut rt = Runtime::default_strict();
        rt.set_aof_path(std::path::PathBuf::from("/tmp/fr-preserve/custom.aof"));

        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"appendonly", b"no"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"appendonly", b"yes"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"CONFIG",
                    b"GET",
                    b"appendonly",
                    b"appendfilename",
                    b"appenddirname",
                ]),
                0,
            ),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"appendonly".to_vec())),
                RespFrame::BulkString(Some(b"yes".to_vec())),
                RespFrame::BulkString(Some(b"appendfilename".to_vec())),
                RespFrame::BulkString(Some(b"custom.aof".to_vec())),
                RespFrame::BulkString(Some(b"appenddirname".to_vec())),
                RespFrame::BulkString(Some(b"/tmp/fr-preserve".to_vec())),
            ]))
        );
    }

    #[test]
    fn config_set_dir_and_dbfilename_update_live_rdb_target() {
        let base_dir = std::env::temp_dir().join("fr_runtime_config_rdb_target");
        let new_dir = base_dir.join("snapshots");
        let expected_path = new_dir.join("custom.rdb");

        let _ = std::fs::create_dir_all(&new_dir);

        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"CONFIG",
                    b"SET",
                    b"dir",
                    new_dir.to_string_lossy().as_bytes(),
                    b"dbfilename",
                    b"custom.rdb",
                ]),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"dir", b"dbfilename"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"dir".to_vec())),
                RespFrame::BulkString(Some(new_dir.to_string_lossy().as_bytes().to_vec(),)),
                RespFrame::BulkString(Some(b"dbfilename".to_vec())),
                RespFrame::BulkString(Some(b"custom.rdb".to_vec())),
            ]))
        );

        rt.execute_frame(command(&[b"SET", b"persisted", b"value"]), 1);
        assert_eq!(
            rt.execute_frame(command(&[b"SAVE"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
        assert!(
            expected_path.exists(),
            "SAVE should write to the live CONFIG SET RDB target"
        );

        let _ = std::fs::remove_file(&expected_path);
    }

    #[test]
    fn config_set_dir_rejects_nonexistent_paths_without_mutating_live_target() {
        let mut rt = Runtime::default_strict();
        rt.set_rdb_path(std::path::PathBuf::from("original.rdb"));

        let missing_dir =
            std::env::temp_dir().join(format!("fr_runtime_missing_dir_{}", std::process::id()));

        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"CONFIG",
                    b"SET",
                    b"dir",
                    missing_dir.to_string_lossy().as_bytes(),
                ]),
                0,
            ),
            RespFrame::Error(
                "ERR CONFIG SET failed (possibly related to argument 'dir') - No such file or directory"
                    .to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"dir", b"dbfilename"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"dir".to_vec())),
                RespFrame::BulkString(Some(b".".to_vec())),
                RespFrame::BulkString(Some(b"dbfilename".to_vec())),
                RespFrame::BulkString(Some(b"original.rdb".to_vec())),
            ]))
        );
    }

    #[test]
    fn config_set_dbfilename_rejects_path_values() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"SET", b"dbfilename", b"nested/custom.rdb"]),
                0,
            ),
            RespFrame::Error(
                "ERR CONFIG SET failed (possibly related to argument 'dbfilename') - dbfilename can't be a path, just a filename"
                    .to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"dir", b"dbfilename"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"dir".to_vec())),
                RespFrame::BulkString(Some(b".".to_vec())),
                RespFrame::BulkString(Some(b"dbfilename".to_vec())),
                RespFrame::BulkString(Some(b"dump.rdb".to_vec())),
            ]))
        );
    }

    #[test]
    fn config_help_is_supported_on_runtime_path() {
        let mut rt = Runtime::default_strict();
        let help = rt.execute_frame(command(&[b"CONFIG", b"HELP"]), 0);
        assert_eq!(help, RespFrame::Array(Some(Runtime::config_help_lines())));
    }

    #[test]
    fn config_help_rejects_extra_arguments_on_runtime_path() {
        let mut rt = Runtime::default_strict();
        let reply = rt.execute_frame(command(&[b"CONFIG", b"HELP", b"extra"]), 0);
        assert_eq!(
            reply,
            RespFrame::Error(
                "ERR wrong number of arguments for 'config|help' subcommand".to_string()
            )
        );
    }

    #[test]
    fn config_set_is_atomic_across_runtime_store_and_static_overrides() {
        let mut rt = Runtime::default_strict();

        let set = rt.execute_frame(
            command(&[
                b"CONFIG",
                b"SET",
                b"list-max-listpack-size",
                b"-5",
                b"timeout",
                b"300",
                b"hz",
                b"100",
                b"maxmemory-policy",
                b"allkeys-lru",
                b"maxmemory",
                b"-1",
            ]),
            0,
        );
        assert_eq!(
            set,
            RespFrame::Error("ERR Invalid argument '-1' for CONFIG SET 'maxmemory'".to_string())
        );

        let timeout = rt.execute_frame(command(&[b"CONFIG", b"GET", b"timeout"]), 1);
        assert_eq!(
            timeout,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"timeout".to_vec())),
                RespFrame::BulkString(Some(b"0".to_vec())),
            ]))
        );

        let hz = rt.execute_frame(command(&[b"CONFIG", b"GET", b"hz"]), 1);
        assert_eq!(
            hz,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"hz".to_vec())),
                RespFrame::BulkString(Some(b"10".to_vec())),
            ]))
        );

        let maxmemory_policy =
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"maxmemory-policy"]), 1);
        assert_eq!(
            maxmemory_policy,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"maxmemory-policy".to_vec())),
                RespFrame::BulkString(Some(b"noeviction".to_vec())),
            ]))
        );

        let list_max_listpack_size =
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"list-max-listpack-size"]), 1);
        assert_eq!(
            list_max_listpack_size,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"list-max-listpack-size".to_vec())),
                RespFrame::BulkString(Some(b"-2".to_vec())),
            ]))
        );
    }

    #[test]
    fn config_set_proto_max_bulk_len_updates_live_parser_limit() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"SET", b"proto-max-bulk-len", b"1"]),
                0
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        let encoded = rt.execute_bytes(b"$2\r\nhi\r\n", 1);
        let parsed = parse_frame(&encoded).expect("parse runtime error reply");
        assert_eq!(
            parsed.frame,
            RespFrame::Error("ERR Protocol error: bulk length exceeds limit".to_string())
        );
    }

    #[test]
    fn config_set_client_output_buffer_limit_updates_live_runtime_state() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"SET", b"client-output-buffer-limit", b"1024"]),
                0
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"GET", b"client-output-buffer-limit"]),
                1
            ),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"client-output-buffer-limit".to_vec())),
                RespFrame::BulkString(Some(b"1024".to_vec())),
            ]))
        );
        assert_eq!(rt.server.output_buffer_limit, 1024);
    }

    // ── AOF persistence round-trip tests ────────────────────────────────

    #[test]
    fn save_and_load_aof_round_trip() {
        let dir = std::env::temp_dir().join("fr_runtime_aof_test");
        let _ = std::fs::create_dir_all(&dir);
        let aof_path = dir.join("test_save.aof");

        // Populate a runtime with various data types
        let mut rt = Runtime::default_strict();
        rt.set_aof_path(aof_path.clone());

        rt.execute_frame(command(&[b"SET", b"str_key", b"hello"]), 100);
        rt.execute_frame(
            command(&[b"HSET", b"hash_key", b"f1", b"v1", b"f2", b"v2"]),
            100,
        );
        rt.execute_frame(command(&[b"RPUSH", b"list_key", b"a", b"b", b"c"]), 100);
        rt.execute_frame(command(&[b"SADD", b"set_key", b"x", b"y", b"z"]), 100);
        rt.execute_frame(
            command(&[b"ZADD", b"zset_key", b"1.5", b"alice", b"2.5", b"bob"]),
            100,
        );

        // SAVE to persist
        let save_result = rt.execute_frame(command(&[b"SAVE"]), 100);
        assert_eq!(save_result, RespFrame::SimpleString("OK".to_string()));

        // Verify the file exists
        assert!(aof_path.exists());

        // Load into a fresh runtime
        let mut rt2 = Runtime::default_strict();
        rt2.set_aof_path(aof_path.clone());
        let loaded = rt2.load_aof(100).expect("load should succeed");
        assert!(loaded > 0, "should have loaded some records");

        // Verify data was restored
        let get = rt2.execute_frame(command(&[b"GET", b"str_key"]), 100);
        assert_eq!(get, RespFrame::BulkString(Some(b"hello".to_vec())));

        let hgetall = rt2.execute_frame(command(&[b"HGET", b"hash_key", b"f1"]), 100);
        assert_eq!(hgetall, RespFrame::BulkString(Some(b"v1".to_vec())));

        let lrange = rt2.execute_frame(command(&[b"LLEN", b"list_key"]), 100);
        assert_eq!(lrange, RespFrame::Integer(3));

        let scard = rt2.execute_frame(command(&[b"SCARD", b"set_key"]), 100);
        assert_eq!(scard, RespFrame::Integer(3));

        let zscore = rt2.execute_frame(command(&[b"ZSCORE", b"zset_key", b"alice"]), 100);
        assert_eq!(zscore, RespFrame::BulkString(Some(b"1.5".to_vec())));

        // Cleanup
        let _ = std::fs::remove_file(&aof_path);
    }

    #[test]
    fn save_and_load_aof_round_trip_with_streams_and_ttl() {
        let dir = std::env::temp_dir().join("fr_runtime_aof_stream_test");
        let _ = std::fs::create_dir_all(&dir);
        let aof_path = dir.join("test_stream.aof");

        let mut rt = Runtime::default_strict();
        rt.set_aof_path(aof_path.clone());

        // Add a stream
        rt.execute_frame(
            command(&[b"XADD", b"mystream", b"1-0", b"name", b"Alice"]),
            100,
        );
        rt.execute_frame(
            command(&[b"XADD", b"mystream", b"2-0", b"name", b"Bob"]),
            100,
        );

        // Add a key with TTL
        rt.execute_frame(
            command(&[b"SET", b"ttl_key", b"expiring", b"PX", b"60000"]),
            100,
        );

        // SAVE to persist
        let save_result = rt.execute_frame(command(&[b"SAVE"]), 100);
        assert_eq!(save_result, RespFrame::SimpleString("OK".to_string()));
        assert!(aof_path.exists());

        // Load into fresh runtime
        let mut rt2 = Runtime::default_strict();
        rt2.set_aof_path(aof_path.clone());
        let loaded = rt2.load_aof(100).expect("load should succeed");
        assert!(loaded > 0);

        // Verify stream was restored
        let xlen = rt2.execute_frame(command(&[b"XLEN", b"mystream"]), 100);
        assert_eq!(xlen, RespFrame::Integer(2));

        // Verify TTL key exists and has TTL
        let get_ttl = rt2.execute_frame(command(&[b"GET", b"ttl_key"]), 100);
        assert_eq!(get_ttl, RespFrame::BulkString(Some(b"expiring".to_vec())));
        let pttl = rt2.execute_frame(command(&[b"PTTL", b"ttl_key"]), 100);
        // TTL should be positive (key not expired)
        if let RespFrame::Integer(ms) = pttl {
            assert!(ms > 0, "TTL key should have positive PTTL, got {ms}");
        } else {
            panic!("Expected integer from PTTL, got: {pttl:?}");
        }

        let _ = std::fs::remove_file(&aof_path);
    }

    #[test]
    fn save_and_load_aof_round_trip_multi_db() {
        let dir = std::env::temp_dir().join("fr_runtime_aof_multidb_test");
        let _ = std::fs::create_dir_all(&dir);
        let aof_path = dir.join("test_multidb.aof");

        let mut rt = Runtime::default_strict();
        rt.set_aof_path(aof_path.clone());

        // DB 0: string
        rt.execute_frame(command(&[b"SET", b"db0:key", b"zero"]), 100);

        // DB 1: hash
        rt.execute_frame(command(&[b"SELECT", b"1"]), 100);
        rt.execute_frame(
            command(&[b"HSET", b"db1:hash", b"f1", b"v1", b"f2", b"v2"]),
            100,
        );

        // DB 2: list
        rt.execute_frame(command(&[b"SELECT", b"2"]), 100);
        rt.execute_frame(command(&[b"RPUSH", b"db2:list", b"a", b"b", b"c"]), 100);

        // DB 3: set
        rt.execute_frame(command(&[b"SELECT", b"3"]), 100);
        rt.execute_frame(command(&[b"SADD", b"db3:set", b"x", b"y", b"z"]), 100);

        // DB 4: sorted set
        rt.execute_frame(command(&[b"SELECT", b"4"]), 100);
        rt.execute_frame(
            command(&[b"ZADD", b"db4:zset", b"1", b"a", b"2", b"b"]),
            100,
        );

        // SAVE
        rt.execute_frame(command(&[b"SELECT", b"0"]), 100);
        assert_eq!(
            rt.execute_frame(command(&[b"SAVE"]), 100),
            RespFrame::SimpleString("OK".to_string())
        );

        // Load into fresh runtime
        let mut rt2 = Runtime::default_strict();
        rt2.set_aof_path(aof_path.clone());
        let loaded = rt2.load_aof(100).expect("load should succeed");
        assert!(loaded > 0);

        // Verify DB 0
        assert_eq!(
            rt2.execute_frame(command(&[b"GET", b"db0:key"]), 100),
            RespFrame::BulkString(Some(b"zero".to_vec()))
        );

        // Verify DB 1
        rt2.execute_frame(command(&[b"SELECT", b"1"]), 100);
        assert_eq!(
            rt2.execute_frame(command(&[b"HGET", b"db1:hash", b"f1"]), 100),
            RespFrame::BulkString(Some(b"v1".to_vec()))
        );
        assert_eq!(
            rt2.execute_frame(command(&[b"HLEN", b"db1:hash"]), 100),
            RespFrame::Integer(2)
        );

        // Verify DB 2
        rt2.execute_frame(command(&[b"SELECT", b"2"]), 100);
        assert_eq!(
            rt2.execute_frame(command(&[b"LLEN", b"db2:list"]), 100),
            RespFrame::Integer(3)
        );

        // Verify DB 3
        rt2.execute_frame(command(&[b"SELECT", b"3"]), 100);
        assert_eq!(
            rt2.execute_frame(command(&[b"SCARD", b"db3:set"]), 100),
            RespFrame::Integer(3)
        );

        // Verify DB 4
        rt2.execute_frame(command(&[b"SELECT", b"4"]), 100);
        assert_eq!(
            rt2.execute_frame(command(&[b"ZCARD", b"db4:zset"]), 100),
            RespFrame::Integer(2)
        );
        assert_eq!(
            rt2.execute_frame(command(&[b"ZSCORE", b"db4:zset", b"b"]), 100),
            RespFrame::BulkString(Some(b"2".to_vec()))
        );

        let _ = std::fs::remove_file(&aof_path);
    }

    #[test]
    fn bgsave_writes_rdb_snapshot_when_path_is_configured() {
        let dir = std::env::temp_dir().join("fr_runtime_rdb_test");
        let _ = std::fs::create_dir_all(&dir);
        let rdb_path = dir.join("test_bgsave.rdb");

        let mut rt = Runtime::default_strict();
        rt.set_rdb_path(rdb_path.clone());
        rt.execute_frame(command(&[b"SET", b"persisted", b"value"]), 100);

        let result = rt.execute_frame(command(&[b"BGSAVE"]), 200);
        assert_eq!(
            result,
            RespFrame::SimpleString("Background saving started".to_string())
        );
        assert!(
            rdb_path.exists(),
            "BGSAVE should write the configured RDB file"
        );

        let (entries, aux) = fr_persist::read_rdb_file(&rdb_path).expect("read rdb");
        assert_eq!(aux.get("frankenredis"), Some(&"true".to_string()));
        assert!(
            entries.iter().any(|entry| {
                entry.key == b"persisted"
                    && entry.value == fr_persist::RdbValue::String(b"value".to_vec())
            }),
            "RDB snapshot should contain the persisted key"
        );

        let _ = std::fs::remove_file(&rdb_path);
    }

    #[test]
    fn live_info_persistence_reports_last_save_time_after_save() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"SAVE"]), 1_700_000_005_000),
            RespFrame::SimpleString("OK".to_string())
        );

        let info = rt.execute_frame(command(&[b"INFO", b"persistence"]), 1_700_000_099_000);
        let RespFrame::BulkString(Some(info_bytes)) = info else {
            panic!("expected bulk INFO response");
        };
        let info = String::from_utf8(info_bytes).expect("utf8 info");
        assert!(info.contains("rdb_last_save_time:1700000005\r\n"), "{info}");
    }

    #[test]
    fn live_info_persistence_reports_aof_enabled_when_configured() {
        let mut rt = Runtime::default_strict();
        rt.set_aof_path(std::path::PathBuf::from("appendonly.aof"));

        let info = rt.execute_frame(command(&[b"INFO", b"persistence"]), 0);
        let RespFrame::BulkString(Some(info_bytes)) = info else {
            panic!("expected bulk INFO response");
        };
        let info = String::from_utf8(info_bytes).expect("utf8 info");
        assert!(info.contains("aof_enabled:1\r\n"), "{info}");
    }

    #[test]
    fn bgsave_rejects_invalid_options_with_syntax_error() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"BGSAVE", b"NOW"]), 1),
            RespFrame::Error("ERR syntax error".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"BGSAVE", b"SCHEDULE", b"EXTRA"]), 2),
            RespFrame::Error("ERR syntax error".to_string())
        );
    }

    #[test]
    fn bgrewriteaof_errors_when_appendonly_is_disabled() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"BGREWRITEAOF"]), 1),
            RespFrame::Error(
                "ERR appendonly is disabled, cannot rewrite append only file".to_string()
            )
        );
    }

    #[test]
    fn shutdown_abort_and_illegal_combinations_follow_redis_validation() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"SHUTDOWN", b"ABORT"]), 1),
            RespFrame::Error("ERR No shutdown in progress.".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SHUTDOWN", b"ABORT", b"NOW"]), 2),
            RespFrame::Error("ERR syntax error".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SHUTDOWN", b"SAVE", b"NOSAVE"]), 3),
            RespFrame::Error("ERR syntax error".to_string())
        );
    }

    #[test]
    fn load_aof_no_path_returns_zero() {
        let mut rt = Runtime::default_strict();
        let count = rt.load_aof(100).expect("should succeed");
        assert_eq!(count, 0);
    }

    #[test]
    fn load_aof_missing_file_returns_empty() {
        let mut rt = Runtime::default_strict();
        rt.set_aof_path(std::path::PathBuf::from(
            "/tmp/fr_nonexistent_aof_test_file.aof",
        ));
        let count = rt.load_aof(100).expect("should succeed for missing file");
        assert_eq!(count, 0);
    }

    #[test]
    fn load_aof_replaces_existing_store_and_tracks_loaded_records() {
        let dir = std::env::temp_dir().join("fr_runtime_aof_replace_test");
        let _ = std::fs::create_dir_all(&dir);
        let aof_path = dir.join("replace_state.aof");
        let records = vec![AofRecord {
            argv: vec![b"SET".to_vec(), b"fresh".to_vec(), b"value".to_vec()],
        }];
        write_aof_file(&aof_path, &records).expect("write test aof");

        let mut rt = Runtime::default_strict();
        rt.set_aof_path(aof_path.clone());
        let stale_reply = rt.execute_frame(command(&[b"SET", b"stale", b"old"]), 0);
        assert_eq!(stale_reply, RespFrame::SimpleString("OK".to_string()));

        let loaded = rt.load_aof(50).expect("load should succeed");
        assert_eq!(loaded, records.len());
        assert_eq!(rt.aof_records(), records.as_slice());

        let stale = rt.execute_frame(command(&[b"GET", b"stale"]), 100);
        assert_eq!(stale, RespFrame::BulkString(None));
        let fresh = rt.execute_frame(command(&[b"GET", b"fresh"]), 100);
        assert_eq!(fresh, RespFrame::BulkString(Some(b"value".to_vec())));

        let _ = std::fs::remove_file(&aof_path);
    }

    #[test]
    fn load_aof_rejects_invalid_replay_without_mutating_existing_state() {
        let dir = std::env::temp_dir().join("fr_runtime_aof_invalid_test");
        let _ = std::fs::create_dir_all(&dir);
        let aof_path = dir.join("invalid_replay.aof");
        let records = vec![
            AofRecord {
                argv: vec![b"SET".to_vec(), b"good".to_vec(), b"value".to_vec()],
            },
            AofRecord {
                argv: vec![b"NOPE".to_vec(), b"bad".to_vec()],
            },
        ];
        write_aof_file(&aof_path, &records).expect("write invalid test aof");

        let mut rt = Runtime::default_strict();
        rt.set_aof_path(aof_path.clone());
        let original_reply = rt.execute_frame(command(&[b"SET", b"keep", b"safe"]), 0);
        assert_eq!(original_reply, RespFrame::SimpleString("OK".to_string()));

        let err = rt
            .load_aof(50)
            .expect_err("invalid replay must fail closed");
        assert_eq!(err, PersistError::InvalidFrame);

        let keep = rt.execute_frame(command(&[b"GET", b"keep"]), 100);
        assert_eq!(keep, RespFrame::BulkString(Some(b"safe".to_vec())));
        let good = rt.execute_frame(command(&[b"GET", b"good"]), 100);
        assert_eq!(good, RespFrame::BulkString(None));

        let _ = std::fs::remove_file(&aof_path);
    }

    #[test]
    fn fr_p2c_004_u013_acl_commands_are_gated_for_non_admin_users() {
        let mut rt = Runtime::default_strict();

        // 1. Create a non-admin user 'alice'
        // Use default (admin) to set up alice
        assert_eq!(
            rt.execute_frame(
                command(&[b"ACL", b"SETUSER", b"alice", b"on", b">pass", b"-@all"]),
                0
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        // 2. Authenticate as alice
        assert_eq!(
            rt.execute_frame(command(&[b"AUTH", b"alice", b"pass"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(rt.session.current_user_name(), b"alice");

        // 3. Try to run an ACL command as alice
        let out = rt.execute_frame(command(&[b"ACL", b"WHOAMI"]), 2);

        // After fix, it should return NOPERM
        assert_eq!(
            out,
            RespFrame::Error(
                "NOPERM this user has no permissions to run the 'ACL' command".to_string()
            )
        );
    }

    #[test]
    fn acl_dryrun_evaluates_target_user_not_current_session() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(
                command(&[b"ACL", b"SETUSER", b"alice", b"on", b">pass", b"-@all"]),
                0
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        let reply = rt.execute_frame(
            command(&[b"ACL", b"DRYRUN", b"alice", b"SET", b"k", b"v"]),
            1,
        );
        assert_eq!(
            reply,
            RespFrame::Error(
                "ERR User 'alice' has no permissions to run the 'SET' command".to_string()
            )
        );
    }

    #[test]
    fn acl_per_command_allow_specific_commands() {
        let mut rt = Runtime::default_strict();
        // Create user with -@all then selectively allow +get +set
        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL", b"SETUSER", b"bob", b"on", b">pass", b"-@all", b"+get", b"+set"
                ]),
                0
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        // Auth as bob
        assert_eq!(
            rt.execute_frame(command(&[b"AUTH", b"bob", b"pass"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );

        // SET and GET should work
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"k", b"v"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"k"]), 3),
            RespFrame::BulkString(Some(b"v".to_vec()))
        );

        // DEL should be denied
        let del_reply = rt.execute_frame(command(&[b"DEL", b"k"]), 4);
        assert!(
            matches!(&del_reply, RespFrame::Error(e) if e.contains("NOPERM")),
            "DEL should be denied, got: {del_reply:?}"
        );
    }

    #[test]
    fn acl_per_command_deny_specific_commands() {
        let mut rt = Runtime::default_strict();
        // Create user with +@all then selectively deny -del -flushdb
        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL",
                    b"SETUSER",
                    b"carol",
                    b"on",
                    b">pass",
                    b"+@all",
                    b"-del",
                    b"-flushdb"
                ]),
                0
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        // Auth as carol
        assert_eq!(
            rt.execute_frame(command(&[b"AUTH", b"carol", b"pass"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );

        // SET and GET should work
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"k", b"v"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"k"]), 3),
            RespFrame::BulkString(Some(b"v".to_vec()))
        );

        // DEL should be denied
        let del_reply = rt.execute_frame(command(&[b"DEL", b"k"]), 4);
        assert!(
            matches!(&del_reply, RespFrame::Error(e) if e.contains("NOPERM")),
            "DEL should be denied, got: {del_reply:?}"
        );

        // FLUSHDB should be denied
        let flush_reply = rt.execute_frame(command(&[b"FLUSHDB"]), 5);
        assert!(
            matches!(&flush_reply, RespFrame::Error(e) if e.contains("NOPERM")),
            "FLUSHDB should be denied, got: {flush_reply:?}"
        );
    }

    #[test]
    fn acl_category_based_permissions() {
        let mut rt = Runtime::default_strict();
        // Create user with only read permission
        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL",
                    b"SETUSER",
                    b"reader",
                    b"on",
                    b">pass",
                    b"-@all",
                    b"+@read",
                    b"+@connection"
                ]),
                0
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        // Set a key as default user first
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"k", b"v"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );

        // Auth as reader
        assert_eq!(
            rt.execute_frame(command(&[b"AUTH", b"reader", b"pass"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );

        // GET should work (it's a read command)
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"k"]), 3),
            RespFrame::BulkString(Some(b"v".to_vec()))
        );

        // SET should be denied (it's a write command)
        let set_reply = rt.execute_frame(command(&[b"SET", b"k", b"new"]), 4);
        assert!(
            matches!(&set_reply, RespFrame::Error(e) if e.contains("NOPERM")),
            "SET should be denied for read-only user, got: {set_reply:?}"
        );
    }

    #[test]
    fn acl_deny_category_overrides_allow_all() {
        let mut rt = Runtime::default_strict();
        // +@all -@dangerous: allow everything except dangerous commands
        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL",
                    b"SETUSER",
                    b"safe",
                    b"on",
                    b">pass",
                    b"+@all",
                    b"-@dangerous"
                ]),
                0
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        // Dryrun: GET should be allowed
        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"DRYRUN", b"safe", b"GET", b"k"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );

        // Dryrun: FLUSHALL is in dangerous category
        let reply = rt.execute_frame(command(&[b"ACL", b"DRYRUN", b"safe", b"FLUSHALL"]), 2);
        assert!(
            matches!(&reply, RespFrame::Error(e) if e.contains("no permissions")),
            "FLUSHALL should be denied for -@dangerous user, got: {reply:?}"
        );
    }

    #[test]
    fn acl_per_command_override_category_deny() {
        let mut rt = Runtime::default_strict();
        // -@all +@read +set: deny all, allow reads, plus specifically allow SET
        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL", b"SETUSER", b"mixed", b"on", b">pass", b"-@all", b"+@read", b"+set"
                ]),
                0
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        // Dryrun: GET should be allowed (via +@read)
        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"DRYRUN", b"mixed", b"GET", b"k"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );

        // Dryrun: SET should be allowed (via +set)
        assert_eq!(
            rt.execute_frame(
                command(&[b"ACL", b"DRYRUN", b"mixed", b"SET", b"k", b"v"]),
                2
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        // Dryrun: DEL should be denied (not in +@read, not explicitly allowed)
        let reply = rt.execute_frame(command(&[b"ACL", b"DRYRUN", b"mixed", b"DEL", b"k"]), 3);
        assert!(
            matches!(&reply, RespFrame::Error(e) if e.contains("no permissions")),
            "DEL should be denied, got: {reply:?}"
        );
    }

    #[test]
    fn acl_getuser_reflects_per_command_permissions() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL", b"SETUSER", b"dave", b"on", b">pass", b"-@all", b"+get", b"+set"
                ]),
                0
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        let reply = rt.execute_frame(command(&[b"ACL", b"GETUSER", b"dave"]), 1);
        // The commands field should reflect the per-command permissions
        if let RespFrame::Array(Some(fields)) = &reply {
            // Find the "commands" field value
            let mut found_commands = false;
            for (i, field) in fields.iter().enumerate() {
                if let RespFrame::BulkString(Some(name)) = field
                    && name == b"commands"
                    && i + 1 < fields.len()
                    && let RespFrame::BulkString(Some(val)) = &fields[i + 1]
                {
                    let val_str = std::str::from_utf8(val).unwrap();
                    assert!(
                        val_str.contains("+get"),
                        "commands string should contain +get, got: {val_str}"
                    );
                    assert!(
                        val_str.contains("+set"),
                        "commands string should contain +set, got: {val_str}"
                    );
                    found_commands = true;
                }
            }
            assert!(
                found_commands,
                "Should have found commands field in GETUSER reply"
            );
        } else {
            panic!("Expected array reply from GETUSER, got: {reply:?}");
        }
    }

    #[test]
    fn acl_list_reflects_per_command_permissions() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL", b"SETUSER", b"eve", b"on", b">pass", b"+@all", b"-del"
                ]),
                0
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        let reply = rt.execute_frame(command(&[b"ACL", b"LIST"]), 1);
        if let RespFrame::Array(Some(entries)) = &reply {
            let eve_entry = entries.iter().find(|e| {
                if let RespFrame::BulkString(Some(s)) = e {
                    String::from_utf8_lossy(s).contains("eve")
                } else {
                    false
                }
            });
            assert!(eve_entry.is_some(), "Should find eve in ACL LIST");
            if let Some(RespFrame::BulkString(Some(s))) = eve_entry {
                let entry_str = String::from_utf8_lossy(s);
                assert!(
                    entry_str.contains("-del"),
                    "ACL LIST for eve should contain -del, got: {entry_str}"
                );
            }
        } else {
            panic!("Expected array reply from ACL LIST, got: {reply:?}");
        }
    }

    #[test]
    fn acl_case_insensitive_command_matching() {
        let mut rt = Runtime::default_strict();
        // Create user allowing only +GET (uppercase)
        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL", b"SETUSER", b"ci", b"on", b">pass", b"-@all", b"+GET"
                ]),
                0
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        // Dryrun with lowercase "get" should still be allowed
        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"DRYRUN", b"ci", b"get", b"k"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );

        // Dryrun with mixed case "Get" should also be allowed
        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"DRYRUN", b"ci", b"Get", b"k"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
    }

    #[test]
    fn acl_allcommands_resets_granular_permissions() {
        let mut rt = Runtime::default_strict();
        // Start with -@all +get, then apply allcommands
        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL",
                    b"SETUSER",
                    b"reset_test",
                    b"on",
                    b">pass",
                    b"-@all",
                    b"+get"
                ]),
                0
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        // DEL denied before allcommands
        let reply = rt.execute_frame(
            command(&[b"ACL", b"DRYRUN", b"reset_test", b"DEL", b"k"]),
            1,
        );
        assert!(matches!(&reply, RespFrame::Error(e) if e.contains("no permissions")));

        // Apply allcommands
        assert_eq!(
            rt.execute_frame(
                command(&[b"ACL", b"SETUSER", b"reset_test", b"allcommands"]),
                2
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        // DEL now allowed
        assert_eq!(
            rt.execute_frame(
                command(&[b"ACL", b"DRYRUN", b"reset_test", b"DEL", b"k"]),
                3
            ),
            RespFrame::SimpleString("OK".to_string())
        );
    }

    #[test]
    fn acl_deny_command_wins_over_allow_all() {
        let mut rt = Runtime::default_strict();
        // +@all -del: explicit deny should override the +@all base
        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL",
                    b"SETUSER",
                    b"deny_test",
                    b"on",
                    b">pass",
                    b"+@all",
                    b"-del"
                ]),
                0
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        // GET allowed
        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"DRYRUN", b"deny_test", b"GET", b"k"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );

        // DEL denied
        let reply = rt.execute_frame(command(&[b"ACL", b"DRYRUN", b"deny_test", b"DEL", b"k"]), 2);
        assert!(
            matches!(&reply, RespFrame::Error(e) if e.contains("no permissions")),
            "DEL should be denied despite +@all, got: {reply:?}"
        );
    }

    #[test]
    fn acl_reset_rule_clears_all_permissions() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(
                command(&[b"ACL", b"SETUSER", b"reset_user", b"on", b">pass", b"+@all"]),
                0
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        // Apply reset
        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"SETUSER", b"reset_user", b"reset"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        // User should have no access after reset
        let reply = rt.execute_frame(
            command(&[b"ACL", b"DRYRUN", b"reset_user", b"GET", b"k"]),
            2,
        );
        assert!(
            matches!(&reply, RespFrame::Error(e) if e.contains("no permissions")),
            "GET should be denied after reset, got: {reply:?}"
        );
    }

}
