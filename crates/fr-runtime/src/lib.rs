#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicU64, Ordering};

use fr_command::{CommandError, dispatch_argv, frame_to_argv};
use fr_config::{
    DecisionAction, DriftSeverity, HardenedDeviationCategory, Mode, RuntimePolicy, ThreatClass,
    TlsCfgError, TlsConfig, TlsListenerTransition, TlsRuntimeState,
    evaluate_tls_hardened_deviation, plan_tls_runtime_apply,
};
use fr_eventloop::{
    AcceptPathError, BarrierOrderError, BootstrapError, CallbackDispatchOrder, EventLoopMode,
    EventLoopPhase, FdRegistrationError, LoopBootstrap, PendingWriteError, PhaseReplayError,
    ReadPathError, TickBudget, TickPlan, apply_tls_accept_rate_limit, plan_fd_setsize_growth,
    plan_readiness_callback_order, plan_tick, replay_phase_trace, validate_accept_path,
    validate_ae_barrier_order, validate_bootstrap, validate_fd_registration_bounds,
    validate_pending_write_delivery, validate_read_path,
};
use fr_protocol::{RespFrame, RespParseError, parse_frame};
use fr_store::Store;

static PACKET_COUNTER: AtomicU64 = AtomicU64::new(1);
const DEFAULT_AUTH_USER: &[u8] = b"default";
const NOAUTH_ERROR: &str = "NOAUTH Authentication required.";
const WRONGPASS_ERROR: &str = "WRONGPASS invalid username-password pair or user is disabled.";
const AUTH_NOT_CONFIGURED_ERROR: &str = "ERR AUTH <password> called without any password configured for the default user. Are you sure your configuration is correct?";
const CLUSTER_UNKNOWN_SUBCOMMAND_ERROR: &str =
    "ERR Unknown subcommand or wrong number of arguments for 'CLUSTER'. Try CLUSTER HELP.";

#[derive(Debug, Clone, PartialEq, Eq)]
struct AuthState {
    requirepass: Option<Vec<u8>>,
    authenticated_user: Option<Vec<u8>>,
}

impl Default for AuthState {
    fn default() -> Self {
        Self {
            requirepass: None,
            authenticated_user: Some(DEFAULT_AUTH_USER.to_vec()),
        }
    }
}

impl AuthState {
    fn set_requirepass(&mut self, requirepass: Option<Vec<u8>>) {
        self.requirepass = requirepass;
        if self.requirepass.is_some() {
            self.authenticated_user = None;
        } else {
            self.authenticated_user = Some(DEFAULT_AUTH_USER.to_vec());
        }
    }

    fn is_authenticated(&self) -> bool {
        self.authenticated_user.is_some()
    }

