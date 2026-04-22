#![deny(unsafe_code)]

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use fr_command::{
    CLIENT_PAUSE_MODE_INVALID, CLIENT_PAUSE_TIMEOUT_INVALID, CommandError, MigrateKeySpec,
    apply_client_caching_mode, apply_client_reply_state, apply_client_tracking_update,
    build_unknown_args_preview, client_tracking_getredir_value, client_trackinginfo_frame,
    command_acl_categories, commands_in_acl_category, dispatch_argv, execute_migrate,
    frame_to_argv, parse_client_tracking_state, parse_migrate_request,
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
    decode_rdb, encode_aof_stream, encode_rdb, read_rdb_file, write_aof_file, write_rdb_file,
};
use fr_protocol::{RespFrame, RespParseError};
use fr_repl::{
    BacklogWindow, PsyncReply, ReplOffset, WaitAofThreshold, WaitThreshold, decide_psync,
    evaluate_wait, evaluate_waitaof, parse_psync_reply,
};
use fr_store::{
    ClientReplyState, ClientTrackingState, CommandHistogram, DispatchAclLogContext,
    DispatchAclPermissionReason, DispatchAclPermissions, DispatchClientContext,
    EvictionLoopFailure, EvictionLoopResult, EvictionLoopStatus, EvictionSafetyGateState,
    MaxmemoryPolicy, PendingAclLogEvent, Store, decode_db_key, encode_db_key, glob_match,
};
use sha2::{Digest, Sha256};

static PACKET_COUNTER: AtomicU64 = AtomicU64::new(1);
static CLIENT_ID_COUNTER: AtomicU64 = AtomicU64::new(1);
const DEFAULT_AUTH_USER: &[u8] = b"default";
const NOAUTH_ERROR: &str = "NOAUTH Authentication required.";
const WRONGPASS_ERROR: &str = "WRONGPASS invalid username-password pair or user is disabled.";
const AUTH_NOT_CONFIGURED_ERROR: &str = "ERR AUTH <password> called without any password configured for the default user. Are you sure your configuration is correct?";
const RDB_DISK_ERROR_WRITE_DENIED: &str = "MISCONF Redis is configured to save RDB snapshots, but it's currently unable to persist to disk. Commands that may modify the data set are disabled, because this instance is configured to report errors during writes if RDB snapshotting fails (stop-writes-on-bgsave-error option). Please check the Redis logs for details about the RDB error.";
const AOF_DISK_ERROR_WRITE_DENIED: &str =
    "MISCONF Errors writing to the AOF file: previous AOF write failed";
#[allow(dead_code)]
const ACL_UNKNOWN_SUBCOMMAND_ERROR: &str =
    "ERR unknown subcommand or wrong number of arguments for 'ACL'. Try ACL HELP.";
const DEFAULT_ACLLOG_MAX_LEN: i64 = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppendFsyncMode {
    No,
    Everysec,
    Always,
}

impl AppendFsyncMode {
    fn as_config_value(self) -> &'static str {
        match self {
            Self::No => "no",
            Self::Everysec => "everysec",
            Self::Always => "always",
        }
    }
}

fn sha256_hex_bytes(input: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(input);
    hex::encode(hasher.finalize()).into_bytes()
}

fn normalize_acl_hash(hash: &str) -> Option<Vec<u8>> {
    if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(hash.to_ascii_lowercase().into_bytes())
    } else {
        None
    }
}

fn acl_setuser_missing_password_error(rule_str: &str) -> String {
    format!(
        "ERR Error in ACL SETUSER modifier '{}': The password you are trying to remove from the user does not exist",
        rule_str
    )
}

fn acl_setuser_allkeys_pattern_error(rule_str: &str) -> String {
    format!(
        "ERR Error in ACL SETUSER modifier '{}': Adding a pattern after the * pattern (or the 'allkeys' flag) is not valid and does not have any effect. Try 'resetkeys' to start with an empty list of patterns",
        rule_str
    )
}

fn acl_setuser_allchannels_pattern_error(rule_str: &str) -> String {
    format!(
        "ERR Error in ACL SETUSER modifier '{}': Adding a pattern after the * pattern (or the 'allchannels' flag) is not valid and does not have any effect. Try 'resetchannels' to start with an empty list of channels",
        rule_str
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AclCommandPermissionError {
    Command,
    Key(Vec<u8>),
    Channel(Vec<u8>),
}

#[derive(Debug, Clone)]
struct AclLogEntry {
    count: u64,
    reason: &'static str,
    context: &'static str,
    object: String,
    username: Vec<u8>,
    client_info: String,
    entry_id: u64,
    created_at_ms: u64,
    updated_at_ms: u64,
}
const DEFAULT_REPL_BACKLOG_SIZE: u64 = 1_048_576;

fn processed_command_counts(argv: &[Vec<u8>]) -> (u64, u64) {
    let Some(command) = argv.first() else {
        return (0, 0);
    };
    let Some(flags) = fr_command::get_command_flags(command) else {
        return (0, 0);
    };

    let mut read_count = 0;
    let mut write_count = 0;
    for flag in flags.split_whitespace() {
        if flag == "write" {
            write_count = 1;
        } else if flag == "readonly" && write_count == 0 {
            read_count = 1;
        }
    }
    (read_count, write_count)
}

fn wrong_arity_error(command: &'static str) -> RespFrame {
    RespFrame::Error(format!(
        "ERR wrong number of arguments for '{}' command",
        command
    ))
}

fn client_info_command_name(argv: &[Vec<u8>]) -> String {
    let Some(command) = argv.first() else {
        return "NULL".to_string();
    };
    let command = String::from_utf8_lossy(command).to_ascii_lowercase();
    let Some(sub) = argv.get(1) else {
        return command;
    };
    let uses_subcommand = matches!(
        command.as_str(),
        "acl"
            | "client"
            | "cluster"
            | "command"
            | "config"
            | "debug"
            | "function"
            | "latency"
            | "memory"
            | "module"
            | "object"
            | "pubsub"
            | "script"
            | "sentinel"
            | "slowlog"
            | "xgroup"
            | "xinfo"
    );
    if !uses_subcommand {
        return command;
    }
    let sub = String::from_utf8_lossy(sub).to_ascii_lowercase();
    format!("{command}|{sub}")
}

fn acl_log_age_seconds(now_ms: u64, timestamp_ms: u64) -> String {
    let age_ms = now_ms.saturating_sub(timestamp_ms);
    let age_seconds = age_ms as f64 / 1000.0;
    let rendered = format!("{age_seconds:.3}");
    rendered
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

fn client_wrong_subcommand_arity(subcommand: &str) -> RespFrame {
    RespFrame::Error(format!(
        "ERR wrong number of arguments for 'client|{}' command",
        subcommand.to_ascii_lowercase()
    ))
}

fn config_wrong_subcommand_arity(subcommand: &str) -> RespFrame {
    RespFrame::Error(format!(
        "ERR wrong number of arguments for 'config|{}' command",
        subcommand.to_ascii_lowercase()
    ))
}

fn histogram_percentile_us(hist: &CommandHistogram, percentile: f64) -> f64 {
    if hist.calls == 0 {
        return 0.0;
    }

    let target_rank = ((percentile / 100.0) * hist.calls as f64).ceil() as u64;
    let target_rank = target_rank.max(1);
    let mut cumulative = 0_u64;
    let buckets = hist.to_buckets();
    for (bucket_start_us, count) in &buckets {
        cumulative = cumulative.saturating_add(*count);
        if cumulative >= target_rank {
            return *bucket_start_us as f64;
        }
    }
    buckets
        .last()
        .map(|(bucket_start_us, _)| *bucket_start_us as f64)
        .unwrap_or(0.0)
}

fn read_failover_frame(
    stream: &mut std::net::TcpStream,
    read_buf: &mut Vec<u8>,
) -> Result<RespFrame, CommandError> {
    let parser_config = fr_protocol::ParserConfig::default();
    loop {
        match fr_protocol::parse_frame_with_config(read_buf, &parser_config) {
            Ok(parsed) => {
                read_buf.drain(..parsed.consumed);
                return Ok(parsed.frame);
            }
            Err(fr_protocol::RespParseError::Incomplete) => {}
            Err(err) => {
                return Err(CommandError::Custom(format!(
                    "ERR FAILOVER target replica replied with invalid RESP: {err}"
                )));
            }
        }

        let mut chunk = [0u8; 4096];
        match std::io::Read::read(stream, &mut chunk) {
            Ok(0) => {
                return Err(CommandError::Custom(
                    "ERR FAILOVER target replica closed the connection.".to_string(),
                ));
            }
            Ok(n) => read_buf.extend_from_slice(&chunk[..n]),
            Err(ref err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                return Err(CommandError::Custom(format!(
                    "ERR FAILOVER target replica read failed: {err}"
                )));
            }
        }
    }
}

fn failover_role_is_master(frame: &RespFrame) -> bool {
    matches!(
        frame,
        RespFrame::Array(Some(items))
            if matches!(
                items.first(),
                Some(RespFrame::BulkString(Some(role))) if role.eq_ignore_ascii_case(b"master")
            )
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientUnblockMode {
    Timeout,
    Error,
}

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
    ("enable-protected-configs", "no"),
    ("enable-debug-command", "no"),
    ("enable-module-command", "no"),
    ("hide-user-data-from-log", "no"),
    ("acl-pubsub-default", "resetchannels"),
];

fn canonical_static_config_param(parameter: &str) -> Option<&'static str> {
    if parameter.eq_ignore_ascii_case("slave-serve-stale-data") {
        return Some("replica-serve-stale-data");
    }
    CONFIG_STATIC_PARAMS
        .iter()
        .find(|&&(name, _)| name.eq_ignore_ascii_case(parameter))
        .map(|&(name, _)| name)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AclUser {
    passwords: Vec<Vec<u8>>,
    enabled: bool,
    /// True when "nopass" rule is set (allow auth without password).
    nopass: bool,
    /// Whether RESTORE payload sanitization remains enabled for this user.
    sanitize_payload: bool,
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
    /// Explicitly allowed key glob patterns, stored without the leading `~`.
    key_patterns: Vec<Vec<u8>>,
    /// True when all keys are allowed regardless of `key_patterns`.
    all_keys: bool,
    /// Explicitly allowed channel patterns, stored without the leading `&`.
    channel_patterns: Vec<Vec<u8>>,
    /// Channel permissions. False means only `channel_patterns` are allowed.
    all_channels: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AclPubsubDefault {
    AllChannels,
    ResetChannels,
}

impl AclPubsubDefault {
    fn as_config_value(self) -> &'static str {
        match self {
            Self::AllChannels => "allchannels",
            Self::ResetChannels => "resetchannels",
        }
    }

    fn grants_all_channels(self) -> bool {
        matches!(self, Self::AllChannels)
    }
}

impl AclUser {
    fn new_default() -> Self {
        Self {
            passwords: Vec::new(),
            enabled: true,
            nopass: true, // default user allows passwordless access
            sanitize_payload: true,
            all_commands: true,
            allowed_commands: HashSet::new(),
            denied_commands: HashSet::new(),
            allowed_categories: HashSet::new(),
            denied_categories: HashSet::new(),
            key_patterns: Vec::new(),
            all_keys: true,
            channel_patterns: Vec::new(),
            all_channels: true,
        }
    }

    /// Restrictive starting state for users created via `ACL SETUSER <name> ...`.
    /// Redis creates new users disabled with no passwords, no commands, no keys,
    /// and no channels — the rules that follow build them up explicitly.
    fn new_restricted(acl_pubsub_default: AclPubsubDefault) -> Self {
        Self {
            passwords: Vec::new(),
            enabled: false,
            nopass: false,
            sanitize_payload: true,
            all_commands: false,
            allowed_commands: HashSet::new(),
            denied_commands: HashSet::new(),
            allowed_categories: HashSet::new(),
            denied_categories: HashSet::new(),
            key_patterns: Vec::new(),
            all_keys: false,
            channel_patterns: Vec::new(),
            all_channels: acl_pubsub_default.grants_all_channels(),
        }
    }

    fn check_password(&self, password: &[u8]) -> bool {
        if self.nopass {
            return true;
        }
        let hash_hex = sha256_hex_bytes(password);
        self.passwords.iter().any(|p| {
            let p_slice = p.as_slice();
            if p_slice.len() != hash_hex.len() {
                return false;
            }
            let mut diff = 0;
            for (a, b) in p_slice.iter().zip(hash_hex.iter()) {
                diff |= a ^ b;
            }
            diff == 0
        })
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

        // Hot-path short-circuit: when this user has no category-level
        // ACL rules at all, the entire `command_categories` computation
        // (which scans ACL_CATEGORIES * COMMAND_TABLE on every command)
        // is dead work — fall straight through to the base permission.
        // Profiling on the default `+@all` user showed ~71% of CPU on
        // this lookup before the short-circuit was added.
        if self.denied_categories.is_empty() && self.allowed_categories.is_empty() {
            return self.all_commands;
        }

        // Check category-level permissions.
        // Get the categories this command belongs to.
        let cmd_categories = command_acl_categories(cmd_lower.as_str());

        // If any denied category contains this command, deny it.
        for denied_cat in &self.denied_categories {
            if cmd_categories.contains(&denied_cat.as_str()) {
                return false;
            }
        }

        // If we have allowed categories, check if any match.
        if !self.allowed_categories.is_empty() {
            for allowed_cat in &self.allowed_categories {
                if cmd_categories.contains(&allowed_cat.as_str()) {
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

    fn keys_string(&self) -> String {
        if self.all_keys {
            return "~*".to_string();
        }

        self.key_patterns
            .iter()
            .map(|pattern| format!("~{}", String::from_utf8_lossy(pattern)))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn is_key_allowed(&self, key: &[u8]) -> bool {
        self.all_keys
            || self
                .key_patterns
                .iter()
                .any(|pattern| glob_match(pattern, key))
    }

    fn channels_string(&self) -> String {
        if self.all_channels {
            return "&*".to_string();
        }

        self.channel_patterns
            .iter()
            .map(|pattern| format!("&{}", String::from_utf8_lossy(pattern)))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn is_channel_allowed(&self, channel: &[u8]) -> bool {
        self.all_channels
            || self
                .channel_patterns
                .iter()
                .any(|pattern| glob_match(pattern, channel))
    }

    fn is_channel_pattern_allowed(&self, pattern: &[u8]) -> bool {
        self.all_channels
            || self
                .channel_patterns
                .iter()
                .any(|allowed| allowed == pattern)
    }

    fn acl_channel_permission_error_for_argv(&self, argv: &[Vec<u8>]) -> Option<Vec<u8>> {
        let command = argv.first()?;

        if eq_ascii_token(command, b"PUBLISH") || eq_ascii_token(command, b"SPUBLISH") {
            let channel = argv.get(1)?;
            return (!self.is_channel_allowed(channel)).then(|| channel.clone());
        }

        if eq_ascii_token(command, b"SUBSCRIBE") || eq_ascii_token(command, b"SSUBSCRIBE") {
            for channel in argv.iter().skip(1) {
                if !self.is_channel_allowed(channel) {
                    return Some(channel.clone());
                }
            }
            return None;
        }

        if eq_ascii_token(command, b"PSUBSCRIBE") {
            for pattern in argv.iter().skip(1) {
                if !self.is_channel_pattern_allowed(pattern) {
                    return Some(pattern.clone());
                }
            }
        }

        None
    }

    fn acl_permission_error_for_argv(&self, argv: &[Vec<u8>]) -> Option<AclCommandPermissionError> {
        let Some(cmd) = argv.first() else {
            return Some(AclCommandPermissionError::Command);
        };

        let cmd_name = String::from_utf8_lossy(cmd);
        if !self.is_command_allowed(&cmd_name) {
            return Some(AclCommandPermissionError::Command);
        }

        if fr_command::check_command_arity(cmd, argv.len()).is_err() {
            return None;
        }

        for key_index in fr_command::command_key_indexes(argv) {
            if let Some(key) = argv.get(key_index)
                && !self.is_key_allowed(key)
            {
                return Some(AclCommandPermissionError::Key(key.clone()));
            }
        }

        if let Some(channel) = self.acl_channel_permission_error_for_argv(argv) {
            return Some(AclCommandPermissionError::Channel(channel));
        }

        None
    }

    fn to_dispatch_acl_permissions(&self) -> DispatchAclPermissions {
        DispatchAclPermissions {
            all_commands: self.all_commands,
            allowed_commands: self.allowed_commands.clone(),
            denied_commands: self.denied_commands.clone(),
            allowed_categories: self.allowed_categories.clone(),
            denied_categories: self.denied_categories.clone(),
            key_patterns: self.key_patterns.clone(),
            all_keys: self.all_keys,
            channel_patterns: self.channel_patterns.clone(),
            all_channels: self.all_channels,
        }
    }

    fn sanitize_payload_flag(&self) -> &'static str {
        if self.sanitize_payload {
            "sanitize-payload"
        } else {
            "skip-sanitize-payload"
        }
    }

    fn acl_list_line(&self, username: &[u8]) -> String {
        let username_str = String::from_utf8_lossy(username);
        let mut parts = vec!["user".to_string(), username_str.into_owned()];
        parts.push(if self.enabled {
            "on".to_string()
        } else {
            "off".to_string()
        });
        parts.push(self.sanitize_payload_flag().to_string());
        if self.nopass {
            parts.push("nopass".to_string());
        }
        if !self.nopass {
            parts.extend(self.passwords.iter().map(|_| "#<hidden>".to_string()));
        }
        if self.all_keys {
            parts.push("~*".to_string());
        } else if !self.key_patterns.is_empty() {
            parts.push("resetkeys".to_string());
            parts.extend(
                self.key_patterns
                    .iter()
                    .map(|pattern| format!("~{}", String::from_utf8_lossy(pattern))),
            );
        }
        if self.all_channels {
            parts.push("&*".to_string());
        } else {
            parts.push("resetchannels".to_string());
            parts.extend(
                self.channel_patterns
                    .iter()
                    .map(|pattern| format!("&{}", String::from_utf8_lossy(pattern))),
            );
        }
        parts.extend(
            self.commands_string()
                .split_whitespace()
                .map(str::to_string),
        );
        parts.join(" ")
    }

    fn acl_save_line(&self, username: &[u8]) -> String {
        let username_str = String::from_utf8_lossy(username);
        let mut parts = vec![
            "user".to_string(),
            username_str.into_owned(),
            "reset".to_string(),
        ];
        parts.push(if self.enabled {
            "on".to_string()
        } else {
            "off".to_string()
        });
        parts.push(self.sanitize_payload_flag().to_string());
        if self.nopass {
            parts.push("nopass".to_string());
        }
        if !self.nopass {
            if self.passwords.is_empty() {
                parts.push("resetpass".to_string());
            } else {
                for password in &self.passwords {
                    parts.push(format!("#{}", String::from_utf8_lossy(password)));
                }
            }
        }
        if self.all_keys {
            parts.push("~*".to_string());
        } else if !self.key_patterns.is_empty() {
            parts.push("resetkeys".to_string());
            parts.extend(
                self.key_patterns
                    .iter()
                    .map(|pattern| format!("~{}", String::from_utf8_lossy(pattern))),
            );
        }
        if self.all_channels {
            parts.push("&*".to_string());
        } else {
            parts.push("resetchannels".to_string());
            parts.extend(
                self.channel_patterns
                    .iter()
                    .map(|pattern| format!("&{}", String::from_utf8_lossy(pattern))),
            );
        }
        parts.extend(
            self.commands_string()
                .split_whitespace()
                .map(str::to_string),
        );
        parts.join(" ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AuthState {
    requirepass: Option<Vec<u8>>,
    acl_pubsub_default: AclPubsubDefault,
    acl_users: BTreeMap<Vec<u8>, AclUser>,
}

impl Default for AuthState {
    fn default() -> Self {
        Self::with_acl_pubsub_default(AclPubsubDefault::ResetChannels)
    }
}

impl AuthState {
    fn with_acl_pubsub_default(acl_pubsub_default: AclPubsubDefault) -> Self {
        let mut acl_users = BTreeMap::new();
        acl_users.insert(DEFAULT_AUTH_USER.to_vec(), AclUser::new_default());
        Self {
            requirepass: None,
            acl_pubsub_default,
            acl_users,
        }
    }

    fn set_acl_pubsub_default(&mut self, acl_pubsub_default: AclPubsubDefault) {
        self.acl_pubsub_default = acl_pubsub_default;
    }

    fn set_requirepass(&mut self, requirepass: Option<Vec<u8>>) {
        self.requirepass = requirepass.clone();
        let default_user = self
            .acl_users
            .entry(DEFAULT_AUTH_USER.to_vec())
            .or_insert_with(AclUser::new_default);
        if let Some(pass) = requirepass {
            default_user.passwords = vec![sha256_hex_bytes(&pass)];
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
        user.passwords = vec![sha256_hex_bytes(&password)];
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

    fn serialize_acl_rules(&self) -> String {
        self.acl_users
            .iter()
            .map(|(name, user)| user.acl_save_line(name))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn load_acl_rules(&mut self, content: &str) -> Result<(), String> {
        let mut loaded = Self::with_acl_pubsub_default(self.acl_pubsub_default);
        for raw_line in content.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 2 || !parts[0].eq_ignore_ascii_case("user") {
                return Err("ERR /ACL file contains invalid format".to_string());
            }
            let username = parts[1].as_bytes().to_vec();
            let rules: Vec<&[u8]> = parts[2..].iter().map(|part| part.as_bytes()).collect();
            loaded.set_user(username, &rules)?;
        }
        *self = loaded;
        Ok(())
    }

    fn get_user(&self, username: &[u8]) -> Option<&AclUser> {
        self.acl_users.get(username)
    }

    fn set_user(&mut self, username: Vec<u8>, rules: &[&[u8]]) -> Result<(), String> {
        let is_default = username == DEFAULT_AUTH_USER;
        let acl_pubsub_default = self.acl_pubsub_default;
        let user = self.acl_users.entry(username).or_insert_with(|| {
            if is_default {
                AclUser::new_default()
            } else {
                AclUser::new_restricted(acl_pubsub_default)
            }
        });
        for rule in rules {
            let rule_str = std::str::from_utf8(rule).unwrap_or("");
            if rule_str.eq_ignore_ascii_case("on") {
                user.enabled = true;
            } else if rule_str.eq_ignore_ascii_case("off") {
                user.enabled = false;
            } else if rule_str.eq_ignore_ascii_case("skip-sanitize-payload") {
                user.sanitize_payload = false;
            } else if rule_str.eq_ignore_ascii_case("sanitize-payload") {
                user.sanitize_payload = true;
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
                user.key_patterns.clear();
            } else if rule_str.eq_ignore_ascii_case("resetkeys") {
                // Redis treats ACL modifiers as order-sensitive, so resetkeys must
                // clear any earlier allkeys/~* grant and let later grants re-enable it.
                user.all_keys = false;
                user.key_patterns.clear();
            } else if rule_str.eq_ignore_ascii_case("allchannels") || rule_str == "&*" {
                user.all_channels = true;
                user.channel_patterns.clear();
            } else if rule_str.eq_ignore_ascii_case("resetchannels") {
                user.all_channels = false;
                user.channel_patterns.clear();
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
                user.passwords.push(sha256_hex_bytes(pass.as_bytes()));
                user.nopass = false; // adding a password disables nopass
            } else if let Some(pass) = rule_str.strip_prefix('<') {
                let hash_hex = sha256_hex_bytes(pass.as_bytes());
                let original_len = user.passwords.len();
                user.passwords
                    .retain(|p| p.as_slice() != hash_hex.as_slice());
                if user.passwords.len() == original_len {
                    return Err(acl_setuser_missing_password_error(rule_str));
                }
            } else if let Some(hash) = rule_str.strip_prefix('#') {
                if let Some(hash_hex) = normalize_acl_hash(hash) {
                    user.passwords.push(hash_hex);
                    user.nopass = false;
                } else {
                    return Err(format!(
                        "ERR Error in ACL SETUSER modifier '{}': Syntax error",
                        rule_str
                    ));
                }
            } else if let Some(hash) = rule_str.strip_prefix('!') {
                if let Some(hash_hex) = normalize_acl_hash(hash) {
                    let original_len = user.passwords.len();
                    user.passwords
                        .retain(|p| p.as_slice() != hash_hex.as_slice());
                    if user.passwords.len() == original_len {
                        return Err(acl_setuser_missing_password_error(rule_str));
                    }
                } else {
                    return Err(format!(
                        "ERR Error in ACL SETUSER modifier '{}': Syntax error",
                        rule_str
                    ));
                }
            } else if rule_str.starts_with('~') {
                if user.all_keys {
                    return Err(acl_setuser_allkeys_pattern_error(rule_str));
                }
                let pattern = rule_str
                    .strip_prefix('~')
                    .expect("tilde-prefixed rule should strip");
                let pattern = pattern.as_bytes().to_vec();
                user.key_patterns.retain(|existing| existing != &pattern);
                user.key_patterns.push(pattern);
                user.all_keys = false;
            } else if rule_str.starts_with('&') {
                if user.all_channels {
                    return Err(acl_setuser_allchannels_pattern_error(rule_str));
                }
                let pattern = rule_str
                    .strip_prefix('&')
                    .expect("ampersand-prefixed rule should strip");
                let pattern = pattern.as_bytes().to_vec();
                user.channel_patterns
                    .retain(|existing| existing != &pattern);
                user.channel_patterns.push(pattern);
                user.all_channels = false;
            } else if rule_str.eq_ignore_ascii_case("reset") {
                // Reset user to default state (no passwords, disabled, no commands).
                user.passwords.clear();
                user.nopass = false;
                user.sanitize_payload = true;
                user.all_commands = false;
                user.allowed_commands.clear();
                user.denied_commands.clear();
                user.allowed_categories.clear();
                user.denied_categories.clear();
                user.all_keys = false;
                user.key_patterns.clear();
                user.channel_patterns.clear();
                user.all_channels = acl_pubsub_default.grants_all_channels();
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

/// Parse ACL file contents and return the canonical `ACL SAVE` serialization.
pub fn canonicalize_acl_rules(content: &str) -> Result<String, String> {
    let mut auth_state = AuthState::default();
    auth_state.load_acl_rules(content)?;
    Ok(auth_state.serialize_acl_rules())
}

/// Parse ACL file contents and return the observable `ACL LIST` entries.
pub fn acl_list_entries_from_rules(content: &str) -> Result<Vec<String>, String> {
    let mut auth_state = AuthState::default();
    auth_state.load_acl_rules(content)?;
    Ok(auth_state.acl_list_entries())
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
    Failover,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiskWriteDenialKind {
    Rdb,
    Aof,
}

impl DiskWriteDenialKind {
    fn error_message(self) -> &'static str {
        match self {
            Self::Rdb => RDB_DISK_ERROR_WRITE_DENIED,
            Self::Aof => AOF_DISK_ERROR_WRITE_DENIED,
        }
    }

    fn reason(self) -> &'static str {
        match self {
            Self::Rdb => "last RDB save failed while stop-writes-on-bgsave-error is enabled",
            Self::Aof => "last AOF write failed while appendonly is enabled",
        }
    }
}

#[cfg(test)]
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
            } else if eq_ascii_token(cmd, b"FAILOVER") {
                Some(RuntimeSpecialCommand::Failover)
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
    } else if command.eq_ignore_ascii_case("FAILOVER") {
        Some(RuntimeSpecialCommand::Failover)
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

#[cfg(test)]
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

fn replicaof_command_name(argv: &[Vec<u8>]) -> &'static str {
    if argv
        .first()
        .is_some_and(|command| command.eq_ignore_ascii_case(b"SLAVEOF"))
    {
        "SLAVEOF"
    } else {
        "REPLICAOF"
    }
}

#[inline]
fn parse_db_index_arg(
    arg: &[u8],
    out_of_range_message: &'static str,
    database_count: usize,
) -> Result<usize, RespFrame> {
    let parsed = parse_i64_arg(arg)
        .map_err(|_| RespFrame::Error("ERR value is not an integer or out of range".to_string()))?;
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
    last_ack_timestamp_ms: u64,
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
        if self.backlog.start_offset.0 > self.backlog.end_offset.0 {
            return 0;
        }
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
    executing_exec: bool,
    command_queue: Vec<Vec<Vec<u8>>>,
    /// (key, fingerprint, dirty_counter_at_watch_time)
    watched_keys: Vec<(Vec<u8>, u64, u64)>,
    watch_dirty: bool,
    exec_abort: bool,
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
    /// The absolute replication offset where the local aof_records buffer starts.
    /// For a primary this is 0. For a replica after FULLRESYNC, this is the
    /// primary's offset at sync time.
    aof_base_offset: u64,
    aof_selected_db: usize,
    replication_runtime_state: ReplicationRuntimeState,
    evidence: EvidenceLedger,
    auth_state: AuthState,
    tls_state: TlsRuntimeState,
    acllog_max_len: i64,
    acl_log: std::collections::VecDeque<AclLogEntry>,
    next_acl_log_entry_id: u64,
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
    /// Idle client timeout in seconds (CONFIG SET timeout). Default 0 = disabled.
    pub client_timeout_sec: u64,
    /// Minimum healthy replicas required before writes are accepted.
    min_replicas_to_write: usize,
    /// Maximum acceptable replica ACK lag, in seconds, for write admission.
    min_replicas_max_lag: u64,
    /// Whether failed RDB persistence should make Redis-compatible write commands fail closed.
    stop_writes_on_bgsave_error: bool,
    /// Replica promotion priority reported in INFO replication / CONFIG.
    pub replica_priority: usize,
    /// Optional username used when this server authenticates to its primary.
    pub masteruser: Option<Vec<u8>>,
    /// Optional password used when this server authenticates to its primary.
    pub masterauth: Option<Vec<u8>>,
    /// Whether to serve stale data when the link to the primary is down (CONFIG SET replica-serve-stale-data). Default yes.
    pub replica_serve_stale_data: bool,
    pub repl_diskless_sync: bool,
    pub repl_diskless_sync_delay_sec: u64,
    pub cluster_allow_reads_when_down: bool,
    pub cluster_allow_pubsubshard_when_down: bool,
    pub cluster_link_sendbuf_limit: u64,
    pub cluster_node_timeout: u64,
    pub cluster_migration_barrier: u64,
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
    /// Whether command latency histograms should be recorded for INFO latencystats.
    latency_tracking: bool,
    /// Percentiles emitted by INFO latencystats.
    latency_percentiles: Vec<f64>,
    /// Last successful save timestamp (seconds since epoch).
    last_save_time_sec: u64,
    /// Path for AOF persistence file (used by SAVE/BGSAVE).
    aof_path: Option<std::path::PathBuf>,
    /// Configured AOF target path, preserved even when appendonly is disabled.
    aof_config_path: Option<std::path::PathBuf>,
    /// CONFIG SET appendfsync policy used for WAITAOF local fsync visibility.
    appendfsync_mode: AppendFsyncMode,
    /// Child PID for BGSAVE
    pub rdb_bgsave_pid: Option<i32>,
    /// Child PID for BGREWRITEAOF
    pub aof_rewrite_pid: Option<i32>,
    /// Whether an AOF rewrite is pending because another background child is active.
    pub aof_rewrite_scheduled: bool,
    /// Path for RDB persistence file (used by SAVE/BGSAVE).
    rdb_path: Option<std::path::PathBuf>,
    /// Path for the main redis.conf configuration file (used by CONFIG REWRITE).
    pub config_file_path: Option<std::path::PathBuf>,
    /// Path for ACL SAVE/LOAD persistence file.
    acl_file_path: Option<std::path::PathBuf>,
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
    /// Client IDs currently blocked in the standalone server event loop.
    pub blocked_client_ids: HashSet<u64>,
    /// Snapshot of active TCP client sessions for multi-client CLIENT LIST output.
    client_sessions: BTreeMap<u64, ClientSession>,
    /// Pending CLIENT UNBLOCK requests to be applied by the standalone server.
    pending_client_unblocks: Vec<(u64, ClientUnblockMode)>,
    /// Pending CLIENT KILL requests (client IDs) to be processed by the standalone server.
    pub pending_client_kills: Vec<u64>,
    /// Flag set when REPLICAOF/SLAVEOF changes should force the event loop
    /// to reset replica sync state (drop primary connection and reconnect).
    replica_reconfigure_requested: bool,
}

impl Default for ServerState {
    fn default() -> Self {
        let mut store = Store::new();
        store.maxmemory_bytes_live = 0;
        Self {
            replication_runtime_state: ReplicationRuntimeState::new(
                "0000000000000000000000000000000000000000".to_string(),
            ),
            store,
            aof_records: Vec::new(),
            aof_base_offset: 0,
            aof_selected_db: 0,
            evidence: EvidenceLedger::default(),
            auth_state: AuthState::default(),
            tls_state: TlsRuntimeState::default(),
            acllog_max_len: DEFAULT_ACLLOG_MAX_LEN,
            acl_log: std::collections::VecDeque::new(),
            next_acl_log_entry_id: 0,
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
            client_timeout_sec: 0,
            min_replicas_to_write: 0,
            min_replicas_max_lag: 10,
            stop_writes_on_bgsave_error: true,
            masteruser: None,
            masterauth: None,
            replica_serve_stale_data: true,
            replica_priority: 100,
            repl_diskless_sync: true,
            repl_diskless_sync_delay_sec: 5,
            cluster_allow_reads_when_down: false,
            cluster_allow_pubsubshard_when_down: true,
            cluster_link_sendbuf_limit: 0,
            cluster_node_timeout: 15_000,
            cluster_migration_barrier: 1,
            output_buffer_limit: 256 * 1024 * 1024, // 256 MiB (reasonable default)
            query_buffer_limit: 1024 * 1024 * 1024, // 1 GiB (Redis default)
            proto_max_bulk_len: 512_000_000,        // Redis default (512 MB, not 512 MiB)
            shutdown_requested: false,
            shutdown_nosave: false,
            command_time_budget_ms: 5000,
            last_save_time_sec: 0,
            latency_tracking: true,
            latency_percentiles: vec![50.0, 99.0, 99.9],
            aof_path: None,
            aof_config_path: None,
            appendfsync_mode: AppendFsyncMode::Everysec,
            rdb_bgsave_pid: None,
            aof_rewrite_pid: None,
            aof_rewrite_scheduled: false,
            rdb_path: None,
            config_file_path: None,
            acl_file_path: None,
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
            blocked_client_ids: HashSet::new(),
            client_sessions: BTreeMap::new(),
            pending_client_unblocks: Vec::new(),
            pending_client_kills: Vec::new(),
            replica_reconfigure_requested: false,
        }
    }
}

impl ServerState {
    pub fn set_aof_path(&mut self, path: std::path::PathBuf) {
        self.aof_config_path = Some(path.clone());
        self.aof_path = Some(path);
        self.replication_ack_state.local_fsync_offset = self.replication_ack_state.primary_offset;
        self.store.set_aof_enabled(true);
    }

    pub fn set_rdb_path(&mut self, path: std::path::PathBuf) {
        self.rdb_path = Some(path);
    }

    pub fn set_acl_file_path(&mut self, path: std::path::PathBuf) {
        self.acl_file_path = Some(path);
    }

    fn record_acl_log_event(
        &mut self,
        reason: &'static str,
        context: &'static str,
        object: String,
        username: Vec<u8>,
        client_info: String,
        timestamp_ms: u64,
    ) {
        let max_len = if self.acllog_max_len < 0 {
            0
        } else {
            self.acllog_max_len as usize
        };
        if max_len == 0 {
            return;
        }

        // Search for an identical event and fold it into the newest entry.
        if let Some(existing) = self.acl_log.iter_mut().find(|e| {
            e.reason == reason
                && e.context == context
                && e.object == object
                && e.username == username
                && e.client_info == client_info
        }) {
            existing.count += 1;
            existing.updated_at_ms = timestamp_ms;
            // Move to front (latest)
            let entry = existing.clone();
            self.acl_log.retain(|e| {
                !(e.reason == reason
                    && e.context == context
                    && e.object == object
                    && e.username == username
                    && e.client_info == client_info)
            });
            self.acl_log.push_front(entry);
            return;
        }

        let entry_id = self.next_acl_log_entry_id;
        self.next_acl_log_entry_id = self.next_acl_log_entry_id.saturating_add(1);
        self.acl_log.push_front(AclLogEntry {
            count: 1,
            reason,
            context,
            object,
            username,
            client_info,
            entry_id,
            created_at_ms: timestamp_ms,
            updated_at_ms: timestamp_ms,
        });

        if self.acl_log.len() > max_len {
            self.acl_log.pop_back();
        }
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
        if !self.store.active_expire_enabled {
            let stats = ActiveExpireCycleStats {
                plan: plan_active_expire_cycle(
                    cycle_kind,
                    0,
                    self.active_expire_db_cursor,
                    1,
                    self.active_expire_budget,
                ),
                sampled_keys: 0,
                evicted_keys: 0,
            };
            self.last_active_expire_cycle = Some(stats);
            return stats;
        }
        let start = Instant::now();
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
        let elapsed_ms = start.elapsed().as_millis() as u64;
        self.store.stat_expire_cycle_cpu_milliseconds = self
            .store
            .stat_expire_cycle_cpu_milliseconds
            .saturating_add(elapsed_ms);
        self.store.stat_expired_stale_perc = if cycle_result.sampled_keys == 0 {
            0
        } else {
            ((cycle_result.evicted_keys as u64) * 100) / (cycle_result.sampled_keys as u64)
        };

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
        self.store.maxmemory_bytes_live = maxmemory_bytes;
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

    fn record_latency_sample(&mut self, argv: &[Vec<u8>], duration_us: u64, now_ms: u64) {
        let threshold_ms = self.store.latency_tracker.threshold_ms;
        if threshold_ms == 0 {
            return;
        }

        let duration_ms = duration_us.div_ceil(1000);
        if duration_ms <= threshold_ms {
            return;
        }

        let event = argv
            .first()
            .and_then(|command| fr_command::get_command_flags(command))
            .map(|flags| {
                if flags.split_whitespace().any(|flag| flag == "fast") {
                    "fast-command"
                } else {
                    "command"
                }
            })
            .unwrap_or("command");
        self.store
            .record_latency_sample(event, duration_ms, now_ms / 1000);
    }

    /// Record command execution latency for LATENCY HISTOGRAM.
    fn record_command_histogram(&mut self, argv: &[Vec<u8>], duration_us: u64) {
        if !self.latency_tracking {
            return;
        }
        let Some(cmd) = argv.first() else { return };
        let Ok(cmd_str) = std::str::from_utf8(cmd) else {
            return;
        };
        self.store.record_command_histogram(cmd_str, duration_us);
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
        if self.local_waitaof_fsync_tracks_primary_offset() {
            self.replication_ack_state.local_fsync_offset =
                self.replication_ack_state.primary_offset;
        }
        self.replication_runtime_state.update_backlog_window(
            self.replication_ack_state.primary_offset,
            self.repl_backlog_size,
        );
    }

    fn local_waitaof_fsync_tracks_primary_offset(&self) -> bool {
        self.aof_path.is_some()
            && self.store.aof_enabled
            && !self.aof_rewrite_scheduled
            && self.appendfsync_mode != AppendFsyncMode::No
    }
}

/// State that is clearly scoped to a single client session.
#[derive(Debug, Clone)]
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
    /// Client-side caching / tracking configuration.
    client_tracking: ClientTrackingState,
    /// Per-client direct reply suppression state for CLIENT REPLY.
    client_reply: ClientReplyState,
    /// Client peer address (set on connection accept).
    pub peer_addr: Option<std::net::SocketAddr>,
    /// Connection acceptance timestamp in ms since epoch.
    pub connected_at_ms: u64,
    /// Last command interaction timestamp in ms since epoch.
    pub last_interaction_ms: u64,
    /// Most recent command recorded for CLIENT LIST/INFO `cmd=`.
    last_command_name: String,
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
            client_tracking: ClientTrackingState::default(),
            client_reply: ClientReplyState::default(),
            peer_addr: None,
            connected_at_ms: 0,
            last_interaction_ms: 0,
            last_command_name: "NULL".to_string(),
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
        self.client_no_evict = false;
        self.client_no_touch = false;
        self.client_tracking = ClientTrackingState::default();
        self.client_reply = ClientReplyState::default();
        self.refresh_authentication_for_server(auth_state, false);
    }
}

#[derive(Debug)]
pub struct Runtime {
    policy: RuntimePolicy,
    pub server: ServerState,
    session: ClientSession,
    execution_source: ExecutionSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveExpireCycleStats {
    pub plan: ActiveExpireCyclePlan,
    pub sampled_keys: usize,
    pub evicted_keys: usize,
}

/// Source for the lazily-computed `input_digest` recorded in the evidence
/// ledger. The digest is only materialized when the ledger is enabled and a
/// threat event actually fires, so the success path pays nothing.
enum ThreatInputDigestSource<'a> {
    Frame(&'a RespFrame),
    Argv(&'a [Vec<u8>]),
    Bytes(&'a [u8]),
    /// Pre-computed bytes (e.g. for TLS config events where the source is a
    /// rendered debug string, not a wire frame).
    Owned(Vec<u8>),
}

impl ThreatInputDigestSource<'_> {
    fn digest(&self) -> String {
        match self {
            Self::Frame(frame) => digest_bytes(&frame.to_bytes()),
            Self::Argv(argv) => digest_bytes(&argv_to_resp_bytes(argv)),
            Self::Bytes(bytes) => digest_bytes(bytes),
            Self::Owned(bytes) => digest_bytes(bytes),
        }
    }
}

/// Encode an argv list as RESP-2 array bytes, used as the canonical
/// representation for threat-event input digests when only the parsed argv is
/// available at the recording site.
fn argv_to_resp_bytes(argv: &[Vec<u8>]) -> Vec<u8> {
    let frame = RespFrame::Array(Some(
        argv.iter()
            .map(|item| RespFrame::BulkString(Some(item.clone())))
            .collect(),
    ));
    frame.to_bytes()
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
    input_source: ThreatInputDigestSource<'a>,
    output: &'a RespFrame,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecutionSource {
    Client,
    AofLoad,
    ReplicationReplay,
}

impl ExecutionSource {
    fn counts_as_unexpected_error_reply(self) -> bool {
        matches!(self, Self::AofLoad | Self::ReplicationReplay)
    }
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
            execution_source: ExecutionSource::Client,
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

    pub fn set_acl_file_path(&mut self, path: std::path::PathBuf) {
        self.server.set_acl_file_path(path);
    }

    pub fn set_config_file_path(&mut self, path: Option<std::path::PathBuf>) {
        self.server.config_file_path = path;
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
        let mut original_store = std::mem::replace(&mut self.server.store, Store::new());
        let original_records = std::mem::take(&mut self.server.aof_records);
        let original_aof_db = self.server.aof_selected_db;
        let original_db = self.session.selected_db;

        self.server.aof_selected_db = 0;
        self.session.selected_db = 0;
        for (index, record) in records.iter().enumerate() {
            let replay_now_ms = now_ms.saturating_add(index as u64);
            let reply = self.with_execution_source(ExecutionSource::AofLoad, |runtime| {
                runtime.execute_frame(record.to_resp_frame(), replay_now_ms)
            });
            if matches!(reply, RespFrame::Error(_)) {
                original_store.stat_total_error_replies = original_store
                    .stat_total_error_replies
                    .saturating_add(self.server.store.stat_total_error_replies);
                original_store.stat_unexpected_error_replies = original_store
                    .stat_unexpected_error_replies
                    .saturating_add(self.server.store.stat_unexpected_error_replies);
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

    /// Load an RDB snapshot from the configured path, replacing store state.
    /// Returns the number of entries loaded, or an error.
    pub fn load_rdb(&mut self, now_ms: u64) -> Result<usize, PersistError> {
        let path = match &self.server.rdb_path {
            Some(path) => path.clone(),
            None => return Ok(0),
        };
        let (entries, _aux) = read_rdb_file(&path)?;
        let count = entries.len();
        let mut store = Store::new();
        apply_rdb_entries_to_store(&mut store, &entries, now_ms)?;
        store.set_aof_enabled(self.server.store.aof_enabled);
        self.server.store = store;
        Ok(count)
    }

    fn handle_debug_reload_requested(&mut self, now_ms: u64) -> RespFrame {
        if let Err(reply) = self.persist_snapshot_to_disk(now_ms, false) {
            return reply;
        }

        if self.server.aof_path.is_some() {
            return match self.load_aof(now_ms.saturating_add(1)) {
                Ok(_) => RespFrame::SimpleString("OK".to_string()),
                Err(_) => RespFrame::Error("ERR failed to reload dataset from AOF".to_string()),
            };
        }

        let Some(path) = self.server.rdb_path.clone() else {
            return RespFrame::Error(
                "ERR DEBUG RELOAD requires configured appendonly or RDB persistence".to_string(),
            );
        };

        match read_rdb_file(&path) {
            Ok((entries, _aux)) => {
                let mut store = Store::new();
                if apply_rdb_entries_to_store(&mut store, &entries, now_ms.saturating_add(1))
                    .is_err()
                {
                    return RespFrame::Error("ERR failed to reload dataset from RDB".to_string());
                }
                self.server.store = store;
                self.session.selected_db = 0;
                RespFrame::SimpleString("OK".to_string())
            }
            Err(_) => RespFrame::Error("ERR failed to reload dataset from RDB".to_string()),
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
        // Convert absolute replication offset to local stream index by subtracting
        // the AOF base offset. The encoded stream starts at aof_base_offset.
        let base_offset = self.server.aof_base_offset;
        let relative_offset = offset.saturating_sub(base_offset);
        let start = usize::try_from(relative_offset).unwrap_or(usize::MAX);
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
        self.replay_records_with_source(records, now_ms, ExecutionSource::AofLoad)
    }

    fn replay_records_with_source(
        &mut self,
        records: &[AofRecord],
        now_ms: u64,
        source: ExecutionSource,
    ) -> Vec<RespFrame> {
        let mut replies = Vec::with_capacity(records.len());
        for (index, record) in records.iter().enumerate() {
            let replay_now_ms = now_ms.saturating_add(index as u64);
            let reply = self.with_execution_source(source, |runtime| {
                runtime.execute_frame(record.to_resp_frame(), replay_now_ms)
            });
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
                let records = decode_aof_stream(payload)?;
                self.replay_records_with_source(
                    &records,
                    now_ms,
                    ExecutionSource::ReplicationReplay,
                );
            }
            PsyncReply::FullResync { replid, offset } => {
                let (entries, _aux) = decode_rdb(payload)?;
                let mut store = Store::new();
                apply_rdb_entries_to_store(&mut store, &entries, now_ms)?;
                self.server.store = store;
                self.server.aof_records.clear();
                // Track the base offset where the local AOF buffer starts.
                // New records will be indexed from 0, but they correspond to
                // absolute offset starting at this value.
                self.server.aof_base_offset = offset.0;
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

    #[must_use]
    pub fn replica_primary_auth(&self) -> Option<(Option<Vec<u8>>, Vec<u8>)> {
        self.server
            .masterauth
            .as_ref()
            .map(|password| (self.server.masteruser.clone(), password.clone()))
    }

    #[must_use]
    pub fn take_replica_reconfigure_request(&mut self) -> bool {
        let requested = self.server.replica_reconfigure_requested;
        self.server.replica_reconfigure_requested = false;
        requested
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

    /// Returns the start offset of the replication backlog.
    /// This is the earliest offset still available in the AOF stream.
    #[must_use]
    pub fn replication_backlog_start_offset(&self) -> ReplOffset {
        self.server.replication_runtime_state.backlog.start_offset
    }

    /// Returns the base offset of the local AOF buffer.
    /// For a primary, this is 0. For a replica after FULLRESYNC, this is
    /// the primary's offset at sync time.
    #[must_use]
    pub fn aof_base_offset(&self) -> u64 {
        self.server.aof_base_offset
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

    /// Retrieve the current client session ID.
    pub fn client_id(&self) -> u64 {
        self.session.client_id
    }

    #[must_use]
    pub fn suppress_current_network_reply(&self) -> bool {
        self.session.client_reply.suppress_current_response
    }

    /// Resolve the current tail ID for a stream key, used by blocking XREAD
    /// callers that must freeze `$` cursors at block time.
    pub fn xread_block_resume_id(&mut self, key: &[u8], now_ms: u64) -> Option<(u64, u64)> {
        self.server.store.xlast_id(key, now_ms).ok().flatten()
    }

    /// Update or insert a session snapshot used by multi-client CLIENT LIST.
    pub fn record_client_session(&mut self, session: &ClientSession) {
        self.server
            .client_sessions
            .insert(session.client_id, session.clone());
    }

    /// Remove a disconnected session from the CLIENT LIST registry.
    pub fn remove_client_session(&mut self, client_id: u64) {
        self.server.client_sessions.remove(&client_id);
    }

    /// Track a new client connection for INFO stats.
    pub fn track_connection_opened(&mut self) {
        self.server.store.stat_total_connections_received += 1;
        self.server.store.stat_connected_clients += 1;
    }

    /// Track network input bytes for INFO stats.
    pub fn track_net_input_bytes(&mut self, bytes: u64) {
        self.server.store.stat_total_net_input_bytes += bytes;
    }

    /// Track network output bytes for INFO stats.
    pub fn track_net_output_bytes(&mut self, bytes: u64) {
        self.server.store.stat_total_net_output_bytes += bytes;
    }

    /// Record an instantaneous ops/sec sample. Call once per server-hz tick.
    pub fn record_ops_sec_sample(&mut self, elapsed_ms: u64) {
        self.server.store.record_ops_sec_sample(elapsed_ms);
    }

    /// Track a rejected connection (maxclients exceeded).
    pub fn track_rejected_connection(&mut self) {
        self.server.store.stat_rejected_connections += 1;
    }

    /// Track a full resync event (PSYNC FULLRESYNC).
    pub fn track_sync_full(&mut self) {
        self.server.store.stat_sync_full += 1;
    }

    /// Track a successful partial resync (PSYNC CONTINUE).
    pub fn track_sync_partial_ok(&mut self) {
        self.server.store.stat_sync_partial_ok += 1;
    }

    /// Track a failed partial resync attempt (fell back to full resync).
    pub fn track_sync_partial_err(&mut self) {
        self.server.store.stat_sync_partial_err += 1;
    }

    /// Track a client disconnection for INFO stats.
    pub fn track_connection_closed(&mut self) {
        self.server.store.stat_connected_clients =
            self.server.store.stat_connected_clients.saturating_sub(1);
    }

    /// Clean up replication/monitor state for a disconnected client.
    pub fn cleanup_disconnected_client(&mut self, client_id: u64) {
        self.disable_monitor(client_id);
        if self
            .server
            .replication_runtime_state
            .replicas
            .remove(&client_id)
            .is_some()
        {
            self.server.refresh_replica_ack_snapshots();
        }
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

    fn session_has_pubsub_permissions_revoked(&self, session: &ClientSession) -> bool {
        let has_pubsub_state = self
            .server
            .pubsub_client_channels
            .get(&session.client_id)
            .is_some_and(|channels| !channels.is_empty())
            || self
                .server
                .pubsub_client_patterns
                .get(&session.client_id)
                .is_some_and(|patterns| !patterns.is_empty())
            || self
                .server
                .pubsub_client_shard_channels
                .get(&session.client_id)
                .is_some_and(|channels| !channels.is_empty());
        if !has_pubsub_state {
            return false;
        }

        let Some(username) = session.authenticated_user.as_ref() else {
            return false;
        };
        let Some(user) = self.server.auth_state.get_user(username) else {
            return true;
        };

        self.server
            .pubsub_client_channels
            .get(&session.client_id)
            .is_some_and(|channels| {
                channels
                    .iter()
                    .any(|channel| !user.is_channel_allowed(channel))
            })
            || self
                .server
                .pubsub_client_patterns
                .get(&session.client_id)
                .is_some_and(|patterns| {
                    patterns
                        .iter()
                        .any(|pattern| !user.is_channel_pattern_allowed(pattern))
                })
            || self
                .server
                .pubsub_client_shard_channels
                .get(&session.client_id)
                .is_some_and(|channels| {
                    channels
                        .iter()
                        .any(|channel| !user.is_channel_allowed(channel))
                })
    }

    fn queue_acl_pubsub_revocations_for_user(&mut self, username: &[u8]) {
        let sessions = self.client_list_sessions();
        for session in sessions.values() {
            if session.authenticated_user.as_deref() != Some(username) {
                continue;
            }
            if self.session_has_pubsub_permissions_revoked(session)
                && !self
                    .server
                    .pending_client_kills
                    .contains(&session.client_id)
            {
                self.server.pending_client_kills.push(session.client_id);
            }
        }
    }

    fn queue_acl_deleted_user_disconnects(&mut self, username: &[u8]) {
        let sessions = self.client_list_sessions();
        for session in sessions.values() {
            if session.authenticated_user.as_deref() == Some(username)
                && !self
                    .server
                    .pending_client_kills
                    .contains(&session.client_id)
            {
                self.server.pending_client_kills.push(session.client_id);
            }
        }
    }

    fn queue_acl_pubsub_revocations_for_all_users(&mut self) {
        let sessions = self.client_list_sessions();
        for session in sessions.values() {
            if self.session_has_pubsub_permissions_revoked(session)
                && !self
                    .server
                    .pending_client_kills
                    .contains(&session.client_id)
            {
                self.server.pending_client_kills.push(session.client_id);
            }
        }
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

    pub fn pubsub_sub_count(&self, client_id: u64) -> usize {
        self.server
            .pubsub_client_channels
            .get(&client_id)
            .map_or(0, std::collections::HashSet::len)
            + self
                .server
                .pubsub_client_patterns
                .get(&client_id)
                .map_or(0, std::collections::HashSet::len)
    }

    pub fn pubsub_shard_sub_count(&self, client_id: u64) -> usize {
        self.server
            .pubsub_client_shard_channels
            .get(&client_id)
            .map_or(0, std::collections::HashSet::len)
    }

    /// Returns true if a given client ID has any active pub/sub subscriptions.
    pub fn is_pubsub_client(&self, client_id: u64) -> bool {
        self.pubsub_sub_count(client_id) > 0
            || self
                .server
                .pubsub_client_patterns
                .get(&client_id)
                .is_some_and(|patterns| !patterns.is_empty())
            || self.pubsub_shard_sub_count(client_id) > 0
    }

    /// Returns true if the current client has any active pub/sub subscriptions.
    pub fn is_in_subscription_mode(&self) -> bool {
        self.is_pubsub_client(self.session.client_id)
    }

    fn pubsub_context_error(command_name: &str) -> RespFrame {
        RespFrame::Error(format!(
            "ERR Can't execute '{}': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT / RESET are allowed in this context",
            command_name
        ))
    }

    fn pubsub_blocked_command_name(argv: &[Vec<u8>]) -> String {
        let command = String::from_utf8_lossy(argv.first().map_or(b"", Vec::as_slice));
        if command.eq_ignore_ascii_case("PUBSUB")
            && let Some(subcommand) = argv.get(1)
        {
            return format!(
                "pubsub|{}",
                String::from_utf8_lossy(subcommand).to_ascii_lowercase()
            );
        }
        command.to_ascii_lowercase()
    }

    fn pubsub_command_allowed_in_subscription_mode(
        special_command: Option<RuntimeSpecialCommand>,
        argv: &[Vec<u8>],
    ) -> bool {
        matches!(
            special_command,
            Some(RuntimeSpecialCommand::Subscribe)
                | Some(RuntimeSpecialCommand::Unsubscribe)
                | Some(RuntimeSpecialCommand::Psubscribe)
                | Some(RuntimeSpecialCommand::Punsubscribe)
                | Some(RuntimeSpecialCommand::Ssubscribe)
                | Some(RuntimeSpecialCommand::Sunsubscribe)
                | Some(RuntimeSpecialCommand::Quit)
                | Some(RuntimeSpecialCommand::Reset)
                | Some(RuntimeSpecialCommand::Pubsub)
        ) || argv
            .first()
            .is_some_and(|command| eq_ascii_token(command, b"PING"))
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

    fn refresh_store_runtime_info_context(&mut self) {
        self.server.store.stat_tracking_clients = 0;
        self.server.store.maxmemory_bytes_live = self.server.maxmemory_bytes;
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

    fn acl_permission_error(&self, argv: &[Vec<u8>]) -> Option<AclCommandPermissionError> {
        let username = self.session.current_user_name();
        let Some(user) = self.server.auth_state.get_user(username) else {
            // User was deleted while session is still active. In Redis, the
            // connection would be killed asynchronously. For compatibility
            // in single-session mode, allow continued access.
            return None;
        };

        user.acl_permission_error_for_argv(argv)
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

    fn apply_masteruser_update(&mut self, masteruser: Option<Vec<u8>>) {
        let changed = self.server.masteruser != masteruser;
        self.server.masteruser = masteruser;
        if changed
            && let ReplicationRoleState::Replica { state, .. } =
                &mut self.server.replication_runtime_state.role
        {
            *state = "connect";
            self.server.replica_reconfigure_requested = true;
        }
    }

    fn apply_masterauth_update(&mut self, masterauth: Option<Vec<u8>>) {
        let changed = self.server.masterauth != masterauth;
        self.server.masterauth = masterauth;
        if changed
            && let ReplicationRoleState::Replica { state, .. } =
                &mut self.server.replication_runtime_state.role
        {
            *state = "connect";
            self.server.replica_reconfigure_requested = true;
        }
    }

    pub fn set_masteruser(&mut self, masteruser: Option<Vec<u8>>) {
        self.apply_masteruser_update(masteruser);
    }

    pub fn set_masterauth(&mut self, masterauth: Option<Vec<u8>>) {
        self.apply_masterauth_update(masterauth);
    }

    pub fn apply_tls_config(
        &mut self,
        candidate: TlsConfig,
        now_ms: u64,
    ) -> Result<(), TlsCfgError> {
        let packet_id = next_packet_id();
        // Render candidate to debug bytes only once; passed into the threat
        // recorder lazily so the success path stays free of digest cost.
        let candidate_debug = format!("{candidate:?}").into_bytes();

        let plan = match plan_tls_runtime_apply(&self.server.tls_state, candidate) {
            Ok(plan) => plan,
            Err(error) => {
                let preferred_deviation = preferred_tls_deviation_for_error(&error);
                let gated_error = self.gate_tls_error_for_mode(error, preferred_deviation);
                self.record_tls_config_event(
                    now_ms,
                    packet_id,
                    candidate_debug,
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

    /// Check and reap any completed background child processes.
    #[allow(unsafe_code)]
    pub fn check_child_processes(&mut self, now_ms: u64) {
        #[cfg(unix)]
        unsafe {
            if let Some(pid) = self.server.rdb_bgsave_pid {
                let mut status = 0;
                let res = libc::waitpid(pid, &mut status, libc::WNOHANG);
                if res == pid {
                    self.server.rdb_bgsave_pid = None;
                    let success = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
                    self.server.store.record_bgsave_status(success);
                }
            }
            if let Some(pid) = self.server.aof_rewrite_pid {
                let mut status = 0;
                let res = libc::waitpid(pid, &mut status, libc::WNOHANG);
                if res == pid {
                    self.server.aof_rewrite_pid = None;
                    let success = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
                    // For now, AOF rewrite CoW is just tracked, but let's record status generically
                    self.server.store.record_aof_bgrewrite_status(success);
                }
            }
        }
        self.maybe_run_scheduled_aof_rewrite(now_ms);
    }

    /// Wait for child processes to finish (useful for tests).
    #[allow(unsafe_code)]
    pub fn wait_for_child_processes(&mut self) {
        #[cfg(unix)]
        unsafe {
            if let Some(pid) = self.server.rdb_bgsave_pid {
                let mut status = 0;
                libc::waitpid(pid, &mut status, 0); // Blocking wait
                self.server.rdb_bgsave_pid = None;
                let success = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
                self.server.store.record_bgsave_status(success);
            }
            if let Some(pid) = self.server.aof_rewrite_pid {
                let mut status = 0;
                libc::waitpid(pid, &mut status, 0); // Blocking wait
                self.server.aof_rewrite_pid = None;
                let success = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
                self.server.store.record_aof_bgrewrite_status(success);
            }
        }
        self.maybe_run_scheduled_aof_rewrite(0);
    }

    fn maybe_run_scheduled_aof_rewrite(&mut self, now_ms: u64) {
        if !self.server.aof_rewrite_scheduled
            || self.server.rdb_bgsave_pid.is_some()
            || self.server.aof_rewrite_pid.is_some()
        {
            return;
        }
        let _ = self.handle_bgrewriteaof_requested(now_ms);
    }

    pub fn execute_frame(&mut self, frame: RespFrame, now_ms: u64) -> RespFrame {
        self.server.store.stat_total_commands_processed += 1;
        if self.session.connected_at_ms == 0 {
            self.session.connected_at_ms = now_ms;
        }
        self.session.last_interaction_ms = self.session.last_interaction_ms.max(now_ms);
        self.refresh_store_runtime_info_context();
        // Parse once, reuse for both stats and execution (eliminates double parse).
        let argv_result = frame_to_argv(&frame);
        if let Ok(argv) = &argv_result {
            self.session.last_command_name = client_info_command_name(argv);
        }
        let current_session = self.session.clone();
        self.record_client_session(&current_session);
        let processed_counts = argv_result
            .as_ref()
            .ok()
            .map(|argv| processed_command_counts(argv))
            .unwrap_or((0, 0));
        let packet_id = next_packet_id();
        // No eager digest computation here. Threat events compute their own
        // digests on demand inside record_threat_event, so the success path
        // pays nothing for the evidence ledger.
        let disable_touch = self.session.client_no_touch
            && argv_result
                .as_ref()
                .ok()
                .and_then(|argv| argv.first())
                .map(|cmd| !eq_ascii_token(cmd, b"TOUCH"))
                .unwrap_or(false);
        let reply = fr_store::with_touch_disabled(disable_touch, || {
            self.execute_frame_internal(frame, argv_result, now_ms, packet_id)
        });
        if matches!(reply, RespFrame::Error(_)) {
            self.server.store.stat_total_error_replies += 1;
            if self.execution_source.counts_as_unexpected_error_reply() {
                self.server.store.stat_unexpected_error_replies += 1;
            }
        }
        self.server.store.stat_total_reads_processed += processed_counts.0;
        self.server.store.stat_total_writes_processed += processed_counts.1;
        reply
    }

    fn with_execution_source<T>(
        &mut self,
        source: ExecutionSource,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let previous = std::mem::replace(&mut self.execution_source, source);
        let output = f(self);
        self.execution_source = previous;
        output
    }

    fn command_requires_replica_write_quorum(
        &self,
        argv: &[Vec<u8>],
        special_command: Option<RuntimeSpecialCommand>,
    ) -> bool {
        if matches!(special_command, Some(RuntimeSpecialCommand::Exec)) {
            return self
                .session
                .transaction_state
                .command_queue
                .iter()
                .any(|queued| {
                    queued
                        .first()
                        .is_some_and(|command| fr_command::is_write_command(command))
                });
        }

        argv.first()
            .is_some_and(|command| fr_command::is_write_command(command))
    }

    fn good_replica_write_count(&self, now_ms: u64) -> usize {
        self.server
            .replication_runtime_state
            .replicas
            .values()
            .filter(|replica| {
                if replica.last_ack_timestamp_ms == 0 {
                    return false;
                }
                let lag_seconds = now_ms.saturating_sub(replica.last_ack_timestamp_ms) / 1000;
                lag_seconds <= self.server.min_replicas_max_lag
            })
            .count()
    }

    fn reject_due_to_replica_write_quorum(
        &self,
        argv: &[Vec<u8>],
        special_command: Option<RuntimeSpecialCommand>,
        now_ms: u64,
    ) -> Option<RespFrame> {
        if !self.command_requires_replica_write_quorum(argv, special_command) {
            return None;
        }
        if !matches!(
            self.server.replication_runtime_state.role,
            ReplicationRoleState::Master
        ) {
            return None;
        }
        if self.server.min_replicas_to_write == 0 || self.server.min_replicas_max_lag == 0 {
            return None;
        }
        if self.good_replica_write_count(now_ms) >= self.server.min_replicas_to_write {
            return None;
        }
        Some(RespFrame::Error(
            "NOREPLICAS Not enough good replicas to write.".to_string(),
        ))
    }

    fn command_subject_to_disk_write_denial(
        &self,
        argv: &[Vec<u8>],
        special_command: Option<RuntimeSpecialCommand>,
    ) -> bool {
        if argv
            .first()
            .is_some_and(|command| eq_ascii_token(command, b"PING"))
        {
            return true;
        }
        self.command_requires_replica_write_quorum(argv, special_command)
    }

    fn active_disk_write_denial(&self) -> Option<DiskWriteDenialKind> {
        if self.server.stop_writes_on_bgsave_error
            && self.server.rdb_path.is_some()
            && !self.server.store.stat_rdb_last_bgsave_ok
        {
            return Some(DiskWriteDenialKind::Rdb);
        }
        if self.server.aof_path.is_some() && !self.server.store.stat_aof_last_write_ok {
            return Some(DiskWriteDenialKind::Aof);
        }
        None
    }

    fn reject_due_to_disk_write_error(
        &mut self,
        argv: &[Vec<u8>],
        special_command: Option<RuntimeSpecialCommand>,
        now_ms: u64,
        packet_id: u64,
    ) -> Option<RespFrame> {
        if !self.command_subject_to_disk_write_denial(argv, special_command) {
            return None;
        }
        let denial = self.active_disk_write_denial()?;
        let reply = RespFrame::Error(denial.error_message().to_string());
        self.record_threat_event(ThreatEventInput {
            now_ms,
            packet_id,
            threat_class: ThreatClass::PersistenceTampering,
            preferred_deviation: None,
            subsystem: "persistence",
            action: "disk_error_write_denial",
            reason_code: "persist.disk_error_write_denied",
            reason: denial.reason().to_string(),
            input_source: ThreatInputDigestSource::Argv(argv),
            output: &reply,
        });
        Some(reply)
    }

    fn reject_stale_replica_read_request(
        &self,
        argv: &[Vec<u8>],
        special_command: Option<RuntimeSpecialCommand>,
    ) -> Option<RespFrame> {
        if self.server.replica_serve_stale_data {
            return None;
        }
        let ReplicationRoleState::Replica { state, .. } =
            &self.server.replication_runtime_state.role
        else {
            return None; // Masters do not enforce replica-serve-stale-data
        };
        if *state == "connected" {
            return None; // Not stale if connected to primary
        }
        let command = argv.first()?;
        // The following commands are permitted during a stale state.
        let permitted = matches!(
            special_command,
            Some(RuntimeSpecialCommand::Acl)
                | Some(RuntimeSpecialCommand::Client)
                | Some(RuntimeSpecialCommand::Replicaof)
                | Some(RuntimeSpecialCommand::Slaveof)
                | Some(RuntimeSpecialCommand::Auth)
                | Some(RuntimeSpecialCommand::Hello)
                | Some(RuntimeSpecialCommand::Shutdown)
                | Some(RuntimeSpecialCommand::Replconf)
                | Some(RuntimeSpecialCommand::Role)
                | Some(RuntimeSpecialCommand::Config)
                | Some(RuntimeSpecialCommand::Readonly)
                | Some(RuntimeSpecialCommand::Readwrite)
                | Some(RuntimeSpecialCommand::Cluster)
                | Some(RuntimeSpecialCommand::Subscribe)
                | Some(RuntimeSpecialCommand::Unsubscribe)
                | Some(RuntimeSpecialCommand::Psubscribe)
                | Some(RuntimeSpecialCommand::Punsubscribe)
                | Some(RuntimeSpecialCommand::Ssubscribe)
                | Some(RuntimeSpecialCommand::Sunsubscribe)
                | Some(RuntimeSpecialCommand::Publish)
                | Some(RuntimeSpecialCommand::Spublish)
                | Some(RuntimeSpecialCommand::Pubsub)
                | Some(RuntimeSpecialCommand::Wait)
                | Some(RuntimeSpecialCommand::Select)
        ) || eq_ascii_token(command, b"INFO")
            || eq_ascii_token(command, b"COMMAND")
            || eq_ascii_token(command, b"LATENCY");

        if permitted {
            return None;
        }

        Some(RespFrame::Error(
            "MASTERDOWN Link with MASTER is down and replica-serve-stale-data is set to 'no'."
                .to_string(),
        ))
    }

    fn execute_frame_internal(
        &mut self,
        frame: RespFrame,
        argv_result: Result<Vec<Vec<u8>>, CommandError>,
        now_ms: u64,
        packet_id: u64,
    ) -> RespFrame {
        if let Some(reply) = self.preflight_gate(&frame, now_ms, packet_id) {
            return reply;
        }

        // Use pre-parsed argv if available, avoiding duplicate parsing.
        let argv = match argv_result {
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
                        input_source: ThreatInputDigestSource::Frame(&frame),
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
                    input_source: ThreatInputDigestSource::Frame(&frame),
                    output: &reply,
                });
                return reply;
            }
        };

        let special_command = classify_runtime_special_command(&argv[0]);
        let command_name_lossy = String::from_utf8_lossy(&argv[0]);
        let command_name = &command_name_lossy;

        match special_command {
            Some(RuntimeSpecialCommand::Auth) => {
                let start = Instant::now();
                let reply = self.handle_auth_command(&argv, now_ms);
                let reply = match apply_client_reply_state(&argv, &mut self.session.client_reply) {
                    Ok(()) => reply,
                    Err(err) => err.to_resp(),
                };
                let elapsed_us = start.elapsed().as_micros() as u64;
                self.record_slowlog(&argv, elapsed_us, now_ms);
                self.server.record_latency_sample(&argv, elapsed_us, now_ms);
                self.server.record_command_histogram(&argv, elapsed_us);
                return reply;
            }
            Some(RuntimeSpecialCommand::Hello) => {
                let start = Instant::now();
                let reply = self.handle_hello_command(&argv, now_ms);
                let reply = match apply_client_reply_state(&argv, &mut self.session.client_reply) {
                    Ok(()) => reply,
                    Err(err) => err.to_resp(),
                };
                let elapsed_us = start.elapsed().as_micros() as u64;
                self.record_slowlog(&argv, elapsed_us, now_ms);
                self.server.record_latency_sample(&argv, elapsed_us, now_ms);
                self.server.record_command_histogram(&argv, elapsed_us);
                return reply;
            }
            _ => {}
        }

        if self.session.requires_auth(&self.server.auth_state) {
            let reply = RespFrame::Error(NOAUTH_ERROR.to_string());
            self.apply_existing_client_reply_suppression_to_undispatched_reply();
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
                input_source: ThreatInputDigestSource::Argv(&argv),
                output: &reply,
            });
            return reply;
        }

        if let Some(permission_error) = self.acl_permission_error(&argv) {
            let (log_reason, object, reason_code, threat_reason, reply) = match permission_error {
                AclCommandPermissionError::Command => (
                    "command",
                    command_name.to_ascii_uppercase(),
                    "auth.noperm_gate_violation",
                    format!(
                        "rejected '{}' prior to dispatch due to insufficient ACL permissions",
                        command_name
                    ),
                    RespFrame::Error(format!(
                        "NOPERM this user has no permissions to run the '{}' command",
                        command_name
                    )),
                ),
                AclCommandPermissionError::Key(key) => {
                    let key_name = String::from_utf8_lossy(&key).into_owned();
                    (
                        "key",
                        key_name.clone(),
                        "auth.noperm_key_gate_violation",
                        format!(
                            "rejected '{}' prior to dispatch because key '{}' is outside the user's ACL patterns",
                            command_name, key_name
                        ),
                        RespFrame::Error("NOPERM No permissions to access a key".to_string()),
                    )
                }
                AclCommandPermissionError::Channel(channel) => {
                    let channel_name = String::from_utf8_lossy(&channel).into_owned();
                    (
                        "channel",
                        channel_name.clone(),
                        "auth.noperm_channel_gate_violation",
                        format!(
                            "rejected '{}' prior to dispatch because channel '{}' is outside the user's ACL channel patterns",
                            command_name, channel_name
                        ),
                        RespFrame::Error("NOPERM No permissions to access a channel".to_string()),
                    )
                }
            };
            self.apply_existing_client_reply_suppression_to_undispatched_reply();
            self.record_acl_log_event(
                log_reason,
                object,
                self.session.current_user_name().to_vec(),
                now_ms,
            );
            self.record_threat_event(ThreatEventInput {
                now_ms,
                packet_id,
                threat_class: ThreatClass::AuthPolicyConfusion,
                preferred_deviation: None,
                subsystem: "admission_gate",
                action: "reject_unauthorized_command",
                reason_code,
                reason: threat_reason,
                input_source: ThreatInputDigestSource::Argv(&argv),
                output: &reply,
            });
            return reply;
        }

        let command_arity_ok = argv
            .first()
            .is_some_and(|command| fr_command::check_command_arity(command, argv.len()).is_ok());
        if command_arity_ok
            && self.is_in_subscription_mode()
            && !Self::pubsub_command_allowed_in_subscription_mode(special_command, &argv)
        {
            self.apply_existing_client_reply_suppression_to_undispatched_reply();
            return Self::pubsub_context_error(&Self::pubsub_blocked_command_name(&argv));
        }
        if command_arity_ok
            && let Some(reply) =
                self.reject_due_to_disk_write_error(&argv, special_command, now_ms, packet_id)
        {
            self.apply_existing_client_reply_suppression_to_undispatched_reply();
            return reply;
        }
        if command_arity_ok
            && let Some(reply) =
                self.reject_due_to_replica_write_quorum(&argv, special_command, now_ms)
        {
            self.apply_existing_client_reply_suppression_to_undispatched_reply();
            return reply;
        }

        if let Some(reply) = self.reject_stale_replica_read_request(&argv, special_command) {
            self.apply_existing_client_reply_suppression_to_undispatched_reply();
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
                self.apply_existing_client_reply_suppression_to_undispatched_reply();
                return RespFrame::Error(
                    "ERR Command not allowed inside a transaction".to_string(),
                );
            }
            if !must_execute_now {
                let cmd_bytes = match argv.first() {
                    Some(cmd) => cmd,
                    None => {
                        self.session.transaction_state.exec_abort = true;
                        self.apply_existing_client_reply_suppression_to_undispatched_reply();
                        return CommandError::InvalidCommandFrame.to_resp();
                    }
                };
                if !fr_command::is_known_command(cmd_bytes) {
                    self.session.transaction_state.exec_abort = true;
                    let cmd_str = std::str::from_utf8(cmd_bytes).unwrap_or("");
                    self.apply_existing_client_reply_suppression_to_undispatched_reply();
                    return CommandError::UnknownCommand {
                        command: fr_command::trim_and_cap_string(cmd_str, 128),
                        args_preview: build_unknown_args_preview(&argv),
                    }
                    .to_resp();
                }
                // Validate arity before queueing (Redis rejects wrong arity
                // immediately and sets EXECABORT).
                if let Err(cmd_name) = fr_command::check_command_arity(cmd_bytes, argv.len()) {
                    self.session.transaction_state.exec_abort = true;
                    self.apply_existing_client_reply_suppression_to_undispatched_reply();
                    return RespFrame::Error(format!(
                        "ERR wrong number of arguments for '{}' command",
                        cmd_name
                    ));
                }
                self.session.transaction_state.command_queue.push(argv);
                self.apply_existing_client_reply_suppression_to_undispatched_reply();
                return RespFrame::SimpleString("QUEUED".to_string());
            }
        }

        // Dispatch runtime-special commands that execute outside of MULTI,
        // or the subset that must execute immediately inside MULTI.
        // Like Redis's call(), we record slowlog for all dispatched commands.
        {
            let special_start = Instant::now();
            let special_reply = match special_command {
                Some(RuntimeSpecialCommand::Acl) => Some(self.handle_acl_command(&argv, now_ms)),
                Some(RuntimeSpecialCommand::Config) => Some(self.handle_config_command(&argv)),
                Some(RuntimeSpecialCommand::Client) => {
                    Some(self.handle_client_command(&argv, now_ms))
                }
                Some(RuntimeSpecialCommand::Role) => Some(self.handle_role_command(&argv)),
                Some(RuntimeSpecialCommand::Replconf) => {
                    Some(self.handle_replconf_command(&argv, now_ms))
                }
                Some(RuntimeSpecialCommand::Psync) | Some(RuntimeSpecialCommand::Sync) => {
                    Some(self.handle_psync_command(&argv))
                }
                Some(RuntimeSpecialCommand::Replicaof) | Some(RuntimeSpecialCommand::Slaveof) => {
                    Some(self.handle_replicaof_command(&argv))
                }
                Some(RuntimeSpecialCommand::Asking) => Some(self.handle_asking_command(&argv)),
                Some(RuntimeSpecialCommand::Readonly) => Some(self.handle_readonly_command(&argv)),
                Some(RuntimeSpecialCommand::Readwrite) => {
                    Some(self.handle_readwrite_command(&argv))
                }
                Some(RuntimeSpecialCommand::Cluster) => {
                    Some(self.handle_cluster_command(&argv, now_ms))
                }
                Some(RuntimeSpecialCommand::Wait) => Some(self.handle_wait_command(&argv)),
                Some(RuntimeSpecialCommand::Waitaof) => Some(self.handle_waitaof_command(&argv)),
                Some(RuntimeSpecialCommand::Multi) => Some(self.handle_multi_command(&argv)),
                Some(RuntimeSpecialCommand::Exec) => {
                    Some(self.handle_exec_command(&argv, now_ms, packet_id))
                }
                Some(RuntimeSpecialCommand::Discard) => Some(self.handle_discard_command(&argv)),
                Some(RuntimeSpecialCommand::Watch) => {
                    Some(self.handle_watch_command(&argv, now_ms))
                }
                Some(RuntimeSpecialCommand::Unwatch) => Some(self.handle_unwatch_command(&argv)),
                Some(RuntimeSpecialCommand::Quit) => {
                    Some(RespFrame::SimpleString("OK".to_string()))
                }
                Some(RuntimeSpecialCommand::Reset) => Some(self.handle_reset_command(&argv)),
                Some(RuntimeSpecialCommand::Slowlog) => Some(self.handle_slowlog_command(&argv)),
                Some(RuntimeSpecialCommand::Save) => Some(self.handle_save_command(&argv, now_ms)),
                Some(RuntimeSpecialCommand::Bgsave) => {
                    Some(self.handle_bgsave_command(&argv, now_ms))
                }
                Some(RuntimeSpecialCommand::Lastsave) => Some(self.handle_lastsave_command(&argv)),
                Some(RuntimeSpecialCommand::Bgrewriteaof) => {
                    Some(self.handle_bgrewriteaof_command(&argv, now_ms))
                }
                Some(RuntimeSpecialCommand::Failover) => Some(self.handle_failover_command(&argv)),
                Some(RuntimeSpecialCommand::Shutdown) => Some(self.handle_shutdown_command(&argv)),
                Some(RuntimeSpecialCommand::Pubsub) => Some(self.handle_pubsub_command(&argv)),
                Some(RuntimeSpecialCommand::Subscribe) => {
                    Some(self.handle_subscribe_command(&argv))
                }
                Some(RuntimeSpecialCommand::Unsubscribe) => {
                    Some(self.handle_unsubscribe_command(&argv))
                }
                Some(RuntimeSpecialCommand::Psubscribe) => {
                    Some(self.handle_psubscribe_command(&argv))
                }
                Some(RuntimeSpecialCommand::Punsubscribe) => {
                    Some(self.handle_punsubscribe_command(&argv))
                }
                Some(RuntimeSpecialCommand::Publish) => Some(self.handle_publish_command(&argv)),
                Some(RuntimeSpecialCommand::Ssubscribe) => {
                    Some(self.handle_ssubscribe_command(&argv))
                }
                Some(RuntimeSpecialCommand::Sunsubscribe) => {
                    Some(self.handle_sunsubscribe_command(&argv))
                }
                Some(RuntimeSpecialCommand::Spublish) => Some(self.handle_spublish_command(&argv)),
                Some(RuntimeSpecialCommand::Select) => Some(self.handle_select_command(&argv)),
                Some(RuntimeSpecialCommand::Swapdb) => Some(self.handle_swapdb_command(&argv)),
                _ => None,
            };
            if let Some(reply) = special_reply {
                let reply = match apply_client_reply_state(&argv, &mut self.session.client_reply) {
                    Ok(()) => reply,
                    Err(err) => err.to_resp(),
                };
                let elapsed_us = special_start.elapsed().as_micros() as u64;
                self.record_slowlog(&argv, elapsed_us, now_ms);
                self.server.record_latency_sample(&argv, elapsed_us, now_ms);
                self.server.record_command_histogram(&argv, elapsed_us);
                return reply;
            }
        }

        if let Some(reply) = self.enforce_maxmemory_before_dispatch(&argv, now_ms, packet_id) {
            self.apply_existing_client_reply_suppression_to_undispatched_reply();
            return reply;
        }

        // Command paths run a fast active-expire cycle to keep short-lived keys
        // from lingering between server ticks.
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
        self.server.record_latency_sample(&argv, elapsed_us, now_ms);
        self.server.record_command_histogram(&argv, elapsed_us);

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
                input_source: ThreatInputDigestSource::Argv(&argv),
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

    pub fn mark_client_blocked(&mut self, client_id: u64) {
        self.server.blocked_client_ids.insert(client_id);
        self.server.store.stat_blocked_clients = self.server.blocked_client_ids.len() as u64;
    }

    pub fn set_blocked_clients_count_for_info(&mut self, blocked_clients: usize) {
        self.server.store.stat_blocked_clients = blocked_clients as u64;
    }

    pub fn mark_client_unblocked(&mut self, client_id: u64) {
        self.server.blocked_client_ids.remove(&client_id);
        self.server.store.stat_blocked_clients = self.server.blocked_client_ids.len() as u64;
    }

    pub fn drain_pending_client_unblocks(&mut self) -> Vec<(u64, ClientUnblockMode)> {
        std::mem::take(&mut self.server.pending_client_unblocks)
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
        let parser_config = self.parser_config();

        match fr_protocol::parse_frame_with_config(input, &parser_config) {
            Ok(parsed) => {
                let argv_result = frame_to_argv(&parsed.frame);
                self.execute_frame_internal(parsed.frame, argv_result, now_ms, packet_id)
                    .to_bytes()
            }
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
                    input_source: ThreatInputDigestSource::Bytes(input),
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
    ) -> Option<RespFrame> {
        if self.server.maxmemory_bytes == 0 {
            self.server.last_eviction_loop = None;
            return None;
        }
        // Redis only enforces maxmemory on write-ish commands. Reads should not
        // trigger eviction loops or overwrite the last eviction result.
        if !Self::command_advances_replication_offset(argv) {
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
            input_source: ThreatInputDigestSource::Argv(argv),
            output: &reply,
        });
        Some(reply)
    }

    fn capture_aof_record(&mut self, argv: &[Vec<u8>]) {
        let is_select = argv
            .first()
            .is_some_and(|cmd| eq_ascii_token(cmd, b"SELECT"));
        if Runtime::command_advances_replication_offset(argv)
            && self.server.aof_selected_db != self.session.selected_db
        {
            if !is_select {
                self.server.capture_aof_record(&[
                    b"SELECT".to_vec(),
                    self.session.selected_db.to_string().into_bytes(),
                ]);
            }
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

    fn current_dispatch_client_context(&self, now_ms: u64) -> DispatchClientContext {
        self.dispatch_client_context_for_session(&self.session, now_ms)
    }

    fn dispatch_client_context_for_session(
        &self,
        session: &ClientSession,
        now_ms: u64,
    ) -> DispatchClientContext {
        let client_id = session.client_id;
        let is_pubsub = self.is_pubsub_client(client_id);
        let age_seconds = now_ms.saturating_sub(session.connected_at_ms) / 1000;
        let idle_seconds = now_ms.saturating_sub(session.last_interaction_ms) / 1000;
        DispatchClientContext {
            client_id,
            client_name: session.client_name.clone(),
            client_lib_name: session.client_lib_name.clone(),
            client_lib_ver: session.client_lib_ver.clone(),
            age_seconds,
            idle_seconds,
            db_index: session.selected_db,
            flags: if session.transaction_state.in_transaction {
                "x".to_string()
            } else {
                "N".to_string()
            },
            peer_addr: session
                .peer_addr
                .map(|addr| addr.to_string())
                .unwrap_or_else(|| "127.0.0.1:0".to_string()),
            authenticated_user: session.current_user_name().to_vec(),
            resp_protocol_version: session.resp_protocol_version,
            channel_subscriptions: self
                .server
                .pubsub_client_channels
                .get(&client_id)
                .map_or(0, HashSet::len),
            pattern_subscriptions: self
                .server
                .pubsub_client_patterns
                .get(&client_id)
                .map_or(0, HashSet::len),
            shard_subscriptions: self
                .server
                .pubsub_client_shard_channels
                .get(&client_id)
                .map_or(0, HashSet::len),
            multi_count: if session.transaction_state.in_transaction {
                session.transaction_state.command_queue.len() as i64
            } else {
                -1
            },
            watch_count: session.transaction_state.watched_keys.len(),
            is_pubsub,
            client_tracking: session.client_tracking.clone(),
            client_reply: session.client_reply.clone(),
            acl_permissions: self
                .server
                .auth_state
                .get_user(session.current_user_name())
                .map(AclUser::to_dispatch_acl_permissions),
        }
    }

    fn apply_existing_client_reply_suppression_to_undispatched_reply(&mut self) {
        let prior_off = self.session.client_reply.off;
        let prior_skip_next = self.session.client_reply.skip_next;
        self.session.client_reply.suppress_current_response = prior_off || prior_skip_next;
        self.session.client_reply.skip_next = false;
    }

    fn client_list_sessions(&self) -> BTreeMap<u64, ClientSession> {
        let mut sessions = self.server.client_sessions.clone();
        sessions.insert(self.session.client_id, self.session.clone());
        sessions
    }

    fn client_type_for_session(&self, session: &ClientSession) -> &'static str {
        if self.is_replica(session.client_id) {
            "replica"
        } else if self.is_pubsub_client(session.client_id) {
            "pubsub"
        } else {
            "normal"
        }
    }

    fn client_info_line_for_session(&self, session: &ClientSession, now_ms: u64) -> String {
        let name = session
            .client_name
            .as_ref()
            .map(|n| String::from_utf8_lossy(n).to_string())
            .unwrap_or_default();
        let peer = session
            .peer_addr
            .map(|a| a.to_string())
            .unwrap_or_else(|| "127.0.0.1:0".to_string());
        let age_seconds = now_ms.saturating_sub(session.connected_at_ms) / 1000;
        let idle_seconds = now_ms.saturating_sub(session.last_interaction_ms) / 1000;
        let channel_subs = self
            .server
            .pubsub_client_channels
            .get(&session.client_id)
            .map_or(0, HashSet::len);
        let pattern_subs = self
            .server
            .pubsub_client_patterns
            .get(&session.client_id)
            .map_or(0, HashSet::len);
        let shard_subs = self
            .server
            .pubsub_client_shard_channels
            .get(&session.client_id)
            .map_or(0, HashSet::len);
        let flags = if session.transaction_state.in_transaction {
            "x"
        } else {
            "N"
        };
        let multi_count = if session.transaction_state.in_transaction {
            session.transaction_state.command_queue.len() as i64
        } else {
            -1
        };
        let lib_name = session.client_lib_name.as_deref().unwrap_or("");
        let lib_ver = session.client_lib_ver.as_deref().unwrap_or("");
        format!(
            "id={} addr={} laddr=127.0.0.1:{} fd=0 name={} age={} idle={} db={} sub={} psub={} ssub={} multi={} watch={} qbuf=0 qbuf-free=0 obl=0 oll=0 omem=0 tot-mem=0 events=r cmd={} user={} lib-name={} lib-ver={} resp={} flags={}\r\n",
            session.client_id,
            peer,
            self.server.store.server_port,
            name,
            age_seconds,
            idle_seconds,
            session.selected_db,
            channel_subs,
            pattern_subs,
            shard_subs,
            multi_count,
            session.transaction_state.watched_keys.len(),
            session.last_command_name,
            String::from_utf8_lossy(session.current_user_name()),
            lib_name,
            lib_ver,
            session.resp_protocol_version,
            flags,
        )
    }

    fn current_acl_log_client_info(&self, now_ms: u64) -> String {
        self.client_info_line_for_session(&self.session, now_ms)
            .trim_end_matches("\r\n")
            .to_string()
    }

    fn current_acl_log_context(&self) -> &'static str {
        if self.server.store.script_nesting_level > 0 {
            "lua"
        } else if self.session.transaction_state.in_transaction
            || self.session.transaction_state.executing_exec
        {
            "multi"
        } else {
            "toplevel"
        }
    }

    fn record_acl_log_event(
        &mut self,
        reason: &'static str,
        object: String,
        username: Vec<u8>,
        now_ms: u64,
    ) {
        self.server.record_acl_log_event(
            reason,
            self.current_acl_log_context(),
            object,
            username,
            self.current_acl_log_client_info(now_ms),
            now_ms,
        );
    }

    fn sync_dispatch_client_context_to_session(&mut self) {
        self.session.client_name = self.server.store.dispatch_client_ctx.client_name.clone();
        self.session.client_lib_name = self
            .server
            .store
            .dispatch_client_ctx
            .client_lib_name
            .clone();
        self.session.client_lib_ver = self.server.store.dispatch_client_ctx.client_lib_ver.clone();
        self.session.client_tracking = self
            .server
            .store
            .dispatch_client_ctx
            .client_tracking
            .clone();
        self.session.client_reply = self.server.store.dispatch_client_ctx.client_reply.clone();
    }

    fn dispatch_with_client_context(
        &mut self,
        argv: &[Vec<u8>],
        now_ms: u64,
    ) -> Result<RespFrame, CommandError> {
        self.server.store.dispatch_client_ctx = self.current_dispatch_client_context(now_ms);
        let mut result = dispatch_argv(argv, &mut self.server.store, now_ms);
        self.sync_dispatch_client_context_to_session();
        if result.is_ok()
            && let Some(reply) = self.handle_deferred_store_runtime_action(now_ms)
        {
            result = Ok(reply);
        }
        result
    }

    fn handle_deferred_store_runtime_action(&mut self, now_ms: u64) -> Option<RespFrame> {
        for event in self.server.store.drain_pending_acl_log_events() {
            self.record_deferred_acl_log_event(event, now_ms);
        }
        if self.server.store.take_debug_reload_requested() {
            return Some(self.handle_debug_reload_requested(now_ms));
        }
        if self.server.store.take_bgrewriteaof_requested() {
            return Some(self.handle_bgrewriteaof_requested(now_ms));
        }
        None
    }

    fn record_deferred_acl_log_event(&mut self, event: PendingAclLogEvent, now_ms: u64) {
        let reason = match event.reason {
            DispatchAclPermissionReason::Command => "command",
            DispatchAclPermissionReason::Key => "key",
            DispatchAclPermissionReason::Channel => "channel",
        };
        let context = match event.context {
            DispatchAclLogContext::Lua => "lua",
        };
        self.server.record_acl_log_event(
            reason,
            context,
            event.object,
            event.username,
            self.current_acl_log_client_info(now_ms),
            now_ms,
        );
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
        if eq_ascii_token(command, b"CLIENT") {
            return Ok(self.handle_client_command(argv, now_ms));
        }
        let namespaced = self.namespace_argv_for_selected_db(argv);
        let mut reply = self.dispatch_with_client_context(&namespaced, now_ms)?;
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

    fn handle_auth_command(&mut self, argv: &[Vec<u8>], now_ms: u64) -> RespFrame {
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
            Err(AuthFailure::WrongPass) => {
                self.record_acl_log_event("auth", "AUTH".to_string(), username.to_vec(), now_ms);
                RespFrame::Error(WRONGPASS_ERROR.to_string())
            }
        }
    }

    fn client_name_is_valid(name: &[u8]) -> bool {
        name.iter().all(|&b| b > b' ')
    }

    fn handle_hello_command(&mut self, argv: &[Vec<u8>], now_ms: u64) -> RespFrame {
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
                if !Self::client_name_is_valid(name) {
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
                    self.record_acl_log_event(
                        "auth",
                        "AUTH".to_string(),
                        username.to_vec(),
                        now_ms,
                    );
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

    fn handle_acl_command(&mut self, argv: &[Vec<u8>], now_ms: u64) -> RespFrame {
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
            self.handle_acl_log(argv, now_ms)
        } else if sub.eq_ignore_ascii_case("SAVE") {
            self.handle_acl_save(argv)
        } else if sub.eq_ignore_ascii_case("LOAD") {
            self.handle_acl_load(argv)
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
                    let dryrun_argv = &argv[3..];
                    let cmd_name = String::from_utf8_lossy(&dryrun_argv[0]);
                    match u.acl_permission_error_for_argv(dryrun_argv) {
                        None => RespFrame::SimpleString("OK".to_string()),
                        Some(AclCommandPermissionError::Command) => {
                            self.record_acl_log_event(
                                "command",
                                cmd_name.to_ascii_uppercase(),
                                username.clone(),
                                now_ms,
                            );
                            RespFrame::Error(format!(
                                "ERR User '{}' has no permissions to run the '{}' command",
                                String::from_utf8_lossy(username),
                                cmd_name
                            ))
                        }
                        Some(AclCommandPermissionError::Key(key)) => {
                            let key_name = String::from_utf8_lossy(&key).into_owned();
                            self.record_acl_log_event(
                                "key",
                                key_name.clone(),
                                username.clone(),
                                now_ms,
                            );
                            RespFrame::Error(format!(
                                "ERR User '{}' has no permissions to access the '{}' key",
                                String::from_utf8_lossy(username),
                                key_name
                            ))
                        }
                        Some(AclCommandPermissionError::Channel(channel)) => {
                            let channel_name = String::from_utf8_lossy(&channel).into_owned();
                            self.record_acl_log_event(
                                "channel",
                                channel_name.clone(),
                                username.clone(),
                                now_ms,
                            );
                            RespFrame::Error(format!(
                                "ERR User '{}' has no permissions to access the '{}' channel",
                                String::from_utf8_lossy(username),
                                channel_name
                            ))
                        }
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
                .map(|n| RespFrame::BulkString(Some(n.to_ascii_lowercase())))
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
            Ok(()) => {
                self.queue_acl_pubsub_revocations_for_user(&argv[2]);
                RespFrame::SimpleString("OK".to_string())
            }
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
                self.queue_acl_deleted_user_disconnects(username);
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
        let mut flags = Vec::new();
        if user.enabled {
            flags.push(RespFrame::BulkString(Some(b"on".to_vec())));
        } else {
            flags.push(RespFrame::BulkString(Some(b"off".to_vec())));
        }
        if user.nopass {
            flags.push(RespFrame::BulkString(Some(b"nopass".to_vec())));
        }
        flags.push(RespFrame::BulkString(Some(
            user.sanitize_payload_flag().as_bytes().to_vec(),
        )));

        let mut passwords = Vec::new();
        for p in &user.passwords {
            passwords.push(RespFrame::BulkString(Some(p.clone())));
        }

        let commands_str = user.commands_string();
        let keys_str = user.keys_string();
        let channels_str = user.channels_string();

        RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"flags".to_vec())),
            RespFrame::Array(Some(flags)),
            RespFrame::BulkString(Some(b"passwords".to_vec())),
            RespFrame::Array(Some(passwords)),
            RespFrame::BulkString(Some(b"commands".to_vec())),
            RespFrame::BulkString(Some(commands_str.into_bytes())),
            RespFrame::BulkString(Some(b"keys".to_vec())),
            RespFrame::BulkString(Some(keys_str.into_bytes())),
            RespFrame::BulkString(Some(b"channels".to_vec())),
            RespFrame::BulkString(Some(channels_str.as_bytes().to_vec())),
            RespFrame::BulkString(Some(b"selectors".to_vec())),
            RespFrame::Array(Some(Vec::new())),
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

    fn acl_log_entry_frame(&self, entry: &AclLogEntry, now_ms: u64) -> RespFrame {
        RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"count".to_vec())),
            RespFrame::Integer(entry.count as i64),
            RespFrame::BulkString(Some(b"reason".to_vec())),
            RespFrame::BulkString(Some(entry.reason.as_bytes().to_vec())),
            RespFrame::BulkString(Some(b"context".to_vec())),
            RespFrame::BulkString(Some(entry.context.as_bytes().to_vec())),
            RespFrame::BulkString(Some(b"object".to_vec())),
            RespFrame::BulkString(Some(entry.object.as_bytes().to_vec())),
            RespFrame::BulkString(Some(b"username".to_vec())),
            RespFrame::BulkString(Some(entry.username.clone())),
            RespFrame::BulkString(Some(b"age-seconds".to_vec())),
            RespFrame::BulkString(Some(
                acl_log_age_seconds(now_ms, entry.updated_at_ms).into_bytes(),
            )),
            RespFrame::BulkString(Some(b"client-info".to_vec())),
            RespFrame::BulkString(Some(entry.client_info.as_bytes().to_vec())),
            RespFrame::BulkString(Some(b"entry-id".to_vec())),
            RespFrame::Integer(entry.entry_id as i64),
            RespFrame::BulkString(Some(b"timestamp-created".to_vec())),
            RespFrame::Integer(entry.created_at_ms as i64),
            RespFrame::BulkString(Some(b"timestamp-last-updated".to_vec())),
            RespFrame::Integer(entry.updated_at_ms as i64),
        ]))
    }

    fn acl_log_entries_response(&self, count: Option<usize>, now_ms: u64) -> RespFrame {
        let entries = self
            .server
            .acl_log
            .iter()
            .take(count.unwrap_or(usize::MAX))
            .map(|entry| self.acl_log_entry_frame(entry, now_ms))
            .collect();
        RespFrame::Array(Some(entries))
    }

    fn handle_acl_log(&mut self, argv: &[Vec<u8>], now_ms: u64) -> RespFrame {
        if argv.len() == 2 {
            self.acl_log_entries_response(None, now_ms)
        } else if argv.len() == 3 {
            if eq_ascii_token(&argv[2], b"RESET") {
                self.server.acl_log.clear();
                self.server.next_acl_log_entry_id = 0;
                RespFrame::SimpleString("OK".to_string())
            } else {
                let count = match parse_i64_arg(&argv[2]) {
                    Ok(c) if c >= 0 => c as usize,
                    _ => return CommandError::InvalidInteger.to_resp(),
                };
                self.acl_log_entries_response(Some(count), now_ms)
            }
        } else {
            CommandError::WrongSubcommandArity {
                command: "ACL",
                subcommand: "LOG".to_string(),
            }
            .to_resp()
        }
    }

    fn handle_acl_save(&self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 2 {
            return CommandError::WrongSubcommandArity {
                command: "ACL",
                subcommand: "SAVE".to_string(),
            }
            .to_resp();
        }
        let Some(path) = &self.server.acl_file_path else {
            return RespFrame::Error("ERR There is no configured ACL file".to_string());
        };
        let content = self.server.auth_state.serialize_acl_rules();
        let tmp_path = path.with_extension("tmp");
        match (|| -> std::io::Result<()> {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)?;
            }
            let mut file = std::fs::File::create(&tmp_path)?;
            std::io::Write::write_all(&mut file, content.as_bytes())?;
            std::io::Write::write_all(&mut file, b"\n")?;
            std::io::Write::flush(&mut file)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&tmp_path, path)?;
            Ok(())
        })() {
            Ok(()) => RespFrame::SimpleString("OK".to_string()),
            Err(err) => RespFrame::Error(format!("ERR {err}")),
        }
    }

    fn handle_acl_load(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 2 {
            return CommandError::WrongSubcommandArity {
                command: "ACL",
                subcommand: "LOAD".to_string(),
            }
            .to_resp();
        }
        let Some(path) = &self.server.acl_file_path else {
            return RespFrame::Error("ERR There is no configured ACL file".to_string());
        };
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(err) => return RespFrame::Error(format!("ERR {err}")),
        };
        let previous = self.server.auth_state.clone();
        match self.server.auth_state.load_acl_rules(&content) {
            Ok(()) => {
                self.session
                    .refresh_authentication_for_server(&self.server.auth_state, true);
                self.queue_acl_pubsub_revocations_for_all_users();
                RespFrame::SimpleString("OK".to_string())
            }
            Err(err) => {
                self.server.auth_state = previous;
                RespFrame::Error(err)
            }
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
            hello_bulk("DRYRUN <username> <command> [<arg> ...]"),
            hello_bulk("    Test if a command would be allowed for the given user."),
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
                return config_wrong_subcommand_arity("RESETSTAT");
            }
            self.server.store.reset_info_stats();
            return RespFrame::SimpleString("OK".to_string());
        }
        if sub.eq_ignore_ascii_case("REWRITE") {
            if argv.len() != 2 {
                return config_wrong_subcommand_arity("REWRITE");
            }
            let Some(path) = &self.server.config_file_path else {
                return RespFrame::Error(
                    "ERR The server is running without a config file".to_string(),
                );
            };

            let mut lines = Vec::new();
            // We'll just write all parameters that are in SUPPORTED_CONFIG_PARAMETERS
            // but use the live values from ServerState/Store when possible.

            // First, get all current values via CONFIG GET *
            let current_config =
                self.handle_config_get(&[b"CONFIG".to_vec(), b"GET".to_vec(), b"*".to_vec()]);
            if let RespFrame::Array(Some(pairs)) = current_config {
                for chunk in pairs.chunks(2) {
                    if let (RespFrame::BulkString(Some(k)), RespFrame::BulkString(Some(v))) =
                        (&chunk[0], &chunk[1])
                    {
                        let key = String::from_utf8_lossy(k);
                        let val = String::from_utf8_lossy(v);
                        // Some parameters shouldn't be in the config file or need special handling
                        if key == "appendonly"
                            || key == "dir"
                            || key == "dbfilename"
                            || key == "aclfile"
                            || key == "save"
                        {
                            // these are usually fine
                        }
                        lines.push(format!("{key} {val}"));
                    }
                }
            }

            let content = lines.join("\n") + "\n";
            let tmp_path = path.with_extension("tmp");
            match (|| -> std::io::Result<()> {
                use std::io::Write;
                let mut file = std::fs::File::create(&tmp_path)?;
                file.write_all(content.as_bytes())?;
                file.sync_all()?;
                drop(file);
                std::fs::rename(&tmp_path, path)?;
                Ok(())
            })() {
                Ok(()) => RespFrame::SimpleString("OK".to_string()),
                Err(err) => RespFrame::Error(format!("ERR Rewriting config file: {err}")),
            }
        } else {
            CommandError::UnknownSubcommand {
                command: "CONFIG",
                subcommand: sub.to_string(),
            }
            .to_resp()
        }
    }

    fn handle_config_help(&self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 2 {
            return config_wrong_subcommand_arity("HELP");
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
            return config_wrong_subcommand_arity("GET");
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
        if pattern == "maxmemory*" {
            entries.push(RespFrame::BulkString(Some(
                b"maxmemory-eviction-tenacity".to_vec(),
            )));
            entries.push(RespFrame::BulkString(Some(b"10".to_vec())));
            entries.push(RespFrame::BulkString(Some(b"maxmemory-clients".to_vec())));
            entries.push(RespFrame::BulkString(Some(b"0".to_vec())));
            entries.push(RespFrame::BulkString(Some(b"maxmemory-policy".to_vec())));
            entries.push(RespFrame::BulkString(Some(
                self.server
                    .store
                    .maxmemory_policy
                    .as_config_str()
                    .as_bytes()
                    .to_vec(),
            )));
            entries.push(RespFrame::BulkString(Some(b"maxmemory-samples".to_vec())));
            entries.push(RespFrame::BulkString(Some(
                self.server
                    .maxmemory_eviction_sample_limit
                    .to_string()
                    .into_bytes(),
            )));
            entries.push(RespFrame::BulkString(Some(b"maxmemory".to_vec())));
            entries.push(RespFrame::BulkString(Some(
                self.server.maxmemory_bytes.to_string().into_bytes(),
            )));
            return;
        }
        if pattern == "lazyfree*" {
            for key in [
                "lazyfree-lazy-user-flush",
                "lazyfree-lazy-expire",
                "lazyfree-lazy-eviction",
                "lazyfree-lazy-server-del",
                "lazyfree-lazy-user-del",
            ] {
                entries.push(RespFrame::BulkString(Some(key.as_bytes().to_vec())));
                entries.push(RespFrame::BulkString(Some(b"no".to_vec())));
            }
            return;
        }
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
        if Self::config_pattern_matches(pattern, "masterauth") {
            entries.push(RespFrame::BulkString(Some(b"masterauth".to_vec())));
            entries.push(RespFrame::BulkString(Some(
                self.server.masterauth.clone().unwrap_or_default(),
            )));
        }
        if Self::config_pattern_matches(pattern, "masteruser") {
            entries.push(RespFrame::BulkString(Some(b"masteruser".to_vec())));
            entries.push(RespFrame::BulkString(Some(
                self.server.masteruser.clone().unwrap_or_default(),
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
                    .store
                    .slowlog_log_slower_than_us
                    .to_string()
                    .into_bytes(),
            )));
        }
        if Self::config_pattern_matches(pattern, "slowlog-max-len") {
            entries.push(RespFrame::BulkString(Some(b"slowlog-max-len".to_vec())));
            entries.push(RespFrame::BulkString(Some(
                self.server.store.slowlog_max_len.to_string().into_bytes(),
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
        if Self::config_pattern_matches(pattern, "replica-serve-stale-data")
            || Self::config_pattern_matches(pattern, "slave-serve-stale-data")
        {
            let key = if Self::config_pattern_matches(pattern, "slave-serve-stale-data") {
                b"slave-serve-stale-data".as_slice()
            } else {
                b"replica-serve-stale-data".as_slice()
            };
            entries.push(RespFrame::BulkString(Some(key.to_vec())));
            entries.push(RespFrame::BulkString(Some(
                if self.server.replica_serve_stale_data {
                    b"yes".to_vec()
                } else {
                    b"no".to_vec()
                },
            )));
        }
        if Self::config_pattern_matches(pattern, "replica-priority")
            || Self::config_pattern_matches(pattern, "slave-priority")
        {
            let key = if Self::config_pattern_matches(pattern, "slave-priority") {
                b"slave-priority".as_slice()
            } else {
                b"replica-priority".as_slice()
            };
            entries.push(RespFrame::BulkString(Some(key.to_vec())));
            entries.push(RespFrame::BulkString(Some(
                self.server.replica_priority.to_string().into_bytes(),
            )));
        }
        if Self::config_pattern_matches(pattern, "repl-diskless-sync") {
            entries.push(RespFrame::BulkString(Some(b"repl-diskless-sync".to_vec())));
            entries.push(RespFrame::BulkString(Some(
                if self.server.repl_diskless_sync {
                    b"yes".to_vec()
                } else {
                    b"no".to_vec()
                },
            )));
        }
        if Self::config_pattern_matches(pattern, "repl-diskless-sync-delay") {
            entries.push(RespFrame::BulkString(Some(
                b"repl-diskless-sync-delay".to_vec(),
            )));
            entries.push(RespFrame::BulkString(Some(
                self.server
                    .repl_diskless_sync_delay_sec
                    .to_string()
                    .into_bytes(),
            )));
        }
        if Self::config_pattern_matches(pattern, "cluster-allow-reads-when-down") {
            entries.push(RespFrame::BulkString(Some(
                b"cluster-allow-reads-when-down".to_vec(),
            )));
            entries.push(RespFrame::BulkString(Some(
                if self.server.cluster_allow_reads_when_down {
                    b"yes".to_vec()
                } else {
                    b"no".to_vec()
                },
            )));
        }
        if Self::config_pattern_matches(pattern, "cluster-allow-pubsubshard-when-down") {
            entries.push(RespFrame::BulkString(Some(
                b"cluster-allow-pubsubshard-when-down".to_vec(),
            )));
            entries.push(RespFrame::BulkString(Some(
                if self.server.cluster_allow_pubsubshard_when_down {
                    b"yes".to_vec()
                } else {
                    b"no".to_vec()
                },
            )));
        }
        if Self::config_pattern_matches(pattern, "cluster-link-sendbuf-limit") {
            entries.push(RespFrame::BulkString(Some(
                b"cluster-link-sendbuf-limit".to_vec(),
            )));
            entries.push(RespFrame::BulkString(Some(
                self.server
                    .cluster_link_sendbuf_limit
                    .to_string()
                    .into_bytes(),
            )));
        }
        if Self::config_pattern_matches(pattern, "cluster-node-timeout") {
            entries.push(RespFrame::BulkString(Some(
                b"cluster-node-timeout".to_vec(),
            )));
            entries.push(RespFrame::BulkString(Some(
                self.server.cluster_node_timeout.to_string().into_bytes(),
            )));
        }
        if Self::config_pattern_matches(pattern, "cluster-migration-barrier") {
            entries.push(RespFrame::BulkString(Some(
                b"cluster-migration-barrier".to_vec(),
            )));
            entries.push(RespFrame::BulkString(Some(
                self.server
                    .cluster_migration_barrier
                    .to_string()
                    .into_bytes(),
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
        if Self::config_pattern_matches(pattern, "stop-writes-on-bgsave-error") {
            entries.push(RespFrame::BulkString(Some(
                b"stop-writes-on-bgsave-error".to_vec(),
            )));
            entries.push(RespFrame::BulkString(Some(
                if self.server.stop_writes_on_bgsave_error {
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
        if Self::config_pattern_matches(pattern, "aclfile") {
            entries.push(RespFrame::BulkString(Some(b"aclfile".to_vec())));
            let path = self
                .server
                .acl_file_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default();
            entries.push(RespFrame::BulkString(Some(path.into_bytes())));
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
            if name == "masterauth"
                || name == "masteruser"
                || name == "maxmemory"
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
                || name == "stop-writes-on-bgsave-error"
                || name == "appendfilename"
                || name == "appenddirname"
                || name == "dbfilename"
                || name == "dir"
                || name == "aclfile"
                || name == "maxclients"
                || name == "busy-reply-threshold"
                || name == "lua-time-limit"
                || name == "maxmemory-samples"
                || name == "repl-backlog-size"
                || name == "repl-timeout"
                || name == "replica-serve-stale-data"
                || name == "replica-priority"
                || name == "slave-priority"
                || name == "repl-diskless-sync"
                || name == "repl-diskless-sync-delay"
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
            return config_wrong_subcommand_arity("SET");
        }

        let mut next_requirepass: Option<Option<Vec<u8>>> = None;
        let mut next_masterauth: Option<Option<Vec<u8>>> = None;
        let mut next_masteruser: Option<Option<Vec<u8>>> = None;
        let mut next_acllog_max_len = self.server.acllog_max_len;
        let mut next_maxmemory: Option<usize> = None;
        let mut next_maxmemory_policy: Option<MaxmemoryPolicy> = None;
        let mut next_lfu_decay_time: Option<u64> = None;
        let mut next_slowlog_slower_than: Option<i64> = None;
        let mut next_slowlog_max_len: Option<usize> = None;
        let mut next_latency_monitor_threshold: Option<u64> = None;
        let mut next_latency_tracking: Option<bool> = None;
        let mut next_latency_percentiles: Option<Vec<f64>> = None;
        let mut next_hz: Option<u64> = None;
        let mut next_maxclients: Option<usize> = None;
        let mut next_client_timeout_sec: Option<u64> = None;
        let mut next_repl_backlog_size: Option<u64> = None;
        let mut next_repl_timeout: Option<u64> = None;
        let mut next_replica_serve_stale_data: Option<bool> = None;
        let mut next_replica_priority: Option<usize> = None;
        let mut next_repl_diskless_sync: Option<bool> = None;
        let mut next_repl_diskless_sync_delay: Option<u64> = None;
        let mut next_cluster_allow_reads_when_down: Option<bool> = None;
        let mut next_cluster_allow_pubsubshard_when_down: Option<bool> = None;
        let mut next_cluster_link_sendbuf_limit: Option<u64> = None;
        let mut next_cluster_node_timeout: Option<u64> = None;
        let mut next_cluster_migration_barrier: Option<u64> = None;
        let mut next_min_replicas_to_write: Option<usize> = None;
        let mut next_min_replicas_max_lag: Option<u64> = None;
        let mut next_stop_writes_on_bgsave_error: Option<bool> = None;
        let mut next_query_buffer_limit: Option<usize> = None;
        let mut next_proto_max_bulk_len: Option<usize> = None;
        let mut next_output_buffer_limit: Option<usize> = None;
        let mut next_maxmemory_samples: Option<usize> = None;
        let mut next_command_time_budget: Option<u64> = None;
        let mut next_appendonly: Option<bool> = None;
        let mut next_appendfsync: Option<AppendFsyncMode> = None;
        let mut next_acl_pubsub_default: Option<AclPubsubDefault> = None;
        let mut next_keyspace_events: Option<u32> = None;
        let mut next_list_max_listpack_size: Option<i64> = None;
        let mut next_rdb_path = self
            .server
            .rdb_path
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from(".").join("dump.rdb"));
        let mut next_acl_file_path = self.server.acl_file_path.clone();
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
            if parameter.eq_ignore_ascii_case("masterauth") {
                next_masterauth = Some(if pair[1].is_empty() {
                    None
                } else {
                    Some(pair[1].clone())
                });
                continue;
            }
            if parameter.eq_ignore_ascii_case("masteruser") {
                next_masteruser = Some(if pair[1].is_empty() {
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
            if parameter.eq_ignore_ascii_case("lfu-decay-time") {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) => value,
                    Err(_) => {
                        return RespFrame::Error(
                            "ERR CONFIG SET failed (possibly related to argument 'lfu-decay-time') - argument couldn't be parsed into an integer"
                                .to_string(),
                        );
                    }
                };
                if parsed < 0 {
                    return RespFrame::Error(
                        "ERR CONFIG SET failed (possibly related to argument 'lfu-decay-time') - argument must be between 0 and 9223372036854775807 inclusive"
                            .to_string(),
                    );
                }
                next_lfu_decay_time = Some(parsed as u64);
                static_override_updates.push(("lfu-decay-time".to_string(), parsed.to_string()));
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
            if parameter.eq_ignore_ascii_case("latency-tracking") {
                let parsed = match std::str::from_utf8(&pair[1]) {
                    Ok(s) if s.eq_ignore_ascii_case("yes") => true,
                    Ok(s) if s.eq_ignore_ascii_case("no") => false,
                    _ => {
                        return RespFrame::Error(
                            "ERR Invalid argument for CONFIG SET 'latency-tracking'".to_string(),
                        );
                    }
                };
                next_latency_tracking = Some(parsed);
                static_override_updates.push((
                    "latency-tracking".to_string(),
                    if parsed {
                        "yes".to_string()
                    } else {
                        "no".to_string()
                    },
                ));
                continue;
            }
            if parameter.eq_ignore_ascii_case("latency-tracking-info-percentiles") {
                let val_str = match std::str::from_utf8(&pair[1]) {
                    Ok(v) => v,
                    Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
                };
                let mut ps = Vec::new();
                for part in val_str.split_whitespace() {
                    let p = match part.parse::<f64>() {
                        Ok(v) if (0.0..=100.0).contains(&v) => v,
                        _ => {
                            return RespFrame::Error(
                                "ERR Invalid argument for CONFIG SET 'latency-tracking-info-percentiles'".to_string(),
                            );
                        }
                    };
                    ps.push(p);
                }
                next_latency_percentiles = Some(ps);
                static_override_updates.push((
                    "latency-tracking-info-percentiles".to_string(),
                    val_str.to_string(),
                ));
                continue;
            }
            if parameter.eq_ignore_ascii_case("latency-monitor-threshold") {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) if value >= 0 => value as u64,
                    Ok(_) => {
                        return RespFrame::Error(
                            "ERR Invalid argument for CONFIG SET 'latency-monitor-threshold'"
                                .to_string(),
                        );
                    }
                    Err(err) => return err.to_resp(),
                };
                next_latency_monitor_threshold = Some(parsed);
                static_override_updates
                    .push(("latency-monitor-threshold".to_string(), parsed.to_string()));
                continue;
            }
            if parameter.eq_ignore_ascii_case("hz") {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) => value,
                    Err(_) => {
                        return RespFrame::Error(
                            "ERR CONFIG SET failed (possibly related to argument 'hz') - argument couldn't be parsed into an integer"
                                .to_string(),
                        );
                    }
                };
                if !(0..=i64::from(i32::MAX)).contains(&parsed) {
                    return RespFrame::Error(
                        "ERR CONFIG SET failed (possibly related to argument 'hz') - argument must be between 0 and 2147483647 inclusive"
                            .to_string(),
                    );
                }
                next_hz = Some(parsed.clamp(1, 500) as u64);
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
            if parameter.eq_ignore_ascii_case("timeout") {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) if value >= 0 => value as u64,
                    Ok(_) => {
                        return RespFrame::Error(
                            "ERR Invalid argument for CONFIG SET 'timeout'".to_string(),
                        );
                    }
                    Err(err) => return err.to_resp(),
                };
                next_client_timeout_sec = Some(parsed);
                static_override_updates.push(("timeout".to_string(), parsed.to_string()));
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
            if parameter.eq_ignore_ascii_case("replica-serve-stale-data")
                || parameter.eq_ignore_ascii_case("slave-serve-stale-data")
            {
                let parsed = match std::str::from_utf8(&pair[1]) {
                    Ok(s) if s.eq_ignore_ascii_case("yes") => true,
                    Ok(s) if s.eq_ignore_ascii_case("no") => false,
                    _ => {
                        return RespFrame::Error(format!(
                            "ERR Invalid argument for CONFIG SET '{parameter}'"
                        ));
                    }
                };
                next_replica_serve_stale_data = Some(parsed);
                static_override_updates.push((
                    "replica-serve-stale-data".to_string(),
                    if parsed {
                        "yes".to_string()
                    } else {
                        "no".to_string()
                    },
                ));
                continue;
            }
            if parameter.eq_ignore_ascii_case("replica-priority")
                || parameter.eq_ignore_ascii_case("slave-priority")
            {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) if value >= 0 => value as usize,
                    Ok(_) => {
                        return RespFrame::Error(format!(
                            "ERR Invalid argument for CONFIG SET '{parameter}'"
                        ));
                    }
                    Err(err) => return err.to_resp(),
                };
                next_replica_priority = Some(parsed);
                continue;
            }
            if parameter.eq_ignore_ascii_case("repl-diskless-sync") {
                let parsed = match std::str::from_utf8(&pair[1]) {
                    Ok(s) if s.eq_ignore_ascii_case("yes") => true,
                    Ok(s) if s.eq_ignore_ascii_case("no") => false,
                    _ => {
                        return RespFrame::Error(
                            "ERR Invalid argument for CONFIG SET 'repl-diskless-sync'".to_string(),
                        );
                    }
                };
                next_repl_diskless_sync = Some(parsed);
                static_override_updates.push((
                    "repl-diskless-sync".to_string(),
                    if parsed {
                        "yes".to_string()
                    } else {
                        "no".to_string()
                    },
                ));
                continue;
            }
            if parameter.eq_ignore_ascii_case("repl-diskless-sync-delay") {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) if value >= 0 => value as u64,
                    Ok(_) => {
                        return RespFrame::Error(
                            "ERR Invalid argument for CONFIG SET 'repl-diskless-sync-delay'"
                                .to_string(),
                        );
                    }
                    Err(err) => return err.to_resp(),
                };
                next_repl_diskless_sync_delay = Some(parsed);
                static_override_updates
                    .push(("repl-diskless-sync-delay".to_string(), parsed.to_string()));
                continue;
            }
            if parameter.eq_ignore_ascii_case("min-replicas-to-write") {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) if value >= 0 => value as usize,
                    Ok(_) => {
                        return RespFrame::Error(
                            "ERR Invalid argument for CONFIG SET 'min-replicas-to-write'"
                                .to_string(),
                        );
                    }
                    Err(err) => return err.to_resp(),
                };
                next_min_replicas_to_write = Some(parsed);
                static_override_updates
                    .push(("min-replicas-to-write".to_string(), parsed.to_string()));
                continue;
            }
            if parameter.eq_ignore_ascii_case("min-replicas-max-lag") {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) if value >= 0 => value as u64,
                    Ok(_) => {
                        return RespFrame::Error(
                            "ERR Invalid argument for CONFIG SET 'min-replicas-max-lag'"
                                .to_string(),
                        );
                    }
                    Err(err) => return err.to_resp(),
                };
                next_min_replicas_max_lag = Some(parsed);
                static_override_updates
                    .push(("min-replicas-max-lag".to_string(), parsed.to_string()));
                continue;
            }
            if parameter.eq_ignore_ascii_case("stop-writes-on-bgsave-error") {
                let parsed = match std::str::from_utf8(&pair[1]) {
                    Ok(s) if s.eq_ignore_ascii_case("yes") => true,
                    Ok(s) if s.eq_ignore_ascii_case("no") => false,
                    _ => {
                        return RespFrame::Error(
                            "ERR Invalid argument for CONFIG SET 'stop-writes-on-bgsave-error'"
                                .to_string(),
                        );
                    }
                };
                next_stop_writes_on_bgsave_error = Some(parsed);
                static_override_updates.push((
                    "stop-writes-on-bgsave-error".to_string(),
                    if parsed {
                        "yes".to_string()
                    } else {
                        "no".to_string()
                    },
                ));
                continue;
            }
            if parameter.eq_ignore_ascii_case("cluster-allow-reads-when-down") {
                let parsed = match std::str::from_utf8(&pair[1]) {
                    Ok(s) if s.eq_ignore_ascii_case("yes") => true,
                    Ok(s) if s.eq_ignore_ascii_case("no") => false,
                    _ => {
                        return RespFrame::Error(
                            "ERR Invalid argument for CONFIG SET 'cluster-allow-reads-when-down'"
                                .to_string(),
                        );
                    }
                };
                next_cluster_allow_reads_when_down = Some(parsed);
                static_override_updates.push((
                    "cluster-allow-reads-when-down".to_string(),
                    if parsed {
                        "yes".to_string()
                    } else {
                        "no".to_string()
                    },
                ));
                continue;
            }
            if parameter.eq_ignore_ascii_case("cluster-allow-pubsubshard-when-down") {
                let parsed = match std::str::from_utf8(&pair[1]) {
                    Ok(s) if s.eq_ignore_ascii_case("yes") => true,
                    Ok(s) if s.eq_ignore_ascii_case("no") => false,
                    _ => {
                        return RespFrame::Error(
                            "ERR Invalid argument for CONFIG SET 'cluster-allow-pubsubshard-when-down'".to_string(),
                        );
                    }
                };
                next_cluster_allow_pubsubshard_when_down = Some(parsed);
                static_override_updates.push((
                    "cluster-allow-pubsubshard-when-down".to_string(),
                    if parsed {
                        "yes".to_string()
                    } else {
                        "no".to_string()
                    },
                ));
                continue;
            }
            if parameter.eq_ignore_ascii_case("cluster-link-sendbuf-limit") {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) if value >= 0 => value as u64,
                    Ok(_) => {
                        return RespFrame::Error(
                            "ERR Invalid argument for CONFIG SET 'cluster-link-sendbuf-limit'"
                                .to_string(),
                        );
                    }
                    Err(err) => return err.to_resp(),
                };
                next_cluster_link_sendbuf_limit = Some(parsed);
                static_override_updates
                    .push(("cluster-link-sendbuf-limit".to_string(), parsed.to_string()));
                continue;
            }
            if parameter.eq_ignore_ascii_case("cluster-node-timeout") {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) if value >= 0 => value as u64,
                    Ok(_) => {
                        return RespFrame::Error(
                            "ERR Invalid argument for CONFIG SET 'cluster-node-timeout'"
                                .to_string(),
                        );
                    }
                    Err(err) => return err.to_resp(),
                };
                next_cluster_node_timeout = Some(parsed);
                static_override_updates
                    .push(("cluster-node-timeout".to_string(), parsed.to_string()));
                continue;
            }
            if parameter.eq_ignore_ascii_case("cluster-migration-barrier") {
                let parsed = match parse_i64_arg(&pair[1]) {
                    Ok(value) if value >= 0 => value as u64,
                    Ok(_) => {
                        return RespFrame::Error(
                            "ERR Invalid argument for CONFIG SET 'cluster-migration-barrier'"
                                .to_string(),
                        );
                    }
                    Err(err) => return err.to_resp(),
                };
                next_cluster_migration_barrier = Some(parsed);
                static_override_updates
                    .push(("cluster-migration-barrier".to_string(), parsed.to_string()));
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
            if parameter.eq_ignore_ascii_case("appendfsync") {
                let value_str = match std::str::from_utf8(&pair[1]) {
                    Ok(value) => value,
                    Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
                };
                let mode = if value_str.eq_ignore_ascii_case("no") {
                    AppendFsyncMode::No
                } else if value_str.eq_ignore_ascii_case("everysec") {
                    AppendFsyncMode::Everysec
                } else if value_str.eq_ignore_ascii_case("always") {
                    AppendFsyncMode::Always
                } else {
                    return RespFrame::Error(format!(
                        "ERR Invalid argument '{value_str}' for CONFIG SET 'appendfsync'"
                    ));
                };
                next_appendfsync = Some(mode);
                static_override_updates.push((
                    "appendfsync".to_string(),
                    mode.as_config_value().to_string(),
                ));
                continue;
            }
            if parameter.eq_ignore_ascii_case("acl-pubsub-default") {
                let value_str = match std::str::from_utf8(&pair[1]) {
                    Ok(value) => value,
                    Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
                };
                let mode = if value_str.eq_ignore_ascii_case("allchannels") {
                    AclPubsubDefault::AllChannels
                } else if value_str.eq_ignore_ascii_case("resetchannels") {
                    AclPubsubDefault::ResetChannels
                } else {
                    return RespFrame::Error(format!(
                        "ERR Invalid argument '{value_str}' for CONFIG SET 'acl-pubsub-default'"
                    ));
                };
                next_acl_pubsub_default = Some(mode);
                static_override_updates.push((
                    "acl-pubsub-default".to_string(),
                    mode.as_config_value().to_string(),
                ));
                continue;
            }
            if parameter.eq_ignore_ascii_case("appendfilename")
                || parameter.eq_ignore_ascii_case("appenddirname")
                || parameter.eq_ignore_ascii_case("always-show-logo")
                || parameter.eq_ignore_ascii_case("bind")
                || parameter.eq_ignore_ascii_case("cluster-config-file")
                || parameter.eq_ignore_ascii_case("cluster-enabled")
                || parameter.eq_ignore_ascii_case("databases")
                || parameter.eq_ignore_ascii_case("daemonize")
                || parameter.eq_ignore_ascii_case("enable-protected-configs")
                || parameter.eq_ignore_ascii_case("enable-debug-command")
                || parameter.eq_ignore_ascii_case("enable-module-command")
                || parameter.eq_ignore_ascii_case("io-threads")
                || parameter.eq_ignore_ascii_case("io-threads-do-reads")
                || parameter.eq_ignore_ascii_case("logfile")
                || parameter.eq_ignore_ascii_case("pidfile")
                || parameter.eq_ignore_ascii_case("port")
                || parameter.eq_ignore_ascii_case("rdbchecksum")
                || parameter.eq_ignore_ascii_case("set-proc-title")
                || parameter.eq_ignore_ascii_case("tcp-backlog")
                || parameter.eq_ignore_ascii_case("unixsocket")
                || parameter.eq_ignore_ascii_case("unixsocketperm")
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
            if parameter.eq_ignore_ascii_case("aclfile") {
                let value_str = match std::str::from_utf8(&pair[1]) {
                    Ok(value) => value,
                    Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
                };
                next_acl_file_path = if value_str.is_empty() {
                    None
                } else {
                    Some(std::path::PathBuf::from(value_str))
                };
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
            if let Some(canonical) = canonical_static_config_param(parameter) {
                let value = String::from_utf8_lossy(&pair[1]).to_string();
                static_override_updates.push((canonical.to_string(), value));
                continue;
            }
            return RespFrame::Error(format!("ERR Unsupported CONFIG parameter '{parameter}'"));
        }

        if let Some(requirepass) = next_requirepass {
            // CONFIG SET requirepass should bridge ACL defaults without dropping this session.
            self.apply_requirepass_update(requirepass, true);
        }
        if let Some(masteruser) = next_masteruser {
            self.apply_masteruser_update(masteruser);
        }
        if let Some(masterauth) = next_masterauth {
            self.apply_masterauth_update(masterauth);
        }
        self.server.acllog_max_len = next_acllog_max_len;
        if let Some(maxmemory) = next_maxmemory {
            self.server.maxmemory_bytes = maxmemory;
            self.server.store.maxmemory_bytes_live = maxmemory;
        }
        if let Some(maxmemory_policy) = next_maxmemory_policy {
            self.server.store.maxmemory_policy = maxmemory_policy;
        }
        if let Some(lfu_decay_time) = next_lfu_decay_time {
            self.server.store.lfu_decay_time = lfu_decay_time;
        }
        if let Some(threshold) = next_slowlog_slower_than {
            self.server.store.slowlog_log_slower_than_us = threshold;
        }
        if let Some(max_len) = next_slowlog_max_len {
            self.server.store.slowlog_max_len = max_len;
            while self.server.store.slowlog.len() > self.server.store.slowlog_max_len {
                self.server.store.slowlog.pop_front();
            }
        }
        if let Some(threshold_ms) = next_latency_monitor_threshold {
            self.server.store.latency_tracker.threshold_ms = threshold_ms;
        }
        if let Some(tracking) = next_latency_tracking {
            self.server.latency_tracking = tracking;
        }
        if let Some(percentiles) = next_latency_percentiles {
            self.server.latency_percentiles = percentiles;
        }
        if let Some(hz) = next_hz {
            self.server.hz = hz;
            self.server.store.server_hz = hz;
        }
        if let Some(mc) = next_maxclients {
            self.server.max_clients = mc;
            self.server.store.server_maxclients = mc as u64;
        }
        if let Some(timeout) = next_client_timeout_sec {
            self.server.client_timeout_sec = timeout;
        }
        if let Some(backlog_size) = next_repl_backlog_size {
            self.server.repl_backlog_size = backlog_size;
            self.server.store.server_repl_backlog_size = backlog_size;
            self.server.replication_runtime_state.update_backlog_window(
                self.server.replication_ack_state.primary_offset,
                backlog_size,
            );
        }
        if let Some(repl_timeout) = next_repl_timeout {
            self.server.repl_timeout_sec = repl_timeout;
        }
        if let Some(serve_stale) = next_replica_serve_stale_data {
            self.server.replica_serve_stale_data = serve_stale;
        }
        if let Some(priority) = next_replica_priority {
            self.server.replica_priority = priority;
        }
        if let Some(diskless) = next_repl_diskless_sync {
            self.server.repl_diskless_sync = diskless;
        }
        if let Some(delay) = next_repl_diskless_sync_delay {
            self.server.repl_diskless_sync_delay_sec = delay;
        }
        if let Some(reads_down) = next_cluster_allow_reads_when_down {
            self.server.cluster_allow_reads_when_down = reads_down;
        }
        if let Some(pubsub_down) = next_cluster_allow_pubsubshard_when_down {
            self.server.cluster_allow_pubsubshard_when_down = pubsub_down;
        }
        if let Some(link_limit) = next_cluster_link_sendbuf_limit {
            self.server.cluster_link_sendbuf_limit = link_limit;
        }
        if let Some(node_timeout) = next_cluster_node_timeout {
            self.server.cluster_node_timeout = node_timeout;
        }
        if let Some(migration_barrier) = next_cluster_migration_barrier {
            self.server.cluster_migration_barrier = migration_barrier;
        }
        if let Some(to_write) = next_min_replicas_to_write {
            self.server.min_replicas_to_write = to_write;
        }
        if let Some(max_lag) = next_min_replicas_max_lag {
            self.server.min_replicas_max_lag = max_lag;
        }
        if let Some(stop) = next_stop_writes_on_bgsave_error {
            self.server.stop_writes_on_bgsave_error = stop;
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
        self.server.acl_file_path = next_acl_file_path;
        if let Some(appendonly) = next_appendonly {
            if appendonly {
                let configured_path = self.server.aof_config_path.clone().unwrap_or_else(|| {
                    std::path::PathBuf::from("appendonlydir").join("appendonly.aof")
                });
                self.server.aof_config_path = Some(configured_path.clone());
                self.server.aof_path = Some(configured_path);
                self.server.aof_rewrite_scheduled =
                    self.server.rdb_bgsave_pid.is_some() || self.server.aof_rewrite_pid.is_some();
                if !self.server.aof_rewrite_scheduled {
                    self.server.replication_ack_state.local_fsync_offset =
                        self.server.replication_ack_state.primary_offset;
                }
            } else {
                self.server.aof_path = None;
                self.server.aof_rewrite_scheduled = false;
            }
            self.server.store.set_aof_enabled(appendonly);
        }
        if let Some(mode) = next_appendfsync {
            self.server.appendfsync_mode = mode;
            if self.server.local_waitaof_fsync_tracks_primary_offset() {
                self.server.replication_ack_state.local_fsync_offset =
                    self.server.replication_ack_state.primary_offset;
            }
        } else if self.server.local_waitaof_fsync_tracks_primary_offset() {
            self.server.replication_ack_state.local_fsync_offset =
                self.server.replication_ack_state.primary_offset;
        }
        if let Some(mode) = next_acl_pubsub_default {
            self.server.auth_state.set_acl_pubsub_default(mode);
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
        RespFrame::Error("ERR This instance has cluster support disabled".to_string())
    }

    fn handle_readonly_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 1 {
            return CommandError::WrongArity("READONLY").to_resp();
        }
        RespFrame::Error("ERR This instance has cluster support disabled".to_string())
    }

    fn handle_readwrite_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 1 {
            return CommandError::WrongArity("READWRITE").to_resp();
        }
        RespFrame::Error("ERR This instance has cluster support disabled".to_string())
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
                return client_wrong_subcommand_arity(sub);
            }
            if !Self::client_name_is_valid(&argv[2]) {
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
                return client_wrong_subcommand_arity(sub);
            }
            match &self.session.client_name {
                Some(name) => RespFrame::BulkString(Some(name.clone())),
                None => RespFrame::BulkString(None),
            }
        } else if sub.eq_ignore_ascii_case("ID") {
            if argv.len() != 2 {
                return client_wrong_subcommand_arity(sub);
            }
            RespFrame::Integer(self.session.client_id as i64)
        } else if sub.eq_ignore_ascii_case("LIST") || sub.eq_ignore_ascii_case("INFO") {
            if sub.eq_ignore_ascii_case("INFO") && argv.len() != 2 {
                return client_wrong_subcommand_arity(sub);
            }
            if sub.eq_ignore_ascii_case("LIST") {
                let sessions = self.client_list_sessions();
                let payload = if argv.len() == 2 {
                    sessions
                        .values()
                        .map(|session| self.client_info_line_for_session(session, now_ms))
                        .collect::<String>()
                        .into_bytes()
                } else if argv.len() == 4 && eq_ascii_token(&argv[2], b"TYPE") {
                    let kind = match std::str::from_utf8(&argv[3]) {
                        Ok(kind) => kind,
                        Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
                    };
                    if !matches!(
                        kind.to_ascii_lowercase().as_str(),
                        "normal" | "master" | "replica" | "pubsub"
                    ) {
                        return RespFrame::Error(format!("ERR Unknown client type '{kind}'"));
                    }
                    sessions
                        .values()
                        .filter(|session| {
                            self.client_type_for_session(session)
                                .eq_ignore_ascii_case(kind)
                        })
                        .map(|session| self.client_info_line_for_session(session, now_ms))
                        .collect::<String>()
                        .into_bytes()
                } else if argv.len() >= 4 && eq_ascii_token(&argv[2], b"ID") {
                    let mut requested_ids = HashSet::new();
                    for id_arg in &argv[3..] {
                        let parsed_id = match parse_i64_arg(id_arg) {
                            Ok(id) if id > 0 => id as u64,
                            _ => return RespFrame::Error("ERR Invalid client ID".to_string()),
                        };
                        requested_ids.insert(parsed_id);
                    }
                    sessions
                        .values()
                        .filter(|session| requested_ids.contains(&session.client_id))
                        .map(|session| self.client_info_line_for_session(session, now_ms))
                        .collect::<String>()
                        .into_bytes()
                } else {
                    return RespFrame::Error("ERR syntax error".to_string());
                };
                return RespFrame::BulkString(Some(payload));
            }
            RespFrame::BulkString(Some(
                self.client_info_line_for_session(&self.session, now_ms)
                    .into_bytes(),
            ))
        } else if sub.eq_ignore_ascii_case("NO-EVICT") {
            if argv.len() != 3 {
                return client_wrong_subcommand_arity(sub);
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
                return client_wrong_subcommand_arity(sub);
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
                return client_wrong_subcommand_arity(sub);
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
                return RespFrame::Error(format!("ERR Unrecognized option '{attr}'"));
            }
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("REPLY") {
            // CLIENT REPLY ON|OFF|SKIP
            if argv.len() != 3 {
                return client_wrong_subcommand_arity(sub);
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
            // CLIENT KILL [ip:port | ID client-id | TYPE type | USER user | SKIPME yes|no]
            if argv.len() < 3 {
                return client_wrong_subcommand_arity(sub);
            }

            let legacy_addr = if argv.len() == 3 {
                Some(match std::str::from_utf8(&argv[2]) {
                    Ok(addr) => addr.to_string(),
                    Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
                })
            } else {
                None
            };
            let mut killed = 0;
            let mut skipme = legacy_addr.is_none();
            let mut filter_id: Option<u64> = None;
            let mut filter_type: Option<String> = None;
            let mut filter_user: Option<Vec<u8>> = None;
            let mut filter_addr = legacy_addr.clone();

            if legacy_addr.is_none() {
                let mut i = 2;
                while i < argv.len() {
                    let opt = match std::str::from_utf8(&argv[i]) {
                        Ok(s) => s,
                        Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
                    };

                    if opt.eq_ignore_ascii_case("ID") && i + 1 < argv.len() {
                        let id = match parse_i64_arg(&argv[i + 1]) {
                            Ok(id) if id > 0 => id as u64,
                            _ => return RespFrame::Error("ERR Invalid client ID".to_string()),
                        };
                        filter_id = Some(id);
                        i += 2;
                    } else if opt.eq_ignore_ascii_case("TYPE") && i + 1 < argv.len() {
                        let kind = match std::str::from_utf8(&argv[i + 1]) {
                            Ok(s) => s.to_string(),
                            Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
                        };
                        if !matches!(
                            kind.to_ascii_lowercase().as_str(),
                            "normal" | "master" | "replica" | "pubsub"
                        ) {
                            return RespFrame::Error(format!("ERR Unknown client type '{kind}'"));
                        }
                        filter_type = Some(kind);
                        i += 2;
                    } else if opt.eq_ignore_ascii_case("USER") && i + 1 < argv.len() {
                        filter_user = Some(argv[i + 1].clone());
                        i += 2;
                    } else if opt.eq_ignore_ascii_case("ADDR") && i + 1 < argv.len() {
                        let addr = match std::str::from_utf8(&argv[i + 1]) {
                            Ok(s) => s.to_string(),
                            Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
                        };
                        filter_addr = Some(addr);
                        i += 2;
                    } else if opt.eq_ignore_ascii_case("SKIPME") && i + 1 < argv.len() {
                        let val = match std::str::from_utf8(&argv[i + 1]) {
                            Ok(s) => s,
                            Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
                        };
                        if val.eq_ignore_ascii_case("yes") {
                            skipme = true;
                        } else if val.eq_ignore_ascii_case("no") {
                            skipme = false;
                        } else {
                            return RespFrame::Error(
                                "ERR argument must be 'yes' or 'no'".to_string(),
                            );
                        }
                        i += 2;
                    } else {
                        return RespFrame::Error("ERR syntax error".to_string());
                    }
                }
            }

            let sessions = self.client_list_sessions();
            let mut targets = Vec::new();

            for session in sessions.values() {
                if skipme && session.client_id == self.session.client_id {
                    continue;
                }
                if let Some(id) = filter_id
                    && session.client_id != id
                {
                    continue;
                }
                if let Some(kind) = &filter_type
                    && !self
                        .client_type_for_session(session)
                        .eq_ignore_ascii_case(kind)
                {
                    continue;
                }
                if let Some(user) = &filter_user
                    && session.authenticated_user.as_ref() != Some(user)
                {
                    continue;
                }
                if let Some(addr) = &filter_addr
                    && session.peer_addr.map(|peer| peer.to_string()).as_deref()
                        != Some(addr.as_str())
                {
                    continue;
                }
                targets.push(session.client_id);
            }

            for id in targets {
                if !self.server.pending_client_kills.contains(&id) {
                    self.server.pending_client_kills.push(id);
                    killed += 1;
                }
            }

            if legacy_addr.is_some() {
                if killed == 0 {
                    RespFrame::Error("ERR No such client".to_string())
                } else {
                    RespFrame::SimpleString("OK".to_string())
                }
            } else {
                RespFrame::Integer(killed)
            }
        } else if sub.eq_ignore_ascii_case("PAUSE") {
            // CLIENT PAUSE timeout [WRITE|ALL]
            if argv.len() < 3 || argv.len() > 4 {
                return client_wrong_subcommand_arity(sub);
            }
            let timeout_ms = match parse_i64_arg(&argv[2]) {
                Ok(ms) => ms.max(0) as u64,
                _ => return RespFrame::Error(CLIENT_PAUSE_TIMEOUT_INVALID.to_string()),
            };
            let pause_all = if argv.len() == 4 {
                let mode =
                    std::str::from_utf8(&argv[3]).map_err(|_| CommandError::InvalidUtf8Argument);
                match mode {
                    Ok(m) if m.eq_ignore_ascii_case("ALL") => true,
                    Ok(m) if m.eq_ignore_ascii_case("WRITE") => false,
                    _ => return RespFrame::Error(CLIENT_PAUSE_MODE_INVALID.to_string()),
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
                return client_wrong_subcommand_arity(sub);
            }
            self.server.client_pause_deadline_ms = 0;
            self.server.client_pause_all = false;
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("TRACKING") {
            // CLIENT TRACKING ON|OFF [REDIRECT id] [PREFIX prefix ...] [BCAST] [OPTIN] [OPTOUT] [NOLOOP]
            if argv.len() < 3 {
                return client_wrong_subcommand_arity(sub);
            }
            let requested = match parse_client_tracking_state(argv) {
                Ok(tracking) => tracking,
                Err(err) => return err.to_resp(),
            };
            if let Some(target_id) = requested.redirect
                && !self.client_list_sessions().contains_key(&target_id)
            {
                return RespFrame::Error(
                    "ERR The client ID you want redirect to does not exist".to_string(),
                );
            }
            let tracking =
                match apply_client_tracking_update(&self.session.client_tracking, requested) {
                    Ok(tracking) => tracking,
                    Err(err) => return err.to_resp(),
                };
            self.session.client_tracking = tracking;
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("CACHING") {
            // CLIENT CACHING YES|NO — valid only in OPTIN / OPTOUT tracking modes.
            if argv.len() != 3 {
                return client_wrong_subcommand_arity(sub);
            }
            let mode = match std::str::from_utf8(&argv[2]) {
                Ok(mode) => mode,
                Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
            };
            if let Err(err) = apply_client_caching_mode(mode, &mut self.session.client_tracking) {
                return err.to_resp();
            }
            RespFrame::SimpleString("OK".to_string())
        } else if sub.eq_ignore_ascii_case("GETREDIR") {
            // CLIENT GETREDIR — returns redirect ID for tracking (-1 = not tracking)
            if argv.len() != 2 {
                return client_wrong_subcommand_arity(sub);
            }
            RespFrame::Integer(client_tracking_getredir_value(
                &self.session.client_tracking,
            ))
        } else if sub.eq_ignore_ascii_case("TRACKINGINFO") {
            // CLIENT TRACKINGINFO — returns tracking info
            if argv.len() != 2 {
                return client_wrong_subcommand_arity(sub);
            }
            client_trackinginfo_frame(&self.session.client_tracking)
        } else if sub.eq_ignore_ascii_case("UNBLOCK") {
            // CLIENT UNBLOCK client-id [TIMEOUT|ERROR]
            if argv.len() != 3 && argv.len() != 4 {
                return client_wrong_subcommand_arity(sub);
            }
            let target_id = match parse_i64_arg(&argv[2]) {
                Ok(id) if id > 0 => id as u64,
                _ => return CommandError::InvalidInteger.to_resp(),
            };
            let mode = if argv.len() == 4 {
                let mode = match std::str::from_utf8(&argv[3]) {
                    Ok(mode) => mode,
                    Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
                };
                if mode.eq_ignore_ascii_case("TIMEOUT") {
                    ClientUnblockMode::Timeout
                } else if mode.eq_ignore_ascii_case("ERROR") {
                    ClientUnblockMode::Error
                } else {
                    return RespFrame::Error(
                        "ERR CLIENT UNBLOCK reason should be TIMEOUT or ERROR".to_string(),
                    );
                }
            } else {
                ClientUnblockMode::Timeout
            };
            let already_pending = self
                .server
                .pending_client_unblocks
                .iter()
                .any(|(pending_id, _)| *pending_id == target_id);
            if self.server.blocked_client_ids.contains(&target_id) && !already_pending {
                self.server.pending_client_unblocks.push((target_id, mode));
                RespFrame::Integer(1)
            } else {
                RespFrame::Integer(0)
            }
        } else if sub.eq_ignore_ascii_case("HELP") {
            if argv.len() != 2 {
                return client_wrong_subcommand_arity(sub);
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
                    Ok(-1) => self.server.store.slowlog_len(),
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
                .store
                .get_slowlog(count)
                .into_iter()
                .map(|entry| {
                    let argv_frames: Vec<RespFrame> = entry
                        .argv
                        .into_iter()
                        .map(|a| RespFrame::BulkString(Some(a)))
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
            RespFrame::Integer(self.server.store.slowlog_len() as i64)
        } else if sub.eq_ignore_ascii_case("RESET") {
            if argv.len() != 2 {
                return CommandError::WrongSubcommandArity {
                    command: "SLOWLOG",
                    subcommand: "RESET".to_string(),
                }
                .to_resp();
            }
            self.server.store.reset_slowlog();
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
        self.server.store.record_slowlog(argv, duration_us, now_ms);
    }

    fn handle_save_command(&mut self, argv: &[Vec<u8>], now_ms: u64) -> RespFrame {
        if argv.len() != 1 {
            return CommandError::WrongArity("SAVE").to_resp();
        }
        if let Err(reply) = self.persist_snapshot_to_disk(now_ms, true) {
            return reply;
        }
        self.server.store.record_save(now_ms, false);
        self.server.last_save_time_sec = self.server.store.last_save_time_sec;
        RespFrame::SimpleString("OK".to_string())
    }

    #[allow(unsafe_code)]
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
        #[cfg(unix)]
        unsafe {
            if self.server.rdb_bgsave_pid.is_some() || self.server.aof_rewrite_pid.is_some() {
                return RespFrame::Error("ERR Background save already in progress".to_string());
            }

            match libc::fork() {
                -1 => {
                    self.server.store.record_bgsave_status(false);
                    RespFrame::Error("ERR Can't bgsave: fork error".to_string())
                }
                0 => {
                    let result = self.persist_snapshot_to_disk(now_ms, true);
                    std::process::exit(if result.is_ok() { 0 } else { 1 });
                }
                pid => {
                    self.server.rdb_bgsave_pid = Some(pid);
                    self.server.store.record_save(now_ms, true);
                    self.server.store.record_bgsave_status(true);
                    self.server.last_save_time_sec = self.server.store.last_save_time_sec;
                    RespFrame::SimpleString("Background saving started".to_string())
                }
            }
        }
        #[cfg(not(unix))]
        {
            if let Err(reply) = self.persist_snapshot_to_disk(now_ms, true) {
                self.server.store.record_bgsave_status(false);
                return reply;
            }
            self.server.store.record_save(now_ms, true);
            self.server.store.record_bgsave_status(true);
            self.server.last_save_time_sec = self.server.store.last_save_time_sec;
            RespFrame::SimpleString("Background saving started".to_string())
        }
    }

    fn persist_snapshot_to_disk(
        &mut self,
        now_ms: u64,
        record_aof_write_status: bool,
    ) -> Result<(), RespFrame> {
        if let Some(path) = &self.server.aof_path {
            let commands = self.server.store.to_aof_commands(now_ms);
            let records = argv_to_aof_records(commands);
            if write_aof_file(path, &records).is_err() {
                if record_aof_write_status {
                    self.server.store.record_aof_write_status(false);
                }
                return Err(RespFrame::Error(
                    "ERR error saving dataset to disk".to_string(),
                ));
            }
            if record_aof_write_status {
                self.server.store.record_aof_write_status(true);
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
        self.handle_bgrewriteaof_requested(now_ms)
    }

    fn handle_bgrewriteaof_requested(&mut self, now_ms: u64) -> RespFrame {
        let Some(path) = &self.server.aof_path else {
            self.server.aof_rewrite_scheduled = false;
            return RespFrame::Error(
                "ERR appendonly is disabled, cannot rewrite append only file".to_string(),
            );
        };
        // Rewrite the AOF file with a snapshot of the current store state.
        let commands = self.server.store.to_aof_commands(now_ms);
        let records = argv_to_aof_records(commands);
        if let Err(_e) = write_aof_file(path, &records) {
            self.server.aof_rewrite_scheduled = false;
            self.server.store.record_aof_bgrewrite_status(false);
            return RespFrame::Error("ERR error rewriting AOF file".to_string());
        }
        self.server.aof_rewrite_scheduled = false;
        self.server.replication_ack_state.local_fsync_offset =
            self.server.replication_ack_state.primary_offset;
        self.server.store.record_aof_rewrite(now_ms);
        self.server.store.record_aof_bgrewrite_status(true);
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
                return replies.pop().unwrap_or(RespFrame::BulkString(None));
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
            return replies.pop().unwrap_or(RespFrame::BulkString(None));
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
            return replies.pop().unwrap_or(RespFrame::BulkString(None));
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
                return replies.pop().unwrap_or(RespFrame::BulkString(None));
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
            return replies.pop().unwrap_or(RespFrame::BulkString(None));
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

        let known_subcommand = sub.eq_ignore_ascii_case("CHANNELS")
            || sub.eq_ignore_ascii_case("NUMSUB")
            || sub.eq_ignore_ascii_case("NUMPAT")
            || sub.eq_ignore_ascii_case("SHARDCHANNELS")
            || sub.eq_ignore_ascii_case("SHARDNUMSUB")
            || sub.eq_ignore_ascii_case("HELP");
        if !known_subcommand {
            return RespFrame::Error(format!(
                "ERR unknown subcommand '{}'. Try PUBSUB HELP.",
                sub
            ));
        }
        if self.is_in_subscription_mode() {
            return Self::pubsub_context_error(&format!("pubsub|{}", sub.to_ascii_lowercase()));
        }
        if sub.eq_ignore_ascii_case("HELP") {
            if argv.len() != 2 {
                return CommandError::SyntaxError.to_resp();
            }
            return RespFrame::Array(Some(vec![
                RespFrame::SimpleString(
                    "PUBSUB <subcommand> [<arg> [value] [opt] ...]. Subcommands are:"
                        .to_string(),
                ),
                RespFrame::SimpleString("CHANNELS [<pattern>]".to_string()),
                RespFrame::SimpleString(
                    "    Return the currently active channels matching a <pattern> (default: '*')."
                        .to_string(),
                ),
                RespFrame::SimpleString("NUMPAT".to_string()),
                RespFrame::SimpleString(
                    "    Return number of subscriptions to patterns.".to_string(),
                ),
                RespFrame::SimpleString("NUMSUB [<channel> ...]".to_string()),
                RespFrame::SimpleString(
                    "    Return the number of subscribers for the specified channels, excluding"
                        .to_string(),
                ),
                RespFrame::SimpleString(
                    "    pattern subscriptions(default: no channels).".to_string(),
                ),
                RespFrame::SimpleString("SHARDCHANNELS [<pattern>]".to_string()),
                RespFrame::SimpleString(
                    "    Return the currently active shard level channels matching a <pattern> (default: '*')."
                        .to_string(),
                ),
                RespFrame::SimpleString("SHARDNUMSUB [<shardchannel> ...]".to_string()),
                RespFrame::SimpleString(
                    "    Return the number of subscribers for the specified shard level channel(s)"
                        .to_string(),
                ),
                RespFrame::SimpleString("HELP".to_string()),
                RespFrame::SimpleString("    Print this help.".to_string()),
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

        unreachable!("known pubsub subcommands are handled above")
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
            return replies.pop().unwrap_or(RespFrame::BulkString(None));
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
                return replies.pop().unwrap_or(RespFrame::BulkString(None));
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
            return replies.pop().unwrap_or(RespFrame::BulkString(None));
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
        let cursor = parse_u64_full_arg(&argv[1])?;

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
                let parsed =
                    parse_i64_arg(&argv[i + 1]).map_err(|_| CommandError::InvalidInteger)?;
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

        if cursor >= filtered.len() as u64 {
            return Ok(RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"0".to_vec())),
                RespFrame::Array(Some(Vec::new())),
            ])));
        }

        let cursor = cursor as usize;
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
            return Err(CommandError::Custom(
                "ERR source and destination objects are the same".to_string(),
            ));
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
        let is_latencystats = section_requested("latencystats");

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
        if is_latencystats
            && let RespFrame::BulkString(Some(bytes)) = self.handle_info_latencystats_section()
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
            let mut reply = self.dispatch_with_client_context(&namespaced, now_ms)?;
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

    fn handle_info_latencystats_section(&mut self) -> RespFrame {
        let mut info = String::from("# Latencystats\r\n");
        let histograms = self.server.store.all_command_histograms();
        for (cmd, hist) in histograms {
            info.push_str(&format!("latency_percentiles_usec_{cmd}:"));
            for (i, &p) in self.server.latency_percentiles.iter().enumerate() {
                if i > 0 {
                    info.push(',');
                }
                let val = histogram_percentile_us(hist, p);
                info.push_str(&format!("p{p}={val:.2}"));
            }
            info.push_str("\r\n");
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
        info.push_str(&format!(
            "rdb_last_bgsave_status:{}\r\n",
            if self.server.store.stat_rdb_last_bgsave_ok {
                "ok"
            } else {
                "err"
            }
        ));
        info.push_str(&format!(
            "rdb_last_bgsave_time_sec:{}\r\n",
            self.server
                .store
                .stat_rdb_last_bgsave_time_sec
                .map_or(-1, |ts| ts as i64)
        ));
        info.push_str("rdb_current_bgsave_time_sec:-1\r\n");
        info.push_str(&format!(
            "rdb_saves:{}\r\n",
            self.server.store.stat_rdb_saves
        ));
        info.push_str("rdb_last_cow_size:0\r\n");
        info.push_str(&format!(
            "aof_enabled:{}\r\n",
            usize::from(self.server.aof_path.is_some())
        ));
        info.push_str(&format!(
            "aof_rewrite_in_progress:{}\r\n",
            usize::from(self.server.aof_rewrite_pid.is_some())
        ));
        info.push_str(&format!(
            "aof_rewrite_scheduled:{}\r\n",
            usize::from(self.server.aof_rewrite_scheduled)
        ));
        info.push_str(&format!(
            "aof_last_rewrite_time_sec:{}\r\n",
            self.server
                .store
                .stat_aof_last_rewrite_time_sec
                .map_or(-1, |ts| ts as i64)
        ));
        info.push_str("aof_current_rewrite_time_sec:-1\r\n");
        info.push_str(&format!(
            "aof_last_bgrewrite_status:{}\r\n",
            if self.server.store.stat_aof_last_bgrewrite_ok {
                "ok"
            } else {
                "err"
            }
        ));
        info.push_str(&format!(
            "aof_last_write_status:{}\r\n",
            if self.server.store.stat_aof_last_write_ok {
                "ok"
            } else {
                "err"
            }
        ));
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
        let role = match &self.server.replication_runtime_state.role {
            ReplicationRoleState::Master => "master",
            ReplicationRoleState::Replica { .. } => "slave",
        };
        let primary_offset =
            i64::try_from(self.server.replication_ack_state.primary_offset.0).unwrap_or(i64::MAX);

        let mut info = String::from("# Replication\r\n");
        info.push_str(&format!("role:{role}\r\n"));

        match &self.server.replication_runtime_state.role {
            ReplicationRoleState::Master => {}
            ReplicationRoleState::Replica { host, port, state } => {
                let master_link_status = if *state == "connected" { "up" } else { "down" };
                let master_last_io_seconds_ago = if *state == "connected" { 0 } else { -1 };
                let master_sync_in_progress = i64::from(*state == "sync");
                info.push_str(&format!(
                    "master_host:{host}\r\n\
master_port:{port}\r\n\
master_link_status:{master_link_status}\r\n\
master_last_io_seconds_ago:{master_last_io_seconds_ago}\r\n\
master_sync_in_progress:{master_sync_in_progress}\r\n\
slave_read_repl_offset:{primary_offset}\r\n\
slave_repl_offset:{primary_offset}\r\n\
slave_priority:{}\r\n",
                    self.server.replica_priority
                ));
            }
        }
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

        info.push_str(&format!("connected_slaves:{connected_replicas}\r\n"));
        info.push_str("master_failover_state:no-failover\r\n");
        info.push_str(&format!("master_replid:{}\r\n", backlog.replid));
        info.push_str("master_replid2:0000000000000000000000000000000000000000\r\n");
        info.push_str(&format!("master_repl_offset:{primary_offset}\r\n"));
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
        match self.dispatch_with_client_context(argv, now_ms) {
            Ok(reply) => reply,
            Err(err) => err.to_resp(),
        }
    }

    fn handle_wait_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 3 {
            return CommandError::WrongArity("WAIT").to_resp();
        }
        let required_replicas = match parse_i64_arg(&argv[1]) {
            Ok(value) if value >= 0 => usize::try_from(value).unwrap_or(usize::MAX),
            Ok(_) => 0,
            Err(_) => return CommandError::InvalidInteger.to_resp(),
        };
        match parse_i64_arg(&argv[2]) {
            Ok(value) if value < 0 => {
                return RespFrame::Error("ERR timeout is negative".to_string());
            }
            Err(_) => {
                return RespFrame::Error(
                    "ERR timeout is not an integer or out of range".to_string(),
                );
            }
            _ => {}
        }

        // Refresh replica ACK snapshots to get current state
        self.server.refresh_replica_ack_snapshots();

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
            Ok(value @ 0..=1) => usize::try_from(value).unwrap_or(usize::MAX),
            Ok(_) => {
                return RespFrame::Error(
                    "ERR value is out of range, value must between 0 and 1".to_string(),
                );
            }
            Err(_) => return CommandError::InvalidInteger.to_resp(),
        };
        let required_replicas = match parse_i64_arg(&argv[2]) {
            Ok(value) if value >= 0 => usize::try_from(value).unwrap_or(usize::MAX),
            _ => {
                return RespFrame::Error("ERR value is out of range, must be positive".to_string());
            }
        };
        match parse_i64_arg(&argv[3]) {
            Ok(value) if value < 0 => {
                return RespFrame::Error("ERR timeout is negative".to_string());
            }
            Err(_) => {
                return RespFrame::Error(
                    "ERR timeout is not an integer or out of range".to_string(),
                );
            }
            _ => {}
        }
        if matches!(
            self.server.replication_runtime_state.role,
            ReplicationRoleState::Replica { .. }
        ) {
            return RespFrame::Error(
                "ERR WAITAOF cannot be used with replica instances. Please also note that writes to replicas are just local and are not propagated.".to_string(),
            );
        }
        if required_local > 0 && !self.server.store.aof_enabled {
            return RespFrame::Error(
                "ERR WAITAOF cannot be used when numlocal is set but appendonly is disabled."
                    .to_string(),
            );
        }

        // Refresh replica ACK snapshots to get current state
        self.server.refresh_replica_ack_snapshots();

        // Redis reports the current local/replica AOF ACK state for the write
        // offset associated with the issuing client, regardless of the requested
        // minima. The minima only determine whether the command is allowed to
        // proceed, not how the returned tuple is counted.
        let required_local_offset = self.server.replication_ack_state.primary_offset;
        let required_replica_offset = self.server.replication_ack_state.primary_offset;

        let outcome = evaluate_waitaof(
            self.server.replication_ack_state.local_fsync_offset,
            &self.server.replication_ack_state.replica_fsync_offsets,
            WaitAofThreshold {
                required_local_offset,
                required_replica_offset,
                required_replicas,
            },
        );
        let local_ack = if self.server.store.aof_enabled && outcome.local_satisfied {
            1
        } else {
            0
        };
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
            ReplicationRoleState::Replica { host, port, state } => {
                let offset = if state.eq_ignore_ascii_case("connected") {
                    i64::try_from(self.server.replication_ack_state.primary_offset.0)
                        .unwrap_or(i64::MAX)
                } else {
                    -1
                };
                RespFrame::Array(Some(vec![
                    hello_bulk("slave"),
                    hello_bulk(host),
                    RespFrame::Integer(i64::from(*port)),
                    hello_bulk(state),
                    RespFrame::Integer(offset),
                ]))
            }
        }
    }

    fn handle_failover_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if matches!(
            self.server.replication_runtime_state.role,
            ReplicationRoleState::Replica { .. }
        ) {
            return RespFrame::Error(
                "ERR FAILOVER is not valid when server is a replica.".to_string(),
            );
        }

        if argv.len() == 2 {
            let arg = match std::str::from_utf8(&argv[1]) {
                Ok(arg) => arg,
                Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
            };
            if arg.eq_ignore_ascii_case("ABORT") {
                return RespFrame::Error("ERR No failover in progress.".to_string());
            }
        }

        let mut target_host: Option<String> = None;
        let mut target_port: Option<u16> = None;
        let mut force = false;
        let mut timeout_ms: Option<u64> = None;
        let mut i = 1;
        while i < argv.len() {
            let arg = match std::str::from_utf8(&argv[i]) {
                Ok(arg) => arg,
                Err(_) => return CommandError::InvalidUtf8Argument.to_resp(),
            };
            if arg.eq_ignore_ascii_case("TIMEOUT") && i + 1 < argv.len() && timeout_ms.is_none() {
                let parsed_timeout = match parse_i64_arg(&argv[i + 1]) {
                    Ok(value) => value,
                    Err(err) => return err.to_resp(),
                };
                if parsed_timeout <= 0 {
                    return RespFrame::Error(
                        "ERR FAILOVER timeout must be greater than 0".to_string(),
                    );
                }
                timeout_ms = Some(parsed_timeout as u64);
                i += 2;
            } else if arg.eq_ignore_ascii_case("TO") && i + 2 < argv.len() && target_host.is_none()
            {
                target_host = Some(String::from_utf8_lossy(&argv[i + 1]).into_owned());
                let parsed_port = match parse_i64_arg(&argv[i + 2]) {
                    Ok(value) => value,
                    Err(err) => return err.to_resp(),
                };
                let Ok(parsed_port) = u16::try_from(parsed_port) else {
                    return CommandError::InvalidInteger.to_resp();
                };
                target_port = Some(parsed_port);
                i += 3;
            } else if arg.eq_ignore_ascii_case("FORCE") && !force {
                force = true;
                i += 1;
            } else {
                return CommandError::SyntaxError.to_resp();
            }
        }

        if self.server.replication_runtime_state.replicas.is_empty() {
            return RespFrame::Error("ERR FAILOVER requires connected replicas.".to_string());
        }

        let selected_target = if let (Some(host), Some(port)) = (target_host, target_port) {
            let is_known_replica =
                self.server
                    .replication_runtime_state
                    .replicas
                    .values()
                    .any(|replica| {
                        let replica_host = replica.ip_address.as_deref().unwrap_or("127.0.0.1");
                        replica.listening_port == port && replica_host.eq_ignore_ascii_case(&host)
                    });
            if !is_known_replica {
                return RespFrame::Error(
                    "ERR FAILOVER target HOST and PORT is not a replica.".to_string(),
                );
            }
            (host, port)
        } else {
            let Some(replica) = self
                .server
                .replication_runtime_state
                .replicas
                .values()
                .filter(|replica| replica.listening_port != 0)
                .max_by_key(|replica| (replica.ack_offset.0, replica.listening_port))
            else {
                return RespFrame::Error("ERR FAILOVER requires connected replicas.".to_string());
            };
            (
                replica
                    .ip_address
                    .clone()
                    .unwrap_or_else(|| "127.0.0.1".to_string()),
                replica.listening_port,
            )
        };

        match self.promote_failover_target(
            &selected_target.0,
            selected_target.1,
            Duration::from_millis(timeout_ms.unwrap_or(5_000)),
        ) {
            Ok(()) => RespFrame::SimpleString("OK".to_string()),
            Err(err) => err.to_resp(),
        }
    }

    fn promote_failover_target(
        &mut self,
        host: &str,
        port: u16,
        timeout: Duration,
    ) -> Result<(), CommandError> {
        let mut stream = std::net::TcpStream::connect((host, port)).map_err(|err| {
            CommandError::Custom(format!(
                "ERR FAILOVER could not connect to target replica: {err}"
            ))
        })?;
        let _ = stream.set_nodelay(true);
        let _ = stream.set_read_timeout(Some(timeout));
        let _ = stream.set_write_timeout(Some(timeout));

        let mut read_buf = Vec::new();
        self.failover_send_command(&mut stream, &mut read_buf, &[b"REPLICAOF", b"NO", b"ONE"])?;

        let deadline = Instant::now() + timeout;
        loop {
            let role_reply = self.failover_send_command(&mut stream, &mut read_buf, &[b"ROLE"])?;
            if failover_role_is_master(&role_reply) {
                break;
            }
            if Instant::now() >= deadline {
                return Err(CommandError::Custom(
                    "ERR FAILOVER target replica did not become a master in time.".to_string(),
                ));
            }
            std::thread::sleep(Duration::from_millis(25));
        }

        self.server.replication_runtime_state.role = ReplicationRoleState::Replica {
            host: host.to_string(),
            port,
            state: "connect",
        };
        self.server.replica_reconfigure_requested = true;
        Ok(())
    }

    fn failover_send_command(
        &self,
        stream: &mut std::net::TcpStream,
        read_buf: &mut Vec<u8>,
        parts: &[&[u8]],
    ) -> Result<RespFrame, CommandError> {
        let frame = RespFrame::Array(Some(
            parts
                .iter()
                .map(|part| RespFrame::BulkString(Some((*part).to_vec())))
                .collect(),
        ));
        std::io::Write::write_all(stream, &frame.to_bytes()).map_err(|err| {
            CommandError::Custom(format!("ERR FAILOVER target replica write failed: {err}"))
        })?;
        let reply = read_failover_frame(stream, read_buf)?;
        if let RespFrame::Error(message) = &reply {
            return Err(CommandError::Custom(message.clone()));
        }
        Ok(reply)
    }

    fn handle_replicaof_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 3 {
            return CommandError::WrongArity(replicaof_command_name(argv)).to_resp();
        }
        let host = String::from_utf8_lossy(&argv[1]).into_owned();
        let port = String::from_utf8_lossy(&argv[2]).into_owned();
        if host.eq_ignore_ascii_case("NO") && port.eq_ignore_ascii_case("ONE") {
            if !matches!(
                self.server.replication_runtime_state.role,
                ReplicationRoleState::Master
            ) {
                self.server.replication_runtime_state.role = ReplicationRoleState::Master;
                self.server
                    .replication_runtime_state
                    .rotate_backlog_identity();
                self.server.replication_runtime_state.replicas.clear();
                self.server.refresh_replica_ack_snapshots();
                self.server.replica_reconfigure_requested = true;

                // Redis parity: WAITAOF on a promoted replica should return immediately for commands
                // executed on the old master. By advancing the local fsync offset to the primary offset,
                // we mark all inherited history as locally satisfied.
                self.server.replication_ack_state.local_fsync_offset =
                    self.server.replication_ack_state.primary_offset;
            }
            return RespFrame::SimpleString("OK".to_string());
        }
        let port = match parse_i64_arg(&argv[2]) {
            Ok(value) => match u16::try_from(value) {
                Ok(port) => port,
                Err(_) => return RespFrame::Error("ERR Invalid master port".to_string()),
            },
            Err(_) => return RespFrame::Error("ERR Invalid master port".to_string()),
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
        self.server.replica_reconfigure_requested = true;
        RespFrame::SimpleString("OK".to_string())
    }

    fn handle_replconf_command(&mut self, argv: &[Vec<u8>], now_ms: u64) -> RespFrame {
        if argv.len().is_multiple_of(2) {
            return CommandError::SyntaxError.to_resp();
        }
        let mut idx = 1;
        while idx < argv.len() {
            let option = String::from_utf8_lossy(&argv[idx]).into_owned();
            if option.eq_ignore_ascii_case("ACK") {
                if matches!(
                    self.server.replication_runtime_state.role,
                    ReplicationRoleState::Replica { .. }
                ) {
                    return RespFrame::SimpleString("OK".to_string());
                }
                let offset = match parse_i64_arg(&argv[idx + 1]) {
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
                    if idx + 3 < argv.len() && eq_ascii_token(&argv[idx + 2], b"FACK") {
                        let fsync_offset = match parse_i64_arg(&argv[idx + 3]) {
                            Ok(value) if value >= 0 => ReplOffset(value as u64),
                            _ => return CommandError::InvalidInteger.to_resp(),
                        };
                        if fsync_offset > replica.fsync_offset {
                            replica.fsync_offset = fsync_offset;
                        }
                    }
                    replica.last_ack_timestamp_ms = now_ms;
                }
                self.server.refresh_replica_ack_snapshots();
                return RespFrame::SimpleString("OK".to_string());
            }
            if option.eq_ignore_ascii_case("FACK") {
                if matches!(
                    self.server.replication_runtime_state.role,
                    ReplicationRoleState::Replica { .. }
                ) {
                    return RespFrame::SimpleString("OK".to_string());
                }
                let offset = match parse_i64_arg(&argv[idx + 1]) {
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
                    replica.last_ack_timestamp_ms = now_ms;
                }
                self.server.refresh_replica_ack_snapshots();
                return RespFrame::SimpleString("OK".to_string());
            }
            if option.eq_ignore_ascii_case("GETACK") {
                self.server.refresh_replica_ack_snapshots();
                return RespFrame::Array(Some(vec![
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
                ]));
            }
            if option.eq_ignore_ascii_case("listening-port") {
                let value = match parse_i64_arg(&argv[idx + 1]) {
                    Ok(value) => value,
                    Err(err) => return err.to_resp(),
                };
                let port = match u16::try_from(value) {
                    Ok(port) => port,
                    Err(_) => return CommandError::InvalidInteger.to_resp(),
                };
                self.server
                    .replication_runtime_state
                    .ensure_replica(self.session.client_id)
                    .listening_port = port;
            } else if option.eq_ignore_ascii_case("ip-address") {
                self.server
                    .replication_runtime_state
                    .ensure_replica(self.session.client_id)
                    .ip_address = Some(String::from_utf8_lossy(&argv[idx + 1]).into_owned());
            } else if option.eq_ignore_ascii_case("capa") {
                // Ignore capability tokens we do not model explicitly.
            } else {
                return RespFrame::Error(format!("ERR Unrecognized REPLCONF option: {option}"));
            }
            idx += 2;
        }
        RespFrame::SimpleString("OK".to_string())
    }

    fn handle_psync_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        let is_sync = argv.first().is_some_and(|cmd| eq_ascii_token(cmd, b"SYNC"));
        if is_sync {
            if argv.len() != 1 {
                return CommandError::WrongArity("SYNC").to_resp();
            }
        } else if argv.len() < 3 {
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
            self.server.store.stat_sync_partial_ok += 1;
            RespFrame::SimpleString("CONTINUE".to_string())
        } else {
            // Track whether this was a failed partial attempt or a fresh full sync.
            if requested_replid != "?" && requested_offset >= 0 {
                // Client attempted partial resync but we couldn't satisfy it.
                self.server.store.stat_sync_partial_err += 1;
            }
            self.server.store.stat_sync_full += 1;
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

    fn handle_multi_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if self.session.transaction_state.in_transaction {
            return RespFrame::Error("ERR MULTI calls can not be nested".to_string());
        }
        if argv.len() != 1 {
            self.session.transaction_state.in_transaction = true;
            self.session.transaction_state.exec_abort = true;
            self.session.transaction_state.command_queue.clear();
            return wrong_arity_error("multi");
        }
        self.session.transaction_state.in_transaction = true;
        self.session.transaction_state.exec_abort = false;
        self.session.transaction_state.command_queue.clear();
        RespFrame::SimpleString("OK".to_string())
    }

    fn handle_exec_command(&mut self, argv: &[Vec<u8>], now_ms: u64, packet_id: u64) -> RespFrame {
        if argv.len() != 1 {
            if self.session.transaction_state.in_transaction {
                self.session.transaction_state.in_transaction = false;
                self.session.transaction_state.exec_abort = false;
                self.session.transaction_state.command_queue.clear();
                self.session.transaction_state.watched_keys.clear();
                self.session.transaction_state.watch_dirty = false;
                return RespFrame::Error(
                    "EXECABORT Transaction discarded because of: wrong number of arguments for 'exec' command"
                        .to_string(),
                );
            }
            return wrong_arity_error("exec");
        }
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
        let watch_failed = self.session.transaction_state.watch_dirty || {
            let mut dirty = false;
            for (key, original_fp, original_mod_count) in
                &self.session.transaction_state.watched_keys
            {
                let current_fp = self.server.store.key_fingerprint(key, now_ms);
                let current_dirty = self.server.store.key_modification_count(key, now_ms);
                if current_fp != *original_fp || current_dirty != *original_mod_count {
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
        self.session.transaction_state.executing_exec = true;

        for argv in &queued {
            if let Some(permission_error) = self.acl_permission_error(argv) {
                let command_name = String::from_utf8_lossy(&argv[0]).into_owned();
                let (log_reason, object, reply) = match permission_error {
                    AclCommandPermissionError::Command => (
                        "command",
                        command_name.to_ascii_uppercase(),
                        RespFrame::Error(format!(
                            "NOPERM this user has no permissions to run the '{}' command",
                            command_name
                        )),
                    ),
                    AclCommandPermissionError::Key(key) => {
                        let key_name = String::from_utf8_lossy(&key).into_owned();
                        (
                            "key",
                            key_name,
                            RespFrame::Error("NOPERM No permissions to access a key".to_string()),
                        )
                    }
                    AclCommandPermissionError::Channel(channel) => {
                        let channel_name = String::from_utf8_lossy(&channel).into_owned();
                        (
                            "channel",
                            channel_name,
                            RespFrame::Error(
                                "NOPERM No permissions to access a channel".to_string(),
                            ),
                        )
                    }
                };
                self.record_acl_log_event(
                    log_reason,
                    object,
                    self.session.current_user_name().to_vec(),
                    now_ms,
                );
                results.push(reply);
                continue;
            }

            if let Some(reply) = self.enforce_maxmemory_before_dispatch(argv, now_ms, packet_id) {
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
            self.server.record_latency_sample(argv, elapsed_us, now_ms);
            self.server.record_command_histogram(argv, elapsed_us);

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
        self.session.transaction_state.executing_exec = false;

        if transaction_dirty {
            self.capture_aof_record(&[b"MULTI".to_vec()]);
            for argv in transaction_aof {
                self.capture_aof_record(&argv);
            }
            self.capture_aof_record(&[b"EXEC".to_vec()]);
        }

        RespFrame::Array(Some(results))
    }

    fn handle_discard_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 1 {
            if self.session.transaction_state.in_transaction {
                self.session.transaction_state.exec_abort = true;
            }
            return wrong_arity_error("discard");
        }
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
                input_source: ThreatInputDigestSource::Frame(frame),
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
                    input_source: ThreatInputDigestSource::Frame(frame),
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
        // Lazy-compute all digests on the cold path. The success path of
        // execute_frame never reaches this branch, so the O(N_keys) state
        // digest is paid only when a threat actually fires.
        //
        // Semantic note: `state_digest_before` and `state_digest_after` are
        // both captured at recording time. For pre-dispatch threats
        // (preflight gate, parse error, auth/perm denial, too-many-args,
        // maxmemory), the store has not been mutated since command entry, so
        // these match the prior `before/after = same snapshot` semantics. For
        // post-dispatch threats (slow-command, EXEC inner threats), the
        // recorded snapshot reflects post-mutation state — see
        // ISOMORPHISM_PROOF_LAZY_DIGEST.md for the documented drift.
        let input_digest = input.input_source.digest();
        let state_snapshot = self.server.store.state_digest();
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
            input_digest,
            output_digest,
            state_digest_before: state_snapshot.clone(),
            state_digest_after: state_snapshot,
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
        candidate_debug_bytes: Vec<u8>,
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
            input_source: ThreatInputDigestSource::Owned(candidate_debug_bytes),
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
    let slen = arg.len();
    if slen == 0 || slen > 20 {
        return Err(CommandError::InvalidInteger);
    }
    if slen == 1 && arg[0] == b'0' {
        return Ok(0);
    }

    let mut p = 0;
    let negative = arg[0] == b'-';
    if negative {
        p += 1;
        if p == slen {
            return Err(CommandError::InvalidInteger);
        }
    }

    if arg[p] >= b'1' && arg[p] <= b'9' {
        let mut v: u64 = (arg[p] - b'0') as u64;
        p += 1;
        while p < slen {
            let b = arg[p];
            if b.is_ascii_digit() {
                if v > (u64::MAX / 10) {
                    return Err(CommandError::InvalidInteger);
                }
                v *= 10;
                let digit = (b - b'0') as u64;
                if v > (u64::MAX - digit) {
                    return Err(CommandError::InvalidInteger);
                }
                v += digit;
                p += 1;
            } else {
                return Err(CommandError::InvalidInteger);
            }
        }

        if negative {
            let limit = (i64::MIN as u64).wrapping_neg();
            if v > limit {
                return Err(CommandError::InvalidInteger);
            }
            return Ok(v.wrapping_neg() as i64);
        } else {
            if v > i64::MAX as u64 {
                return Err(CommandError::InvalidInteger);
            }
            return Ok(v as i64);
        }
    }

    Err(CommandError::InvalidInteger)
}

fn parse_u64_full_arg(arg: &[u8]) -> Result<u64, CommandError> {
    let slen = arg.len();
    if slen == 0 || slen > 20 {
        return Err(CommandError::InvalidInteger);
    }
    if slen == 1 && arg[0] == b'0' {
        return Ok(0);
    }
    if arg[0] < b'1' || arg[0] > b'9' {
        return Err(CommandError::InvalidInteger);
    }

    let mut v: u64 = (arg[0] - b'0') as u64;
    for &b in &arg[1..] {
        if !b.is_ascii_digit() {
            return Err(CommandError::InvalidInteger);
        }
        if v > (u64::MAX / 10) {
            return Err(CommandError::InvalidInteger);
        }
        v *= 10;
        let digit = (b - b'0') as u64;
        if v > (u64::MAX - digit) {
            return Err(CommandError::InvalidInteger);
        }
        v += digit;
    }

    Ok(v)
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
                let groups = store
                    .stream_consumer_groups(&key)
                    .map(|gs| {
                        gs.iter()
                            .map(|(name, group)| fr_persist::RdbStreamConsumerGroup {
                                name: name.clone(),
                                last_delivered_id_ms: group.last_delivered_id.0,
                                last_delivered_id_seq: group.last_delivered_id.1,
                                consumers: group.consumers.iter().cloned().collect(),
                                pending: group
                                    .pending
                                    .iter()
                                    .map(|((ms, seq), pe)| fr_persist::RdbStreamPendingEntry {
                                        entry_id_ms: *ms,
                                        entry_id_seq: *seq,
                                        consumer: pe.consumer.clone(),
                                        deliveries: pe.deliveries,
                                        last_delivered_ms: pe.last_delivered_ms,
                                    })
                                    .collect(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                RdbValue::Stream(stream_entries, watermark, groups)
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
            RdbValue::Stream(stream_entries, watermark, groups) => {
                for (ms, seq, fields) in stream_entries {
                    let field_pairs: Vec<(Vec<u8>, Vec<u8>)> =
                        fields.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                    let _ = store.xadd(&key, (*ms, *seq), &field_pairs, now_ms);
                }
                if let Some((wm_ms, wm_seq)) = watermark {
                    let _ = store.xsetid(&key, (*wm_ms, *wm_seq), now_ms);
                }
                // Restore consumer groups from RDB snapshot.
                for group in groups {
                    let consumers: std::collections::BTreeSet<Vec<u8>> =
                        group.consumers.iter().cloned().collect();
                    let mut pending = std::collections::BTreeMap::new();
                    for pe in &group.pending {
                        pending.insert(
                            (pe.entry_id_ms, pe.entry_id_seq),
                            fr_store::StreamPendingEntry {
                                consumer: pe.consumer.clone(),
                                deliveries: pe.deliveries,
                                last_delivered_ms: pe.last_delivered_ms,
                            },
                        );
                    }
                    store.restore_stream_group(
                        &key,
                        group.name.clone(),
                        (group.last_delivered_id_ms, group.last_delivered_id_seq),
                        consumers,
                        pending,
                    );
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

    use fr_command::{
        CLIENT_PAUSE_MODE_INVALID, CLIENT_PAUSE_TIMEOUT_INVALID, CommandError, dispatch_argv,
    };
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
    use fr_store::sha1_hex_public;

    use super::{
        AOF_DISK_ERROR_WRITE_DENIED, AclPubsubDefault, ClientSession, ClientUnblockMode,
        ClusterClientMode, ClusterSubcommand, DEFAULT_AUTH_USER, RDB_DISK_ERROR_WRITE_DENIED,
        Runtime, ServerState, acl_list_entries_from_rules, canonical_static_config_param,
        canonicalize_acl_rules, classify_cluster_subcommand, classify_cluster_subcommand_linear,
        classify_runtime_special_command, classify_runtime_special_command_linear,
        client_wrong_subcommand_arity, sha256_hex_bytes, store_to_rdb_entries, wrong_arity_error,
    };

    fn command(parts: &[&[u8]]) -> RespFrame {
        RespFrame::Array(Some(
            parts
                .iter()
                .map(|part| RespFrame::BulkString(Some((*part).to_vec())))
                .collect(),
        ))
    }

    fn parse_all_resp_frames(bytes: &[u8]) -> Vec<RespFrame> {
        let mut frames = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            let parsed = parse_frame(&bytes[offset..]).expect("runtime output must be valid RESP");
            assert!(
                parsed.consumed > 0,
                "runtime output parser must make progress"
            );
            offset += parsed.consumed;
            frames.push(parsed.frame);
        }
        frames
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
        assert_eq!(rt.server.client_timeout_sec, 30);
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
        assert!(rt.server.store.stat_expired_keys >= 1);
        assert!(rt.server.store.stat_expired_stale_perc >= 1);
        assert!(rt.server.store.stat_expire_cycle_cpu_milliseconds <= 10);

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
        assert_eq!(rt.server.store.stat_expired_keys, 20);
        assert_eq!(rt.server.store.stat_expired_stale_perc, 100);

        assert_eq!(
            rt.execute_frame(command(&[b"DBSIZE"]), 10),
            RespFrame::Integer(0)
        );
    }

    #[test]
    fn config_resetstat_clears_expire_and_evict_counters() {
        let mut rt = Runtime::default_strict();
        rt.server.store.stat_total_commands_processed = 9;
        rt.server.store.stat_total_connections_received = 4;
        rt.server.store.stat_unexpected_error_replies = 2;
        rt.server.store.stat_total_error_replies = 5;
        rt.server.store.stat_total_reads_processed = 6;
        rt.server.store.stat_total_writes_processed = 7;
        rt.server.store.stat_expired_keys = 3;
        rt.server.store.stat_evicted_keys = 2;
        rt.server.store.stat_expired_stale_perc = 50;
        rt.server.store.stat_expire_cycle_cpu_milliseconds = 7;
        rt.server.store.stat_keyspace_hits = 11;
        rt.server.store.stat_keyspace_misses = 4;
        rt.server.store.stat_rejected_connections = 2;
        rt.server.store.stat_sync_full = 1;
        rt.server.store.stat_sync_partial_ok = 3;
        rt.server.store.stat_sync_partial_err = 1;
        rt.server.store.stat_used_memory_peak = 999;
        rt.server.store.stat_total_net_input_bytes = 1000;
        rt.server.store.stat_total_net_output_bytes = 2000;

        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"RESETSTAT"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(rt.server.store.stat_total_commands_processed, 0);
        assert_eq!(rt.server.store.stat_total_connections_received, 0);
        assert_eq!(rt.server.store.stat_unexpected_error_replies, 0);
        assert_eq!(rt.server.store.stat_total_error_replies, 0);
        assert_eq!(rt.server.store.stat_total_reads_processed, 0);
        assert_eq!(rt.server.store.stat_total_writes_processed, 0);
        assert_eq!(rt.server.store.stat_expired_keys, 0);
        assert_eq!(rt.server.store.stat_evicted_keys, 0);
        assert_eq!(rt.server.store.stat_expired_stale_perc, 0);
        assert_eq!(rt.server.store.stat_expire_cycle_cpu_milliseconds, 0);
        assert_eq!(rt.server.store.stat_keyspace_hits, 0);
        assert_eq!(rt.server.store.stat_keyspace_misses, 0);
        assert_eq!(rt.server.store.stat_rejected_connections, 0);
        assert_eq!(rt.server.store.stat_sync_full, 0);
        assert_eq!(rt.server.store.stat_sync_partial_ok, 0);
        assert_eq!(rt.server.store.stat_sync_partial_err, 0);
        assert_eq!(rt.server.store.stat_used_memory_peak, 0);
        assert_eq!(rt.server.store.stat_total_net_input_bytes, 0);
        assert_eq!(rt.server.store.stat_total_net_output_bytes, 0);
    }

    #[test]
    fn config_resetstat_clears_info_latencystats_histograms() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"SET", b"latency-tracking", b"yes"]),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"missing"]), 1),
            RespFrame::BulkString(None)
        );

        let before_reset = rt.execute_frame(command(&[b"INFO", b"latencystats"]), 2);
        let RespFrame::BulkString(Some(before_reset_bytes)) = before_reset else {
            panic!("expected bulk info response");
        };
        let before_reset = String::from_utf8(before_reset_bytes).expect("utf8 info");
        assert!(before_reset.contains("latency_percentiles_usec_GET:"));

        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"RESETSTAT"]), 3),
            RespFrame::SimpleString("OK".to_string())
        );

        let after_reset = rt.execute_frame(command(&[b"INFO", b"latencystats"]), 4);
        let RespFrame::BulkString(Some(after_reset_bytes)) = after_reset else {
            panic!("expected bulk info response");
        };
        let after_reset = String::from_utf8(after_reset_bytes).expect("utf8 info");
        assert!(
            !after_reset.contains("latency_percentiles_usec_GET:"),
            "{after_reset}"
        );
    }

    #[test]
    fn total_error_replies_counts_runtime_and_delegated_errors() {
        let mut rt = Runtime::default_strict();
        rt.set_requirepass(Some(b"secret".to_vec()));

        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"k"]), 0),
            RespFrame::Error("NOAUTH Authentication required.".to_string())
        );
        assert_eq!(rt.server.store.stat_total_error_replies, 1);

        assert_eq!(
            rt.execute_frame(command(&[b"AUTH", b"secret"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(rt.server.store.stat_total_error_replies, 1);

        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"k", b"v", b"NX", b"XX"]), 2),
            RespFrame::Error("ERR syntax error".to_string())
        );
        assert_eq!(rt.server.store.stat_total_error_replies, 2);
        assert_eq!(rt.server.store.stat_unexpected_error_replies, 0);
    }

    #[test]
    fn unexpected_error_replies_count_aof_load_and_replication_replay_errors() {
        let mut rt = Runtime::default_strict();

        let replayed = rt
            .replay_aof_stream(b"*2\r\n$4\r\nNOPE\r\n$3\r\nbad\r\n", 0)
            .expect("decodeable replay stream should execute");
        assert_eq!(replayed.len(), 1);
        let Some(RespFrame::Error(message)) = replayed.first() else {
            unreachable!("expected replayed error reply");
        };
        assert!(
            message.starts_with("ERR unknown command 'NOPE'"),
            "{message}"
        );
        assert_eq!(rt.server.store.stat_unexpected_error_replies, 1);
        assert_eq!(rt.server.store.stat_total_error_replies, 1);

        rt.apply_replication_sync_payload("CONTINUE", b"*2\r\n$4\r\nNOPE\r\n$3\r\nbad\r\n", 1)
            .expect("replication payload should decode");
        assert_eq!(rt.server.store.stat_unexpected_error_replies, 2);
        assert_eq!(rt.server.store.stat_total_error_replies, 2);

        let info = rt.execute_frame(command(&[b"INFO", b"stats"]), 2);
        let RespFrame::BulkString(Some(bytes)) = info else {
            unreachable!("expected INFO stats bulk string");
        };
        let info = String::from_utf8(bytes).expect("utf8");
        assert!(info.contains("unexpected_error_replies:2\r\n"), "{info}");
        assert!(info.contains("total_error_replies:2\r\n"), "{info}");
    }

    #[test]
    fn total_reads_and_writes_processed_counts_classified_commands() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"k", b"v"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(rt.server.store.stat_total_reads_processed, 0);
        assert_eq!(rt.server.store.stat_total_writes_processed, 1);

        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"k"]), 1),
            RespFrame::BulkString(Some(b"v".to_vec()))
        );
        assert_eq!(rt.server.store.stat_total_reads_processed, 1);
        assert_eq!(rt.server.store.stat_total_writes_processed, 1);

        assert_eq!(
            rt.execute_frame(command(&[b"PING"]), 2),
            RespFrame::SimpleString("PONG".to_string())
        );
        assert_eq!(rt.server.store.stat_total_reads_processed, 1);
        assert_eq!(rt.server.store.stat_total_writes_processed, 1);

        let info = rt.execute_frame(command(&[b"INFO", b"stats"]), 3);
        let RespFrame::BulkString(Some(bytes)) = info else {
            unreachable!("expected INFO stats bulk string");
        };
        let info = String::from_utf8(bytes).expect("utf8");
        assert!(info.contains("total_reads_processed:1\r\n"));
        assert!(info.contains("total_writes_processed:1\r\n"));
        assert_eq!(rt.server.store.stat_total_reads_processed, 1);
        assert_eq!(rt.server.store.stat_total_writes_processed, 1);
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

        let missing_pass = rt.execute_frame(command(&[b"HELLO", b"3", b"AUTH", b"alice"]), 0);
        assert_eq!(
            missing_pass,
            RespFrame::Error("ERR syntax error".to_string())
        );

        let missing_user_and_pass = rt.execute_frame(command(&[b"HELLO", b"3", b"AUTH"]), 0);
        assert_eq!(
            missing_user_and_pass,
            RespFrame::Error("ERR syntax error".to_string())
        );
    }

    #[test]
    fn hello_uses_last_auth_in_chain() {
        let mut rt = Runtime::default_strict();
        rt.add_user(b"alice".to_vec(), b"secret1".to_vec());
        rt.add_user(b"bob".to_vec(), b"secret2".to_vec());

        let reply = rt.execute_frame(
            command(&[
                b"HELLO", b"2", b"AUTH", b"bob", b"secret2", b"AUTH", b"alice", b"secret1",
            ]),
            0,
        );
        assert!(matches!(reply, RespFrame::Array(Some(_))));
        assert!(rt.is_authenticated());
        assert_eq!(rt.session.current_user_name(), b"alice");
    }

    #[test]
    fn hello_failed_later_auth_does_not_clobber_existing_user_or_name() {
        let mut rt = Runtime::default_strict();
        rt.add_user(b"alice".to_vec(), b"secret1".to_vec());
        rt.add_user(b"bob".to_vec(), b"secret2".to_vec());

        assert_eq!(
            rt.execute_frame(command(&[b"AUTH", b"alice", b"secret1"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"SETNAME", b"client0"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );

        let reply = rt.execute_frame(
            command(&[
                b"HELLO", b"2", b"AUTH", b"alice", b"secret1", b"AUTH", b"bob", b"secret2",
                b"AUTH", b"alice", b"wrong", b"SETNAME", b"client1",
            ]),
            2,
        );
        assert_eq!(
            reply,
            RespFrame::Error(
                "WRONGPASS invalid username-password pair or user is disabled.".to_string()
            )
        );
        assert!(rt.is_authenticated());
        assert_eq!(rt.session.current_user_name(), b"alice");
        assert_eq!(rt.session.client_name, Some(b"client0".to_vec()));
        assert_eq!(rt.session.resp_protocol_version, 2);
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
            other => unreachable!("unexpected client list response: {other:?}"),
        };
        assert!(info.contains("name=alpha"));
        assert!(info.contains("db=5"));
        assert!(info.contains("user=default"));
        assert!(info.contains("resp=3"));
        assert!(info.contains("age=0"));
        assert!(info.contains("idle=0"));
    }

    #[test]
    fn hello_setname_rejects_control_chars_without_leaking_state() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"SETNAME", b"keep"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );

        let reply = rt.execute_frame(command(&[b"HELLO", b"3", b"SETNAME", b"bad\nname"]), 1);
        assert_eq!(
            reply,
            RespFrame::Error(
                "ERR Client names cannot contain spaces, newlines or special characters."
                    .to_string()
            )
        );
        assert_eq!(rt.session.client_name, Some(b"keep".to_vec()));
        assert_eq!(rt.session.resp_protocol_version, 2);
    }

    #[test]
    fn hello_uses_last_setname_in_chain() {
        let mut rt = Runtime::default_strict();

        let reply = rt.execute_frame(
            command(&[
                b"HELLO", b"3", b"SETNAME", b"client1", b"SETNAME", b"client2",
            ]),
            0,
        );
        assert!(matches!(reply, RespFrame::Array(Some(_))));
        assert_eq!(rt.session.client_name, Some(b"client2".to_vec()));
        assert_eq!(rt.session.resp_protocol_version, 3);
    }

    #[test]
    fn client_list_reports_age_and_resets_idle_for_current_command() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"SETNAME", b"alpha"]), 1_000),
            RespFrame::SimpleString("OK".to_string())
        );

        let client_list = rt.execute_frame(command(&[b"CLIENT", b"LIST"]), 3_500);
        let info = match client_list {
            RespFrame::BulkString(Some(info)) => String::from_utf8(info).expect("client info utf8"),
            other => unreachable!("unexpected client list response: {other:?}"),
        };
        assert!(info.contains("age=2"), "expected age seconds in {info}");
        assert!(info.contains("idle=0"), "expected idle reset in {info}");
    }

    #[test]
    fn client_identity_commands_are_rejected_from_eval_scripts_without_leaking_state() {
        let mut rt = Runtime::default_strict();
        rt.session.client_name = Some(b"before".to_vec());
        let id_script = b"return redis.call('CLIENT','ID')";
        let setname_script = b"return redis.call('CLIENT','SETNAME','lua-client')";
        let getname_script = b"return redis.call('CLIENT','GETNAME')";

        assert_eq!(
            rt.execute_frame(command(&[b"EVAL", id_script, b"0"]), 0),
            RespFrame::Error(format!(
                "ERR This Redis command is not allowed from script script: {}, on @user_script:1.",
                sha1_hex_public(id_script)
            ))
        );

        assert_eq!(
            rt.execute_frame(command(&[b"EVAL", setname_script, b"0"]), 1),
            RespFrame::Error(format!(
                "ERR This Redis command is not allowed from script script: {}, on @user_script:1.",
                sha1_hex_public(setname_script)
            ))
        );

        assert_eq!(
            rt.execute_frame(command(&[b"EVAL", getname_script, b"0"]), 2),
            RespFrame::Error(format!(
                "ERR This Redis command is not allowed from script script: {}, on @user_script:1.",
                sha1_hex_public(getname_script)
            ))
        );
        assert_eq!(
            rt.session.client_name.as_deref(),
            Some(b"before".as_slice())
        );
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
        assert_eq!(
            rt.execute_frame(
                command(&[b"CLIENT", b"SETINFO", b"LIB-NAME", b"redis-rs"]),
                2
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"SETINFO", b"LIB-VER", b"1.2.3"]), 2),
            RespFrame::SimpleString("OK".to_string())
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
        assert_eq!(rt.session.client_lib_name.as_deref(), Some("redis-rs"));
        assert_eq!(rt.session.client_lib_ver.as_deref(), Some("1.2.3"));
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
    fn psubscribe_enables_subscription_mode_and_blocks_non_control_commands() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"PSUBSCRIBE", b"news:*"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"psubscribe".to_vec())),
                RespFrame::BulkString(Some(b"news:*".to_vec())),
                RespFrame::Integer(1),
            ]))
        );
        assert!(rt.is_in_subscription_mode());
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"k"]), 1),
            RespFrame::Error(
                "ERR Can't execute 'get': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT / RESET are allowed in this context".to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"PUBLISH", b"news:1", b"payload"]), 2),
            RespFrame::Error(
                "ERR Can't execute 'publish': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT / RESET are allowed in this context".to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"PUBSUB", b"NUMPAT"]), 3),
            RespFrame::Error(
                "ERR Can't execute 'pubsub|numpat': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT / RESET are allowed in this context".to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"PUBSUB", b"HELP"]), 4),
            RespFrame::Error(
                "ERR Can't execute 'pubsub|help': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT / RESET are allowed in this context".to_string()
            )
        );
    }

    #[test]
    fn pubsub_multi_subscribe_encodes_as_resp_sequence() {
        let mut rt = Runtime::default_strict();
        let reply = rt.execute_frame(command(&[b"SUBSCRIBE", b"alpha", b"beta"]), 0);
        let RespFrame::Sequence(items) = &reply else {
            unreachable!("expected RESP sequence, got {reply:?}");
        };
        assert_eq!(items.len(), 2);
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
        let observer = rt.new_session();

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

        let _first_client = rt.swap_session(second_client);
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

        let second_client = rt.swap_session(observer);
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

        let observer = rt.swap_session(second_client);
        let _ = rt.swap_session(observer);
        let _ = rt.swap_session(previous);
    }

    #[test]
    fn live_pubsub_help_and_arity_follow_redis_syntax() {
        let mut rt = Runtime::default_strict();
        let help = rt.execute_frame(command(&[b"PUBSUB", b"HELP"]), 0);
        let RespFrame::Array(Some(lines)) = help else {
            unreachable!("expected pubsub help array");
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
            RespFrame::Error("ERR unknown subcommand 'BOGUS'. Try PUBSUB HELP.".to_string())
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
    fn move_same_db_errors_before_key_lookup_like_redis() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"moveme", b"value"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"MOVE", b"moveme", b"0"]), 1),
            RespFrame::Error("ERR source and destination objects are the same".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"moveme"]), 2),
            RespFrame::BulkString(Some(b"value".to_vec()))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"MOVE", b"missing", b"0"]), 3),
            RespFrame::Error("ERR source and destination objects are the same".to_string())
        );
    }

    #[test]
    fn select_rejects_noncanonical_db_index() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"SELECT", b"+1"]), 0),
            RespFrame::Error("ERR value is not an integer or out of range".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SELECT", b"01"]), 1),
            RespFrame::Error("ERR value is not an integer or out of range".to_string())
        );
    }

    #[test]
    fn scan_count_rejects_noncanonical_integer() {
        let mut rt = Runtime::default_strict();
        let reply = rt.execute_frame(command(&[b"SCAN", b"0", b"COUNT", b"+1"]), 0);
        assert_eq!(
            reply,
            RespFrame::Error("ERR value is not an integer or out of range".to_string())
        );
        let reply = rt.execute_frame(command(&[b"SCAN", b"0", b"COUNT", b"01"]), 1);
        assert_eq!(
            reply,
            RespFrame::Error("ERR value is not an integer or out of range".to_string())
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
                RespFrame::BulkString(Some(b"aclfile".to_vec())),
                RespFrame::BulkString(Some(Vec::new())),
                RespFrame::BulkString(Some(b"acl-pubsub-default".to_vec())),
                RespFrame::BulkString(Some(b"resetchannels".to_vec())),
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
                Err(err) => unreachable!("failed to parse runtime test frame: {err:?}"),
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
                    other => unreachable!("expected bulk string, got {other:?}"),
                })
                .collect(),
            other => unreachable!("expected array frame, got {other:?}"),
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
    fn fr_p2c_005_u003_runtime_aof_stream_preserves_select_boundaries() {
        let mut source = Runtime::default_strict();
        assert_eq!(
            source.execute_frame(command(&[b"SET", b"db0:key", b"zero"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            source.execute_frame(command(&[b"SELECT", b"2"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            source.execute_frame(command(&[b"SET", b"db2:key", b"two"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            source.execute_frame(command(&[b"INCR", b"db2:counter"]), 3),
            RespFrame::Integer(1)
        );
        assert_eq!(
            source.execute_frame(command(&[b"SELECT", b"0"]), 4),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            source.execute_frame(command(&[b"SET", b"db0:other", b"again"]), 5),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            source.execute_frame(command(&[b"SELECT", b"2"]), 6),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            source.execute_frame(command(&[b"SET", b"db2:other", b"twice"]), 7),
            RespFrame::SimpleString("OK".to_string())
        );

        let decoded = decode_aof_stream(&source.encoded_aof_stream()).expect("decode aof stream");
        let propagated: Vec<Vec<Vec<u8>>> =
            decoded.iter().map(|record| record.argv.clone()).collect();
        assert_eq!(
            propagated,
            vec![
                vec![b"SET".to_vec(), b"db0:key".to_vec(), b"zero".to_vec()],
                vec![b"SELECT".to_vec(), b"2".to_vec()],
                vec![b"SET".to_vec(), b"db2:key".to_vec(), b"two".to_vec()],
                vec![b"INCR".to_vec(), b"db2:counter".to_vec()],
                vec![b"SELECT".to_vec(), b"0".to_vec()],
                vec![b"SET".to_vec(), b"db0:other".to_vec(), b"again".to_vec()],
                vec![b"SELECT".to_vec(), b"2".to_vec()],
                vec![b"SET".to_vec(), b"db2:other".to_vec(), b"twice".to_vec()],
            ],
            "AOF propagation must preserve replay-visible SELECT boundaries without redundant same-DB SELECTs"
        );

        let mut target = Runtime::default_strict();
        let replies = target.replay_aof_records(&decoded, 100);
        assert_eq!(replies.len(), decoded.len());

        assert_eq!(
            target.execute_frame(command(&[b"SELECT", b"0"]), 199),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            target.execute_frame(command(&[b"GET", b"db0:key"]), 200),
            RespFrame::BulkString(Some(b"zero".to_vec()))
        );
        assert_eq!(
            target.execute_frame(command(&[b"GET", b"db0:other"]), 201),
            RespFrame::BulkString(Some(b"again".to_vec()))
        );
        assert_eq!(
            target.execute_frame(command(&[b"GET", b"db2:key"]), 202),
            RespFrame::BulkString(None)
        );
        assert_eq!(
            target.execute_frame(command(&[b"SELECT", b"2"]), 203),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            target.execute_frame(command(&[b"GET", b"db2:key"]), 204),
            RespFrame::BulkString(Some(b"two".to_vec()))
        );
        assert_eq!(
            target.execute_frame(command(&[b"GET", b"db2:counter"]), 205),
            RespFrame::BulkString(Some(b"1".to_vec()))
        );
        assert_eq!(
            target.execute_frame(command(&[b"GET", b"db2:other"]), 206),
            RespFrame::BulkString(Some(b"twice".to_vec()))
        );
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
    fn fr_p2c_006_u005_wait_requires_arity_and_matches_redis_integer_taxonomy() {
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

        let negative_numreplicas = rt.execute_frame(command(&[b"WAIT", b"-1", b"0"]), 2);
        assert_eq!(negative_numreplicas, RespFrame::Integer(0));

        let invalid_timeout = rt.execute_frame(command(&[b"WAIT", b"1", b"nope"]), 3);
        assert_eq!(
            invalid_timeout,
            RespFrame::Error("ERR timeout is not an integer or out of range".to_string())
        );

        let negative_timeout = rt.execute_frame(command(&[b"WAIT", b"1", b"-1"]), 4);
        assert_eq!(
            negative_timeout,
            RespFrame::Error("ERR timeout is negative".to_string())
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
    fn fr_p2c_005_u011_waitaof_requires_replica_fack_not_plain_ack() {
        let mut rt = Runtime::default_strict();
        rt.set_aof_path(std::path::PathBuf::from("appendonly.aof"));
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"fr:p2c:005:waitaof", b"value"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"REPLCONF", b"listening-port", b"6380"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        let required_offset = rt.replication_primary_offset().0.to_string();

        assert_eq!(
            rt.execute_frame(
                command(&[b"REPLCONF", b"ACK", required_offset.as_bytes()]),
                2,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        let plain_ack_only = rt.execute_frame(command(&[b"WAITAOF", b"1", b"1", b"0"]), 3);
        assert_eq!(
            plain_ack_only,
            RespFrame::Array(Some(vec![RespFrame::Integer(1), RespFrame::Integer(0)]))
        );

        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"REPLCONF",
                    b"ACK",
                    required_offset.as_bytes(),
                    b"FACK",
                    required_offset.as_bytes(),
                ]),
                4,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        let fack_satisfies_replica_aof_threshold =
            rt.execute_frame(command(&[b"WAITAOF", b"1", b"1", b"0"]), 5);
        assert_eq!(
            fack_satisfies_replica_aof_threshold,
            RespFrame::Array(Some(vec![RespFrame::Integer(1), RespFrame::Integer(1)]))
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
            RespFrame::Error("ERR value is out of range, must be positive".to_string())
        );

        let invalid_timeout = rt.execute_frame(command(&[b"WAITAOF", b"1", b"0", b"-1"]), 1);
        assert_eq!(
            invalid_timeout,
            RespFrame::Error("ERR timeout is negative".to_string())
        );
    }

    #[test]
    fn waitaof_without_appendonly_reports_no_local_ack_even_when_threshold_is_zero() {
        let mut rt = Runtime::default_strict();

        let zero_replicas = rt.execute_frame(command(&[b"WAITAOF", b"0", b"0", b"1"]), 0);
        assert_eq!(
            zero_replicas,
            RespFrame::Array(Some(vec![RespFrame::Integer(0), RespFrame::Integer(0)]))
        );

        let unmet_replicas = rt.execute_frame(command(&[b"WAITAOF", b"0", b"5", b"1"]), 1);
        assert_eq!(
            unmet_replicas,
            RespFrame::Array(Some(vec![RespFrame::Integer(0), RespFrame::Integer(0)]))
        );

        rt.set_aof_path(std::path::PathBuf::from("appendonly.aof"));
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"appendonly", b"no"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );

        let preserved_path_unmet_replicas =
            rt.execute_frame(command(&[b"WAITAOF", b"0", b"5", b"1"]), 3);
        assert_eq!(
            preserved_path_unmet_replicas,
            RespFrame::Array(Some(vec![RespFrame::Integer(0), RespFrame::Integer(0)]))
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
    fn waitaof_without_local_aof_can_still_report_replica_fsync_acks() {
        let mut rt = Runtime::default_strict();
        rt.set_replication_ack_state_for_tests(41, 0, &[41], &[41]);

        let out = rt.execute_frame(command(&[b"WAITAOF", b"0", b"1", b"0"]), 0);
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![RespFrame::Integer(0), RespFrame::Integer(1)]))
        );
    }

    #[test]
    fn waitaof_without_replica_aof_can_still_report_local_ack() {
        let mut rt = Runtime::default_strict();
        rt.set_aof_path(std::path::PathBuf::from("appendonly.aof"));
        rt.set_replication_ack_state_for_tests(41, 41, &[41], &[0]);

        let out = rt.execute_frame(command(&[b"WAITAOF", b"1", b"0", b"0"]), 0);
        assert_eq!(
            out,
            RespFrame::Array(Some(vec![RespFrame::Integer(1), RespFrame::Integer(0)]))
        );
    }

    #[test]
    fn waitaof_appendfsync_always_reports_local_ack_immediately() {
        let mut rt = Runtime::default_strict();
        rt.set_aof_path(std::path::PathBuf::from("appendonly.aof"));

        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"appendfsync", b"always"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"INCR", b"waitaof:always"]), 1),
            RespFrame::Integer(1)
        );
        assert_eq!(
            rt.execute_frame(command(&[b"WAITAOF", b"1", b"0", b"0"]), 2),
            RespFrame::Array(Some(vec![RespFrame::Integer(1), RespFrame::Integer(0)]))
        );
    }

    #[test]
    fn config_set_appendfsync_no_leaves_local_waitaof_unsatisfied() {
        let mut rt = Runtime::default_strict();
        rt.set_aof_path(std::path::PathBuf::from("appendonly.aof"));

        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"appendfsync", b"no"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"INCR", b"waitaof:no-fsync"]), 1),
            RespFrame::Integer(1)
        );
        assert_eq!(
            rt.execute_frame(command(&[b"WAITAOF", b"1", b"0", b"50"]), 2),
            RespFrame::Array(Some(vec![RespFrame::Integer(0), RespFrame::Integer(0)]))
        );

        assert_eq!(
            rt.execute_frame(command(&[b"BGREWRITEAOF"]), 3),
            RespFrame::SimpleString("Background append only file rewriting started".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"WAITAOF", b"1", b"0", b"50"]), 4),
            RespFrame::Array(Some(vec![RespFrame::Integer(1), RespFrame::Integer(0)]))
        );
    }

    #[test]
    fn waitaof_stays_unsatisfied_until_postponed_aof_rewrite_runs() {
        let dir = std::env::temp_dir().join("fr_runtime_waitaof_postponed_aof_rewrite");
        let _ = std::fs::create_dir_all(&dir);

        let mut rt = Runtime::default_strict();
        rt.server.aof_config_path = Some(dir.join("appendonly.aof"));
        rt.server.rdb_bgsave_pid = Some(42);

        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"appendonly", b"yes"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"x", b"y"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"WAITAOF", b"1", b"0", b"10000"]), 2),
            RespFrame::Array(Some(vec![RespFrame::Integer(0), RespFrame::Integer(0)]))
        );

        let info = rt.execute_frame(command(&[b"INFO", b"persistence"]), 3);
        let RespFrame::BulkString(Some(info_bytes)) = info else {
            unreachable!("expected bulk INFO response");
        };
        let info = String::from_utf8(info_bytes).expect("utf8 info");
        assert!(info.contains("aof_rewrite_scheduled:1\r\n"), "{info}");

        rt.server.rdb_bgsave_pid = None;
        rt.maybe_run_scheduled_aof_rewrite(4);

        assert_eq!(
            rt.execute_frame(command(&[b"WAITAOF", b"1", b"0", b"10000"]), 5),
            RespFrame::Array(Some(vec![RespFrame::Integer(1), RespFrame::Integer(0)]))
        );

        let info = rt.execute_frame(command(&[b"INFO", b"persistence"]), 6);
        let RespFrame::BulkString(Some(info_bytes)) = info else {
            unreachable!("expected bulk INFO response");
        };
        let info = String::from_utf8(info_bytes).expect("utf8 info");
        assert!(info.contains("aof_rewrite_scheduled:0\r\n"), "{info}");
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
    fn client_tracking_stateful_info_matches_redis() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"TRACKING", b"ON", b"OPTIN"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"CACHING", b"YES"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"GETREDIR"]), 3),
            RespFrame::Integer(0)
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"TRACKINGINFO"]), 4),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"flags".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"on".to_vec())),
                    RespFrame::BulkString(Some(b"optin".to_vec())),
                    RespFrame::BulkString(Some(b"caching-yes".to_vec())),
                ])),
                RespFrame::BulkString(Some(b"redirect".to_vec())),
                RespFrame::Integer(0),
                RespFrame::BulkString(Some(b"prefixes".to_vec())),
                RespFrame::Array(Some(Vec::new())),
            ]))
        );
        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"CLIENT",
                    b"TRACKING",
                    b"ON",
                    b"NOLOOP",
                    b"PREFIX",
                    b"foo",
                    b"BCAST",
                    b"PREFIX",
                    b"bar",
                ]),
                5,
            ),
            RespFrame::Error("ERR You can't switch BCAST mode on/off before disabling tracking for this client, and then re-enabling it with a different mode.".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"TRACKINGINFO"]), 6),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"flags".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"on".to_vec())),
                    RespFrame::BulkString(Some(b"optin".to_vec())),
                    RespFrame::BulkString(Some(b"caching-yes".to_vec())),
                ])),
                RespFrame::BulkString(Some(b"redirect".to_vec())),
                RespFrame::Integer(0),
                RespFrame::BulkString(Some(b"prefixes".to_vec())),
                RespFrame::Array(Some(Vec::new())),
            ]))
        );
        assert_eq!(
            rt.execute_frame(
                command(&[b"CLIENT", b"TRACKING", b"OFF", b"PREFIX", b"ignored"]),
                7
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"CLIENT",
                    b"TRACKING",
                    b"ON",
                    b"NOLOOP",
                    b"PREFIX",
                    b"foo",
                    b"BCAST",
                    b"PREFIX",
                    b"bar",
                ]),
                8,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"TRACKINGINFO"]), 9),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"flags".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"on".to_vec())),
                    RespFrame::BulkString(Some(b"bcast".to_vec())),
                    RespFrame::BulkString(Some(b"noloop".to_vec())),
                ])),
                RespFrame::BulkString(Some(b"redirect".to_vec())),
                RespFrame::Integer(0),
                RespFrame::BulkString(Some(b"prefixes".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"bar".to_vec())),
                    RespFrame::BulkString(Some(b"foo".to_vec())),
                ])),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"TRACKING", b"OFF"]), 10),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"TRACKINGINFO"]), 11),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"flags".to_vec())),
                RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"off".to_vec()))])),
                RespFrame::BulkString(Some(b"redirect".to_vec())),
                RespFrame::Integer(-1),
                RespFrame::BulkString(Some(b"prefixes".to_vec())),
                RespFrame::Array(Some(Vec::new())),
            ]))
        );
    }

    #[test]
    fn client_caching_requires_tracking_modes_matching_redis() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"CACHING", b"YES"]), 1),
            RespFrame::Error(
                "ERR CLIENT CACHING can be called only when the client is in tracking mode with OPTIN or OPTOUT mode enabled".to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"TRACKING", b"ON", b"OPTIN"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"CACHING", b"YES"]), 3),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"CACHING", b"NO"]), 4),
            RespFrame::Error(
                "ERR CLIENT CACHING NO is only valid when tracking is enabled in OPTOUT mode."
                    .to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"TRACKING", b"ON", b"OPTOUT"]), 5),
            RespFrame::Error(
                "ERR You can't switch OPTIN/OPTOUT mode before disabling tracking for this client, and then re-enabling it with a different mode.".to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"CACHING", b"YES"]), 6),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"TRACKING", b"OFF"]), 7),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"TRACKING", b"ON", b"OPTOUT"]), 8),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"CACHING", b"NO"]), 9),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"CACHING", b"YES"]), 10),
            RespFrame::Error(
                "ERR CLIENT CACHING YES is only valid when tracking is enabled in OPTIN mode."
                    .to_string()
            )
        );
    }

    #[test]
    fn client_tracking_redirect_requires_known_client_and_persists_redirect_id() {
        let mut rt = Runtime::default_strict();
        let peer = rt.new_session();
        rt.record_client_session(&peer);
        let redirect_id = peer.client_id.to_string();

        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"CLIENT",
                    b"TRACKING",
                    b"ON",
                    b"REDIRECT",
                    redirect_id.as_bytes(),
                    b"BCAST",
                    b"PREFIX",
                    b"foo",
                    b"NOLOOP",
                ]),
                1,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"GETREDIR"]), 2),
            RespFrame::Integer(peer.client_id as i64)
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"TRACKINGINFO"]), 3),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"flags".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"on".to_vec())),
                    RespFrame::BulkString(Some(b"bcast".to_vec())),
                    RespFrame::BulkString(Some(b"noloop".to_vec())),
                ])),
                RespFrame::BulkString(Some(b"redirect".to_vec())),
                RespFrame::Integer(peer.client_id as i64),
                RespFrame::BulkString(Some(b"prefixes".to_vec())),
                RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"foo".to_vec()))])),
            ]))
        );
        assert_eq!(
            rt.execute_frame(
                command(&[b"CLIENT", b"TRACKING", b"ON", b"REDIRECT", b"0"]),
                4,
            ),
            RespFrame::Error("ERR The client ID you want redirect to does not exist".to_string())
        );
    }

    #[test]
    fn client_trackinginfo_reports_optout_caching_no() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"TRACKING", b"ON", b"OPTOUT"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"CACHING", b"NO"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"TRACKINGINFO"]), 3),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"flags".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"on".to_vec())),
                    RespFrame::BulkString(Some(b"optout".to_vec())),
                    RespFrame::BulkString(Some(b"caching-no".to_vec())),
                ])),
                RespFrame::BulkString(Some(b"redirect".to_vec())),
                RespFrame::Integer(0),
                RespFrame::BulkString(Some(b"prefixes".to_vec())),
                RespFrame::Array(Some(Vec::new())),
            ]))
        );
    }

    #[test]
    fn client_list_reports_live_pubsub_counts() {
        let mut rt = Runtime::default_strict();
        let observer = rt.new_session();

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

        let previous = rt.swap_session(observer);
        let client_list = rt.execute_frame(command(&[b"CLIENT", b"LIST"]), 4);
        let info = match client_list {
            RespFrame::BulkString(Some(info)) => String::from_utf8(info).expect("client list utf8"),
            other => unreachable!("unexpected client list response: {other:?}"),
        };
        assert!(info.contains("sub=1"));
        assert!(info.contains("psub=1"));
        assert!(info.contains("ssub=1"));
        let _observer = rt.swap_session(previous);
    }

    #[test]
    fn client_list_includes_registered_peer_sessions() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"SETNAME", b"alpha"]), 1_000),
            RespFrame::SimpleString("OK".to_string())
        );

        let mut peer = rt.new_session();
        peer.client_name = Some(b"beta".to_vec());
        peer.selected_db = 4;
        peer.connected_at_ms = 500;
        peer.last_interaction_ms = 750;
        peer.last_command_name = "client|setname".to_string();
        rt.record_client_session(&peer);

        let list_all = rt.execute_frame(command(&[b"CLIENT", b"LIST"]), 4_000);
        let info = match list_all {
            RespFrame::BulkString(Some(info)) => String::from_utf8(info).expect("client list utf8"),
            other => unreachable!("unexpected client list response: {other:?}"),
        };
        assert!(
            info.contains("name=alpha"),
            "missing current client from {info}"
        );
        assert!(
            info.contains("name=beta"),
            "missing peer client from {info}"
        );
        assert!(
            info.contains(&format!("id={}", peer.client_id)),
            "missing peer id from {info}"
        );

        let peer_id = peer.client_id.to_string();
        let list_peer_only = rt.execute_frame(
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"CLIENT".to_vec())),
                RespFrame::BulkString(Some(b"LIST".to_vec())),
                RespFrame::BulkString(Some(b"ID".to_vec())),
                RespFrame::BulkString(Some(peer_id.clone().into_bytes())),
            ])),
            4_001,
        );
        let filtered = match list_peer_only {
            RespFrame::BulkString(Some(info)) => String::from_utf8(info).expect("client list utf8"),
            other => unreachable!("unexpected filtered client list response: {other:?}"),
        };
        assert!(
            filtered.contains("name=beta"),
            "missing peer filter hit from {filtered}"
        );
        assert!(
            !filtered.contains("name=alpha"),
            "current client should not be in peer-only filter: {filtered}"
        );
    }

    #[test]
    fn client_info_rejects_extra_arguments() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"INFO", b"extra"]), 1),
            client_wrong_subcommand_arity("INFO")
        );
    }

    #[test]
    fn client_list_applies_single_client_type_and_id_filters() {
        let mut rt = Runtime::default_strict();
        // Get the actual client ID (varies based on test execution order)
        let id_frame = rt.execute_frame(command(&[b"CLIENT", b"ID"]), 0);
        let client_id = match id_frame {
            RespFrame::Integer(n) => n.to_string(),
            other => unreachable!("CLIENT ID should return integer, got {other:?}"),
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
            client_wrong_subcommand_arity("HELP")
        );
    }

    #[test]
    fn client_reply_state_transitions_match_redis() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"REPLY", b"OFF"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        assert!(rt.session.client_reply.off);
        assert!(!rt.session.client_reply.skip_next);
        assert!(rt.session.client_reply.suppress_current_response);

        assert_eq!(
            rt.execute_frame(command(&[b"PING"]), 2),
            RespFrame::SimpleString("PONG".to_string())
        );
        assert!(rt.session.client_reply.off);
        assert!(!rt.session.client_reply.skip_next);
        assert!(rt.session.client_reply.suppress_current_response);

        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"REPLY", b"ON"]), 3),
            RespFrame::SimpleString("OK".to_string())
        );
        assert!(!rt.session.client_reply.off);
        assert!(!rt.session.client_reply.skip_next);
        assert!(!rt.session.client_reply.suppress_current_response);

        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"REPLY", b"SKIP"]), 4),
            RespFrame::SimpleString("OK".to_string())
        );
        assert!(!rt.session.client_reply.off);
        assert!(rt.session.client_reply.skip_next);
        assert!(rt.session.client_reply.suppress_current_response);

        assert_eq!(
            rt.execute_frame(command(&[b"PING"]), 5),
            RespFrame::SimpleString("PONG".to_string())
        );
        assert!(!rt.session.client_reply.off);
        assert!(!rt.session.client_reply.skip_next);
        assert!(rt.session.client_reply.suppress_current_response);

        assert_eq!(
            rt.execute_frame(command(&[b"PING"]), 6),
            RespFrame::SimpleString("PONG".to_string())
        );
        assert!(!rt.session.client_reply.off);
        assert!(!rt.session.client_reply.skip_next);
        assert!(!rt.session.client_reply.suppress_current_response);
    }

    #[test]
    fn client_subcommand_arity_matches_redis_wording() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"SETNAME"]), 1),
            client_wrong_subcommand_arity("SETNAME")
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"KILL"]), 2),
            client_wrong_subcommand_arity("KILL")
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"PAUSE"]), 3),
            client_wrong_subcommand_arity("PAUSE")
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"TRACKING"]), 4),
            client_wrong_subcommand_arity("TRACKING")
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"CACHING"]), 5),
            client_wrong_subcommand_arity("CACHING")
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"GETREDIR", b"extra"]), 6),
            client_wrong_subcommand_arity("GETREDIR")
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"TRACKINGINFO", b"extra"]), 7),
            client_wrong_subcommand_arity("TRACKINGINFO")
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"UNBLOCK"]), 8),
            client_wrong_subcommand_arity("UNBLOCK")
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"UNPAUSE", b"extra"]), 9),
            client_wrong_subcommand_arity("UNPAUSE")
        );
    }

    #[test]
    fn client_kill_legacy_addr_uses_ok_or_no_such_client() {
        let mut rt = Runtime::default_strict();
        rt.session.peer_addr = Some("10.0.0.9:7777".parse().expect("socket addr"));

        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"KILL", b"10.0.0.9:9999"]), 1),
            RespFrame::Error("ERR No such client".to_string())
        );

        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"KILL", b"10.0.0.9:7777"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(rt.server.pending_client_kills, vec![rt.session.client_id]);
    }

    #[test]
    fn slowlog_get_uses_redis_default_count_and_minus_one_means_all() {
        let mut rt = Runtime::default_strict();
        rt.server.store.slowlog_max_len = 64;
        rt.server.store.slowlog_log_slower_than_us = 0;
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
            unreachable!("expected default slowlog array");
        };
        assert_eq!(default_entries.len(), 10);

        // The first SLOWLOG GET above was also recorded (threshold=0), so 12+1=13
        let all = rt.execute_frame(command(&[b"SLOWLOG", b"GET", b"-1"]), 100);
        let RespFrame::Array(Some(all_entries)) = all else {
            unreachable!("expected all slowlog array");
        };
        assert_eq!(all_entries.len(), 13);
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
    fn config_set_latency_monitor_threshold_updates_live_tracker_and_get() {
        let mut rt = Runtime::default_strict();

        assert_eq!(rt.server.store.latency_tracker.threshold_ms, 0);
        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"SET", b"latency-monitor-threshold", b"7",]),
                1,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(rt.server.store.latency_tracker.threshold_ms, 7);
        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"GET", b"latency-monitor-threshold"]),
                2
            ),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"latency-monitor-threshold".to_vec())),
                RespFrame::BulkString(Some(b"7".to_vec())),
            ]))
        );
    }

    #[test]
    fn runtime_latency_recording_respects_threshold() {
        let mut rt = Runtime::default_strict();

        rt.server.store.latency_tracker.threshold_ms = 1;

        rt.server
            .record_latency_sample(&[b"PING".to_vec()], 1_000, 5_000);
        assert!(rt.server.store.latency_history("fast-command").is_empty());

        rt.server
            .record_latency_sample(&[b"PING".to_vec()], 1_001, 6_000);
        assert_eq!(
            rt.server.store.latency_history("fast-command"),
            vec![fr_store::LatencySample {
                timestamp_sec: 6,
                duration_ms: 2,
            }]
        );

        rt.server.record_latency_sample(
            &[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()],
            2_500,
            7_000,
        );
        assert_eq!(
            rt.server.store.latency_history("command"),
            vec![fr_store::LatencySample {
                timestamp_sec: 7,
                duration_ms: 3,
            }]
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
            RespFrame::Error("ERR CLIENT UNBLOCK reason should be TIMEOUT or ERROR".to_string())
        );
    }

    #[test]
    fn client_unblock_queues_request_for_blocked_client() {
        let mut rt = Runtime::default_strict();
        rt.mark_client_blocked(42);

        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"UNBLOCK", b"42", b"ERROR"]), 1),
            RespFrame::Integer(1)
        );
        assert_eq!(
            rt.drain_pending_client_unblocks(),
            vec![(42, ClientUnblockMode::Error)]
        );
    }

    #[test]
    fn client_unblock_deduplicates_pending_requests() {
        let mut rt = Runtime::default_strict();
        rt.mark_client_blocked(42);

        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"UNBLOCK", b"42", b"ERROR"]), 1),
            RespFrame::Integer(1)
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"UNBLOCK", b"42", b"TIMEOUT"]), 2),
            RespFrame::Integer(0)
        );
        assert_eq!(
            rt.drain_pending_client_unblocks(),
            vec![(42, ClientUnblockMode::Error)]
        );
    }

    #[test]
    fn exec_uses_runtime_client_unblock_path() {
        let mut rt = Runtime::default_strict();
        rt.mark_client_blocked(9);

        assert_eq!(
            rt.execute_frame(command(&[b"MULTI"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"UNBLOCK", b"9"]), 2),
            RespFrame::SimpleString("QUEUED".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"EXEC"]), 3),
            RespFrame::Array(Some(vec![RespFrame::Integer(1)]))
        );
        assert_eq!(
            rt.drain_pending_client_unblocks(),
            vec![(9, ClientUnblockMode::Timeout)]
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
    fn client_pause_rejects_invalid_mode_with_redis_error() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"PAUSE", b"100", b"wat"]), 500),
            RespFrame::Error(CLIENT_PAUSE_MODE_INVALID.to_string())
        );
    }

    #[test]
    fn client_pause_rejects_invalid_timeout_with_redis_error() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"PAUSE", b"notanumber"]), 500),
            RespFrame::Error(CLIENT_PAUSE_TIMEOUT_INVALID.to_string())
        );
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
                RespFrame::Integer(-1),
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
                RespFrame::Integer(-1),
            ]))
        );
    }

    #[test]
    fn failover_is_rejected_when_server_is_a_replica() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"REPLICAOF", b"127.0.0.1", b"6380"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"FAILOVER"]), 1),
            RespFrame::Error("ERR FAILOVER is not valid when server is a replica.".to_string())
        );
    }

    #[test]
    fn failover_promotes_target_replica_and_demotes_self() {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind failover target listener");
        let addr = listener.local_addr().expect("failover target addr");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept failover target");
            let mut read_buf = Vec::new();

            let promote =
                runtime_test_frame_to_argv(read_runtime_test_frame(&mut stream, &mut read_buf));
            assert_eq!(
                promote,
                vec![b"REPLICAOF".to_vec(), b"NO".to_vec(), b"ONE".to_vec()]
            );
            std::io::Write::write_all(
                &mut stream,
                &RespFrame::SimpleString("OK".to_string()).to_bytes(),
            )
            .expect("write failover promote reply");

            let role =
                runtime_test_frame_to_argv(read_runtime_test_frame(&mut stream, &mut read_buf));
            assert_eq!(role, vec![b"ROLE".to_vec()]);
            std::io::Write::write_all(
                &mut stream,
                &RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"master".to_vec())),
                    RespFrame::Integer(0),
                    RespFrame::Array(Some(Vec::new())),
                ]))
                .to_bytes(),
            )
            .expect("write failover role reply");
        });

        let mut rt = Runtime::default_strict();
        rt.server
            .replication_runtime_state
            .ensure_replica(42)
            .listening_port = addr.port();
        rt.server
            .replication_runtime_state
            .ensure_replica(42)
            .ip_address = Some("127.0.0.1".to_string());
        rt.server
            .replication_runtime_state
            .ensure_replica(42)
            .ack_offset = crate::ReplOffset(128);

        let port = addr.port().to_string();
        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"FAILOVER",
                    b"TO",
                    b"127.0.0.1",
                    port.as_bytes(),
                    b"FORCE",
                    b"TIMEOUT",
                    b"5000",
                ]),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"ROLE"]), 1),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"slave".to_vec())),
                RespFrame::BulkString(Some(b"127.0.0.1".to_vec())),
                RespFrame::Integer(i64::from(addr.port())),
                RespFrame::BulkString(Some(b"connect".to_vec())),
                RespFrame::Integer(-1),
            ]))
        );
        assert_eq!(
            rt.replica_sync_target(),
            Some(("127.0.0.1".to_string(), addr.port()))
        );
        assert!(rt.take_replica_reconfigure_request());
        assert_eq!(
            rt.server
                .replication_runtime_state
                .replicas
                .get(&42)
                .expect("replica still tracked")
                .listening_port,
            addr.port()
        );

        server.join().expect("failover target thread");
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
            unreachable!("expected bulk INFO response");
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
            unreachable!("expected bulk INFO response");
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
            unreachable!("expected bulk INFO response");
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
    fn config_set_repl_backlog_size_recomputes_window_without_new_writes() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"key1", b"value1"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"key2", b"value2"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        let primary_offset = rt.replication_primary_offset().0;

        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"repl-backlog-size", b"1"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );

        let info = rt.execute_frame(command(&[b"INFO", b"replication"]), 3);
        let RespFrame::BulkString(Some(info_bytes)) = info else {
            unreachable!("expected bulk INFO response");
        };
        let info = String::from_utf8(info_bytes).expect("utf8 info");
        assert!(info.contains("repl_backlog_size:1\r\n"), "{info}");
        assert!(
            info.contains(&format!(
                "repl_backlog_first_byte_offset:{primary_offset}\r\n"
            )),
            "{info}"
        );
    }

    #[test]
    fn repl_backlog_histlen_is_zero_when_backlog_disabled() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"repl-backlog-size", b"0"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"key", b"value"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );

        let info = rt.execute_frame(command(&[b"INFO", b"replication"]), 2);
        let RespFrame::BulkString(Some(info_bytes)) = info else {
            unreachable!("expected bulk INFO response");
        };
        let info = String::from_utf8(info_bytes).expect("utf8 info");
        assert!(info.contains("repl_backlog_size:0\r\n"), "{info}");
        assert!(info.contains("repl_backlog_histlen:0\r\n"), "{info}");
    }

    #[test]
    fn cleanup_disconnected_client_clears_replica_and_monitor_state() {
        let mut rt = Runtime::default_strict();
        let client_id = rt.session.client_id;

        let _ = rt.execute_frame(command(&[b"PSYNC", b"?", b"-1"]), 0);
        assert!(
            rt.server
                .replication_runtime_state
                .replicas
                .contains_key(&client_id)
        );
        assert_eq!(rt.server.replication_ack_state.replica_ack_offsets.len(), 1);

        rt.enable_monitor();
        assert!(rt.server.monitor_clients.contains(&client_id));

        rt.cleanup_disconnected_client(client_id);
        assert!(
            !rt.server
                .replication_runtime_state
                .replicas
                .contains_key(&client_id)
        );
        assert!(
            rt.server
                .replication_ack_state
                .replica_ack_offsets
                .is_empty()
        );
        assert!(!rt.server.monitor_clients.contains(&client_id));
    }

    #[test]
    fn replication_info_reports_replica_link_fields() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"REPLICAOF", b"127.0.0.1", b"6380"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );

        let info = rt.execute_frame(command(&[b"INFO", b"replication"]), 1);
        let RespFrame::BulkString(Some(info_bytes)) = info else {
            unreachable!("expected bulk INFO response");
        };
        let info = String::from_utf8(info_bytes).expect("utf8 info");
        assert!(info.contains("role:slave\r\n"), "{info}");
        assert!(info.contains("master_host:127.0.0.1\r\n"), "{info}");
        assert!(info.contains("master_port:6380\r\n"), "{info}");
        assert!(info.contains("master_link_status:down\r\n"), "{info}");
        assert!(info.contains("master_last_io_seconds_ago:-1\r\n"), "{info}");
        assert!(info.contains("master_sync_in_progress:0\r\n"), "{info}");
        assert!(info.contains("slave_read_repl_offset:0\r\n"), "{info}");
        assert!(info.contains("slave_repl_offset:0\r\n"), "{info}");

        rt.set_replica_connection_state("sync");
        let sync_info = rt.execute_frame(command(&[b"INFO", b"replication"]), 2);
        let RespFrame::BulkString(Some(sync_info_bytes)) = sync_info else {
            unreachable!("expected bulk INFO response");
        };
        let sync_info = String::from_utf8(sync_info_bytes).expect("utf8 info");
        assert!(
            sync_info.contains("master_link_status:down\r\n"),
            "{sync_info}"
        );
        assert!(
            sync_info.contains("master_sync_in_progress:1\r\n"),
            "{sync_info}"
        );

        rt.set_replica_connection_state("connected");
        let connected_info = rt.execute_frame(command(&[b"INFO", b"replication"]), 3);
        let RespFrame::BulkString(Some(connected_info_bytes)) = connected_info else {
            unreachable!("expected bulk INFO response");
        };
        let connected_info = String::from_utf8(connected_info_bytes).expect("utf8 info");
        assert!(
            connected_info.contains("master_link_status:up\r\n"),
            "{connected_info}"
        );
        assert!(
            connected_info.contains("master_last_io_seconds_ago:0\r\n"),
            "{connected_info}"
        );
    }

    #[test]
    fn replication_info_lists_downstream_replicas_even_on_intermediate_replica() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"REPLICAOF", b"127.0.0.1", b"6380"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        rt.set_replica_connection_state("connected");
        assert_eq!(
            rt.execute_frame(command(&[b"REPLCONF", b"listening-port", b"6381"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );

        let info = rt.execute_frame(command(&[b"INFO", b"replication"]), 2);
        let RespFrame::BulkString(Some(info_bytes)) = info else {
            unreachable!("expected bulk INFO response");
        };
        let info = String::from_utf8(info_bytes).expect("utf8 info");
        assert!(info.contains("role:slave\r\n"), "{info}");
        assert!(info.contains("master_host:127.0.0.1\r\n"), "{info}");
        assert!(info.contains("master_port:6380\r\n"), "{info}");
        assert!(info.contains("master_link_status:up\r\n"), "{info}");
        assert!(
            info.contains("slave0:ip=127.0.0.1,port=6381,state=online,offset=0,lag=0\r\n"),
            "{info}"
        );
        assert!(info.contains("connected_slaves:1\r\n"), "{info}");
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
        assert_eq!(replica.fsync_offset, fr_repl::ReplOffset(0));
    }

    #[test]
    fn replication_replconf_matches_legacy_arity_and_unknown_option_behavior() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"REPLCONF"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"REPLCONF", b"ACK"]), 1),
            RespFrame::Error("ERR syntax error".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"REPLCONF", b"UNKNOWN", b"something"]), 2),
            RespFrame::Error("ERR Unrecognized REPLCONF option: UNKNOWN".to_string())
        );
    }

    #[test]
    fn replication_replicaof_invalid_port_uses_redis_error_text() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"REPLICAOF", b"127.0.0.1", b"+6379"]), 0),
            RespFrame::Error("ERR Invalid master port".to_string())
        );
    }

    #[test]
    fn replication_slaveof_wrong_arity_uses_alias_name() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"SLAVEOF"]), 0),
            RespFrame::Error("ERR wrong number of arguments for 'slaveof' command".to_string())
        );
    }

    #[test]
    fn replication_psync_accepts_trailing_arguments_like_legacy_redis() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"PSYNC", b"?", b"-1", b"extra"]), 0),
            RespFrame::SimpleString(
                "FULLRESYNC 0000000000000000000000000000000000000000 0".to_string()
            )
        );
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
            other => unreachable!("expected fullresync, got {other:?}"),
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
            other => unreachable!("expected fullresync, got {other:?}"),
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
    fn replication_continue_apply_accepts_psync2_replid_suffix() {
        let mut primary = Runtime::default_strict();
        assert_eq!(
            primary.execute_frame(command(&[b"SET", b"alpha", b"1"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        let fullresync = match primary.execute_frame(command(&[b"PSYNC", b"?", b"-1"]), 1) {
            RespFrame::SimpleString(line) => line,
            other => unreachable!("expected fullresync, got {other:?}"),
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
        replica
            .apply_replication_sync_payload(
                "CONTINUE 1111111111111111111111111111111111111111",
                &backlog,
                5,
            )
            .expect("apply psync2-style continue backlog");

        assert_eq!(
            replica.execute_frame(command(&[b"GET", b"alpha"]), 6),
            RespFrame::BulkString(Some(b"1".to_vec()))
        );
        assert_eq!(
            replica.execute_frame(command(&[b"GET", b"beta"]), 7),
            RespFrame::BulkString(Some(b"2".to_vec()))
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
            b"FAILOVER",
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
                    Err(err) => unreachable!("unexpected linear classifier error: {err:?}"),
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
                    Err(err) => unreachable!("unexpected optimized classifier error: {err:?}"),
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

        let cluster_disabled =
            RespFrame::Error("ERR This instance has cluster support disabled".to_string());

        let help = rt.execute_frame(command(&[b"CLUSTER", b"HELP"]), 0);
        assert_eq!(help, cluster_disabled);

        let info = rt.execute_frame(command(&[b"CLUSTER", b"INFO"]), 0);
        assert_eq!(info, cluster_disabled);

        let myid = rt.execute_frame(command(&[b"CLUSTER", b"MYID"]), 0);
        assert_eq!(myid, cluster_disabled);

        let slots = rt.execute_frame(command(&[b"CLUSTER", b"SLOTS"]), 0);
        assert_eq!(slots, cluster_disabled);

        let shards = rt.execute_frame(command(&[b"CLUSTER", b"SHARDS"]), 0);
        assert_eq!(shards, cluster_disabled);

        let nodes = rt.execute_frame(command(&[b"CLUSTER", b"NODES"]), 0);
        assert_eq!(nodes, cluster_disabled);

        let keyslot = rt.execute_frame(command(&[b"CLUSTER", b"KEYSLOT", b"foo"]), 0);
        assert_eq!(keyslot, cluster_disabled);

        let getkeysinslot =
            rt.execute_frame(command(&[b"CLUSTER", b"GETKEYSINSLOT", b"12182", b"10"]), 0);
        assert_eq!(getkeysinslot, cluster_disabled);

        let countkeysinslot =
            rt.execute_frame(command(&[b"CLUSTER", b"COUNTKEYSINSLOT", b"12182"]), 0);
        assert_eq!(countkeysinslot, cluster_disabled);

        let reset = rt.execute_frame(command(&[b"CLUSTER", b"RESET"]), 0);
        assert_eq!(reset, cluster_disabled);

        let unknown = rt.execute_frame(command(&[b"CLUSTER", b"NOPE"]), 0);
        assert_eq!(
            unknown,
            RespFrame::Error("ERR unknown subcommand 'NOPE'. Try CLUSTER HELP.".to_string())
        );

        let keyslot_wrong_arity = rt.execute_frame(command(&[b"CLUSTER", b"KEYSLOT"]), 0);
        assert_eq!(
            keyslot_wrong_arity,
            RespFrame::Error(
                "ERR wrong number of arguments for 'cluster|keyslot' command".to_string()
            )
        );
    }

    #[test]
    fn cluster_myid_returns_node_id_when_cluster_is_enabled() {
        let mut rt = Runtime::default_strict();
        rt.server.store.cluster_enabled = true;

        let myid = rt.execute_frame(command(&[b"CLUSTER", b"MYID"]), 0);
        assert_eq!(
            myid,
            RespFrame::BulkString(Some(rt.server.store.server_run_id.as_bytes().to_vec()))
        );
    }

    #[test]
    fn cluster_myshardid_returns_stable_shard_id_when_cluster_is_enabled() {
        let mut rt = Runtime::default_strict();
        rt.server.store.cluster_enabled = true;

        let myshardid = rt.execute_frame(command(&[b"CLUSTER", b"MYSHARDID"]), 0);
        assert_eq!(
            myshardid,
            RespFrame::BulkString(Some(rt.server.store.cluster_shard_id.as_bytes().to_vec()))
        );

        let myshardid_again = rt.execute_frame(command(&[b"CLUSTER", b"MYSHARDID"]), 1);
        assert_eq!(myshardid_again, myshardid);
        assert_eq!(rt.server.store.cluster_shard_id.len(), 40);
    }

    #[test]
    fn cluster_shards_returns_enabled_topology() {
        let mut rt = Runtime::default_strict();
        rt.server.store.cluster_enabled = true;

        let shards = rt.execute_frame(command(&[b"CLUSTER", b"SHARDS"]), 0);
        assert_eq!(
            shards,
            RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"slots".to_vec())),
                RespFrame::Array(Some(vec![RespFrame::Integer(0), RespFrame::Integer(16383)])),
                RespFrame::BulkString(Some(b"nodes".to_vec())),
                RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"id".to_vec())),
                    RespFrame::BulkString(Some(rt.server.store.server_run_id.as_bytes().to_vec())),
                    RespFrame::BulkString(Some(b"port".to_vec())),
                    RespFrame::Integer(i64::from(rt.server.store.server_port)),
                    RespFrame::BulkString(Some(b"ip".to_vec())),
                    RespFrame::BulkString(Some(b"127.0.0.1".to_vec())),
                    RespFrame::BulkString(Some(b"endpoint".to_vec())),
                    RespFrame::BulkString(Some(b"127.0.0.1".to_vec())),
                    RespFrame::BulkString(Some(b"role".to_vec())),
                    RespFrame::BulkString(Some(b"master".to_vec())),
                    RespFrame::BulkString(Some(b"replication-offset".to_vec())),
                    RespFrame::Integer(0),
                    RespFrame::BulkString(Some(b"health".to_vec())),
                    RespFrame::BulkString(Some(b"online".to_vec())),
                ]))])),
            ]))]))
        );
    }

    #[test]
    fn fr_p2c_007_u007_client_cluster_mode_flags_transition_cleanly() {
        let mut rt = Runtime::default_strict();
        assert!(!rt.is_cluster_read_only());
        assert!(!rt.is_cluster_asking());

        let readonly = rt.execute_frame(command(&[b"READONLY"]), 0);
        assert_eq!(
            readonly,
            RespFrame::Error("ERR This instance has cluster support disabled".to_string())
        );
        assert!(!rt.is_cluster_read_only());
        assert!(!rt.is_cluster_asking());

        let asking = rt.execute_frame(command(&[b"ASKING"]), 0);
        assert_eq!(
            asking,
            RespFrame::Error("ERR This instance has cluster support disabled".to_string())
        );
        assert!(!rt.is_cluster_read_only());
        assert!(!rt.is_cluster_asking());

        let readwrite = rt.execute_frame(command(&[b"READWRITE"]), 0);
        assert_eq!(
            readwrite,
            RespFrame::Error("ERR This instance has cluster support disabled".to_string())
        );
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

        let no_args = rt.execute_frame(command(&[b"NOPE"]), 1);
        assert_eq!(
            no_args,
            RespFrame::Error("ERR unknown command 'NOPE', with args beginning with: ".to_string())
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
    fn execute_bytes_matches_execute_frame_for_sequence_replies() {
        let raw = command(&[b"SUBSCRIBE", b"alpha", b"beta"]).to_bytes();
        let mut bytes_runtime = Runtime::default_strict();
        let mut frame_runtime = Runtime::default_strict();

        let bytes_reply = parse_all_resp_frames(&bytes_runtime.execute_bytes(&raw, 0));
        let frame_reply = parse_all_resp_frames(
            &frame_runtime
                .execute_frame(command(&[b"SUBSCRIBE", b"alpha", b"beta"]), 0)
                .to_bytes(),
        );

        assert_eq!(bytes_reply, frame_reply);
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
            session_caching: true,
            session_cache_size: 20 * 1024,
            session_cache_timeout_sec: 300,
            cluster_announce_tls_port: Some(16380),
            max_new_tls_connections_per_cycle: 64,
        }
    }

    fn unique_temp_path(prefix: &str, extension: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "{prefix}_{}_{}.{}",
            std::process::id(),
            unique,
            extension
        ))
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
    fn multi_extra_args_match_redis_execabort_flow() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"MULTI", b"extra"]), 0),
            wrong_arity_error("multi")
        );
        assert_eq!(
            rt.execute_frame(command(&[b"EXEC", b"extra"]), 1),
            RespFrame::Error(
                "EXECABORT Transaction discarded because of: wrong number of arguments for 'exec' command"
                    .to_string(),
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"MULTI"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"DISCARD"]), 3),
            RespFrame::SimpleString("OK".to_string())
        );
    }

    #[test]
    fn discard_extra_args_preserve_tainted_transaction_until_exec() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"MULTI"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"DISCARD", b"extra"]), 1),
            wrong_arity_error("discard")
        );
        assert_eq!(
            rt.execute_frame(command(&[b"multi"]), 2),
            RespFrame::Error("ERR MULTI calls can not be nested".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"exec"]), 3),
            RespFrame::Error(
                "EXECABORT Transaction discarded because of previous errors.".to_string(),
            )
        );
    }

    #[test]
    fn unknown_command_inside_multi_includes_args_preview() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"MULTI"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"NOPE", b"a", b"b"]), 1),
            RespFrame::Error(
                "ERR unknown command 'NOPE', with args beginning with: 'a' 'b' ".to_string(),
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"EXEC"]), 2),
            RespFrame::Error(
                "EXECABORT Transaction discarded because of previous errors.".to_string(),
            )
        );
    }

    #[test]
    fn min_replicas_blocks_writes_without_healthy_replicas() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"CONFIG",
                    b"SET",
                    b"min-replicas-to-write",
                    b"1",
                    b"min-replicas-max-lag",
                    b"10",
                ]),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"blocked", b"value"]), 1_000),
            RespFrame::Error("NOREPLICAS Not enough good replicas to write.".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"blocked"]), 2_000),
            RespFrame::BulkString(None)
        );
    }

    #[test]
    fn min_replicas_rejects_multi_writes_without_tainting_exec() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"SET", b"min-replicas-to-write", b"1"]),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"MULTI"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"blocked", b"value"]), 2_000),
            RespFrame::Error("NOREPLICAS Not enough good replicas to write.".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"EXEC"]), 3_000),
            RespFrame::Array(Some(vec![]))
        );
    }

    #[test]
    fn min_replicas_max_lag_zero_disables_write_gate() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"CONFIG",
                    b"SET",
                    b"min-replicas-to-write",
                    b"1",
                    b"min-replicas-max-lag",
                    b"0",
                ]),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"allowed", b"value"]), 1_000),
            RespFrame::SimpleString("OK".to_string())
        );
    }

    #[test]
    fn min_replicas_allows_writes_after_recent_replica_ack() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"CONFIG",
                    b"SET",
                    b"min-replicas-to-write",
                    b"1",
                    b"min-replicas-max-lag",
                    b"10",
                ]),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        let replica = rt.server.replication_runtime_state.ensure_replica(42);
        replica.last_ack_timestamp_ms = 9_000;

        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"allowed", b"value"]), 10_000),
            RespFrame::SimpleString("OK".to_string())
        );
    }

    #[test]
    fn config_set_replica_serve_stale_data_round_trips_live_values_and_alias() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"GET", b"replica-serve-stale-data"]),
                0
            ),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"replica-serve-stale-data".to_vec())),
                RespFrame::BulkString(Some(b"yes".to_vec())),
            ]))
        );

        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"SET", b"slave-serve-stale-data", b"no"]),
                1,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert!(!rt.server.replica_serve_stale_data);
        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"CONFIG",
                    b"GET",
                    b"replica-serve-stale-data",
                    b"slave-serve-stale-data",
                ]),
                2,
            ),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"replica-serve-stale-data".to_vec())),
                RespFrame::BulkString(Some(b"no".to_vec())),
                RespFrame::BulkString(Some(b"slave-serve-stale-data".to_vec())),
                RespFrame::BulkString(Some(b"no".to_vec())),
            ]))
        );
    }

    #[test]
    fn stale_replica_blocks_data_commands_when_replica_serve_stale_data_is_disabled() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"REPLICAOF", b"127.0.0.1", b"6380"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"SET", b"replica-serve-stale-data", b"no"]),
                1,
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        let expected = RespFrame::Error(
            "MASTERDOWN Link with MASTER is down and replica-serve-stale-data is set to 'no'."
                .to_string(),
        );
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"blocked"]), 2),
            expected
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"blocked", b"value"]), 3),
            expected
        );
    }

    #[test]
    fn stale_replica_allows_control_plane_commands_to_reconfigure_or_inspect_state() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"REPLICAOF", b"127.0.0.1", b"6380"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"SET", b"replica-serve-stale-data", b"no"]),
                1,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"ROLE"]), 2),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"slave".to_vec())),
                RespFrame::BulkString(Some(b"127.0.0.1".to_vec())),
                RespFrame::Integer(6380),
                RespFrame::BulkString(Some(b"connect".to_vec())),
                RespFrame::Integer(-1),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"REPLICAOF", b"NO", b"ONE"]), 3),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"missing"]), 4),
            RespFrame::BulkString(None)
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
        assert_eq!(rt.server.maxmemory_bytes, 1_073_741_824);
        assert_eq!(rt.server.store.maxmemory_bytes_live, 1_073_741_824);
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
    fn maxmemory_enforcement_skips_reads() {
        let mut rt = Runtime::default_strict();
        rt.configure_maxmemory_enforcement(64, 0, 5, 4);
        assert!(rt.last_eviction_loop_result().is_none());

        let reply = rt.execute_frame(command(&[b"GET", b"missing"]), 0);
        assert_eq!(reply, RespFrame::BulkString(None));
        assert!(rt.last_eviction_loop_result().is_none());
    }

    #[test]
    fn info_clients_reads_blocked_clients_from_runtime_context() {
        let mut rt = Runtime::default_strict();
        rt.mark_client_blocked(41);
        rt.mark_client_blocked(42);

        let out = rt.execute_frame(command(&[b"INFO", b"clients"]), 0);
        let RespFrame::BulkString(Some(bytes)) = out else {
            unreachable!("expected bulk string");
        };
        let info = String::from_utf8(bytes).expect("utf8");
        assert!(info.contains("blocked_clients:2\r\n"));
        assert!(info.contains("tracking_clients:0\r\n"));
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
    fn config_set_master_credentials_reconfigures_replica_link() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"REPLICAOF", b"127.0.0.1", b"6380"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        let _ = rt.take_replica_reconfigure_request();
        rt.set_replica_connection_state("connected");

        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"CONFIG",
                    b"SET",
                    b"masteruser",
                    b"replica",
                    b"masterauth",
                    b"secret",
                ]),
                1,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"GET", b"masteruser", b"masterauth"]),
                2
            ),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"masteruser".to_vec())),
                RespFrame::BulkString(Some(b"replica".to_vec())),
                RespFrame::BulkString(Some(b"masterauth".to_vec())),
                RespFrame::BulkString(Some(b"secret".to_vec())),
            ]))
        );
        assert_eq!(
            rt.replica_primary_auth(),
            Some((Some(b"replica".to_vec()), b"secret".to_vec()))
        );
        assert_eq!(
            rt.replica_sync_target(),
            Some(("127.0.0.1".to_string(), 6380))
        );
        assert!(rt.take_replica_reconfigure_request());
    }

    #[test]
    fn canonical_static_config_param_lookup_is_case_insensitive() {
        assert_eq!(
            canonical_static_config_param("appendonly"),
            Some("appendonly")
        );
        assert_eq!(
            canonical_static_config_param("APPENDONLY"),
            Some("appendonly")
        );
        assert_eq!(canonical_static_config_param("does-not-exist"), None);
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
    fn config_set_aclfile_round_trips_live_value() {
        let mut rt = Runtime::default_strict();
        let aclfile_path = unique_temp_path("fr_runtime_aclfile_config", "acl");

        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"CONFIG",
                    b"SET",
                    b"aclfile",
                    aclfile_path.to_string_lossy().as_bytes(),
                ]),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"aclfile"]), 1),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"aclfile".to_vec())),
                RespFrame::BulkString(Some(aclfile_path.to_string_lossy().as_bytes().to_vec())),
            ]))
        );

        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"aclfile", b""]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"aclfile"]), 3),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"aclfile".to_vec())),
                RespFrame::BulkString(Some(Vec::new())),
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
    fn config_set_rejects_immutable_parameters() {
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
        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"SET", b"cluster-enabled", b"yes"]),
                0
            ),
            RespFrame::Error(
                "ERR CONFIG SET failed (possibly related to argument 'cluster-enabled') - can't set immutable config"
                    .to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"cluster-enabled"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"cluster-enabled".to_vec())),
                RespFrame::BulkString(Some(b"no".to_vec())),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[
                b"CONFIG",
                b"SET",
                b"cluster-config-file",
                b"custom-nodes.conf",
            ]), 0),
            RespFrame::Error(
                "ERR CONFIG SET failed (possibly related to argument 'cluster-config-file') - can't set immutable config"
                    .to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"cluster-config-file"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"cluster-config-file".to_vec())),
                RespFrame::BulkString(Some(b"nodes.conf".to_vec())),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"io-threads", b"4"]), 0),
            RespFrame::Error(
                "ERR CONFIG SET failed (possibly related to argument 'io-threads') - can't set immutable config"
                    .to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"io-threads"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"io-threads".to_vec())),
                RespFrame::BulkString(Some(b"1".to_vec())),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[
                b"CONFIG",
                b"SET",
                b"io-threads-do-reads",
                b"yes",
            ]), 0),
            RespFrame::Error(
                "ERR CONFIG SET failed (possibly related to argument 'io-threads-do-reads') - can't set immutable config"
                    .to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"io-threads-do-reads"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"io-threads-do-reads".to_vec())),
                RespFrame::BulkString(Some(b"no".to_vec())),
            ]))
        );
        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"SET", b"enable-protected-configs", b"yes"]),
                0
            ),
            RespFrame::Error(
                "ERR CONFIG SET failed (possibly related to argument 'enable-protected-configs') - can't set immutable config"
                    .to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"GET", b"enable-protected-configs"]),
                0
            ),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"enable-protected-configs".to_vec())),
                RespFrame::BulkString(Some(b"no".to_vec())),
            ]))
        );
        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"SET", b"enable-debug-command", b"yes"]),
                0
            ),
            RespFrame::Error(
                "ERR CONFIG SET failed (possibly related to argument 'enable-debug-command') - can't set immutable config"
                    .to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"enable-debug-command"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"enable-debug-command".to_vec())),
                RespFrame::BulkString(Some(b"no".to_vec())),
            ]))
        );
        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"SET", b"enable-module-command", b"yes"]),
                0
            ),
            RespFrame::Error(
                "ERR CONFIG SET failed (possibly related to argument 'enable-module-command') - can't set immutable config"
                    .to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"enable-module-command"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"enable-module-command".to_vec())),
                RespFrame::BulkString(Some(b"no".to_vec())),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"databases", b"32"]), 0),
            RespFrame::Error(
                "ERR CONFIG SET failed (possibly related to argument 'databases') - can't set immutable config"
                    .to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"databases"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"databases".to_vec())),
                RespFrame::BulkString(Some(b"16".to_vec())),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"tcp-backlog", b"1024"]), 0),
            RespFrame::Error(
                "ERR CONFIG SET failed (possibly related to argument 'tcp-backlog') - can't set immutable config"
                    .to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"tcp-backlog"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"tcp-backlog".to_vec())),
                RespFrame::BulkString(Some(b"511".to_vec())),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[
                b"CONFIG",
                b"SET",
                b"unixsocket",
                b"/tmp/redis.sock",
            ]), 0),
            RespFrame::Error(
                "ERR CONFIG SET failed (possibly related to argument 'unixsocket') - can't set immutable config"
                    .to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"unixsocket"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"unixsocket".to_vec())),
                RespFrame::BulkString(Some(Vec::new())),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"unixsocketperm", b"700"]), 0),
            RespFrame::Error(
                "ERR CONFIG SET failed (possibly related to argument 'unixsocketperm') - can't set immutable config"
                    .to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"unixsocketperm"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"unixsocketperm".to_vec())),
                RespFrame::BulkString(Some(b"0".to_vec())),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"rdbchecksum", b"no"]), 0),
            RespFrame::Error(
                "ERR CONFIG SET failed (possibly related to argument 'rdbchecksum') - can't set immutable config"
                    .to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"rdbchecksum"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"rdbchecksum".to_vec())),
                RespFrame::BulkString(Some(b"yes".to_vec())),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"daemonize", b"yes"]), 0),
            RespFrame::Error(
                "ERR CONFIG SET failed (possibly related to argument 'daemonize') - can't set immutable config"
                    .to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"daemonize"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"daemonize".to_vec())),
                RespFrame::BulkString(Some(b"no".to_vec())),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"logfile", b"/tmp/redis.log"]), 0),
            RespFrame::Error(
                "ERR CONFIG SET failed (possibly related to argument 'logfile') - can't set immutable config"
                    .to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"logfile"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"logfile".to_vec())),
                RespFrame::BulkString(Some(Vec::new())),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"pidfile", b"/tmp/redis.pid"]), 0),
            RespFrame::Error(
                "ERR CONFIG SET failed (possibly related to argument 'pidfile') - can't set immutable config"
                    .to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"pidfile"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"pidfile".to_vec())),
                RespFrame::BulkString(Some(Vec::new())),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"always-show-logo", b"no"]), 0),
            RespFrame::Error(
                "ERR CONFIG SET failed (possibly related to argument 'always-show-logo') - can't set immutable config"
                    .to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"always-show-logo"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"always-show-logo".to_vec())),
                RespFrame::BulkString(Some(b"yes".to_vec())),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"set-proc-title", b"no"]), 0),
            RespFrame::Error(
                "ERR CONFIG SET failed (possibly related to argument 'set-proc-title') - can't set immutable config"
                    .to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"set-proc-title"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"set-proc-title".to_vec())),
                RespFrame::BulkString(Some(b"yes".to_vec())),
            ]))
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
                "ERR wrong number of arguments for 'config|help' command".to_string()
            )
        );
    }

    #[test]
    fn config_subcommand_arity_matches_redis_command_wording() {
        let mut rt = Runtime::default_strict();

        for (argv, expected) in [
            (
                command(&[b"CONFIG", b"GET"]),
                "ERR wrong number of arguments for 'config|get' command",
            ),
            (
                command(&[b"CONFIG", b"SET", b"maxmemory"]),
                "ERR wrong number of arguments for 'config|set' command",
            ),
            (
                command(&[b"CONFIG", b"RESETSTAT", b"extra"]),
                "ERR wrong number of arguments for 'config|resetstat' command",
            ),
            (
                command(&[b"CONFIG", b"REWRITE", b"extra"]),
                "ERR wrong number of arguments for 'config|rewrite' command",
            ),
        ] {
            assert_eq!(
                rt.execute_frame(argv, 0),
                RespFrame::Error(expected.to_string())
            );
        }
    }

    #[test]
    fn config_rewrite_requires_loaded_config_file() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"REWRITE"]), 0),
            RespFrame::Error("ERR The server is running without a config file".to_string())
        );
    }

    #[test]
    fn config_rewrite_surfaces_filesystem_failures() {
        let mut rt = Runtime::default_strict();
        let unique_suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("unix epoch")
            .as_nanos();
        let missing_dir = std::env::temp_dir().join(format!(
            "fr_runtime_config_rewrite_missing_{}_{}",
            std::process::id(),
            unique_suffix
        ));
        rt.set_config_file_path(Some(missing_dir.join("redis.conf")));

        let RespFrame::Error(err) = rt.execute_frame(command(&[b"CONFIG", b"REWRITE"]), 0) else {
            panic!("expected CONFIG REWRITE filesystem error"); // ubs:ignore — test assertion
        };
        assert!(
            err.starts_with("ERR Rewriting config file: "),
            "unexpected rewrite error: {err}"
        );
    }

    #[test]
    fn config_set_hz_matches_redis_validation_and_clamping() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"hz", b"abc"]), 0),
            RespFrame::Error(
                "ERR CONFIG SET failed (possibly related to argument 'hz') - argument couldn't be parsed into an integer"
                    .to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"hz", b"-1"]), 0),
            RespFrame::Error(
                "ERR CONFIG SET failed (possibly related to argument 'hz') - argument must be between 0 and 2147483647 inclusive"
                    .to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"hz", b"0"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"hz"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"hz".to_vec())),
                RespFrame::BulkString(Some(b"1".to_vec())),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"hz", b"501"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"hz"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"hz".to_vec())),
                RespFrame::BulkString(Some(b"500".to_vec())),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"SET", b"hz", b"2147483648"]), 0),
            RespFrame::Error(
                "ERR CONFIG SET failed (possibly related to argument 'hz') - argument must be between 0 and 2147483647 inclusive"
                    .to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"hz"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"hz".to_vec())),
                RespFrame::BulkString(Some(b"500".to_vec())),
            ]))
        );
    }

    #[test]
    fn config_get_prefix_patterns_match_redis_ordering() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"maxmemory*"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"maxmemory-eviction-tenacity".to_vec())),
                RespFrame::BulkString(Some(b"10".to_vec())),
                RespFrame::BulkString(Some(b"maxmemory-clients".to_vec())),
                RespFrame::BulkString(Some(b"0".to_vec())),
                RespFrame::BulkString(Some(b"maxmemory-policy".to_vec())),
                RespFrame::BulkString(Some(b"noeviction".to_vec())),
                RespFrame::BulkString(Some(b"maxmemory-samples".to_vec())),
                RespFrame::BulkString(Some(b"5".to_vec())),
                RespFrame::BulkString(Some(b"maxmemory".to_vec())),
                RespFrame::BulkString(Some(b"0".to_vec())),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CONFIG", b"GET", b"lazyfree*"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"lazyfree-lazy-user-flush".to_vec())),
                RespFrame::BulkString(Some(b"no".to_vec())),
                RespFrame::BulkString(Some(b"lazyfree-lazy-expire".to_vec())),
                RespFrame::BulkString(Some(b"no".to_vec())),
                RespFrame::BulkString(Some(b"lazyfree-lazy-eviction".to_vec())),
                RespFrame::BulkString(Some(b"no".to_vec())),
                RespFrame::BulkString(Some(b"lazyfree-lazy-server-del".to_vec())),
                RespFrame::BulkString(Some(b"no".to_vec())),
                RespFrame::BulkString(Some(b"lazyfree-lazy-user-del".to_vec())),
                RespFrame::BulkString(Some(b"no".to_vec())),
            ]))
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

    #[test]
    fn config_set_latency_tracking_controls_info_latencystats() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"CONFIG",
                    b"SET",
                    b"latency-tracking",
                    b"no",
                    b"latency-tracking-info-percentiles",
                    b"50 99.9",
                ]),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert!(!rt.server.latency_tracking);
        assert_eq!(rt.server.latency_percentiles, vec![50.0, 99.9]);

        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"CONFIG",
                    b"GET",
                    b"latency-tracking",
                    b"latency-tracking-info-percentiles",
                ]),
                1,
            ),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"latency-tracking".to_vec())),
                RespFrame::BulkString(Some(b"no".to_vec())),
                RespFrame::BulkString(Some(b"latency-tracking-info-percentiles".to_vec(),)),
                RespFrame::BulkString(Some(b"50 99.9".to_vec())),
            ]))
        );

        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"missing"]), 2),
            RespFrame::BulkString(None)
        );

        let disabled_info = rt.execute_frame(command(&[b"INFO", b"latencystats"]), 3);
        let RespFrame::BulkString(Some(disabled_info_bytes)) = disabled_info else {
            unreachable!("expected bulk INFO response");
        };
        let disabled_info = String::from_utf8(disabled_info_bytes).expect("utf8 info");
        assert!(
            disabled_info.contains("# Latencystats\r\n"),
            "{disabled_info}"
        );
        assert!(
            !disabled_info.contains("latency_percentiles_usec_GET:"),
            "{disabled_info}"
        );

        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"SET", b"latency-tracking", b"yes"]),
                4
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"missing"]), 5),
            RespFrame::BulkString(None)
        );

        let enabled_info = rt.execute_frame(command(&[b"INFO", b"latencystats"]), 6);
        let RespFrame::BulkString(Some(enabled_info_bytes)) = enabled_info else {
            unreachable!("expected bulk INFO response");
        };
        let enabled_info = String::from_utf8(enabled_info_bytes).expect("utf8 info");
        assert!(enabled_info.contains("latency_percentiles_usec_GET:p50="));
        assert!(enabled_info.contains("p99.9="));
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
            unreachable!("Expected integer from PTTL, got: {pttl:?}");
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
    fn save_and_load_aof_round_trip_preserves_move_and_swapdb_multi_db_state() {
        let dir = std::env::temp_dir().join("fr_runtime_aof_move_swapdb_test");
        let _ = std::fs::create_dir_all(&dir);
        let aof_path = dir.join("test_move_swapdb.aof");

        let mut rt = Runtime::default_strict();
        rt.set_aof_path(aof_path.clone());

        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"move_me", b"zero"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"MOVE", b"move_me", b"2"]), 1),
            RespFrame::Integer(1)
        );

        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"swap_key", b"db0"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SELECT", b"2"]), 3),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"swap_key", b"db2"]), 4),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SWAPDB", b"0", b"2"]), 5),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SELECT", b"0"]), 6),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SAVE"]), 7),
            RespFrame::SimpleString("OK".to_string())
        );

        let mut rt2 = Runtime::default_strict();
        rt2.set_aof_path(aof_path.clone());
        let loaded = rt2.load_aof(100).expect("load should succeed");
        assert!(loaded > 0);

        assert_eq!(
            rt2.execute_frame(command(&[b"GET", b"move_me"]), 100),
            RespFrame::BulkString(Some(b"zero".to_vec()))
        );
        assert_eq!(
            rt2.execute_frame(command(&[b"GET", b"swap_key"]), 101),
            RespFrame::BulkString(Some(b"db2".to_vec()))
        );

        assert_eq!(
            rt2.execute_frame(command(&[b"SELECT", b"2"]), 102),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt2.execute_frame(command(&[b"GET", b"move_me"]), 103),
            RespFrame::BulkString(None)
        );
        assert_eq!(
            rt2.execute_frame(command(&[b"GET", b"swap_key"]), 104),
            RespFrame::BulkString(Some(b"db0".to_vec()))
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
        rt.wait_for_child_processes();
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
    fn bgsave_rdb_snapshot_preserves_move_and_swapdb_multi_db_state() {
        let dir = std::env::temp_dir().join("fr_runtime_rdb_move_swapdb_test");
        let _ = std::fs::create_dir_all(&dir);
        let rdb_path = dir.join("test_move_swapdb.rdb");

        let mut rt = Runtime::default_strict();
        rt.set_rdb_path(rdb_path.clone());

        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"move_me", b"zero"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"MOVE", b"move_me", b"2"]), 1),
            RespFrame::Integer(1)
        );

        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"swap_key", b"db0"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SELECT", b"2"]), 3),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"swap_key", b"db2"]), 4),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SWAPDB", b"0", b"2"]), 5),
            RespFrame::SimpleString("OK".to_string())
        );

        assert_eq!(
            rt.execute_frame(command(&[b"BGSAVE"]), 6),
            RespFrame::SimpleString("Background saving started".to_string())
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
        rt.check_child_processes(6);

        assert!(
            rdb_path.exists(),
            "BGSAVE should write the configured RDB file"
        );

        let (entries, aux) = fr_persist::read_rdb_file(&rdb_path).expect("read rdb");
        assert_eq!(aux.get("frankenredis"), Some(&"true".to_string()));
        assert!(entries.iter().any(|entry| {
            entry.db == 0
                && entry.key == b"move_me"
                && entry.value == fr_persist::RdbValue::String(b"zero".to_vec())
        }));
        assert!(entries.iter().any(|entry| {
            entry.db == 0
                && entry.key == b"swap_key"
                && entry.value == fr_persist::RdbValue::String(b"db2".to_vec())
        }));
        assert!(entries.iter().any(|entry| {
            entry.db == 2
                && entry.key == b"swap_key"
                && entry.value == fr_persist::RdbValue::String(b"db0".to_vec())
        }));
        assert!(
            !entries
                .iter()
                .any(|entry| entry.db == 2 && entry.key == b"move_me")
        );

        let mut restored = Runtime::default_strict();
        super::apply_rdb_entries_to_store(&mut restored.server.store, &entries, 6)
            .expect("apply rdb entries");

        assert_eq!(
            restored.execute_frame(command(&[b"GET", b"move_me"]), 7),
            RespFrame::BulkString(Some(b"zero".to_vec()))
        );
        assert_eq!(
            restored.execute_frame(command(&[b"GET", b"swap_key"]), 8),
            RespFrame::BulkString(Some(b"db2".to_vec()))
        );
        assert_eq!(
            restored.execute_frame(command(&[b"SELECT", b"2"]), 9),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            restored.execute_frame(command(&[b"GET", b"move_me"]), 10),
            RespFrame::BulkString(None)
        );
        assert_eq!(
            restored.execute_frame(command(&[b"GET", b"swap_key"]), 11),
            RespFrame::BulkString(Some(b"db0".to_vec()))
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
            unreachable!("expected bulk INFO response");
        };
        let info = String::from_utf8(info_bytes).expect("utf8 info");
        assert!(info.contains("rdb_last_save_time:1700000005\r\n"), "{info}");
        assert!(info.contains("rdb_saves:1\r\n"), "{info}");
        assert!(info.contains("rdb_last_bgsave_status:ok\r\n"), "{info}");
        assert!(info.contains("rdb_last_bgsave_time_sec:-1\r\n"), "{info}");
        assert!(info.contains("aof_last_bgrewrite_status:ok\r\n"), "{info}");
        assert!(info.contains("aof_last_write_status:ok\r\n"), "{info}");
    }

    #[test]
    fn live_info_persistence_reports_aof_enabled_when_configured() {
        let mut rt = Runtime::default_strict();
        rt.set_aof_path(std::path::PathBuf::from("appendonly.aof"));

        let info = rt.execute_frame(command(&[b"INFO", b"persistence"]), 0);
        let RespFrame::BulkString(Some(info_bytes)) = info else {
            unreachable!("expected bulk INFO response");
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
    fn bgsave_failure_updates_info_persistence_status() {
        let dir = std::env::temp_dir().join("fr_runtime_bgsave_failure_status_dir");
        let _ = std::fs::create_dir_all(&dir);
        let mut rt = Runtime::default_strict();
        rt.set_rdb_path(dir.clone());

        assert_eq!(
            rt.execute_frame(command(&[b"BGSAVE"]), 1),
            RespFrame::SimpleString("Background saving started".to_string())
        );

        // Wait for child to exit
        std::thread::sleep(std::time::Duration::from_millis(50));
        rt.check_child_processes(2);

        let info = rt.execute_frame(command(&[b"INFO", b"persistence"]), 2);
        let RespFrame::BulkString(Some(info_bytes)) = info else {
            unreachable!("expected bulk INFO response");
        };
        let info = String::from_utf8(info_bytes).expect("utf8 info");
        assert!(info.contains("rdb_last_bgsave_status:err\r\n"), "{info}");
    }

    #[test]
    fn save_failure_updates_aof_last_write_status() {
        let dir = std::env::temp_dir().join("fr_runtime_save_failure_status_dir");
        let _ = std::fs::create_dir_all(&dir);
        let mut rt = Runtime::default_strict();
        rt.set_aof_path(dir.clone());

        assert_eq!(
            rt.execute_frame(command(&[b"SAVE"]), 1),
            RespFrame::Error("ERR error saving dataset to disk".to_string())
        );

        let info = rt.execute_frame(command(&[b"INFO", b"persistence"]), 2);
        let RespFrame::BulkString(Some(info_bytes)) = info else {
            unreachable!("expected bulk INFO response");
        };
        let info = String::from_utf8(info_bytes).expect("utf8 info");
        assert!(info.contains("aof_last_write_status:err\r\n"), "{info}");
    }

    #[test]
    fn fr_p2c_005_u009_rdb_disk_error_denies_writes_until_config_disabled() {
        let dir = std::env::temp_dir().join("fr_runtime_rdb_disk_error_write_deny_dir");
        let _ = std::fs::create_dir_all(&dir);
        let mut rt = Runtime::default_strict();
        rt.set_rdb_path(dir);

        assert_eq!(
            rt.execute_frame(command(&[b"BGSAVE"]), 1),
            RespFrame::SimpleString("Background saving started".to_string())
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
        rt.check_child_processes(1);

        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"blocked", b"write"]), 2),
            RespFrame::Error(RDB_DISK_ERROR_WRITE_DENIED.to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"PING"]), 3),
            RespFrame::Error(RDB_DISK_ERROR_WRITE_DENIED.to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"blocked"]), 4),
            RespFrame::BulkString(None)
        );

        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"SET", b"stop-writes-on-bgsave-error", b"no"]),
                5
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"allowed", b"after-config"]), 6),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"GET", b"stop-writes-on-bgsave-error"]),
                7
            ),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"stop-writes-on-bgsave-error".to_vec())),
                RespFrame::BulkString(Some(b"no".to_vec())),
            ]))
        );
    }

    #[test]
    fn fr_p2c_005_u009_aof_disk_error_denies_writes() {
        let dir = std::env::temp_dir().join("fr_runtime_aof_disk_error_write_deny_dir");
        let _ = std::fs::create_dir_all(&dir);
        let mut rt = Runtime::default_strict();
        rt.set_aof_path(dir);

        assert_eq!(
            rt.execute_frame(command(&[b"SAVE"]), 1),
            RespFrame::Error("ERR error saving dataset to disk".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"blocked", b"write"]), 2),
            RespFrame::Error(AOF_DISK_ERROR_WRITE_DENIED.to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"blocked"]), 3),
            RespFrame::BulkString(None)
        );
        assert!(
            rt.aof_records().is_empty(),
            "denied writes must not advance the replay-visible AOF stream"
        );

        let info = rt.execute_frame(command(&[b"INFO", b"persistence"]), 4);
        let RespFrame::BulkString(Some(info_bytes)) = info else {
            assert!(
                matches!(info, RespFrame::BulkString(Some(_))),
                "expected bulk INFO response, got {info:?}"
            );
            return;
        };
        let info = String::from_utf8(info_bytes).expect("utf8 info");
        assert!(info.contains("aof_last_write_status:err\r\n"), "{info}");
    }

    #[test]
    fn debug_reload_aof_snapshot_failure_does_not_poison_aof_write_status() {
        let dir = std::env::temp_dir().join("fr_runtime_debug_reload_aof_failure_dir");
        let _ = std::fs::create_dir_all(&dir);
        let mut rt = Runtime::default_strict();
        rt.set_aof_path(dir);

        assert_eq!(
            rt.execute_frame(command(&[b"DEBUG", b"RELOAD"]), 1),
            RespFrame::Error("ERR error saving dataset to disk".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"after_debug_reload", b"ok"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );

        let info = rt.execute_frame(command(&[b"INFO", b"persistence"]), 3);
        let RespFrame::BulkString(Some(info_bytes)) = info else {
            unreachable!("expected bulk INFO response");
        };
        let info = String::from_utf8(info_bytes).expect("utf8 info");
        assert!(info.contains("aof_last_write_status:ok\r\n"), "{info}");
    }

    #[test]
    fn bgrewriteaof_rewrites_multi_db_move_and_swapdb_state() {
        let dir = std::env::temp_dir().join("fr_runtime_bgrewriteaof_move_swapdb_test");
        let _ = std::fs::create_dir_all(&dir);
        let aof_path = dir.join("rewrite_move_swapdb.aof");
        let stale_records = vec![AofRecord {
            argv: vec![b"SET".to_vec(), b"stale".to_vec(), b"old".to_vec()],
        }];
        write_aof_file(&aof_path, &stale_records).expect("seed stale aof");

        let mut rt = Runtime::default_strict();
        rt.set_aof_path(aof_path.clone());

        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"move_me", b"zero"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"MOVE", b"move_me", b"2"]), 1),
            RespFrame::Integer(1)
        );

        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"swap_key", b"db0"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SELECT", b"2"]), 3),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"swap_key", b"db2"]), 4),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SWAPDB", b"0", b"2"]), 5),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SELECT", b"0"]), 6),
            RespFrame::SimpleString("OK".to_string())
        );

        assert_eq!(
            rt.execute_frame(command(&[b"BGREWRITEAOF"]), 7),
            RespFrame::SimpleString("Background append only file rewriting started".to_string())
        );

        let mut restored = Runtime::default_strict();
        restored.set_aof_path(aof_path.clone());
        let loaded = restored.load_aof(100).expect("load rewritten aof");
        assert!(loaded > 0, "rewritten AOF should contain snapshot records");

        assert_eq!(
            restored.execute_frame(command(&[b"GET", b"stale"]), 101),
            RespFrame::BulkString(None)
        );
        assert_eq!(
            restored.execute_frame(command(&[b"GET", b"move_me"]), 102),
            RespFrame::BulkString(Some(b"zero".to_vec()))
        );
        assert_eq!(
            restored.execute_frame(command(&[b"GET", b"swap_key"]), 103),
            RespFrame::BulkString(Some(b"db2".to_vec()))
        );
        assert_eq!(
            restored.execute_frame(command(&[b"SELECT", b"2"]), 104),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            restored.execute_frame(command(&[b"GET", b"move_me"]), 105),
            RespFrame::BulkString(None)
        );
        assert_eq!(
            restored.execute_frame(command(&[b"GET", b"swap_key"]), 106),
            RespFrame::BulkString(Some(b"db0".to_vec()))
        );

        let info = rt.execute_frame(command(&[b"INFO", b"persistence"]), 8);
        let RespFrame::BulkString(Some(info_bytes)) = info else {
            unreachable!("expected bulk INFO response");
        };
        let info = String::from_utf8(info_bytes).expect("utf8 info");
        assert!(info.contains("aof_last_rewrite_time_sec:0\r\n"), "{info}");
        assert!(info.contains("aof_last_bgrewrite_status:ok\r\n"), "{info}");

        let _ = std::fs::remove_file(&aof_path);
    }

    #[test]
    fn bgrewriteaof_failure_updates_info_persistence_status() {
        let dir = std::env::temp_dir().join("fr_runtime_bgrewrite_failure_status_dir");
        let _ = std::fs::create_dir_all(&dir);
        let mut rt = Runtime::default_strict();
        rt.set_aof_path(dir.clone());

        assert_eq!(
            rt.execute_frame(command(&[b"BGREWRITEAOF"]), 1),
            RespFrame::Error("ERR error rewriting AOF file".to_string())
        );

        let info = rt.execute_frame(command(&[b"INFO", b"persistence"]), 2);
        let RespFrame::BulkString(Some(info_bytes)) = info else {
            unreachable!("expected bulk INFO response");
        };
        let info = String::from_utf8(info_bytes).expect("utf8 info");
        assert!(info.contains("aof_last_bgrewrite_status:err\r\n"), "{info}");
    }

    #[test]
    fn eval_bgrewriteaof_rejects_from_scripts_and_leaves_aof_unrewritten() {
        let dir = std::env::temp_dir().join("fr_runtime_eval_bgrewriteaof_test");
        let _ = std::fs::create_dir_all(&dir);
        let aof_path = dir.join("eval_bgrewriteaof.aof");
        let stale_records = vec![AofRecord {
            argv: vec![b"SET".to_vec(), b"stale".to_vec(), b"old".to_vec()],
        }];
        write_aof_file(&aof_path, &stale_records).expect("seed stale aof");

        let mut rt = Runtime::default_strict();
        rt.set_aof_path(aof_path.clone());

        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"fresh", b"value"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        let script = b"return redis.call('BGREWRITEAOF')";
        assert_eq!(
            rt.execute_frame(command(&[b"EVAL", script, b"0"]), 1),
            RespFrame::Error(format!(
                "ERR This Redis command is not allowed from script script: {}, on @user_script:1.",
                sha1_hex_public(script)
            ))
        );

        let mut restored = Runtime::default_strict();
        restored.set_aof_path(aof_path.clone());
        let loaded = restored.load_aof(100).expect("load unchanged aof");
        assert!(
            loaded > 0,
            "AOF should still contain the preseeded and appended records"
        );

        assert_eq!(
            restored.execute_frame(command(&[b"GET", b"stale"]), 101),
            RespFrame::BulkString(Some(b"old".to_vec()))
        );
        assert_eq!(
            restored.execute_frame(command(&[b"GET", b"fresh"]), 102),
            RespFrame::BulkString(None)
        );

        let _ = std::fs::remove_file(&aof_path);
    }

    #[test]
    fn debug_reload_reloads_from_aof_snapshot() {
        let dir = std::env::temp_dir().join("fr_runtime_debug_reload_aof_test");
        let _ = std::fs::create_dir_all(&dir);
        let aof_path = dir.join("debug-reload.aof");

        let mut rt = Runtime::default_strict();
        rt.set_aof_path(aof_path.clone());
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"reload:key", b"two"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"DEBUG", b"RELOAD"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"reload:key"]), 3),
            RespFrame::BulkString(Some(b"two".to_vec()))
        );

        let _ = std::fs::remove_file(&aof_path);
    }

    #[test]
    fn debug_reload_reloads_from_rdb_snapshot_when_aof_is_disabled() {
        let dir = std::env::temp_dir().join("fr_runtime_debug_reload_rdb_test");
        let _ = std::fs::create_dir_all(&dir);
        let rdb_path = dir.join("debug-reload.rdb");

        let mut rt = Runtime::default_strict();
        rt.set_rdb_path(rdb_path.clone());
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"reload:key", b"two"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"DEBUG", b"RELOAD"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"reload:key"]), 3),
            RespFrame::BulkString(Some(b"two".to_vec()))
        );

        let _ = std::fs::remove_file(&rdb_path);
    }

    #[test]
    fn active_expire_cycle_is_skipped_when_debug_toggle_disables_it() {
        let mut rt = Runtime::default_strict();
        rt.server
            .store
            .set(b"expire-me".to_vec(), b"value".to_vec(), Some(1_000), 0);
        rt.server.store.active_expire_enabled = false;

        let stats = rt.run_active_expire_cycle(500, ActiveExpireCycleKind::Fast);

        assert_eq!(stats.sampled_keys, 0);
        assert_eq!(stats.evicted_keys, 0);
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"expire-me"]), 500),
            RespFrame::BulkString(Some(b"value".to_vec()))
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
        assert_eq!(rt.server.store.stat_unexpected_error_replies, 1);
        assert_eq!(rt.server.store.stat_total_error_replies, 1);

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
                    b"ACL", b"SETUSER", b"bob", b"on", b">pass", b"-@all", b"+get", b"+set",
                    b"allkeys"
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
                    b"-flushdb",
                    b"allkeys"
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
    fn acl_cat_returns_redis_canonical_category_order() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"CAT"]), 0),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"keyspace".to_vec())),
                RespFrame::BulkString(Some(b"read".to_vec())),
                RespFrame::BulkString(Some(b"write".to_vec())),
                RespFrame::BulkString(Some(b"set".to_vec())),
                RespFrame::BulkString(Some(b"sortedset".to_vec())),
                RespFrame::BulkString(Some(b"list".to_vec())),
                RespFrame::BulkString(Some(b"hash".to_vec())),
                RespFrame::BulkString(Some(b"string".to_vec())),
                RespFrame::BulkString(Some(b"bitmap".to_vec())),
                RespFrame::BulkString(Some(b"hyperloglog".to_vec())),
                RespFrame::BulkString(Some(b"geo".to_vec())),
                RespFrame::BulkString(Some(b"stream".to_vec())),
                RespFrame::BulkString(Some(b"pubsub".to_vec())),
                RespFrame::BulkString(Some(b"admin".to_vec())),
                RespFrame::BulkString(Some(b"fast".to_vec())),
                RespFrame::BulkString(Some(b"slow".to_vec())),
                RespFrame::BulkString(Some(b"blocking".to_vec())),
                RespFrame::BulkString(Some(b"dangerous".to_vec())),
                RespFrame::BulkString(Some(b"connection".to_vec())),
                RespFrame::BulkString(Some(b"transaction".to_vec())),
                RespFrame::BulkString(Some(b"scripting".to_vec())),
            ]))
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
                    b"+@connection",
                    b"allkeys"
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
                    b"-@dangerous",
                    b"allkeys"
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
                    b"ACL", b"SETUSER", b"mixed", b"on", b">pass", b"-@all", b"+@read", b"+set",
                    b"allkeys"
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
            unreachable!("Expected array reply from GETUSER, got: {reply:?}");
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
            unreachable!("Expected array reply from ACL LIST, got: {reply:?}");
        }
    }

    #[test]
    fn acl_case_insensitive_command_matching() {
        let mut rt = Runtime::default_strict();
        // Create user allowing only +GET (uppercase)
        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL", b"SETUSER", b"ci", b"on", b">pass", b"-@all", b"+GET", b"allkeys"
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
                    b"+get",
                    b"allkeys"
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
                    b"-del",
                    b"allkeys"
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
    fn acl_log_rejects_noncanonical_integer() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"LOG", b"+1"]), 0),
            RespFrame::Error("ERR value is not an integer or out of range".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"LOG", b"foo"]), 1),
            RespFrame::Error("ERR value is not an integer or out of range".to_string())
        );
    }

    #[test]
    fn acl_log_reports_age_seconds_from_command_time() {
        let mut rt = Runtime::default_strict();
        rt.session.client_id = 4;
        rt.session.last_command_name = "auth".to_string();
        rt.record_acl_log_event("auth", "AUTH".to_string(), b"default".to_vec(), 1_000);

        let reply = rt.execute_frame(command(&[b"ACL", b"LOG"]), 5_500);
        let entries = match reply {
            RespFrame::Array(Some(entries)) => entries,
            other => {
                assert!(
                    matches!(other, RespFrame::Array(_)),
                    "expected ACL LOG array reply, got {other:?}"
                );
                return;
            }
        };
        let entry = match &entries[0] {
            RespFrame::Array(Some(entry)) => entry,
            other => {
                assert!(
                    matches!(other, RespFrame::Array(_)),
                    "expected ACL LOG entry array, got {other:?}"
                );
                return;
            }
        };
        assert_eq!(entry.len(), 20);
        assert_eq!(entry[0], RespFrame::BulkString(Some(b"count".to_vec())));
        assert_eq!(entry[1], RespFrame::Integer(1));
        assert_eq!(entry[2], RespFrame::BulkString(Some(b"reason".to_vec())));
        assert_eq!(entry[3], RespFrame::BulkString(Some(b"auth".to_vec())));
        assert_eq!(entry[4], RespFrame::BulkString(Some(b"context".to_vec())));
        assert_eq!(entry[5], RespFrame::BulkString(Some(b"toplevel".to_vec())));
        assert_eq!(entry[6], RespFrame::BulkString(Some(b"object".to_vec())));
        assert_eq!(entry[7], RespFrame::BulkString(Some(b"AUTH".to_vec())));
        assert_eq!(entry[8], RespFrame::BulkString(Some(b"username".to_vec())));
        assert_eq!(entry[9], RespFrame::BulkString(Some(b"default".to_vec())));
        assert_eq!(
            entry[10],
            RespFrame::BulkString(Some(b"age-seconds".to_vec()))
        );
        assert_eq!(entry[11], RespFrame::BulkString(Some(b"4.5".to_vec())));
        assert_eq!(
            entry[12],
            RespFrame::BulkString(Some(b"client-info".to_vec()))
        );
        let RespFrame::BulkString(Some(bytes)) = &entry[13] else {
            assert!(
                matches!(&entry[13], RespFrame::BulkString(Some(_))),
                "expected ACL LOG client-info bulk string, got {:?}",
                entry[13]
            );
            return;
        };
        let client_info = String::from_utf8_lossy(bytes).to_string();
        assert!(client_info.contains("id=4"));
        assert!(client_info.contains("cmd=auth"));
        assert!(client_info.contains("user=default"));
        assert!(client_info.contains("resp=2"));
        assert_eq!(entry[14], RespFrame::BulkString(Some(b"entry-id".to_vec())));
        assert_eq!(entry[15], RespFrame::Integer(0));
        assert_eq!(
            entry[16],
            RespFrame::BulkString(Some(b"timestamp-created".to_vec()))
        );
        assert_eq!(entry[17], RespFrame::Integer(1_000));
        assert_eq!(
            entry[18],
            RespFrame::BulkString(Some(b"timestamp-last-updated".to_vec()))
        );
        assert_eq!(entry[19], RespFrame::Integer(1_000));
    }

    #[test]
    fn acl_log_records_channel_access_violations_with_channel_name() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"LOG", b"RESET"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(
                command(&[b"ACL", b"SETUSER", b"antirez", b"on", b">foo", b"+publish"]),
                1,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"AUTH", b"antirez", b"foo"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(
                command(&[b"PUBLISH", b"somechannelnotallowed", b"nullmsg"]),
                3
            ),
            RespFrame::Error("NOPERM No permissions to access a channel".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"AUTH", b"default", b""]), 4),
            RespFrame::SimpleString("OK".to_string())
        );

        let reply = rt.execute_frame(command(&[b"ACL", b"LOG"]), 5);
        let RespFrame::Array(Some(entries)) = reply else {
            unreachable!("expected ACL LOG array");
        };
        let RespFrame::Array(Some(entry)) = &entries[0] else {
            unreachable!("expected ACL LOG entry array");
        };

        assert_eq!(entry[2], RespFrame::BulkString(Some(b"reason".to_vec())));
        assert_eq!(entry[3], RespFrame::BulkString(Some(b"channel".to_vec())));
        assert_eq!(entry[6], RespFrame::BulkString(Some(b"object".to_vec())));
        assert_eq!(
            entry[7],
            RespFrame::BulkString(Some(b"somechannelnotallowed".to_vec()))
        );
        assert_eq!(entry[8], RespFrame::BulkString(Some(b"username".to_vec())));
        assert_eq!(entry[9], RespFrame::BulkString(Some(b"antirez".to_vec())));
    }

    #[test]
    fn acl_log_marks_transaction_queue_denials_as_multi() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"LOG", b"RESET"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL", b"SETUSER", b"antirez", b"on", b">foo", b"+@all", b"allkeys", b"-incr",
                ]),
                1,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"AUTH", b"antirez", b"foo"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"MULTI"]), 3),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"INCR", b"foo"]), 4),
            RespFrame::Error(
                "NOPERM this user has no permissions to run the 'INCR' command".to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"EXEC"]), 5),
            RespFrame::Array(Some(vec![]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"AUTH", b"default", b""]), 6),
            RespFrame::SimpleString("OK".to_string())
        );

        let RespFrame::Array(Some(entries)) = rt.execute_frame(command(&[b"ACL", b"LOG"]), 7)
        else {
            unreachable!("expected ACL LOG array");
        };
        let RespFrame::Array(Some(entry)) = &entries[0] else {
            unreachable!("expected ACL LOG entry array");
        };

        assert_eq!(entry[4], RespFrame::BulkString(Some(b"context".to_vec())));
        assert_eq!(entry[5], RespFrame::BulkString(Some(b"multi".to_vec())));
        assert_eq!(entry[6], RespFrame::BulkString(Some(b"object".to_vec())));
        assert_eq!(entry[7], RespFrame::BulkString(Some(b"INCR".to_vec())));
    }

    #[test]
    fn acl_log_marks_exec_time_denials_as_multi_with_cmd_exec() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"LOG", b"RESET"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL", b"SETUSER", b"antirez", b"on", b">foo", b"+@all", b"allkeys", b"+incr",
                ]),
                1,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"AUTH", b"antirez", b"foo"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"MULTI"]), 3),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"INCR", b"object:1234"]), 4),
            RespFrame::SimpleString("QUEUED".to_string())
        );
        assert!(
            rt.server
                .auth_state
                .set_user(b"antirez".to_vec(), &[b"-incr"])
                .is_ok()
        );

        let RespFrame::Array(Some(exec_results)) = rt.execute_frame(command(&[b"EXEC"]), 5) else {
            unreachable!("expected EXEC array reply");
        };
        assert_eq!(
            exec_results[0],
            RespFrame::Error(
                "NOPERM this user has no permissions to run the 'INCR' command".to_string()
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"AUTH", b"default", b""]), 6),
            RespFrame::SimpleString("OK".to_string())
        );

        let RespFrame::Array(Some(entries)) = rt.execute_frame(command(&[b"ACL", b"LOG"]), 7)
        else {
            unreachable!("expected ACL LOG array");
        };
        let RespFrame::Array(Some(entry)) = &entries[0] else {
            unreachable!("expected ACL LOG entry array");
        };

        assert_eq!(entry[4], RespFrame::BulkString(Some(b"context".to_vec())));
        assert_eq!(entry[5], RespFrame::BulkString(Some(b"multi".to_vec())));
        assert_eq!(entry[6], RespFrame::BulkString(Some(b"object".to_vec())));
        assert_eq!(entry[7], RespFrame::BulkString(Some(b"INCR".to_vec())));
        let RespFrame::BulkString(Some(client_info)) = &entry[13] else {
            unreachable!("expected ACL LOG client-info bulk string");
        };
        assert!(String::from_utf8_lossy(client_info).contains("cmd=exec"));
    }

    #[test]
    fn acl_log_marks_script_denials_as_lua() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"LOG", b"RESET"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL", b"SETUSER", b"antirez", b"on", b">foo", b"+@all", b"allkeys", b"-incr",
                ]),
                1,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"AUTH", b"antirez", b"foo"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
        let RespFrame::Error(err) = rt.execute_frame(
            command(&[b"EVAL", b"return redis.call('incr','foo')", b"0"]),
            3,
        ) else {
            unreachable!("expected EVAL ACL failure");
        };
        assert!(err.contains("NOPERM this user has no permissions to run the 'incr' command"));
        assert_eq!(
            rt.execute_frame(command(&[b"AUTH", b"default", b""]), 4),
            RespFrame::SimpleString("OK".to_string())
        );

        let RespFrame::Array(Some(entries)) = rt.execute_frame(command(&[b"ACL", b"LOG"]), 5)
        else {
            unreachable!("expected ACL LOG array");
        };
        let RespFrame::Array(Some(entry)) = &entries[0] else {
            unreachable!("expected ACL LOG entry array");
        };

        assert_eq!(entry[2], RespFrame::BulkString(Some(b"reason".to_vec())));
        assert_eq!(entry[3], RespFrame::BulkString(Some(b"command".to_vec())));
        assert_eq!(entry[4], RespFrame::BulkString(Some(b"context".to_vec())));
        assert_eq!(entry[5], RespFrame::BulkString(Some(b"lua".to_vec())));
        assert_eq!(entry[6], RespFrame::BulkString(Some(b"object".to_vec())));
        assert_eq!(entry[7], RespFrame::BulkString(Some(b"INCR".to_vec())));
        assert_eq!(entry[8], RespFrame::BulkString(Some(b"username".to_vec())));
        assert_eq!(entry[9], RespFrame::BulkString(Some(b"antirez".to_vec())));
        let RespFrame::BulkString(Some(client_info)) = &entry[13] else {
            unreachable!("expected ACL LOG client-info bulk string");
        };
        let client_info = String::from_utf8_lossy(client_info);
        assert!(client_info.contains("cmd=eval"));
        assert!(client_info.contains("user=antirez"));
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

    #[test]
    fn acl_setuser_resetkeys_modifier_is_order_sensitive() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL",
                    b"SETUSER",
                    b"order1",
                    b"on",
                    b"nopass",
                    b"allcommands",
                    b"allkeys",
                    b"resetkeys",
                ]),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"GETUSER", b"order1"]), 1),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"flags".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"on".to_vec())),
                    RespFrame::BulkString(Some(b"nopass".to_vec())),
                    RespFrame::BulkString(Some(b"sanitize-payload".to_vec())),
                ])),
                RespFrame::BulkString(Some(b"passwords".to_vec())),
                RespFrame::Array(Some(Vec::new())),
                RespFrame::BulkString(Some(b"commands".to_vec())),
                RespFrame::BulkString(Some(b"+@all".to_vec())),
                RespFrame::BulkString(Some(b"keys".to_vec())),
                RespFrame::BulkString(Some(Vec::new())),
                RespFrame::BulkString(Some(b"channels".to_vec())),
                RespFrame::BulkString(Some(Vec::new())),
                RespFrame::BulkString(Some(b"selectors".to_vec())),
                RespFrame::Array(Some(Vec::new())),
            ]))
        );

        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL",
                    b"SETUSER",
                    b"order2",
                    b"on",
                    b"nopass",
                    b"allcommands",
                    b"resetkeys",
                    b"allkeys",
                ]),
                2,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"GETUSER", b"order2"]), 3),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"flags".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"on".to_vec())),
                    RespFrame::BulkString(Some(b"nopass".to_vec())),
                    RespFrame::BulkString(Some(b"sanitize-payload".to_vec())),
                ])),
                RespFrame::BulkString(Some(b"passwords".to_vec())),
                RespFrame::Array(Some(Vec::new())),
                RespFrame::BulkString(Some(b"commands".to_vec())),
                RespFrame::BulkString(Some(b"+@all".to_vec())),
                RespFrame::BulkString(Some(b"keys".to_vec())),
                RespFrame::BulkString(Some(b"~*".to_vec())),
                RespFrame::BulkString(Some(b"channels".to_vec())),
                RespFrame::BulkString(Some(Vec::new())),
                RespFrame::BulkString(Some(b"selectors".to_vec())),
                RespFrame::Array(Some(Vec::new())),
            ]))
        );
    }

    #[test]
    fn acl_getuser_reports_sha256_passwords_for_new_restricted_users() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(
                command(&[b"ACL", b"SETUSER", b"alice", b"on", b">password123"]),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"GETUSER", b"alice"]), 1),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"flags".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"on".to_vec())),
                    RespFrame::BulkString(Some(b"sanitize-payload".to_vec())),
                ])),
                RespFrame::BulkString(Some(b"passwords".to_vec())),
                RespFrame::Array(Some(vec![RespFrame::BulkString(Some(
                    b"ef92b778bafe771e89245b89ecbc08a44a4e166c06659911881f383d4473e94f".to_vec(),
                ))])),
                RespFrame::BulkString(Some(b"commands".to_vec())),
                RespFrame::BulkString(Some(b"-@all".to_vec())),
                RespFrame::BulkString(Some(b"keys".to_vec())),
                RespFrame::BulkString(Some(Vec::new())),
                RespFrame::BulkString(Some(b"channels".to_vec())),
                RespFrame::BulkString(Some(Vec::new())),
                RespFrame::BulkString(Some(b"selectors".to_vec())),
                RespFrame::Array(Some(Vec::new())),
            ]))
        );
    }

    #[test]
    fn acl_setuser_missing_plaintext_password_removal_returns_redis_error() {
        let mut rt = Runtime::default_strict();
        assert_eq!(
            rt.execute_frame(
                command(&[b"ACL", b"SETUSER", b"alice", b"on", b">password123"]),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"SETUSER", b"alice", b"<missing"]), 1),
            RespFrame::Error(
                "ERR Error in ACL SETUSER modifier '<missing': The password you are trying to remove from the user does not exist"
                    .to_string(),
            )
        );

        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"GETUSER", b"alice"]), 2),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"flags".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"on".to_vec())),
                    RespFrame::BulkString(Some(b"sanitize-payload".to_vec())),
                ])),
                RespFrame::BulkString(Some(b"passwords".to_vec())),
                RespFrame::Array(Some(vec![RespFrame::BulkString(Some(
                    b"ef92b778bafe771e89245b89ecbc08a44a4e166c06659911881f383d4473e94f".to_vec(),
                ))])),
                RespFrame::BulkString(Some(b"commands".to_vec())),
                RespFrame::BulkString(Some(b"-@all".to_vec())),
                RespFrame::BulkString(Some(b"keys".to_vec())),
                RespFrame::BulkString(Some(Vec::new())),
                RespFrame::BulkString(Some(b"channels".to_vec())),
                RespFrame::BulkString(Some(Vec::new())),
                RespFrame::BulkString(Some(b"selectors".to_vec())),
                RespFrame::Array(Some(Vec::new())),
            ]))
        );
    }

    #[test]
    fn acl_setuser_missing_hash_password_removal_returns_redis_error() {
        let mut rt = Runtime::default_strict();
        let missing_hash = String::from_utf8(sha256_hex_bytes(b"missing")).expect("valid hex");
        let missing_rule = format!("!{missing_hash}");

        assert_eq!(
            rt.execute_frame(
                command(&[b"ACL", b"SETUSER", b"alice", b"on", b">password123"]),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        assert_eq!(
            rt.execute_frame(
                command(&[b"ACL", b"SETUSER", b"alice", missing_rule.as_bytes()]),
                1,
            ),
            RespFrame::Error(format!(
                "ERR Error in ACL SETUSER modifier '{}': The password you are trying to remove from the user does not exist",
                missing_rule
            ),)
        );

        assert_eq!(
            rt.execute_frame(command(&[b"AUTH", b"alice", b"password123"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
    }

    #[test]
    fn acl_skip_sanitize_payload_round_trips_through_getuser_list_and_save() {
        let mut rt = Runtime::default_strict();
        let acl_path = unique_temp_path("fr_runtime_acl_skip_sanitize", "acl");
        rt.set_acl_file_path(acl_path.clone());

        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL",
                    b"SETUSER",
                    b"skippy",
                    b"on",
                    b"nopass",
                    b"skip-sanitize-payload",
                    b"+@all",
                ]),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        let expected_getuser = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"flags".to_vec())),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"on".to_vec())),
                RespFrame::BulkString(Some(b"nopass".to_vec())),
                RespFrame::BulkString(Some(b"skip-sanitize-payload".to_vec())),
            ])),
            RespFrame::BulkString(Some(b"passwords".to_vec())),
            RespFrame::Array(Some(Vec::new())),
            RespFrame::BulkString(Some(b"commands".to_vec())),
            RespFrame::BulkString(Some(b"+@all".to_vec())),
            RespFrame::BulkString(Some(b"keys".to_vec())),
            RespFrame::BulkString(Some(Vec::new())),
            RespFrame::BulkString(Some(b"channels".to_vec())),
            RespFrame::BulkString(Some(Vec::new())),
            RespFrame::BulkString(Some(b"selectors".to_vec())),
            RespFrame::Array(Some(Vec::new())),
        ]));

        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"GETUSER", b"skippy"]), 1),
            expected_getuser
        );

        let list = rt.execute_frame(command(&[b"ACL", b"LIST"]), 2);
        let RespFrame::Array(Some(entries)) = list else {
            unreachable!("expected ACL LIST to return an array");
        };
        let skippy = entries
            .iter()
            .find_map(|entry| match entry {
                RespFrame::BulkString(Some(line))
                    if String::from_utf8_lossy(line).contains("skippy") =>
                {
                    Some(String::from_utf8_lossy(line).into_owned())
                }
                _ => None,
            })
            .expect("expected skippy in ACL LIST");
        assert_eq!(
            skippy,
            "user skippy on skip-sanitize-payload nopass resetchannels +@all"
        );

        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"SAVE"]), 3),
            RespFrame::SimpleString("OK".to_string())
        );
        let saved = std::fs::read_to_string(&acl_path).expect("ACL SAVE should write acl file");
        assert!(
            saved.contains("user skippy reset on skip-sanitize-payload nopass resetchannels +@all"),
            "saved ACL file should preserve skip-sanitize-payload, got: {saved}"
        );

        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"DELUSER", b"skippy"]), 4),
            RespFrame::Integer(1)
        );
        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"LOAD"]), 5),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"GETUSER", b"skippy"]), 6),
            expected_getuser
        );

        let _ = std::fs::remove_file(&acl_path);
    }

    #[test]
    fn acl_pubsub_default_drives_new_user_channel_defaults_without_mutating_existing_users() {
        let mut rt = Runtime::default_strict();

        let channels_for = |rt: &mut Runtime, username: &[u8], client_id: u64| -> Vec<u8> {
            let argv: [&[u8]; 3] = [b"ACL", b"GETUSER", username];
            let reply = rt.execute_frame(command(&argv), client_id);
            assert!(
                matches!(reply, RespFrame::Array(Some(_))),
                "expected ACL GETUSER array, got {reply:?}"
            );
            let items = match reply {
                RespFrame::Array(Some(items)) => items,
                _ => Vec::new(),
            };
            items
                .windows(2)
                .find_map(|pair| match (&pair[0], &pair[1]) {
                    (RespFrame::BulkString(Some(name)), RespFrame::BulkString(Some(value)))
                        if name == b"channels" =>
                    {
                        Some(value.clone())
                    }
                    _ => None,
                })
                .expect("expected ACL GETUSER channels entry")
        };

        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"SET", b"acl-pubsub-default", b"allchannels"]),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"SETUSER", b"mydefault"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(channels_for(&mut rt, b"mydefault", 2), b"&*".to_vec());

        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"SET", b"acl-pubsub-default", b"resetchannels"]),
                3,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(channels_for(&mut rt, b"mydefault", 4), b"&*".to_vec());

        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"SETUSER", b"fresh"]), 5),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(channels_for(&mut rt, b"fresh", 6), Vec::new());
    }

    #[test]
    fn acl_pubsub_default_drives_default_user_reset_channel_permissions() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"SET", b"acl-pubsub-default", b"allchannels"]),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL", b"SETUSER", b"default", b"reset", b"on", b"nopass", b"~*", b"+@all",
                ]),
                1,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"PUBLISH", b"hello", b"world"]), 2),
            RespFrame::Integer(0)
        );

        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"SET", b"acl-pubsub-default", b"resetchannels"]),
                3,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL", b"SETUSER", b"default", b"reset", b"on", b"nopass", b"~*", b"+@all",
                ]),
                4,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"PUBLISH", b"hello", b"world"]), 5),
            RespFrame::Error("NOPERM No permissions to access a channel".to_string())
        );
    }

    #[test]
    fn acl_key_patterns_round_trip_through_getuser_list_and_save() {
        let mut rt = Runtime::default_strict();
        let acl_path = unique_temp_path("fr_runtime_acl_key_patterns", "acl");
        rt.set_acl_file_path(acl_path.clone());

        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL",
                    b"SETUSER",
                    b"alice",
                    b"on",
                    b">pass",
                    b"-@all",
                    b"+get",
                    b"+set",
                    b"resetkeys",
                    b"~foo:*",
                    b"~bar:*",
                ]),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"GETUSER", b"alice"]), 1),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"flags".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"on".to_vec())),
                    RespFrame::BulkString(Some(b"sanitize-payload".to_vec())),
                ])),
                RespFrame::BulkString(Some(b"passwords".to_vec())),
                RespFrame::Array(Some(vec![RespFrame::BulkString(Some(
                    b"d74ff0ee8da3b9806b18c877dbf29bbde50b5bd8e4dad7a3a725000feb82e8f1".to_vec(),
                ))])),
                RespFrame::BulkString(Some(b"commands".to_vec())),
                RespFrame::BulkString(Some(b"+get +set".to_vec())),
                RespFrame::BulkString(Some(b"keys".to_vec())),
                RespFrame::BulkString(Some(b"~foo:* ~bar:*".to_vec())),
                RespFrame::BulkString(Some(b"channels".to_vec())),
                RespFrame::BulkString(Some(Vec::new())),
                RespFrame::BulkString(Some(b"selectors".to_vec())),
                RespFrame::Array(Some(Vec::new())),
            ]))
        );

        let list = rt.execute_frame(command(&[b"ACL", b"LIST"]), 2);
        let RespFrame::Array(Some(entries)) = list else {
            unreachable!("expected ACL LIST to return an array");
        };
        let alice = entries
            .iter()
            .find_map(|entry| match entry {
                RespFrame::BulkString(Some(line))
                    if String::from_utf8_lossy(line).contains("alice") =>
                {
                    Some(String::from_utf8_lossy(line).into_owned())
                }
                _ => None,
            })
            .expect("expected alice in ACL LIST");
        assert_eq!(
            alice,
            "user alice on sanitize-payload #<hidden> resetkeys ~foo:* ~bar:* resetchannels +get +set"
        );

        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"SAVE"]), 3),
            RespFrame::SimpleString("OK".to_string())
        );
        let saved = std::fs::read_to_string(&acl_path).expect("ACL SAVE should write acl file");
        assert!(
            saved.contains(
                "user alice reset on sanitize-payload #d74ff0ee8da3b9806b18c877dbf29bbde50b5bd8e4dad7a3a725000feb82e8f1 resetkeys ~foo:* ~bar:* resetchannels +get +set"
            ),
            "saved ACL file should preserve key patterns, got: {saved}"
        );
    }

    #[test]
    fn acl_key_patterns_gate_runtime_commands_and_dryrun() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL",
                    b"SETUSER",
                    b"alice",
                    b"on",
                    b">pass",
                    b"-@all",
                    b"+get",
                    b"+set",
                    b"resetkeys",
                    b"~foo:*",
                    b"~bar:*",
                ]),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        assert_eq!(
            rt.execute_frame(
                command(&[b"ACL", b"DRYRUN", b"alice", b"SET", b"foo:1", b"v"]),
                1,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(
                command(&[b"ACL", b"DRYRUN", b"alice", b"SET", b"zap:3", b"v"]),
                2,
            ),
            RespFrame::Error(
                "ERR User 'alice' has no permissions to access the 'zap:3' key".to_string(),
            )
        );

        assert_eq!(
            rt.execute_frame(command(&[b"AUTH", b"alice", b"pass"]), 3),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"foo:1", b"a"]), 4),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"bar:2", b"b"]), 5),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"zap:3", b"c"]), 6),
            RespFrame::Error("NOPERM No permissions to access a key".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"foo:1"]), 7),
            RespFrame::BulkString(Some(b"a".to_vec()))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"zap:3"]), 8),
            RespFrame::Error("NOPERM No permissions to access a key".to_string())
        );
    }

    #[test]
    fn acl_setuser_rejects_key_pattern_after_allkeys_without_resetkeys() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(
                command(&[b"ACL", b"SETUSER", b"alice", b"on", b"nopass", b"allkeys"]),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"SETUSER", b"alice", b"~foo:*"]), 1),
            RespFrame::Error(
                "ERR Error in ACL SETUSER modifier '~foo:*': Adding a pattern after the * pattern (or the 'allkeys' flag) is not valid and does not have any effect. Try 'resetkeys' to start with an empty list of patterns"
                    .to_string(),
            )
        );
        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"GETUSER", b"alice"]), 2),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"flags".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"on".to_vec())),
                    RespFrame::BulkString(Some(b"nopass".to_vec())),
                    RespFrame::BulkString(Some(b"sanitize-payload".to_vec())),
                ])),
                RespFrame::BulkString(Some(b"passwords".to_vec())),
                RespFrame::Array(Some(Vec::new())),
                RespFrame::BulkString(Some(b"commands".to_vec())),
                RespFrame::BulkString(Some(b"-@all".to_vec())),
                RespFrame::BulkString(Some(b"keys".to_vec())),
                RespFrame::BulkString(Some(b"~*".to_vec())),
                RespFrame::BulkString(Some(b"channels".to_vec())),
                RespFrame::BulkString(Some(Vec::new())),
                RespFrame::BulkString(Some(b"selectors".to_vec())),
                RespFrame::Array(Some(Vec::new())),
            ]))
        );
    }

    #[test]
    fn acl_channel_patterns_round_trip_through_getuser_list_and_save() {
        let mut rt = Runtime::default_strict();
        let acl_path = unique_temp_path("fr_runtime_acl_channel_patterns", "acl");
        rt.set_acl_file_path(acl_path.clone());

        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL",
                    b"SETUSER",
                    b"alice",
                    b"on",
                    b">pass",
                    b"+@all",
                    b"~*",
                    b"resetchannels",
                    b"&foo:1",
                    b"&bar:*",
                    b"&orders",
                ]),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"GETUSER", b"alice"]), 1),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"flags".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"on".to_vec())),
                    RespFrame::BulkString(Some(b"sanitize-payload".to_vec())),
                ])),
                RespFrame::BulkString(Some(b"passwords".to_vec())),
                RespFrame::Array(Some(vec![RespFrame::BulkString(Some(
                    b"d74ff0ee8da3b9806b18c877dbf29bbde50b5bd8e4dad7a3a725000feb82e8f1".to_vec(),
                ))])),
                RespFrame::BulkString(Some(b"commands".to_vec())),
                RespFrame::BulkString(Some(b"+@all".to_vec())),
                RespFrame::BulkString(Some(b"keys".to_vec())),
                RespFrame::BulkString(Some(b"~*".to_vec())),
                RespFrame::BulkString(Some(b"channels".to_vec())),
                RespFrame::BulkString(Some(b"&foo:1 &bar:* &orders".to_vec())),
                RespFrame::BulkString(Some(b"selectors".to_vec())),
                RespFrame::Array(Some(Vec::new())),
            ]))
        );

        let list = rt.execute_frame(command(&[b"ACL", b"LIST"]), 2);
        let RespFrame::Array(Some(entries)) = list else {
            unreachable!("expected ACL LIST to return an array");
        };
        let alice = entries
            .iter()
            .find_map(|entry| match entry {
                RespFrame::BulkString(Some(line))
                    if String::from_utf8_lossy(line).contains("alice") =>
                {
                    Some(String::from_utf8_lossy(line).into_owned())
                }
                _ => None,
            })
            .expect("expected alice in ACL LIST");
        assert_eq!(
            alice,
            "user alice on sanitize-payload #<hidden> ~* resetchannels &foo:1 &bar:* &orders +@all"
        );

        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"SAVE"]), 3),
            RespFrame::SimpleString("OK".to_string())
        );
        let saved = std::fs::read_to_string(&acl_path).expect("ACL SAVE should write acl file");
        assert!(
            saved.contains(
                "user alice reset on sanitize-payload #d74ff0ee8da3b9806b18c877dbf29bbde50b5bd8e4dad7a3a725000feb82e8f1 ~* resetchannels &foo:1 &bar:* &orders +@all"
            ),
            "saved ACL file should preserve channel patterns, got: {saved}"
        );

        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"DELUSER", b"alice"]), 4),
            RespFrame::Integer(1)
        );
        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"LOAD"]), 5),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"GETUSER", b"alice"]), 6),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"flags".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"on".to_vec())),
                    RespFrame::BulkString(Some(b"sanitize-payload".to_vec())),
                ])),
                RespFrame::BulkString(Some(b"passwords".to_vec())),
                RespFrame::Array(Some(vec![RespFrame::BulkString(Some(
                    b"d74ff0ee8da3b9806b18c877dbf29bbde50b5bd8e4dad7a3a725000feb82e8f1".to_vec(),
                ))])),
                RespFrame::BulkString(Some(b"commands".to_vec())),
                RespFrame::BulkString(Some(b"+@all".to_vec())),
                RespFrame::BulkString(Some(b"keys".to_vec())),
                RespFrame::BulkString(Some(b"~*".to_vec())),
                RespFrame::BulkString(Some(b"channels".to_vec())),
                RespFrame::BulkString(Some(b"&foo:1 &bar:* &orders".to_vec())),
                RespFrame::BulkString(Some(b"selectors".to_vec())),
                RespFrame::Array(Some(Vec::new())),
            ]))
        );

        let _ = std::fs::remove_file(&acl_path);
    }

    #[test]
    fn acl_channel_patterns_gate_publish_and_subscribe_commands() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL",
                    b"SETUSER",
                    b"psuser",
                    b"on",
                    b">pspass",
                    b"-@all",
                    b"+publish",
                    b"+spublish",
                    b"+subscribe",
                    b"+psubscribe",
                    b"+ssubscribe",
                    b"resetchannels",
                    b"&foo:1",
                    b"&bar:*",
                ]),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        assert_eq!(
            rt.execute_frame(command(&[b"AUTH", b"psuser", b"pspass"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"PUBLISH", b"foo:1", b"hello"]), 2),
            RespFrame::Integer(0)
        );
        assert_eq!(
            rt.execute_frame(command(&[b"PUBLISH", b"bar:2", b"hello"]), 3),
            RespFrame::Integer(0)
        );
        assert_eq!(
            rt.execute_frame(command(&[b"PUBLISH", b"zap:3", b"hello"]), 4),
            RespFrame::Error("NOPERM No permissions to access a channel".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SPUBLISH", b"foo:1", b"hello"]), 5),
            RespFrame::Integer(0)
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SPUBLISH", b"bar:2", b"hello"]), 6),
            RespFrame::Integer(0)
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SPUBLISH", b"zap:3", b"hello"]), 7),
            RespFrame::Error("NOPERM No permissions to access a channel".to_string())
        );

        let subscriber = rt.new_session();
        let previous = rt.swap_session(subscriber);
        assert_eq!(
            rt.execute_frame(command(&[b"AUTH", b"psuser", b"pspass"]), 8),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SUBSCRIBE", b"foo:1"]), 9),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"subscribe".to_vec())),
                RespFrame::BulkString(Some(b"foo:1".to_vec())),
                RespFrame::Integer(1),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SUBSCRIBE", b"bar:2"]), 10),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"subscribe".to_vec())),
                RespFrame::BulkString(Some(b"bar:2".to_vec())),
                RespFrame::Integer(2),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SUBSCRIBE", b"zap:3"]), 11),
            RespFrame::Error("NOPERM No permissions to access a channel".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"PSUBSCRIBE", b"bar:*"]), 12),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"psubscribe".to_vec())),
                RespFrame::BulkString(Some(b"bar:*".to_vec())),
                RespFrame::Integer(3),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"PSUBSCRIBE", b"bar:baz"]), 13),
            RespFrame::Error("NOPERM No permissions to access a channel".to_string())
        );
        let _subscriber = rt.swap_session(previous);

        let shard_subscriber = rt.new_session();
        let previous = rt.swap_session(shard_subscriber);
        assert_eq!(
            rt.execute_frame(command(&[b"AUTH", b"psuser", b"pspass"]), 15),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SSUBSCRIBE", b"foo:1"]), 16),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"ssubscribe".to_vec())),
                RespFrame::BulkString(Some(b"foo:1".to_vec())),
                RespFrame::Integer(1),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SSUBSCRIBE", b"bar:2"]), 17),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"ssubscribe".to_vec())),
                RespFrame::BulkString(Some(b"bar:2".to_vec())),
                RespFrame::Integer(2),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SSUBSCRIBE", b"zap:3"]), 18),
            RespFrame::Error("NOPERM No permissions to access a channel".to_string())
        );
        let _shard_subscriber = rt.swap_session(previous);
    }

    #[test]
    fn acl_channel_pattern_revocations_queue_pubsub_clients_for_disconnect() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL",
                    b"SETUSER",
                    b"psuser",
                    b"on",
                    b">pspass",
                    b"-@all",
                    b"+subscribe",
                    b"+psubscribe",
                    b"+ssubscribe",
                    b"resetchannels",
                    b"&foo:1",
                    b"&bar:*",
                    b"&orders",
                ]),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        let mut subscriber = rt.new_session();
        subscriber.client_name = Some(b"pardoned".to_vec());
        let previous = rt.swap_session(subscriber);
        assert_eq!(
            rt.execute_frame(command(&[b"AUTH", b"psuser", b"pspass"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SUBSCRIBE", b"foo:1"]), 2),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"subscribe".to_vec())),
                RespFrame::BulkString(Some(b"foo:1".to_vec())),
                RespFrame::Integer(1),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"SSUBSCRIBE", b"orders"]), 3),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"ssubscribe".to_vec())),
                RespFrame::BulkString(Some(b"orders".to_vec())),
                RespFrame::Integer(1),
            ]))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"PSUBSCRIBE", b"bar:*"]), 4),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"psubscribe".to_vec())),
                RespFrame::BulkString(Some(b"bar:*".to_vec())),
                RespFrame::Integer(2),
            ]))
        );
        let subscriber = rt.swap_session(previous);

        rt.server.pending_client_kills.clear();
        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL",
                    b"SETUSER",
                    b"psuser",
                    b"resetchannels",
                    b"&foo:1",
                    b"&bar:*",
                    b"&orders",
                    b"&baz:qaz",
                    b"&zoo:*",
                ]),
                5,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert!(
            rt.server.pending_client_kills.is_empty(),
            "retaining existing channel grants should not queue disconnects"
        );

        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"SETUSER", b"psuser", b"allchannels"]), 6),
            RespFrame::SimpleString("OK".to_string())
        );
        assert!(
            rt.server.pending_client_kills.is_empty(),
            "broadening to allchannels should not queue disconnects"
        );

        assert_eq!(
            rt.execute_frame(
                command(&[b"ACL", b"SETUSER", b"psuser", b"resetchannels"]),
                7,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(rt.server.pending_client_kills, vec![subscriber.client_id]);
    }

    #[test]
    fn acl_deluser_queues_authenticated_sessions_for_disconnect() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL", b"SETUSER", b"alice", b"on", b">pass", b"+@all", b"~*", b"&*"
                ]),
                0,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL", b"SETUSER", b"bob", b"on", b">pass", b"+@all", b"~*", b"&*"
                ]),
                1,
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        let previous = rt.swap_session(rt.new_session());
        assert_eq!(
            rt.execute_frame(command(&[b"AUTH", b"alice", b"pass"]), 2),
            RespFrame::SimpleString("OK".to_string())
        );
        let alice_one_snapshot = rt.session.clone();
        rt.record_client_session(&alice_one_snapshot);
        let alice_one = rt.swap_session(previous);

        let previous = rt.swap_session(rt.new_session());
        assert_eq!(
            rt.execute_frame(command(&[b"AUTH", b"alice", b"pass"]), 3),
            RespFrame::SimpleString("OK".to_string())
        );
        let alice_two_snapshot = rt.session.clone();
        rt.record_client_session(&alice_two_snapshot);
        let alice_two = rt.swap_session(previous);

        let previous = rt.swap_session(rt.new_session());
        assert_eq!(
            rt.execute_frame(command(&[b"AUTH", b"bob", b"pass"]), 4),
            RespFrame::SimpleString("OK".to_string())
        );
        let bob_snapshot = rt.session.clone();
        rt.record_client_session(&bob_snapshot);
        let _bob = rt.swap_session(previous);

        rt.server.pending_client_kills.clear();
        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"DELUSER", b"alice"]), 5),
            RespFrame::Integer(1)
        );

        let mut kills = rt.server.pending_client_kills.clone();
        kills.sort_unstable();
        let mut expected = vec![alice_one.client_id, alice_two.client_id];
        expected.sort_unstable();
        assert_eq!(kills, expected);
    }

    #[test]
    fn acl_setuser_rejects_channel_pattern_after_allchannels_without_resetchannels() {
        let mut rt = Runtime::default_strict();
        rt.server
            .auth_state
            .set_acl_pubsub_default(AclPubsubDefault::AllChannels);

        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"SETUSER", b"onechannel", b"&test"]), 0),
            RespFrame::Error(
                "ERR Error in ACL SETUSER modifier '&test': Adding a pattern after the * pattern (or the 'allchannels' flag) is not valid and does not have any effect. Try 'resetchannels' to start with an empty list of channels"
                    .to_string(),
            )
        );
    }

    #[test]
    fn acl_help_lists_dryrun_subcommand() {
        let mut rt = Runtime::default_strict();
        let reply = rt.execute_frame(command(&[b"ACL", b"HELP"]), 0);
        let RespFrame::Array(Some(items)) = reply else {
            unreachable!("expected ACL HELP to return an array");
        };
        assert!(
            items.contains(&RespFrame::BulkString(Some(
                b"DRYRUN <username> <command> [<arg> ...]".to_vec()
            ))),
            "ACL HELP should list DRYRUN"
        );
    }

    #[test]
    fn acl_save_and_load_round_trip_persists_users() {
        let mut rt = Runtime::default_strict();
        let acl_path = unique_temp_path("fr_runtime_acl_roundtrip", "acl");
        rt.set_acl_file_path(acl_path.clone());

        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL", b"SETUSER", b"alice", b"on", b">pass", b"-@all", b"+get", b"~*", b"&*"
                ]),
                0
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"SAVE"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );

        let saved = std::fs::read_to_string(&acl_path).expect("ACL SAVE should write acl file");
        assert!(
            saved.contains(
                "user alice reset on sanitize-payload #d74ff0ee8da3b9806b18c877dbf29bbde50b5bd8e4dad7a3a725000feb82e8f1 ~* &* +get"
            ),
            "saved ACL file should contain serialized alice rules, got: {saved}"
        );

        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"DELUSER", b"alice"]), 2),
            RespFrame::Integer(1)
        );
        assert_eq!(
            rt.execute_frame(
                command(&[b"ACL", b"SETUSER", b"bob", b"on", b">pass", b"+@all"]),
                3
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"LOAD"]), 4),
            RespFrame::SimpleString("OK".to_string())
        );

        let users = rt.execute_frame(command(&[b"ACL", b"USERS"]), 5);
        let RespFrame::Array(Some(entries)) = users else {
            unreachable!("expected ACL USERS to return an array");
        };
        assert!(
            entries.contains(&RespFrame::BulkString(Some(b"default".to_vec()))),
            "default user should remain present after ACL LOAD"
        );
        assert!(
            entries.contains(&RespFrame::BulkString(Some(b"alice".to_vec()))),
            "alice should be restored from saved ACL file"
        );
        assert!(
            !entries.contains(&RespFrame::BulkString(Some(b"bob".to_vec()))),
            "bob should not survive ACL LOAD rollback to saved state"
        );

        let list = rt.execute_frame(command(&[b"ACL", b"LIST"]), 6);
        let RespFrame::Array(Some(list_entries)) = list else {
            unreachable!("expected ACL LIST to return an array");
        };
        let alice = list_entries
            .iter()
            .find_map(|entry| match entry {
                RespFrame::BulkString(Some(line))
                    if String::from_utf8_lossy(line).contains("alice") =>
                {
                    Some(String::from_utf8_lossy(line).into_owned())
                }
                _ => None,
            })
            .expect("expected alice in ACL LIST after ACL LOAD");
        assert_eq!(alice, "user alice on sanitize-payload #<hidden> ~* &* +get");

        let _ = std::fs::remove_file(&acl_path);
    }

    #[test]
    fn acl_load_invalid_file_preserves_previous_state() {
        let mut rt = Runtime::default_strict();
        let acl_path = unique_temp_path("fr_runtime_acl_invalid", "acl");
        rt.set_acl_file_path(acl_path.clone());

        assert_eq!(
            rt.execute_frame(
                command(&[
                    b"ACL", b"SETUSER", b"alice", b"on", b">pass", b"-@all", b"+get", b"~*", b"&*"
                ]),
                0
            ),
            RespFrame::SimpleString("OK".to_string())
        );

        let before = rt.execute_frame(command(&[b"ACL", b"LIST"]), 1);
        std::fs::write(&acl_path, "totally invalid acl contents\n").expect("should write acl");

        let reply = rt.execute_frame(command(&[b"ACL", b"LOAD"]), 2);
        assert!(
            matches!(reply, RespFrame::Error(ref err) if err == "ERR /ACL file contains invalid format"),
            "ACL LOAD should fail on invalid file, got: {reply:?}"
        );
        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"LIST"]), 3),
            before,
            "failed ACL LOAD must preserve prior auth state"
        );

        let _ = std::fs::remove_file(&acl_path);
    }

    #[test]
    fn acl_save_and_load_require_configured_aclfile() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"SAVE"]), 0),
            RespFrame::Error("ERR There is no configured ACL file".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"ACL", b"LOAD"]), 1),
            RespFrame::Error("ERR There is no configured ACL file".to_string())
        );
    }

    #[test]
    fn canonicalize_acl_rules_round_trip_is_stable() {
        let content = "\
# comment
user alice reset on >pass -@all +get ~* &*
user bob reset off nopass +@all
";
        let canonical = canonicalize_acl_rules(content).expect("valid ACL file should parse");
        assert_eq!(
            canonicalize_acl_rules(&canonical).expect("canonical ACL should reparse"),
            canonical,
            "canonical ACL serialization should be stable across reparses"
        );
        assert_eq!(
            acl_list_entries_from_rules(content).expect("valid ACL file should list"),
            acl_list_entries_from_rules(&canonical).expect("canonical ACL should list"),
            "ACL LIST view should remain stable after canonicalization"
        );
    }

    #[test]
    fn canonicalize_acl_rules_rejects_invalid_lines() {
        assert_eq!(
            canonicalize_acl_rules("not-a-user-line\n"),
            Err("ERR /ACL file contains invalid format".to_string())
        );
    }

    #[test]
    fn client_no_touch_skips_lru_updates_except_touch_command() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"k", b"v"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"CLIENT", b"NO-TOUCH", b"ON"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"GET", b"k"]), 1_000),
            RespFrame::BulkString(Some(b"v".to_vec()))
        );
        assert_eq!(
            rt.execute_frame(command(&[b"OBJECT", b"IDLETIME", b"k"]), 2_000),
            RespFrame::Integer(2)
        );
        assert_eq!(
            rt.execute_frame(command(&[b"TOUCH", b"k"]), 2_500),
            RespFrame::Integer(1)
        );
        assert_eq!(
            rt.execute_frame(command(&[b"OBJECT", b"IDLETIME", b"k"]), 3_000),
            RespFrame::Integer(0)
        );
    }

    #[test]
    fn object_idletime_matches_missing_key_and_lfu_policy_errors() {
        let mut rt = Runtime::default_strict();

        assert_eq!(
            rt.execute_frame(command(&[b"OBJECT", b"IDLETIME", b"missing"]), 0),
            RespFrame::BulkString(None)
        );

        assert_eq!(
            rt.execute_frame(command(&[b"SET", b"k", b"v"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"SET", b"maxmemory-policy", b"allkeys-lfu",]),
                2,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"OBJECT", b"IDLETIME", b"k"]), 3),
            RespFrame::Error(
                "ERR An LFU maxmemory policy is selected, idle time not tracked. Please note that when switching between policies at runtime LRU and LFU data will take some time to adjust.".to_string()
            )
        );
    }

    #[test]
    fn fr_p2c_006_waitaof_promoted_replica_local_acknowledgment_semantics() {
        let mut rt = Runtime::default_strict();
        rt.set_aof_path(std::path::PathBuf::from("appendonly.aof"));

        // Setup as a replica
        assert_eq!(
            rt.execute_frame(command(&[b"REPLICAOF", b"127.0.0.1", b"6379"]), 0),
            RespFrame::SimpleString("OK".to_string())
        );

        // Receive some replicated commands, advancing primary offset
        rt.server.replication_ack_state.primary_offset = fr_repl::ReplOffset(100);
        // But local fsync offset is lagging
        rt.server.replication_ack_state.local_fsync_offset = fr_repl::ReplOffset(10);

        // Promote to master
        assert_eq!(
            rt.execute_frame(command(&[b"REPLICAOF", b"NO", b"ONE"]), 1),
            RespFrame::SimpleString("OK".to_string())
        );

        // WAITAOF for offset 100 should immediately satisfy local threshold
        // because it was promoted at offset 100
        let waitaof_reply = rt.execute_frame(command(&[b"WAITAOF", b"1", b"0", b"0"]), 2);
        assert_eq!(
            waitaof_reply,
            RespFrame::Array(Some(vec![RespFrame::Integer(1), RespFrame::Integer(0)]))
        );
    }
}