    fn requires_auth(&self) -> bool {
        self.requirepass.is_some() && !self.is_authenticated()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClusterClientMode {
    ReadWrite,
    ReadOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RuntimeSpecialCommand {
    Auth,
    Hello,
    Asking,
    Readonly,
    Readwrite,
    Cluster,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClusterSubcommand {
    Help,
    Unknown,
}

#[inline]
fn classify_runtime_special_command(cmd: &[u8]) -> Option<RuntimeSpecialCommand> {
    match cmd.len() {
        4 => {
            if eq_ascii_token(cmd, b"AUTH") {
                Some(RuntimeSpecialCommand::Auth)
            } else {
                None
            }
        }
        5 => {
            if eq_ascii_token(cmd, b"HELLO") {
                Some(RuntimeSpecialCommand::Hello)
            } else {
                None
            }
        }
        6 => {
            if eq_ascii_token(cmd, b"ASKING") {
                Some(RuntimeSpecialCommand::Asking)
            } else {
                None
            }
        }
        7 => {
            if eq_ascii_token(cmd, b"CLUSTER") {
                Some(RuntimeSpecialCommand::Cluster)
            } else {
                None
            }
        }
        8 => {
            if eq_ascii_token(cmd, b"READONLY") {
                Some(RuntimeSpecialCommand::Readonly)
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
        _ => None,
    }
}

#[cfg(test)]
fn classify_runtime_special_command_linear(cmd: &[u8]) -> Option<RuntimeSpecialCommand> {
    let command = std::str::from_utf8(cmd).ok()?;
    if command.eq_ignore_ascii_case("AUTH") {
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
    evidence: EvidenceLedger,
    tls_state: TlsRuntimeState,
    auth_state: AuthState,
    cluster_state: ClusterClientState,
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
            evidence: EvidenceLedger::default(),
            tls_state: TlsRuntimeState::default(),
            auth_state: AuthState::default(),
            cluster_state: ClusterClientState::default(),
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
    pub fn tls_runtime_state(&self) -> &TlsRuntimeState {
        &self.tls_state
    }

    pub fn set_requirepass(&mut self, requirepass: Option<Vec<u8>>) {
        self.auth_state.set_requirepass(requirepass);
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
            Some(RuntimeSpecialCommand::Asking) => return self.handle_asking_command(&argv),
            Some(RuntimeSpecialCommand::Readonly) => return self.handle_readonly_command(&argv),
            Some(RuntimeSpecialCommand::Readwrite) => return self.handle_readwrite_command(&argv),
            Some(RuntimeSpecialCommand::Cluster) => return self.handle_cluster_command(&argv),
            _ => {}
        }

        match dispatch_argv(&argv, &mut self.store, now_ms) {
            Ok(reply) => reply,
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

    fn handle_auth_command(&mut self, argv: &[Vec<u8>]) -> RespFrame {
        if argv.len() != 2 && argv.len() != 3 {
            return command_error_to_resp(CommandError::WrongArity("AUTH"));
        }

        let Some(required_password) = self.auth_state.requirepass.as_deref() else {
            return RespFrame::Error(AUTH_NOT_CONFIGURED_ERROR.to_string());
        };

        let (username, password) = if argv.len() == 2 {
            (DEFAULT_AUTH_USER, argv[1].as_slice())
        } else {
            (argv[1].as_slice(), argv[2].as_slice())
        };

        if username == DEFAULT_AUTH_USER && password == required_password {
            self.auth_state.authenticated_user = Some(DEFAULT_AUTH_USER.to_vec());
            return RespFrame::SimpleString("OK".to_string());
        }

        RespFrame::Error(WRONGPASS_ERROR.to_string())
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

        let mut index = 2;
        let mut auth_credentials: Option<(&[u8], &[u8])> = None;
        while index < argv.len() {
            let option = match std::str::from_utf8(&argv[index]) {
                Ok(option) => option,
                Err(_) => return command_error_to_resp(CommandError::InvalidUtf8Argument),
            };
            if option.eq_ignore_ascii_case("AUTH") {
                if index + 2 >= argv.len() {
                    return command_error_to_resp(CommandError::SyntaxError);
                }
                auth_credentials = Some((argv[index + 1].as_slice(), argv[index + 2].as_slice()));
                index += 3;
                continue;
            }
            return command_error_to_resp(CommandError::SyntaxError);
        }

        if let Some((username, password)) = auth_credentials {
            let Some(required_password) = self.auth_state.requirepass.as_deref() else {
                return RespFrame::Error(AUTH_NOT_CONFIGURED_ERROR.to_string());
            };
            if username != DEFAULT_AUTH_USER || password != required_password {
                return RespFrame::Error(WRONGPASS_ERROR.to_string());
            }
            self.auth_state.authenticated_user = Some(DEFAULT_AUTH_USER.to_vec());
        } else if self.auth_state.requires_auth() {
            return RespFrame::Error(NOAUTH_ERROR.to_string());
        }

        build_hello_response(protocol_version)
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
            fr_store::StoreError::IntegerOverflow => {
                RespFrame::Error("ERR increment or decrement would overflow".to_string())
            }
            fr_store::StoreError::KeyNotFound => RespFrame::Error("ERR no such key".to_string()),
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
        AcceptPathError, BarrierOrderError, EVENT_LOOP_PHASE_ORDER, EventLoopMode, EventLoopPhase,
        FdRegistrationError, LoopBootstrap, PendingWriteError, ReadPathError, ReadinessCallback,
        TickBudget,
    };
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
    fn fr_p2c_004_runtime_special_command_classifier_matches_linear_reference() {
        let samples: &[&[u8]] = &[
            b"AUTH",
            b"auth",
            b"HeLlO",
            b"ASKING",
            b"readonly",
            b"READWRITE",
            b"cluster",
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
            b"AUTH",
            b"HELLO",
            b"READONLY",
            b"READWRITE",
            b"CLUSTER",
            b"ASKING",
            b"DEL",
            b"MGET",
            b"MSET",
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
}
